//! Harness hook installers for coding agent integrations.
//!
//! Each harness (Claude Code, Codex, etc.) has an installer that configures the
//! agent to send activity state notifications back to the Shard supervisor via
//! `shardctl notify`.

use std::path::{Path, PathBuf};

use crate::Result;
use crate::error::ShardError;

/// Install Claude Code hooks that report activity state to the supervisor.
///
/// This modifies `~/.claude/settings.json` to register command hooks on key
/// lifecycle events. The hooks invoke `shardctl notify <state>` which sends
/// an `ActivityUpdate` frame to the supervisor's named pipe.
///
/// The installation is idempotent — safe to call on every session creation.
/// If Claude Code is not installed (no `~/.claude/` directory), this is a no-op.
pub fn install_claude_code_hooks(shardctl_path: &Path) -> Result<()> {
    let claude_dir = home_dir()?.join(".claude");
    if !claude_dir.exists() {
        tracing::debug!("~/.claude/ not found, skipping Claude Code hook install");
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

    let shardctl = shardctl_path.to_string_lossy().replace('\\', "/");

    // Build hook entries for each event
    let hook_events = [
        ("UserPromptSubmit", "active"),
        ("PreToolUse", "active"),
        ("PermissionRequest", "blocked"),
        ("Stop", "idle"),
    ];

    let hooks_obj = settings
        .as_object_mut()
        .ok_or_else(|| ShardError::Other("settings.json root is not an object".into()))?
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));

    let hooks_map = hooks_obj
        .as_object_mut()
        .ok_or_else(|| ShardError::Other("settings.json hooks is not an object".into()))?;

    for (event, state) in &hook_events {
        let command = format!("{shardctl} notify {state}");

        let event_hooks = hooks_map
            .entry(*event)
            .or_insert_with(|| serde_json::json!([]));

        let arr = event_hooks
            .as_array_mut()
            .ok_or_else(|| ShardError::Other(format!("hooks.{event} is not an array")))?;

        // Remove any existing shard hook entry (idempotent update).
        // Identify ours by checking if any inner hook command contains "shardctl".
        arr.retain(|entry| {
            !entry
                .get("hooks")
                .and_then(|h| h.as_array())
                .map_or(false, |hooks| {
                    hooks.iter().any(|h| {
                        h.get("command")
                            .and_then(|c| c.as_str())
                            .map_or(false, |c| c.contains("shardctl"))
                    })
                })
        });

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

/// Check if Claude Code hooks are already installed.
pub fn claude_code_hooks_installed() -> bool {
    let Ok(home) = home_dir() else { return false };
    let settings_path = home.join(".claude").join("settings.json");

    let Ok(content) = std::fs::read_to_string(&settings_path) else {
        return false;
    };
    let Ok(settings) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };

    // Check if any hook command contains "shardctl"
    settings
        .get("hooks")
        .and_then(|h| h.as_object())
        .map(|hooks| {
            hooks.values().any(|arr| {
                arr.as_array().map_or(false, |entries| {
                    entries.iter().any(|entry| {
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
                    })
                })
            })
        })
        .unwrap_or(false)
}

fn home_dir() -> Result<PathBuf> {
    directories::UserDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .ok_or_else(|| ShardError::Other("cannot determine home directory".into()))
}
