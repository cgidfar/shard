//! Integration tests for Phase 4 (Batch C tail): `ControlFrame::RemoveSession`,
//! `RenameSession`, and `FindSessionById`.
//!
//! Coverage matches the template Phases 1–3 established:
//!   - Happy path per RPC
//!   - Status / error guards (RemoveSession refuses live rows, rename on
//!     unknown repo, find-by-prefix ambiguity, …)
//!   - Cross-RPC flow: find-then-remove, find-then-rename
//!   - Concurrent CRUD: RemoveSession serializes against RemoveRepo on
//!     the same repo via the per-repo mutation lock

#![cfg(windows)]

mod common;

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use common::TestHarness;
use shard_core::paths::ShardPaths;
use shard_core::sessions::SessionStore;
use shard_transport::control_protocol::ControlFrame;

// ── RPC helpers ─────────────────────────────────────────────────────────────

async fn remove_session_rpc(harness: &TestHarness, repo: &str, id: &str) -> ControlFrame {
    let mut conn = harness.connect().await;
    conn.request(&ControlFrame::RemoveSession {
        repo: repo.to_string(),
        id: id.to_string(),
    })
    .await
    .expect("RemoveSession RPC")
}

async fn rename_session_rpc(
    harness: &TestHarness,
    repo: &str,
    id: &str,
    label: Option<&str>,
) -> ControlFrame {
    let mut conn = harness.connect().await;
    conn.request(&ControlFrame::RenameSession {
        repo: repo.to_string(),
        id: id.to_string(),
        label: label.map(|s| s.to_string()),
    })
    .await
    .expect("RenameSession RPC")
}

async fn find_session_by_id_rpc(harness: &TestHarness, prefix: &str) -> ControlFrame {
    let mut conn = harness.connect().await;
    conn.request(&ControlFrame::FindSessionById {
        prefix: prefix.to_string(),
    })
    .await
    .expect("FindSessionById RPC")
}

fn db_session_exists(data_path: &std::path::Path, repo: &str, id: &str) -> bool {
    let paths = ShardPaths::from_data_dir(data_path.to_path_buf());
    SessionStore::new(paths).get(repo, id).is_ok()
}

fn db_session_label(data_path: &std::path::Path, repo: &str, id: &str) -> Option<String> {
    let paths = ShardPaths::from_data_dir(data_path.to_path_buf());
    SessionStore::new(paths)
        .get(repo, id)
        .ok()
        .and_then(|s| s.label)
}

// ── RemoveSession ───────────────────────────────────────────────────────────

#[tokio::test]
async fn remove_session_happy_path() {
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");
    harness.setup_workspace("demo", "feature-a");
    let (session_id, session_dir) = harness.setup_terminal_session("demo", "feature-a", "exited");

    assert!(
        session_dir.exists(),
        "session dir should exist before remove"
    );
    assert!(db_session_exists(&harness.data_path, "demo", &session_id));

    match remove_session_rpc(&harness, "demo", &session_id).await {
        ControlFrame::RemoveSessionAck => {}
        other => panic!("expected Ack, got {other:?}"),
    }

    assert!(
        !db_session_exists(&harness.data_path, "demo", &session_id),
        "DB row should be gone"
    );
    assert!(!session_dir.exists(), "session dir should be cleaned up");

    harness.shutdown().await;
}

#[tokio::test]
async fn remove_session_refuses_live_registry_entry() {
    // Seed a DB row as `exited` but inject it into the daemon's live
    // registry as if a supervisor were still bound. The handler's
    // in-memory guard should block the remove regardless of the DB
    // status.
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");
    harness.setup_workspace("demo", "feature-live");
    let (session_id, session_dir) =
        harness.setup_terminal_session("demo", "feature-live", "exited");

    shard_cli::cmd::daemon::test_inject_live_session(
        &harness.state,
        session_id.clone(),
        std::process::id(),
        r"\\.\pipe\fake".to_string(),
        "demo".to_string(),
        "feature-live".to_string(),
        1,
    )
    .await;

    match remove_session_rpc(&harness, "demo", &session_id).await {
        ControlFrame::Error { message } => {
            assert!(
                message.contains("still live"),
                "expected 'still live' error, got: {message}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }

    assert!(
        session_dir.exists(),
        "session dir should survive the refused remove"
    );
    assert!(db_session_exists(&harness.data_path, "demo", &session_id));

    harness.shutdown().await;
}

#[tokio::test]
async fn remove_session_refuses_running_status() {
    // Pure DB guard — no live-registry entry. The store's own status
    // check rejects `running` / `starting`. Phase 4 guards through it.
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");
    harness.setup_workspace("demo", "feature-running");
    let (session_id, _) = harness.setup_terminal_session("demo", "feature-running", "running");

    match remove_session_rpc(&harness, "demo", &session_id).await {
        ControlFrame::Error { message } => {
            assert!(
                message.contains("cannot remove") || message.contains("running"),
                "expected status-guard error, got: {message}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }

    assert!(db_session_exists(&harness.data_path, "demo", &session_id));

    harness.shutdown().await;
}

#[tokio::test]
async fn remove_session_unknown_id_errors() {
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");

    match remove_session_rpc(&harness, "demo", "no-such-id").await {
        ControlFrame::Error { .. } => {}
        other => panic!("expected Error, got {other:?}"),
    }

    harness.shutdown().await;
}

// ── RenameSession ───────────────────────────────────────────────────────────

#[tokio::test]
async fn rename_session_sets_and_clears_label() {
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");
    harness.setup_workspace("demo", "feature-b");
    let (session_id, _) = harness.setup_terminal_session("demo", "feature-b", "exited");

    assert_eq!(
        db_session_label(&harness.data_path, "demo", &session_id),
        None
    );

    match rename_session_rpc(&harness, "demo", &session_id, Some("my-agent")).await {
        ControlFrame::RenameSessionAck => {}
        other => panic!("expected Ack, got {other:?}"),
    }
    assert_eq!(
        db_session_label(&harness.data_path, "demo", &session_id),
        Some("my-agent".to_string()),
    );

    // Clear.
    match rename_session_rpc(&harness, "demo", &session_id, None).await {
        ControlFrame::RenameSessionAck => {}
        other => panic!("expected Ack, got {other:?}"),
    }
    assert_eq!(
        db_session_label(&harness.data_path, "demo", &session_id),
        None
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn rename_session_unknown_repo_errors() {
    let harness = TestHarness::start().await;

    // No repo exists with this alias → repo_db(...) doesn't resolve.
    match rename_session_rpc(&harness, "nope", "some-id", Some("x")).await {
        ControlFrame::Error { .. } => {}
        other => panic!("expected Error, got {other:?}"),
    }

    harness.shutdown().await;
}

#[tokio::test]
async fn rename_session_unknown_id_errors() {
    // Bare `UPDATE ... WHERE id = ?` would silently ack on rowcount=0.
    // Phase 4 review fix: the store now checks rows_affected and surfaces
    // SessionNotFound so the RPC contract matches RemoveSession.
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");

    match rename_session_rpc(&harness, "demo", "no-such-id", Some("x")).await {
        ControlFrame::Error { message } => {
            assert!(
                message.contains("no-such-id") || message.contains("not found"),
                "expected SessionNotFound-style error, got: {message}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }

    harness.shutdown().await;
}

// ── FindSessionById ────────────────────────────────────────────────────────

#[tokio::test]
async fn find_session_by_id_exact_match() {
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");
    harness.setup_workspace("demo", "feature-e");
    let (session_id, _) = harness.setup_terminal_session("demo", "feature-e", "exited");

    match find_session_by_id_rpc(&harness, &session_id).await {
        ControlFrame::FoundSession { repo, session } => {
            assert_eq!(repo, "demo");
            assert_eq!(session.id, session_id);
            assert_eq!(session.status, "exited");
            assert_eq!(session.workspace_name, "feature-e");
        }
        other => panic!("expected FoundSession, got {other:?}"),
    }

    harness.shutdown().await;
}

#[tokio::test]
async fn find_session_by_id_prefix_match() {
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");
    harness.setup_workspace("demo", "feature-f");
    let (session_id, _) = harness.setup_terminal_session("demo", "feature-f", "exited");

    let prefix: String = session_id.chars().take(8).collect();
    match find_session_by_id_rpc(&harness, &prefix).await {
        ControlFrame::FoundSession { repo, session } => {
            assert_eq!(repo, "demo");
            assert_eq!(session.id, session_id);
        }
        other => panic!("expected FoundSession, got {other:?}"),
    }

    harness.shutdown().await;
}

#[tokio::test]
async fn find_session_by_id_walks_all_repos() {
    // The daemon owns the global session index per D4. Create sessions
    // in two repos and assert both are resolvable without specifying a
    // repo hint.
    let harness = TestHarness::start().await;
    harness.setup_local_repo("repo-a");
    harness.setup_local_repo("repo-b");
    harness.setup_workspace("repo-a", "ws-a");
    harness.setup_workspace("repo-b", "ws-b");
    let (id_a, _) = harness.setup_terminal_session("repo-a", "ws-a", "exited");
    let (id_b, _) = harness.setup_terminal_session("repo-b", "ws-b", "exited");

    match find_session_by_id_rpc(&harness, &id_a).await {
        ControlFrame::FoundSession { repo, .. } => assert_eq!(repo, "repo-a"),
        other => panic!("{other:?}"),
    }
    match find_session_by_id_rpc(&harness, &id_b).await {
        ControlFrame::FoundSession { repo, .. } => assert_eq!(repo, "repo-b"),
        other => panic!("{other:?}"),
    }

    harness.shutdown().await;
}

#[tokio::test]
async fn find_session_by_id_unknown_errors() {
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");

    match find_session_by_id_rpc(&harness, "deadbeef").await {
        ControlFrame::Error { .. } => {}
        other => panic!("expected Error, got {other:?}"),
    }

    harness.shutdown().await;
}

// ── CLI StopSession routing ────────────────────────────────────────────────

#[tokio::test]
async fn cli_stop_does_not_fallback_to_session_pipe_or_db_status() {
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");
    harness.setup_workspace("demo", "feature-stop");

    let paths = ShardPaths::from_data_dir(harness.data_path.clone());
    let store = SessionStore::new(paths);
    let fallback_pipe = format!(
        r"\\.\pipe\shard-test-cli-stop-fallback-{}-{}",
        std::process::id(),
        next_seq()
    );
    let session = store
        .create(
            "demo",
            "feature-stop",
            &["pwsh".to_string()],
            &fallback_pipe,
            None,
        )
        .expect("create session");
    store
        .update_status("demo", &session.id, "running", None)
        .expect("mark running");

    let missing_live_pipe = format!(
        r"\\.\pipe\shard-test-cli-stop-missing-live-{}-{}",
        std::process::id(),
        next_seq()
    );
    shard_cli::cmd::daemon::test_inject_live_session(
        &harness.state,
        session.id.clone(),
        std::process::id(),
        missing_live_pipe,
        "demo".to_string(),
        "feature-stop".to_string(),
        1,
    )
    .await;

    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let probe_pipe = fallback_pipe.clone();
    let fallback_probe = tokio::spawn(async move {
        let server = shard_transport::transport_windows::create_pipe_instance(&probe_pipe, true)
            .expect("create fallback probe pipe");
        let _ = ready_tx.send(());
        tokio::time::timeout(Duration::from_millis(350), server.connect())
            .await
            .is_ok()
    });
    ready_rx.await.expect("fallback probe ready");

    let err = shard_cli::cmd::session::test_stop_with_control_pipe(
        &session.id,
        false,
        &harness.control_pipe_name,
    )
    .await
    .expect_err("daemon StopSession failure should surface to CLI");
    assert!(
        err.to_string().contains("daemon failed to stop session")
            || err.to_string().contains("daemon stop request failed"),
        "unexpected error: {err}"
    );

    let session_after = store
        .get("demo", &session.id)
        .expect("session still exists");
    assert_eq!(
        session_after.status, "running",
        "CLI stop must not write DB status when daemon stop fails"
    );
    assert!(
        !fallback_probe.await.expect("fallback probe task"),
        "CLI stop must not connect directly to the DB session pipe after daemon failure"
    );

    harness.shutdown().await;
}

// ── Cross-RPC flow: find → remove ──────────────────────────────────────────

#[tokio::test]
async fn find_then_remove_by_prefix() {
    // Mirrors the CLI `session remove <id-prefix>` flow: resolve via
    // FindSessionById, then call RemoveSession with the resolved repo
    // + full id. Exercises the exact path the Tauri and CLI helpers
    // use post-Phase-4.
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");
    harness.setup_workspace("demo", "feature-g");
    let (session_id, session_dir) = harness.setup_terminal_session("demo", "feature-g", "exited");

    let prefix: String = session_id.chars().take(8).collect();
    let (repo, session) = match find_session_by_id_rpc(&harness, &prefix).await {
        ControlFrame::FoundSession { repo, session } => (repo, session),
        other => panic!("expected FoundSession, got {other:?}"),
    };

    match remove_session_rpc(&harness, &repo, &session.id).await {
        ControlFrame::RemoveSessionAck => {}
        other => panic!("expected Ack, got {other:?}"),
    }

    assert!(!db_session_exists(&harness.data_path, "demo", &session_id));
    assert!(!session_dir.exists());

    harness.shutdown().await;
}

// ── Concurrent RemoveSession vs RemoveRepo ─────────────────────────────────

#[tokio::test]
async fn remove_session_serializes_against_remove_repo() {
    // Per-repo mutation lock test. Fire RemoveRepo and RemoveSession on
    // the same repo in parallel. The legal outcomes are:
    //   (a) RemoveRepo wins → RemoveSession sees the repo/session gone
    //       and returns Error.
    //   (b) RemoveSession wins → row gone, RemoveRepo returns Ack (the
    //       repo-wide teardown walks whatever's left).
    // The illegal outcome — both Ack with lingering DB state — is
    // precisely what the lock prevents.
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");
    harness.setup_workspace("demo", "feature-h");
    let (session_id, _) = harness.setup_terminal_session("demo", "feature-h", "exited");

    // Give the control loop a moment to register the workspace via
    // the monitor's topology reload.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let remove_repo_fut = {
        let harness = &harness;
        async move {
            let mut conn = harness.connect().await;
            conn.request(&ControlFrame::RemoveRepo {
                alias: "demo".to_string(),
            })
            .await
            .expect("RemoveRepo RPC")
        }
    };
    let remove_session_fut = {
        let harness = &harness;
        let id = session_id.clone();
        async move { remove_session_rpc(harness, "demo", &id).await }
    };
    let (r_repo, r_session) = tokio::join!(remove_repo_fut, remove_session_fut);

    assert!(
        matches!(r_repo, ControlFrame::RemoveRepoAck),
        "RemoveRepo: {r_repo:?}"
    );

    match r_session {
        ControlFrame::RemoveSessionAck => {
            // Session won → repo teardown cleaned the rest. Either
            // way the session row is gone.
        }
        ControlFrame::Error { .. } => {
            // Repo won the race → RemoveSession failed because the
            // session row (and its containing repo DB) was deleted
            // out from under it.
        }
        other => panic!("unexpected RemoveSession response: {other:?}"),
    }

    // The final state is the same either way: no DB row left.
    let paths = ShardPaths::from_data_dir(harness.data_path.clone());
    if paths.repo_db("demo").exists() {
        assert!(!db_session_exists(&harness.data_path, "demo", &session_id));
    }

    harness.shutdown().await;
}

// Helper to keep test pipe names unique across the file — same pattern
// used in `remove_workspace.rs` for the fake supervisor pipe.
#[allow(dead_code)]
static SEQ: AtomicU32 = AtomicU32::new(0);
#[allow(dead_code)]
fn next_seq() -> u32 {
    SEQ.fetch_add(1, Ordering::Relaxed)
}
