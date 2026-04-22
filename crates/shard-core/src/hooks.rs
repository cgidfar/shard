//! Harness hook installers for coding agent integrations.
//!
//! Each harness (Claude Code, Codex, etc.) has an installer that configures the
//! agent to send activity state notifications back to the Shard supervisor via
//! `shardctl notify`.

use std::path::{Path, PathBuf};

use crate::Result;
use crate::error::ShardError;

/// The four Claude Code hook events we register, paired with the shard
/// activity state each event maps to. Kept as a single source of truth so
/// installer and predicate agree on what "fully configured" means.
const CLAUDE_HOOK_EVENTS: &[(&str, &str)] = &[
    ("UserPromptSubmit", "active"),
    ("PreToolUse", "active"),
    ("PermissionRequest", "blocked"),
    ("Stop", "idle"),
];

/// Install Claude Code hooks that report activity state to the supervisor.
///
/// Convenience wrapper over [`install_claude_code_hooks_in_home`] that
/// resolves the home directory from the current user profile. If Claude
/// Code is not installed (no `~/.claude/` directory), this is a no-op.
///
/// **Postcondition vs write-occurred:** `Ok(())` does not imply hooks are
/// present afterwards — the no-`.claude/` case returns `Ok(())` without
/// writing anything. Use [`claude_code_hooks_installed`] to check the
/// postcondition directly.
pub fn install_claude_code_hooks(shardctl_path: &Path) -> Result<()> {
    let home = home_dir()?;
    install_claude_code_hooks_in_home(&home, shardctl_path)
}

/// Check if Claude Code hooks are already fully installed for the given
/// shardctl path. Convenience wrapper over
/// [`claude_code_hooks_installed_in_home`].
pub fn claude_code_hooks_installed(shardctl_path: &Path) -> bool {
    let Ok(home) = home_dir() else { return false };
    claude_code_hooks_installed_in_home(&home, shardctl_path)
}

/// Install Claude Code hooks against an explicit home directory.
///
/// Test seam for the daemon integration tests: production callers use the
/// convenience wrapper [`install_claude_code_hooks`]. Parameterizing the
/// home directory avoids touching the developer's real
/// `~/.claude/settings.json` from tests without the unsafe
/// `std::env::set_var` dance.
///
/// Modifies `<home>/.claude/settings.json` to register command hooks on
/// the four Claude Code lifecycle events in [`CLAUDE_HOOK_EVENTS`]. The
/// hooks invoke `shardctl notify <state>` which sends an
/// `ActivityUpdate` frame to the supervisor's named pipe.
///
/// The installation is idempotent — safe to call repeatedly. Any stale
/// `shardctl`-containing entries are stripped before the current ones
/// are appended.
pub fn install_claude_code_hooks_in_home(home: &Path, shardctl_path: &Path) -> Result<()> {
    let claude_dir = home.join(".claude");
    if !claude_dir.exists() {
        tracing::debug!("{} not found, skipping Claude Code hook install", claude_dir.display());
        return Ok(());
    }

    let settings_path = claude_dir.join("settings.json");

    // Read existing settings or start fresh
    let mut settings: serde_json::Value = if settings_path.exists() {
        let content = std::fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content)
            .map_err(|e| ShardError::Other(format!("failed to parse settings.json: {e}")))?
    } else {
        serde_json::json!({})
    };

    let shardctl = shardctl_command_string(shardctl_path);

    let hooks_obj = settings
        .as_object_mut()
        .ok_or_else(|| ShardError::Other("settings.json root is not an object".into()))?
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));

    let hooks_map = hooks_obj
        .as_object_mut()
        .ok_or_else(|| ShardError::Other("settings.json hooks is not an object".into()))?;

    for (event, state) in CLAUDE_HOOK_EVENTS {
        let command = format!("{shardctl} notify {state}");

        let event_hooks = hooks_map
            .entry(*event)
            .or_insert_with(|| serde_json::json!([]));

        let arr = event_hooks
            .as_array_mut()
            .ok_or_else(|| ShardError::Other(format!("hooks.{event} is not an array")))?;

        // Remove any existing shard hook entry (idempotent update).
        // Identify ours by checking if any inner hook command contains "shardctl".
        arr.retain(|entry| !entry_contains_shardctl(entry));

        // Add our hook entry — no matcher so it fires for all tools/events
        arr.push(serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": command,
            }],
        }));
    }

    // Atomic write: write to temp file then rename
    let tmp_path = settings_path.with_extension("json.tmp");
    let formatted = serde_json::to_string_pretty(&settings)
        .map_err(|e| ShardError::Other(format!("failed to serialize settings: {e}")))?;
    std::fs::write(&tmp_path, &formatted)?;
    if let Err(e) = std::fs::rename(&tmp_path, &settings_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(ShardError::Other(format!(
            "failed to update settings.json (is Claude Code running?): {e}"
        )));
    }

    tracing::info!("installed Claude Code hooks in {}", settings_path.display());
    Ok(())
}

/// Check whether the post-install state is already in place for the given
/// `home` + `shardctl_path`. Predicate is deliberately narrow ("the install
/// would be a no-op"): every event in [`CLAUDE_HOOK_EVENTS`] must contain
/// exactly one shard-owned entry whose command is the freshly-rendered
/// `<shardctl> notify <state>`. Partial configs (only some events wired),
/// stale configs (pointing at an old shardctl path), and duplicate shard
/// entries all return `false` so the installer runs and converges the file
/// to the correct shape.
///
/// Non-shard entries (anything whose inner command doesn't contain
/// `"shardctl"`) are ignored — users may have their own hooks mixed in.
pub fn claude_code_hooks_installed_in_home(home: &Path, shardctl_path: &Path) -> bool {
    let settings_path = home.join(".claude").join("settings.json");

    let Ok(content) = std::fs::read_to_string(&settings_path) else {
        return false;
    };
    let Ok(settings) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };

    let shardctl = shardctl_command_string(shardctl_path);

    let Some(hooks) = settings.get("hooks").and_then(|h| h.as_object()) else {
        return false;
    };

    for (event, state) in CLAUDE_HOOK_EVENTS {
        let expected = format!("{shardctl} notify {state}");
        let Some(arr) = hooks.get(*event).and_then(|a| a.as_array()) else {
            return false;
        };

        // Count shard-owned entries and check they match the expected command.
        let mut matching = 0usize;
        let mut mismatch = false;
        for entry in arr {
            if !entry_contains_shardctl(entry) {
                continue;
            }
            if entry_has_command(entry, &expected) {
                matching += 1;
            } else {
                mismatch = true;
            }
        }

        if mismatch || matching != 1 {
            return false;
        }
    }

    true
}

/// Render the shardctl path the way the installer writes it into
/// settings.json (backslashes normalized to forward slashes). Kept as a
/// helper so the predicate and installer agree byte-for-byte.
fn shardctl_command_string(shardctl_path: &Path) -> String {
    shardctl_path.to_string_lossy().replace('\\', "/")
}

/// Does an event-array entry contain a nested `hooks[].command` that
/// mentions `shardctl`? Matches the installer's retain-predicate so the
/// "is this one of ours" check stays symmetric.
fn entry_contains_shardctl(entry: &serde_json::Value) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .map_or(false, |hooks| {
            hooks.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .map_or(false, |c| c.contains("shardctl"))
            })
        })
}

/// Does every nested `hooks[].command` in this entry equal `expected`?
/// Tight equality on purpose: a stale shardctl path (different directory,
/// different state mapping, extra flags) mismatches and forces a rewrite.
fn entry_has_command(entry: &serde_json::Value, expected: &str) -> bool {
    let Some(inner) = entry.get("hooks").and_then(|h| h.as_array()) else {
        return false;
    };
    if inner.is_empty() {
        return false;
    }
    inner.iter().all(|h| {
        h.get("command")
            .and_then(|c| c.as_str())
            .map_or(false, |c| c == expected)
    })
}

fn home_dir() -> Result<PathBuf> {
    default_hooks_home()
        .ok_or_else(|| ShardError::Other("cannot determine home directory".into()))
}

/// Resolve the user's home directory via `directories::UserDirs::new()`.
/// Returns `None` on the rare platforms where no home can be derived.
///
/// Exposed so the daemon can pick between an explicit-home override
/// (test seam) and the real user home without re-importing the
/// `directories` crate in every consumer.
pub fn default_hooks_home() -> Option<PathBuf> {
    directories::UserDirs::new().map(|d| d.home_dir().to_path_buf())
}
