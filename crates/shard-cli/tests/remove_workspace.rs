//! Integration tests for `ControlFrame::RemoveWorkspace` (Phase 1 of the
//! daemon-broker migration). The whole phase is a Windows handle-race fix,
//! so every test runs on Windows and asserts concrete on-disk state.
//!
//! Test coverage (per plan §Phase 1):
//!   1. Happy path
//!   2. Live-session path (SHA-55 repro): fake supervisor holding the pipe
//!   3. Watcher-held path: debouncer observed at least one event first
//!   4. Concurrent-mutation rejection: lifecycle gate blocks SpawnSession
//!   5. Idempotency: two `RemoveWorkspace` RPCs in parallel
//!   6. Partial failure: injected `worktree_remove` error → `broken` state
//!      → retry succeeds

#![cfg(windows)]

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::TestHarness;
use shard_core::workspaces::WorkspaceGitOps;
use shard_transport::control_protocol::ControlFrame;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Issue a RemoveWorkspace RPC on a fresh connection and return the response.
async fn remove_workspace(
    harness: &TestHarness,
    repo: &str,
    name: &str,
) -> ControlFrame {
    let mut conn = harness.connect().await;
    conn.request(&ControlFrame::RemoveWorkspace {
        repo: repo.to_string(),
        name: name.to_string(),
    })
    .await
    .expect("RemoveWorkspace RPC")
}

fn db_has_workspace(data_path: &Path, repo: &str, ws_name: &str) -> bool {
    let paths = shard_core::paths::ShardPaths::from_data_dir(data_path.to_path_buf());
    let store = shard_core::workspaces::WorkspaceStore::new(paths);
    store.get(repo, ws_name).is_ok()
}

fn db_workspace_sessions(data_path: &Path, repo: &str, ws_name: &str) -> usize {
    let paths = shard_core::paths::ShardPaths::from_data_dir(data_path.to_path_buf());
    let store = shard_core::sessions::SessionStore::new(paths);
    store
        .list(repo, Some(ws_name))
        .expect("list workspace sessions")
        .len()
}

// ── 1. Happy path ────────────────────────────────────────────────────────────

#[tokio::test]
async fn remove_workspace_happy_path() {
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");
    let ws_path = harness.setup_workspace("demo", "feature-a");

    assert!(ws_path.exists(), "workspace dir should exist before remove");
    assert!(db_has_workspace(&harness.data_path, "demo", "feature-a"));

    match remove_workspace(&harness, "demo", "feature-a").await {
        ControlFrame::RemoveWorkspaceAck => {}
        other => panic!("expected Ack, got {other:?}"),
    }

    assert!(
        !ws_path.exists(),
        "workspace dir should be gone after remove"
    );
    assert!(
        !db_has_workspace(&harness.data_path, "demo", "feature-a"),
        "DB row should be gone"
    );

    harness.shutdown().await;
}

// ── 2. Live-session path (SHA-55 repro) ─────────────────────────────────────

/// Spawn a fake supervisor: accept on a named pipe, wait for Resume+Stop,
/// reply with Status{code: 0}. Replicates the stop-and-drain protocol
/// `stop_session_and_wait` expects. Runs until `stopped` fires.
async fn spawn_fake_supervisor(pipe_name: String) -> tokio::task::JoinHandle<()> {
    use shard_transport::transport_windows::create_pipe_instance;

    tokio::spawn(async move {
        let mut server = create_pipe_instance(&pipe_name, true).expect("create pipe");
        server.connect().await.expect("accept client");

        // Read frames until we see StopGraceful, then reply Status{0} and exit.
        loop {
            let mut len_buf = [0u8; 4];
            match server.read_exact(&mut len_buf).await {
                Ok(_) => {}
                Err(_) => return,
            }
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut buf = vec![0u8; len];
            if server.read_exact(&mut buf).await.is_err() {
                return;
            }
            if buf.is_empty() {
                continue;
            }
            let type_byte = buf[0];
            // Session protocol type bytes: 0x02 = StopGraceful, 0x03 = StopForce.
            // See crates/shard-transport/src/protocol.rs.
            if type_byte == 0x02 || type_byte == 0x03 {
                // Reply Status { code: 0 } (type 0x05).
                let payload = vec![0u8, 0u8, 0u8, 0u8]; // u32 exit code = 0
                let mut out = Vec::new();
                out.extend_from_slice(&(1u32 + payload.len() as u32).to_be_bytes());
                out.push(0x05);
                out.extend_from_slice(&payload);
                let _ = server.write_all(&out).await;
                let _ = server.flush().await;
                return;
            }
        }
    })
}

#[tokio::test]
async fn remove_workspace_with_live_session() {
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");
    let ws_path = harness.setup_workspace("demo", "feature-b");

    // Register a fake supervisor against a uniquely-named session pipe.
    let session_id = "test-session-0001".to_string();
    let pipe_name =
        format!(r"\\.\pipe\shard-session-test-{}-{}",
            std::process::id(),
            next_seq());
    let fake = spawn_fake_supervisor(pipe_name.clone()).await;
    // Give the pipe server a moment to bind before we inject the record.
    tokio::time::sleep(Duration::from_millis(50)).await;

    shard_cli::cmd::daemon::test_inject_live_session(
        &harness.state,
        session_id.clone(),
        std::process::id(), // alive PID (ourselves) — graceful path should exit before force-kill
        pipe_name,
        "demo".to_string(),
        "feature-b".to_string(),
        // Deliberately non-zero, deliberately non-matching: if graceful
        // ever regresses into the force-kill path, force_kill_pid_checked
        // will refuse on the creation-time mismatch rather than terminate
        // the test process itself (whose PID we used above).
        1,
    )
    .await;
    assert_eq!(
        shard_cli::cmd::daemon::test_live_session_count(&harness.state).await,
        1
    );

    match remove_workspace(&harness, "demo", "feature-b").await {
        ControlFrame::RemoveWorkspaceAck => {}
        other => panic!("expected Ack, got {other:?}"),
    }

    assert!(!ws_path.exists(), "workspace dir should be gone");
    assert!(!db_has_workspace(&harness.data_path, "demo", "feature-b"));

    // Live session should have been stopped and removed from the registry.
    assert_eq!(
        shard_cli::cmd::daemon::test_live_session_count(&harness.state).await,
        0
    );
    let _ = fake.await;
    harness.shutdown().await;
}

#[tokio::test]
async fn remove_workspace_with_stopped_persisted_session() {
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");
    let ws_path = harness.setup_workspace("demo", "feature-stopped");
    let (_session_id, session_dir) =
        harness.setup_terminal_session("demo", "feature-stopped", "stopped");

    assert!(session_dir.exists(), "session dir should exist before remove");
    assert_eq!(
        db_workspace_sessions(&harness.data_path, "demo", "feature-stopped"),
        1
    );

    match remove_workspace(&harness, "demo", "feature-stopped").await {
        ControlFrame::RemoveWorkspaceAck => {}
        other => panic!("expected Ack, got {other:?}"),
    }

    assert!(!ws_path.exists(), "workspace dir should be gone");
    assert!(!session_dir.exists(), "session dir should be gone");
    assert!(!db_has_workspace(
        &harness.data_path,
        "demo",
        "feature-stopped"
    ));
    assert_eq!(
        db_workspace_sessions(&harness.data_path, "demo", "feature-stopped"),
        0
    );

    harness.shutdown().await;
}

static SEQ: AtomicU32 = AtomicU32::new(0);
fn next_seq() -> u32 {
    SEQ.fetch_add(1, Ordering::Relaxed)
}

// ── 3. Watcher-held path ────────────────────────────────────────────────────

#[tokio::test]
async fn remove_workspace_while_watcher_live() {
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");
    let ws_path = harness.setup_workspace("demo", "watched");

    // Poke the daemon so the monitor loads this repo and creates a debouncer.
    let mut conn = harness.connect().await;
    conn.send(&ControlFrame::TopologyChanged {
        repo_alias: Some("demo".to_string()),
    })
    .await
    .expect("topology poke");
    drop(conn);

    // Give notify time to bind its ReadDirectoryChangesW handle and fire an
    // event by touching a file inside the workspace. The SHA-55 fix is what
    // makes this survive — without the ack-after-drop, RemoveDirectoryW
    // races the notify handle.
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(ws_path.join("notify-probe.txt"), "probe\n")
        .expect("touch file to stimulate debouncer");
    tokio::time::sleep(Duration::from_millis(500)).await;

    match remove_workspace(&harness, "demo", "watched").await {
        ControlFrame::RemoveWorkspaceAck => {}
        other => panic!("expected Ack, got {other:?}"),
    }

    assert!(!ws_path.exists(), "workspace dir should be gone");
    assert!(!db_has_workspace(&harness.data_path, "demo", "watched"));

    harness.shutdown().await;
}

// ── 4. Concurrent-mutation rejection ────────────────────────────────────────

#[tokio::test]
async fn concurrent_mutation_blocked_during_delete() {
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");
    harness.setup_workspace("demo", "feature-c");

    // Enter the deleting state via the lifecycle API directly, then assert
    // that check_can_mutate blocks. This exercises the gate a
    // SpawnSession / CreateWorkspace handler would consult. (The full
    // SpawnSession RPC path can't run in this harness because it would
    // require spawning a real supervisor binary.)
    let lifecycle = harness.state.lifecycle.clone();
    let guard = match lifecycle.begin_delete("demo", "feature-c") {
        shard_cli::cmd::lifecycle::BeginDelete::Started(g) => g,
        other => panic!("expected Started, got {other:?}"),
    };

    let check =
        shard_cli::cmd::daemon::test_lifecycle_check(&harness.state, "demo", "feature-c");
    assert!(
        check.is_err(),
        "mutation should be blocked while delete is in flight, got {check:?}"
    );
    let err_text = check.unwrap_err();
    assert!(
        err_text.contains("being deleted"),
        "expected 'being deleted' error, got '{err_text}'"
    );

    // Release the gate — workspace returns to Active.
    guard.rollback();
    assert!(
        shard_cli::cmd::daemon::test_lifecycle_check(&harness.state, "demo", "feature-c")
            .is_ok()
    );

    harness.shutdown().await;
}

// ── 5. Idempotency ──────────────────────────────────────────────────────────

#[tokio::test]
async fn two_parallel_removes_both_ok() {
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");
    harness.setup_workspace("demo", "feature-d");

    // Fire two RPCs in parallel. The first owns the delete; the second
    // joins via AlreadyDeleting → Gone → Ack. Both should return Ack;
    // exactly one actually does the work.
    let h1 = {
        let harness = &harness;
        async move { remove_workspace(harness, "demo", "feature-d").await }
    };
    let h2 = {
        let harness = &harness;
        async move { remove_workspace(harness, "demo", "feature-d").await }
    };
    let (r1, r2) = tokio::join!(h1, h2);

    assert!(
        matches!(r1, ControlFrame::RemoveWorkspaceAck),
        "first: {r1:?}"
    );
    assert!(
        matches!(r2, ControlFrame::RemoveWorkspaceAck),
        "second: {r2:?}"
    );
    assert!(!db_has_workspace(&harness.data_path, "demo", "feature-d"));

    harness.shutdown().await;
}

// ── is_base=true path (D11) ─────────────────────────────────────────────────

#[tokio::test]
async fn remove_base_workspace_preserves_checkout_dir() {
    // Base workspaces (is_base=true) point at the user's original checkout
    // for local repos (SHA-56 marks this as intentional asymmetry). The
    // D11 invariant is that RemoveWorkspace on a base workspace deletes
    // only the DB row — the checkout stays on disk untouched.
    use shard_core::paths::ShardPaths;
    use shard_core::workspaces::{WorkspaceMode, WorkspaceStore};

    let harness = TestHarness::start().await;
    let (_alias, repo_path) = harness.setup_local_repo("demo");

    let ws_store = WorkspaceStore::new(ShardPaths::from_data_dir(
        harness.data_path.clone(),
    ));
    let base = ws_store
        .create("demo", Some("base"), WorkspaceMode::NewBranch, Some("main"), true)
        .expect("create base workspace");
    assert!(base.is_base);
    harness.state.lifecycle.register_active("demo", &base.name);

    // Sanity: base points at the original checkout directory.
    assert_eq!(std::path::PathBuf::from(&base.path), repo_path);
    assert!(repo_path.exists());

    match remove_workspace(&harness, "demo", &base.name).await {
        ControlFrame::RemoveWorkspaceAck => {}
        other => panic!("expected Ack, got {other:?}"),
    }

    // DB row gone, directory preserved.
    assert!(!db_has_workspace(&harness.data_path, "demo", &base.name));
    assert!(
        repo_path.exists(),
        "base workspace checkout must survive remove"
    );
    assert!(
        repo_path.join("README.md").exists(),
        "contents of the checkout must be untouched"
    );

    harness.shutdown().await;
}

// ── SpawnSession gate enforcement ───────────────────────────────────────────

#[tokio::test]
async fn spawn_session_blocked_during_active_delete() {
    // Asserts the fix for the D5 gate on SpawnSession. Uses the
    // lifecycle API directly to put the workspace into Deleting, then
    // fires a SpawnSession RPC and expects a typed error — no supervisor
    // process is spawned because the handler short-circuits.
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");
    harness.setup_workspace("demo", "feature-gate");

    let guard = match harness
        .state
        .lifecycle
        .begin_delete("demo", "feature-gate")
    {
        shard_cli::cmd::lifecycle::BeginDelete::Started(g) => g,
        other => panic!("expected Started, got {other:?}"),
    };

    let mut conn = harness.connect().await;
    let response = conn
        .request(&ControlFrame::SpawnSession {
            repo: "demo".to_string(),
            workspace: "feature-gate".to_string(),
            command: vec!["cmd.exe".to_string()],
            harness: None,
        })
        .await
        .expect("SpawnSession RPC");

    match response {
        ControlFrame::Error { message } => {
            assert!(
                message.contains("being deleted"),
                "expected 'being deleted' error, got: {message}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }

    // Releasing the guard should return the workspace to accepting mutations.
    guard.rollback();
    assert!(shard_cli::cmd::daemon::test_lifecycle_check(
        &harness.state,
        "demo",
        "feature-gate",
    )
    .is_ok());

    harness.shutdown().await;
}

// ── 6. Partial failure → broken state → retry ───────────────────────────────

/// Git-ops stub that fails `worktree_remove` the first N times then delegates
/// to the real impl. Lets us exercise the `broken` transition and recovery.
struct FlakyGitOps {
    real: shard_core::workspaces::RealGitOps,
    fail_count: AtomicU32,
}

impl FlakyGitOps {
    fn new(fail_count: u32) -> Arc<Self> {
        Arc::new(Self {
            real: shard_core::workspaces::RealGitOps,
            fail_count: AtomicU32::new(fail_count),
        })
    }
}

impl WorkspaceGitOps for FlakyGitOps {
    fn worktree_remove(
        &self,
        repo_dir: &Path,
        worktree_path: &Path,
    ) -> shard_core::Result<()> {
        let prev = self.fail_count.fetch_sub(1, Ordering::Relaxed);
        if prev > 0 {
            return Err(shard_core::ShardError::Other(
                "injected worktree_remove failure".to_string(),
            ));
        }
        self.real.worktree_remove(repo_dir, worktree_path)
    }

    fn worktree_prune(&self, repo_dir: &Path) -> shard_core::Result<()> {
        self.real.worktree_prune(repo_dir)
    }

    fn worktree_list_porcelain(
        &self,
        repo_dir: &Path,
    ) -> shard_core::Result<Vec<shard_core::git::WorktreeEntry>> {
        self.real.worktree_list_porcelain(repo_dir)
    }
}

#[tokio::test]
async fn partial_failure_marks_broken_then_retry_completes() {
    let flaky = FlakyGitOps::new(1); // fail first call, succeed after
    let harness = TestHarness::start_with_git_ops(flaky).await;
    harness.setup_local_repo("demo");
    let ws_path = harness.setup_workspace("demo", "feature-e");

    // First attempt: worktree_remove fails → broken state, DB row preserved.
    match remove_workspace(&harness, "demo", "feature-e").await {
        ControlFrame::Error { message } => {
            assert!(
                message.contains("broken") || message.contains("failed"),
                "expected broken/failed message, got: {message}"
            );
        }
        other => panic!("expected Error on first attempt, got {other:?}"),
    }

    // State should be Broken; directory still present; DB row preserved.
    assert!(ws_path.exists(), "workspace dir should still exist");
    assert!(db_has_workspace(&harness.data_path, "demo", "feature-e"));
    let check =
        shard_cli::cmd::daemon::test_lifecycle_check(&harness.state, "demo", "feature-e");
    assert!(check.is_err(), "state should be Broken");
    assert!(
        check.unwrap_err().contains("broken"),
        "expected broken error"
    );

    // Second attempt: the flaky ops now returns success → delete completes.
    match remove_workspace(&harness, "demo", "feature-e").await {
        ControlFrame::RemoveWorkspaceAck => {}
        other => panic!("expected Ack on retry, got {other:?}"),
    }

    assert!(!ws_path.exists(), "workspace dir should be gone after retry");
    assert!(!db_has_workspace(&harness.data_path, "demo", "feature-e"));

    harness.shutdown().await;
}

// ── StopSession vs RemoveWorkspace race ─────────────────────────────────────

/// Concurrent `StopSession` and `RemoveWorkspace` on the same repo must
/// serialize through the per-repo mutation lock. The forbidden outcome is
/// the workspace removal completing while a stale live-session entry still
/// references the workspace, or the StopSession backstop resurrecting any
/// state for a workspace that has already been removed.
#[tokio::test]
async fn stop_session_racing_remove_workspace_does_not_leak_state() {
    let harness = TestHarness::start().await;
    harness.setup_local_repo("demo");
    let ws_path = harness.setup_workspace("demo", "feature-stop-race");

    let session_id = "01993333-3333-7333-8333-333333333333".to_string();
    let pipe_name = format!(
        r"\\.\pipe\shard-test-stop-rmws-race-{}-{}",
        std::process::id(),
        next_seq()
    );
    let _supervisor = spawn_fake_supervisor(pipe_name.clone()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    shard_cli::cmd::daemon::test_inject_live_session(
        &harness.state,
        session_id.clone(),
        std::process::id(),
        pipe_name,
        "demo".to_string(),
        "feature-stop-race".to_string(),
        1,
    )
    .await;
    assert_eq!(
        shard_cli::cmd::daemon::test_live_session_count(&harness.state).await,
        1
    );

    let stop = async {
        let mut conn = harness.connect().await;
        conn.request(&ControlFrame::StopSession {
            session_id: session_id.clone(),
            force: false,
        })
        .await
        .expect("StopSession RPC")
    };
    let remove = async { remove_workspace(&harness, "demo", "feature-stop-race").await };
    let (stop_r, remove_r) = tokio::join!(stop, remove);

    // RemoveWorkspace cascades through any bound live session, so it always
    // succeeds. StopSession either lands first (StopAck) or finds the
    // session already cleared by RemoveWorkspace (Error). Both are legal.
    assert!(
        matches!(stop_r, ControlFrame::StopAck | ControlFrame::Error { .. }),
        "StopSession outcome: {stop_r:?}"
    );
    assert!(
        matches!(remove_r, ControlFrame::RemoveWorkspaceAck),
        "RemoveWorkspace outcome: {remove_r:?}"
    );

    // Workspace must be fully gone — no resurrection of the directory or DB row.
    assert!(!ws_path.exists(), "workspace dir must not be resurrected");
    assert!(
        !db_has_workspace(&harness.data_path, "demo", "feature-stop-race"),
        "workspace DB row must not be resurrected"
    );
    // Live registry must be empty — the StopSession backstop must not
    // re-insert an entry for a workspace that no longer exists.
    assert_eq!(
        shard_cli::cmd::daemon::test_live_session_count(&harness.state).await,
        0,
        "live registry must be empty after race"
    );

    harness.shutdown().await;
}
