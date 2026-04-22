//! Integration tests for Phase 5 (Batch D): `ControlFrame::InstallHarnessHooks`.
//!
//! Each test starts a daemon with `state.hooks_home_override = Some(tempdir)`
//! so the handler operates against an isolated home directory. Without that
//! seam the tests would pollute the developer's real `~/.claude/settings.json`
//! — a sharp edge the Phase 5 plan explicitly calls out.
//!
//! Coverage maps 1:1 to the ack matrix documented in
//! `docs/daemon-broker-migration.md` Phase 5:
//!   - install written when no prior config
//!   - skipped when `.claude/` absent
//!   - idempotent (already configured)
//!   - codex variant returns not-yet-implemented
//!   - unknown harness soft-skips (not Error)
//!   - concurrent installs serialize (ack multiset + final-file assertions)
//!   - non-shard settings keys survive the rewrite
//!   - malformed settings.json surfaces as Error
//!   - partial/stale shard config converges (Codex round 2 finding)

#![cfg(windows)]

mod common;

use std::path::Path;
use std::path::PathBuf;

use common::{HarnessOptions, TestHarness};
use shard_transport::control_protocol::ControlFrame;
use tempfile::TempDir;

// ── RPC helper ──────────────────────────────────────────────────────────────

async fn install_hooks_rpc(harness: &TestHarness, harness_arg: &str) -> ControlFrame {
    let mut conn = harness.connect().await;
    conn.request(&ControlFrame::InstallHarnessHooks {
        harness: harness_arg.to_string(),
    })
    .await
    .expect("InstallHarnessHooks RPC")
}

fn settings_path(home: &Path) -> PathBuf {
    home.join(".claude").join("settings.json")
}

fn read_settings_json(home: &Path) -> serde_json::Value {
    let content = std::fs::read_to_string(settings_path(home)).expect("read settings.json");
    serde_json::from_str(&content).expect("parse settings.json")
}

/// Count shard-owned entries (any entry whose inner hooks contain a
/// `shardctl` command) under each event. Returns a map ordered by the
/// event names the installer writes.
fn shard_entry_counts(settings: &serde_json::Value) -> Vec<(String, usize)> {
    let mut result = Vec::new();
    let Some(hooks) = settings.get("hooks").and_then(|h| h.as_object()) else {
        return result;
    };
    for (event, arr) in hooks {
        let Some(arr) = arr.as_array() else { continue };
        let count = arr
            .iter()
            .filter(|entry| {
                entry
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|inner| {
                        inner.iter().any(|h| {
                            h.get("command")
                                .and_then(|c| c.as_str())
                                .map(|c| c.contains("shardctl"))
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            })
            .count();
        result.push((event.clone(), count));
    }
    result.sort();
    result
}

fn assert_all_events_one_shard_entry(settings: &serde_json::Value) {
    let expected_events = ["PermissionRequest", "PreToolUse", "Stop", "UserPromptSubmit"];
    let counts = shard_entry_counts(settings);
    let seen: Vec<String> = counts.iter().map(|(e, _)| e.clone()).collect();
    for event in &expected_events {
        assert!(
            seen.iter().any(|e| e == event),
            "event {event} missing from settings.json: {seen:?}",
        );
    }
    for (event, count) in &counts {
        if expected_events.contains(&event.as_str()) {
            assert_eq!(
                *count, 1,
                "event {event} should have exactly one shard entry, got {count}",
            );
        }
    }
}

// ── Happy path + ack matrix rows ────────────────────────────────────────────

#[tokio::test]
async fn install_claude_code_happy_path() {
    let home = TempDir::new().expect("tempdir");
    std::fs::create_dir_all(home.path().join(".claude")).expect("create .claude");

    let harness = TestHarness::start_with(HarnessOptions {
        hooks_home_override: Some(home.path().to_path_buf()),
        ..Default::default()
    })
    .await;

    match install_hooks_rpc(&harness, "claude-code").await {
        ControlFrame::InstallHarnessHooksAck {
            installed,
            skipped_reason,
        } => {
            assert!(installed, "happy path should Ack installed=true");
            assert_eq!(skipped_reason, None, "no reason on fresh install");
        }
        other => panic!("unexpected response: {other:?}"),
    }

    assert!(
        settings_path(home.path()).exists(),
        "settings.json should exist after install",
    );
    let settings = read_settings_json(home.path());
    assert_all_events_one_shard_entry(&settings);

    harness.shutdown().await;
}

#[tokio::test]
async fn install_claude_code_skips_when_dir_absent() {
    // Empty temp home: no .claude/ subdirectory. Mirrors the "claude
    // code not installed" case — the original plan missed this (Codex
    // round 1 HIGH).
    let home = TempDir::new().expect("tempdir");

    let harness = TestHarness::start_with_hooks_home(home.path().to_path_buf()).await;

    match install_hooks_rpc(&harness, "claude-code").await {
        ControlFrame::InstallHarnessHooksAck {
            installed,
            skipped_reason,
        } => {
            assert!(!installed, "no .claude/ dir → installed=false");
            let reason = skipped_reason.expect("skipped_reason required");
            assert!(
                reason.contains("claude code not installed"),
                "reason should identify missing claude code: {reason:?}",
            );
        }
        other => panic!("unexpected response: {other:?}"),
    }

    assert!(
        !settings_path(home.path()).exists(),
        "settings.json should NOT be created when .claude/ is absent",
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn install_claude_code_idempotent() {
    let home = TempDir::new().expect("tempdir");
    std::fs::create_dir_all(home.path().join(".claude")).expect("create .claude");

    let harness = TestHarness::start_with_hooks_home(home.path().to_path_buf()).await;

    // First call — fresh install.
    match install_hooks_rpc(&harness, "claude-code").await {
        ControlFrame::InstallHarnessHooksAck {
            installed: true,
            skipped_reason: None,
        } => {}
        other => panic!("first call unexpected: {other:?}"),
    }

    let settings_before = read_settings_json(home.path());

    // Second call — predicate says "install would be no-op".
    match install_hooks_rpc(&harness, "claude-code").await {
        ControlFrame::InstallHarnessHooksAck {
            installed,
            skipped_reason,
        } => {
            assert!(installed, "already-configured also reports installed=true");
            assert_eq!(
                skipped_reason.as_deref(),
                Some("already configured"),
                "reason must carry the already-configured signal",
            );
        }
        other => panic!("second call unexpected: {other:?}"),
    }

    let settings_after = read_settings_json(home.path());
    assert_eq!(
        settings_before, settings_after,
        "idempotent call must not rewrite settings.json contents",
    );
    assert_all_events_one_shard_entry(&settings_after);

    harness.shutdown().await;
}

#[tokio::test]
async fn install_codex_returns_skipped() {
    let home = TempDir::new().expect("tempdir");
    // Doesn't matter whether .claude/ exists — Codex installer is a
    // no-op regardless.
    let harness = TestHarness::start_with_hooks_home(home.path().to_path_buf()).await;

    match install_hooks_rpc(&harness, "codex").await {
        ControlFrame::InstallHarnessHooksAck {
            installed,
            skipped_reason,
        } => {
            assert!(!installed, "Codex installer is unimplemented → false");
            // Verbatim wording from the plan — Phase 5 settled on this
            // string exactly. Round 3 flagged the inconsistency.
            assert_eq!(
                skipped_reason.as_deref(),
                Some("codex hooks not yet implemented"),
            );
        }
        other => panic!("unexpected response: {other:?}"),
    }

    harness.shutdown().await;
}

#[tokio::test]
async fn install_unknown_harness_soft_skips() {
    let home = TempDir::new().expect("tempdir");
    let harness = TestHarness::start_with_hooks_home(home.path().to_path_buf()).await;

    match install_hooks_rpc(&harness, "fictional-harness").await {
        ControlFrame::InstallHarnessHooksAck {
            installed,
            skipped_reason,
        } => {
            assert!(!installed, "unknown harness soft-skips with installed=false");
            let reason = skipped_reason.expect("soft-skip must carry a reason");
            assert!(
                reason.contains("unknown harness"),
                "reason should identify unknown harness: {reason:?}",
            );
        }
        ControlFrame::Error { message } => {
            panic!(
                "unknown harness should soft-skip with Ack, not Error; got: {message}",
            );
        }
        other => panic!("unexpected response: {other:?}"),
    }

    harness.shutdown().await;
}

// ── Concurrency + state-preservation ────────────────────────────────────────

#[tokio::test]
async fn install_concurrent_serializes() {
    let home = TempDir::new().expect("tempdir");
    std::fs::create_dir_all(home.path().join(".claude")).expect("create .claude");

    let harness = TestHarness::start_with_hooks_home(home.path().to_path_buf()).await;

    // Two parallel RPCs on independent connections.
    let (a, b) = tokio::join!(
        install_hooks_rpc(&harness, "claude-code"),
        install_hooks_rpc(&harness, "claude-code"),
    );

    // Ack multiset assertion: one call must write, the other must
    // observe the written state and report "already configured". If
    // the mutex wrapped only the install (not the query), both could
    // see "not installed" and both would perform a write.
    let mut acks: Vec<(bool, Option<String>)> = Vec::new();
    for ack in [a, b] {
        match ack {
            ControlFrame::InstallHarnessHooksAck {
                installed,
                skipped_reason,
            } => acks.push((installed, skipped_reason)),
            other => panic!("unexpected response: {other:?}"),
        }
    }
    acks.sort_by_key(|(_, reason)| reason.clone());

    assert_eq!(acks.len(), 2);
    let fresh_installs = acks.iter().filter(|(_, r)| r.is_none()).count();
    let already_configured = acks
        .iter()
        .filter(|(_, r)| r.as_deref() == Some("already configured"))
        .count();
    assert_eq!(
        fresh_installs, 1,
        "exactly one concurrent call should write; acks={acks:?}",
    );
    assert_eq!(
        already_configured, 1,
        "exactly one concurrent call should see already-configured; acks={acks:?}",
    );
    assert!(
        acks.iter().all(|(installed, _)| *installed),
        "both concurrent calls must report installed=true as postcondition",
    );

    // Final-file assertion: each of the 4 expected events must have
    // exactly one shard entry. Without the mutex wrapping query+install,
    // the racing writes could interleave and leave duplicates.
    let settings = read_settings_json(home.path());
    assert_all_events_one_shard_entry(&settings);

    harness.shutdown().await;
}

#[tokio::test]
async fn install_with_existing_settings_preserves_other_keys() {
    let home = TempDir::new().expect("tempdir");
    let claude_dir = home.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).expect("create .claude");

    // Seed settings.json with an unrelated top-level key; the installer
    // must not clobber it.
    let seeded = serde_json::json!({
        "theme": "dark",
        "hooks": {},
    });
    std::fs::write(
        claude_dir.join("settings.json"),
        serde_json::to_string_pretty(&seeded).unwrap(),
    )
    .expect("write seeded settings");

    let harness = TestHarness::start_with_hooks_home(home.path().to_path_buf()).await;

    match install_hooks_rpc(&harness, "claude-code").await {
        ControlFrame::InstallHarnessHooksAck {
            installed: true,
            skipped_reason: None,
        } => {}
        other => panic!("unexpected response: {other:?}"),
    }

    let settings = read_settings_json(home.path());
    assert_eq!(
        settings.get("theme").and_then(|v| v.as_str()),
        Some("dark"),
        "unrelated keys must survive the rewrite",
    );
    assert_all_events_one_shard_entry(&settings);

    harness.shutdown().await;
}

#[tokio::test]
async fn install_malformed_settings_errors() {
    let home = TempDir::new().expect("tempdir");
    let claude_dir = home.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).expect("create .claude");

    // Non-JSON garbage in settings.json — installer should return Error,
    // not a vacuous Ack.
    std::fs::write(claude_dir.join("settings.json"), "{ this is not json")
        .expect("write garbage");

    let harness = TestHarness::start_with_hooks_home(home.path().to_path_buf()).await;

    match install_hooks_rpc(&harness, "claude-code").await {
        ControlFrame::Error { message } => {
            assert!(
                message.contains("install claude code hooks")
                    || message.contains("parse settings"),
                "error message should identify the failure: {message}",
            );
        }
        other => panic!("expected Error frame, got: {other:?}"),
    }

    harness.shutdown().await;
}

#[tokio::test]
async fn install_partial_stale_config_converges() {
    // Codex round 2 finding: the old predicate returned `true` if any
    // entry contained a `shardctl` substring anywhere under `hooks`.
    // A partial config (only 2 of 4 events wired) or a stale one
    // (different shardctl path than the current daemon) would then
    // mis-skip via the "already configured" branch even though the
    // postcondition was false. The tightened predicate forces both
    // cases into the install branch, which converges to the 4-event
    // shape via the existing retain-then-append logic.
    let home = TempDir::new().expect("tempdir");
    let claude_dir = home.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).expect("create .claude");

    // Seed: only 2 of the 4 events wired, and with a stale shardctl
    // path that won't match the daemon's exe_path.
    let stale = serde_json::json!({
        "hooks": {
            "UserPromptSubmit": [{
                "hooks": [{
                    "type": "command",
                    "command": "C:/old/path/shardctl notify active",
                }],
            }],
            "Stop": [{
                "hooks": [{
                    "type": "command",
                    "command": "C:/old/path/shardctl notify idle",
                }],
            }],
        },
    });
    std::fs::write(
        claude_dir.join("settings.json"),
        serde_json::to_string_pretty(&stale).unwrap(),
    )
    .expect("write stale settings");

    let harness = TestHarness::start_with_hooks_home(home.path().to_path_buf()).await;

    match install_hooks_rpc(&harness, "claude-code").await {
        ControlFrame::InstallHarnessHooksAck {
            installed,
            skipped_reason,
        } => {
            assert!(installed, "post-converge installed=true");
            assert_eq!(
                skipped_reason, None,
                "partial/stale seed should drop into install branch — \
                 the plan's tightened predicate would return None here, \
                 not already-configured: {skipped_reason:?}",
            );
        }
        other => panic!("unexpected response: {other:?}"),
    }

    // All 4 events are now present with exactly one shard entry, and
    // the stale path is gone.
    let settings = read_settings_json(home.path());
    assert_all_events_one_shard_entry(&settings);
    let hooks = settings
        .get("hooks")
        .and_then(|h| h.as_object())
        .expect("hooks object");
    for (_event, arr) in hooks {
        let arr = arr.as_array().expect("event array");
        for entry in arr {
            let inner = entry
                .get("hooks")
                .and_then(|h| h.as_array())
                .expect("entry hooks array");
            for h in inner {
                let command = h
                    .get("command")
                    .and_then(|c| c.as_str())
                    .expect("command string");
                assert!(
                    !command.contains("C:/old/path"),
                    "stale shardctl path should be stripped: {command}",
                );
            }
        }
    }

    harness.shutdown().await;
}
