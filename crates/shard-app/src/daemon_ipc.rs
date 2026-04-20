//! App-side helpers for talking to the daemon over the control pipe.
//!
//! Two consumers live here:
//!   - **Mutation RPC helpers** (`remove_workspace`, `add_repo`, …) —
//!     request/response wrappers that hide the connect/handshake/extract-ack
//!     boilerplate. Tauri command handlers become thin translators:
//!     one-line RPC call, one-line event emit. The shape is copied
//!     per-batch as the migration progresses.
//!   - `run_state_subscriber` — long-lived background task that subscribes
//!     to daemon state updates and re-emits them as Tauri events. Handles
//!     reconnect-with-backoff on daemon drop so the UI never wedges.

use std::collections::HashMap;
use std::time::Duration;

use shard_core::repos::Repository;
use shard_core::state::RepoState;
use shard_core::workspaces::{BranchInfo, Workspace, WorkspaceMode, WorkspaceWithStatus};
use shard_transport::control_protocol::ControlFrame;
use shard_transport::daemon_client;
use tauri::{AppHandle, Emitter, Manager};
use tracing::{debug, info, warn};

use crate::state::AppState;

// ── Mutation RPCs ───────────────────────────────────────────────────────────
//
// Shape copied by every future mutation-RPC helper:
//   1. `connect` + `handshake` — surface any transport failure as an error
//      string the frontend can render.
//   2. `request_typed` — one closure extracts the success variant, the
//      helper folds `ControlFrame::Error { message }` into `Err`.
//   3. Return plain `Result<T, String>` so Tauri commands don't need to
//      translate between `DaemonError` / `io::Error` / etc.

/// Ask the daemon to remove a workspace. See
/// `crates/shard-cli/src/cmd/daemon.rs::handle_remove_workspace` for the
/// atomic workflow (SHA-55 fix).
pub async fn remove_workspace(repo: &str, name: &str) -> Result<(), String> {
    let mut conn = daemon_client::connect()
        .await
        .map_err(|e| format!("daemon connect failed: {e}"))?;
    conn.handshake()
        .await
        .map_err(|e| format!("daemon handshake failed: {e}"))?;
    conn.request_typed(
        &ControlFrame::RemoveWorkspace {
            repo: repo.to_string(),
            name: name.to_string(),
        },
        |f| match f {
            ControlFrame::RemoveWorkspaceAck => Ok(()),
            other => Err(other),
        },
    )
    .await
    .map_err(|e| e.to_string())
}

/// Create a workspace via the daemon RPC. See
/// `crates/shard-cli/src/cmd/daemon.rs::handle_create_workspace` for the
/// gate / register / poke sequence.
pub async fn create_workspace(
    repo: &str,
    name: Option<String>,
    mode: WorkspaceMode,
    branch: Option<String>,
) -> Result<Workspace, String> {
    let mut conn = daemon_client::connect()
        .await
        .map_err(|e| format!("daemon connect failed: {e}"))?;
    conn.handshake()
        .await
        .map_err(|e| format!("daemon handshake failed: {e}"))?;
    conn.request_typed(
        &ControlFrame::CreateWorkspace {
            repo: repo.to_string(),
            name,
            mode,
            branch,
        },
        |f| match f {
            ControlFrame::CreateWorkspaceAck { workspace } => Ok(workspace),
            other => Err(other),
        },
    )
    .await
    .map_err(|e| e.to_string())
}

/// List workspaces for `repo`, enriched with live `WorkspaceStatus` from the
/// daemon monitor. The daemon joins DB + monitor snapshot server-side so
/// the caller sees a consistent view.
pub async fn list_workspaces(repo: &str) -> Result<Vec<WorkspaceWithStatus>, String> {
    let mut conn = daemon_client::connect()
        .await
        .map_err(|e| format!("daemon connect failed: {e}"))?;
    conn.handshake()
        .await
        .map_err(|e| format!("daemon handshake failed: {e}"))?;
    conn.request_typed(
        &ControlFrame::ListWorkspaces {
            repo: repo.to_string(),
        },
        |f| match f {
            ControlFrame::WorkspaceList { items } => Ok(items),
            other => Err(other),
        },
    )
    .await
    .map_err(|e| e.to_string())
}

/// Enumerate branches + current worktree occupancy for `repo`. Drives the
/// new-workspace wizard's branch picker.
pub async fn list_branch_info(repo: &str) -> Result<Vec<BranchInfo>, String> {
    let mut conn = daemon_client::connect()
        .await
        .map_err(|e| format!("daemon connect failed: {e}"))?;
    conn.handshake()
        .await
        .map_err(|e| format!("daemon handshake failed: {e}"))?;
    conn.request_typed(
        &ControlFrame::ListBranchInfo {
            repo: repo.to_string(),
        },
        |f| match f {
            ControlFrame::BranchInfoList { branches } => Ok(branches),
            other => Err(other),
        },
    )
    .await
    .map_err(|e| e.to_string())
}

/// Register a repo via the daemon RPC. The daemon clones (remote) or
/// references (local) the repo, auto-creates the base workspace, and
/// returns the persisted row. See
/// `crates/shard-cli/src/cmd/daemon.rs::handle_add_repo`.
pub async fn add_repo(url: &str, alias: Option<String>) -> Result<Repository, String> {
    let mut conn = daemon_client::connect()
        .await
        .map_err(|e| format!("daemon connect failed: {e}"))?;
    conn.handshake()
        .await
        .map_err(|e| format!("daemon handshake failed: {e}"))?;
    conn.request_typed(
        &ControlFrame::AddRepo {
            url: url.to_string(),
            alias,
        },
        |f| match f {
            ControlFrame::AddRepoAck { repo } => Ok(repo),
            other => Err(other),
        },
    )
    .await
    .map_err(|e| e.to_string())
}

/// Tear down a repo via the daemon RPC. The daemon stops all bound
/// sessions, drops the watcher, removes worktrees + DB rows, and (for
/// remote repos) the bare clone. Local checkouts are preserved.
pub async fn remove_repo(alias: &str) -> Result<(), String> {
    let mut conn = daemon_client::connect()
        .await
        .map_err(|e| format!("daemon connect failed: {e}"))?;
    conn.handshake()
        .await
        .map_err(|e| format!("daemon handshake failed: {e}"))?;
    conn.request_typed(
        &ControlFrame::RemoveRepo {
            alias: alias.to_string(),
        },
        |f| match f {
            ControlFrame::RemoveRepoAck => Ok(()),
            other => Err(other),
        },
    )
    .await
    .map_err(|e| e.to_string())
}

/// `git fetch --all --prune` against a repo's source. No DB mutation.
pub async fn sync_repo(alias: &str) -> Result<(), String> {
    let mut conn = daemon_client::connect()
        .await
        .map_err(|e| format!("daemon connect failed: {e}"))?;
    conn.handshake()
        .await
        .map_err(|e| format!("daemon handshake failed: {e}"))?;
    conn.request_typed(
        &ControlFrame::SyncRepo {
            alias: alias.to_string(),
        },
        |f| match f {
            ControlFrame::SyncRepoAck => Ok(()),
            other => Err(other),
        },
    )
    .await
    .map_err(|e| e.to_string())
}

/// List registered repositories via the daemon so readers agree with the
/// event stream.
pub async fn list_repos() -> Result<Vec<Repository>, String> {
    let mut conn = daemon_client::connect()
        .await
        .map_err(|e| format!("daemon connect failed: {e}"))?;
    conn.handshake()
        .await
        .map_err(|e| format!("daemon handshake failed: {e}"))?;
    conn.request_typed(
        &ControlFrame::ListRepos,
        |f| match f {
            ControlFrame::RepoList { repos } => Ok(repos),
            other => Err(other),
        },
    )
    .await
    .map_err(|e| e.to_string())
}

// ── State subscriber ────────────────────────────────────────────────────────

/// Event payload for `workspace-status-changed` Tauri events. `status` is
/// `None` when the workspace has disappeared from the daemon's snapshot
/// (e.g. it was removed or its repo was dropped) — the frontend should
/// stop displaying live state for that row.
#[derive(serde::Serialize, Clone)]
struct WorkspaceStatusChanged<'a> {
    repo: &'a str,
    workspace: &'a str,
    status: Option<&'a shard_core::state::WorkspaceStatus>,
}

/// Subscribe to the daemon's state broadcast and re-emit updates as Tauri
/// events for the frontend to consume. Runs in its own background task for
/// the lifetime of the app.
///
/// Reconnect strategy: start at 100ms, double on each consecutive failure
/// (capped at 5s). Reset to 100ms after any connection that successfully
/// handshaked — only a run of cold failures should ramp the backoff. This
/// keeps a normal daemon restart from stalling the UI for several seconds.
/// The last-known `RepoState` cache stays warm across disconnects so the
/// sidebar does not flash missing/dead rows during the gap.
pub async fn run_state_subscriber(app: AppHandle) {
    const BASE_BACKOFF: Duration = Duration::from_millis(100);
    const MAX_BACKOFF: Duration = Duration::from_secs(5);
    let mut backoff = BASE_BACKOFF;

    loop {
        match connect_and_subscribe(&app).await {
            Ok(()) => {
                // Reset so a normal daemon restart doesn't accumulate ramp delay.
                backoff = BASE_BACKOFF;
                debug!("state subscriber: daemon closed the subscription cleanly");
            }
            Err(e) => {
                debug!("state subscriber: {e}");
            }
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// One attempt at opening a subscribe connection and processing updates
/// until the daemon goes away or the pipe errors.
async fn connect_and_subscribe(app: &AppHandle) -> std::io::Result<()> {
    let mut conn = daemon_client::connect().await?;
    conn.handshake().await?;
    conn.send(&ControlFrame::Subscribe).await?;
    info!("state subscriber: connected to daemon");

    loop {
        let frame = match conn.recv().await? {
            Some(f) => f,
            None => return Ok(()),
        };

        match frame {
            ControlFrame::StateSnapshot { state } => {
                apply_snapshot(app, state).await;
            }
            ControlFrame::SessionsChanged { repo: _ } => {
                // Daemon detected a supervisor death and marked a session
                // exited in the DB. Kick the sidebar to re-query; we don't
                // bother scoping the refresh by repo since list_sessions
                // is cheap and sidebar.refresh() reloads everything.
                let _ = app.emit("sidebar-changed", ());
            }
            ControlFrame::Error { message } => {
                warn!("state subscriber: daemon error: {message}");
                return Ok(());
            }
            other => {
                debug!("state subscriber: unexpected frame: {other:?}");
            }
        }
    }
}

/// Merge a new `RepoState` into the app's cache and emit targeted Tauri
/// events for anything that actually changed. Idempotent — applying the
/// same snapshot twice is a no-op.
async fn apply_snapshot(app: &AppHandle, state: RepoState) {
    let app_state = app.state::<AppState>();
    let mut cache = app_state.repo_states.lock().await;

    let alias = state.repo_alias.clone();
    let prev = cache.get(&alias).cloned();

    // Ignore out-of-order snapshots (defensive against reconnect reorderings).
    // Version is monotonic per repo, so anything non-increasing is stale —
    // even if the payload happens to differ, trusting a lower-versioned
    // snapshot would overwrite fresher state.
    if let Some(ref prev_state) = prev {
        if state.version <= prev_state.version {
            return;
        }
    }

    // Diff workspaces and emit per-workspace change events.
    let empty: HashMap<String, shard_core::state::WorkspaceStatus> = HashMap::new();
    let prev_ws = prev.as_ref().map(|p| &p.workspaces).unwrap_or(&empty);

    for (ws_name, status) in &state.workspaces {
        let changed = match prev_ws.get(ws_name) {
            Some(prev) => prev != status,
            None => true,
        };
        if changed {
            let _ = app.emit(
                "workspace-status-changed",
                WorkspaceStatusChanged {
                    repo: &alias,
                    workspace: ws_name,
                    status: Some(status),
                },
            );
        }
    }

    // Workspaces that disappeared from the snapshot (e.g. because the
    // user removed them, or their repo was dropped). Belt-and-suspenders
    // alongside `sidebar-changed`: ensures the frontend clears any lingering
    // live-state overlay before the next structural refresh lands.
    for (ws_name, _) in prev_ws {
        if !state.workspaces.contains_key(ws_name) {
            let _ = app.emit(
                "workspace-status-changed",
                WorkspaceStatusChanged {
                    repo: &alias,
                    workspace: ws_name,
                    status: None,
                },
            );
        }
    }

    cache.insert(alias, state);
}
