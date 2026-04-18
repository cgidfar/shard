//! Integration-test harness: in-process headless daemon + client.
//!
//! Each test starts a fresh daemon on a unique control-pipe name, rooted
//! at a `TempDir`-backed `ShardPaths`. Teardown happens through
//! `TestHarness::shutdown()` or the `Drop` impl. Designed so that
//! `cargo test` can run several of these in parallel without cross-test
//! interference (no shared data dir, no shared pipe name, no shared
//! environment variable mutation).

#![cfg(windows)]
#![allow(dead_code)] // some helpers are only used by a subset of tests

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use std::sync::Arc;

use shard_cli::cmd::daemon::{self, DaemonConfig, DaemonState, ShutdownMode};
use shard_core::paths::ShardPaths;
use shard_core::workspaces::{default_git_ops, WorkspaceGitOps};
use shard_transport::daemon_client::{connect_to_with_retry, DaemonConnection};
use tempfile::TempDir;
use tokio::net::windows::named_pipe::NamedPipeClient;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// A live daemon running on the current tokio runtime.
pub struct TestHarness {
    /// Owned so the directory is deleted on Drop (after the daemon exits).
    _data_dir: TempDir,
    pub data_path: PathBuf,
    pub control_pipe_name: String,
    /// Handle to the daemon's internal state. Tests use this to inject
    /// live sessions and inspect the lifecycle registry.
    pub state: Arc<DaemonState>,
    shutdown_tx: watch::Sender<ShutdownMode>,
    daemon_task: Option<JoinHandle<shard_core::Result<()>>>,
}

/// Per-test unique pipe counter. Combined with the process PID this
/// produces pipe names that never collide, even across parallel test
/// binaries on the same machine.
static PIPE_COUNTER: AtomicU64 = AtomicU64::new(0);

impl TestHarness {
    /// Start a headless daemon in the background with production git-ops.
    pub async fn start() -> Self {
        Self::start_with_git_ops(default_git_ops()).await
    }

    /// Start a headless daemon with a custom [`WorkspaceGitOps`]. Used by
    /// the partial-failure test to inject a forced `worktree_remove`
    /// failure.
    pub async fn start_with_git_ops(git_ops: Arc<dyn WorkspaceGitOps>) -> Self {
        let data_dir = TempDir::new().expect("create tempdir");
        let data_path = data_dir.path().to_path_buf();

        let pid = std::process::id();
        let seq = PIPE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let control_pipe_name = format!(r"\\.\pipe\shard-test-{pid}-{seq}");

        let paths = ShardPaths::from_data_dir(data_path.clone());
        paths.ensure_dirs().expect("ensure_dirs");

        let config = DaemonConfig {
            paths,
            control_pipe_name: control_pipe_name.clone(),
            git_ops,
        };

        let (shutdown_tx, shutdown_rx) = watch::channel(ShutdownMode::Running);

        // Build the state synchronously so the test can grab a handle
        // before the control loop starts. run_headless_daemon_with_state
        // then drives the loop on a spawned task.
        let state =
            daemon::build_headless_state(config, shutdown_tx.clone()).expect("build state");

        let task_state = state.clone();
        let daemon_task = tokio::spawn(async move {
            daemon::run_headless_daemon_with_state(task_state, shutdown_rx).await
        });

        // Wait for the control pipe to exist by trying a short-retry connect.
        // If the daemon can't come up in 5s, something is wrong.
        let mut probe =
            connect_to_with_retry(&control_pipe_name, Duration::from_secs(5))
                .await
                .expect("daemon control pipe should come up within 5s");
        probe.handshake().await.expect("handshake on probe connect");
        drop(probe);

        Self {
            _data_dir: data_dir,
            data_path,
            control_pipe_name,
            state,
            shutdown_tx,
            daemon_task: Some(daemon_task),
        }
    }

    /// Open and handshake a fresh connection to the daemon.
    pub async fn connect(&self) -> DaemonConnection<NamedPipeClient> {
        let mut conn = connect_to_with_retry(&self.control_pipe_name, Duration::from_secs(2))
            .await
            .expect("connect to test daemon");
        conn.handshake().await.expect("handshake");
        conn
    }

    /// Ask the daemon to shut down gracefully and wait for its task to finish.
    pub async fn shutdown(mut self) {
        let _ = self.shutdown_tx.send(ShutdownMode::Graceful);
        if let Some(task) = self.daemon_task.take() {
            // Give the daemon up to 5s to wind down. If it hangs, abort and
            // surface the panic — a hung test is worse than a failed one.
            match tokio::time::timeout(Duration::from_secs(5), task).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(e))) => panic!("daemon returned error: {e}"),
                Ok(Err(e)) => panic!("daemon task panicked: {e}"),
                Err(_) => panic!("daemon did not shut down within 5s"),
            }
        }
    }

    /// Create a fresh local git repo in a temp location and register it
    /// with this daemon's DB. Returns (alias, repo_path).
    pub fn setup_local_repo(&self, alias: &str) -> (String, PathBuf) {
        use shard_core::repos::RepositoryStore;

        let repo_path = self.data_path.join(format!("repo-source-{alias}"));
        std::fs::create_dir_all(&repo_path).expect("create repo dir");

        // Initialize as a git repo and make one commit so worktree add works.
        run_git(&["init", "-b", "main"], &repo_path);
        run_git(&["config", "user.name", "test"], &repo_path);
        run_git(&["config", "user.email", "test@example.com"], &repo_path);
        std::fs::write(repo_path.join("README.md"), "test\n").expect("write readme");
        run_git(&["add", "."], &repo_path);
        run_git(&["commit", "-m", "initial"], &repo_path);

        let repo_store = RepositoryStore::new(
            shard_core::paths::ShardPaths::from_data_dir(self.data_path.clone()),
        );
        repo_store
            .add(repo_path.to_str().unwrap(), Some(alias))
            .expect("register repo");

        (alias.to_string(), repo_path)
    }

    /// Create a fresh workspace in `repo` named `ws_name`. Returns its
    /// filesystem path. The workspace is registered in the daemon's
    /// lifecycle registry in `Active` state.
    pub fn setup_workspace(&self, repo: &str, ws_name: &str) -> PathBuf {
        use shard_core::workspaces::{WorkspaceMode, WorkspaceStore};

        let ws_store = WorkspaceStore::new(
            shard_core::paths::ShardPaths::from_data_dir(self.data_path.clone()),
        );
        let ws = ws_store
            .create(
                repo,
                Some(ws_name),
                WorkspaceMode::NewBranch,
                Some("main"),
                false,
            )
            .expect("create workspace");
        // Register in lifecycle so the handler's gate checks work.
        self.state.lifecycle.register_active(repo, &ws.name);
        PathBuf::from(ws.path)
    }
}

fn run_git(args: &[&str], cwd: &std::path::Path) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?} failed in {cwd:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        // Best-effort cleanup if the test forgot to `.shutdown().await`.
        // We can't .await here, so the daemon task will be aborted on
        // runtime teardown; the TempDir drops after.
        let _ = self.shutdown_tx.send(ShutdownMode::Force);
        if let Some(task) = self.daemon_task.take() {
            task.abort();
        }
    }
}
