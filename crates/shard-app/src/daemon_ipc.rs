//! App-side helpers for talking to the daemon over the control pipe.
//!
//! Two consumers live here:
//!   - `spawn_topology_poke` — fire-and-forget notification sent from Tauri
//!     commands after a repo/workspace mutation commits, so the daemon can
//!     reload its in-memory topology.
//!   - `run_state_subscriber` — long-lived background task that subscribes
//!     to daemon state updates and re-emits them as Tauri events. Handles
//!     reconnect-with-backoff on daemon drop so the UI never wedges.

use std::collections::HashMap;
use std::time::Duration;

use shard_core::state::RepoState;
use shard_transport::control_protocol::ControlFrame;
use shard_transport::daemon_client;
use tauri::{AppHandle, Emitter, Manager};
use tracing::{debug, info, warn};

use crate::state::AppState;

// ── Topology pokes ──────────────────────────────────────────────────────────

/// Fire-and-forget notification to the daemon that the repo/workspace
/// topology has just changed. `None` requests a full reload; `Some(alias)`
/// scopes the reload to one repo. Poke failures are logged, never
/// propagated — the UI must never block on daemon health.
pub fn spawn_topology_poke(alias: Option<String>) {
    tauri::async_runtime::spawn(async move {
        if let Err(e) = send_topology_poke(alias.clone()).await {
            debug!("topology poke (alias={:?}) failed: {e}", alias);
        }
    });
}

async fn send_topology_poke(alias: Option<String>) -> std::io::Result<()> {
    let mut conn = daemon_client::connect().await?;
    conn.handshake().await?;
    conn.send(&ControlFrame::TopologyChanged { repo_alias: alias })
        .await?;
    Ok(())
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

/// Event payload for `session-liveness-changed` Tauri events.
#[derive(serde::Serialize, Clone)]
struct SessionLivenessChanged<'a> {
    session_id: &'a str,
    alive: bool,
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
    if let Some(ref prev_state) = prev {
        if state.version <= prev_state.version && prev_state.workspaces == state.workspaces
            && prev_state.sessions_alive == state.sessions_alive
        {
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

    // Diff session liveness. Only emit when a value changes — the set of
    // sessions themselves is driven by the existing sidebar-changed path.
    let prev_sessions: HashMap<String, bool> =
        prev.as_ref().map(|p| p.sessions_alive.clone()).unwrap_or_default();
    for (sid, alive) in &state.sessions_alive {
        let changed = prev_sessions.get(sid) != Some(alive);
        if changed {
            let _ = app.emit(
                "session-liveness-changed",
                SessionLivenessChanged {
                    session_id: sid,
                    alive: *alive,
                },
            );
        }
    }

    cache.insert(alias, state);
}
