use shard_core::state::WorkspaceStatus;
use shard_core::workspaces::{BranchInfo, Workspace, WorkspaceMode, WorkspaceStore};
use shard_core::ShardPaths;
use tauri::{Emitter, Manager};

use crate::daemon_ipc::spawn_topology_poke;
use crate::state::AppState;

/// Workspace payload surfaced to the frontend. Carries both the persisted
/// row (path, is_base, created_at) and the live derived `WorkspaceStatus`
/// computed by the daemon monitor.
///
/// `status` is `None` when the monitor has not yet reported a snapshot for
/// this repo — e.g. immediately after adding a repo, before the next round
/// trip completes. The frontend should render such rows with a neutral
/// placeholder and rely on the `workspace-status-changed` event to fill in.
#[derive(serde::Serialize, Clone)]
pub struct WorkspaceWithStatus {
    #[serde(flatten)]
    pub workspace: Workspace,
    pub status: Option<WorkspaceStatus>,
}

#[tauri::command]
pub async fn list_workspaces(
    app: tauri::AppHandle,
    repo: String,
) -> Result<Vec<WorkspaceWithStatus>, String> {
    let store = WorkspaceStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    let workspaces = store.list(&repo).map_err(|e| e.to_string())?;

    let state = app.state::<AppState>();
    let repo_states = state.repo_states.lock().await;
    let snapshot = repo_states.get(&repo);

    let enriched = workspaces
        .into_iter()
        .map(|ws| {
            let status = snapshot.and_then(|s| s.workspaces.get(&ws.name).cloned());
            WorkspaceWithStatus { workspace: ws, status }
        })
        .collect();

    Ok(enriched)
}

#[tauri::command]
pub fn create_workspace(
    app: tauri::AppHandle,
    repo: String,
    name: Option<String>,
    mode: WorkspaceMode,
    branch: Option<String>,
) -> Result<Workspace, String> {
    let store = WorkspaceStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    let ws = store
        .create(&repo, name.as_deref(), mode, branch.as_deref(), false)
        .map_err(|e| e.to_string())?;
    spawn_topology_poke(Some(repo));
    let _ = app.emit("sidebar-changed", ());
    Ok(ws)
}

#[tauri::command]
pub fn list_repo_branches(repo: String) -> Result<Vec<BranchInfo>, String> {
    let store = WorkspaceStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    store.list_branch_info(&repo).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn remove_workspace(app: tauri::AppHandle, repo: String, name: String) -> Result<(), String> {
    let store = WorkspaceStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    store.remove(&repo, &name).map_err(|e| e.to_string())?;
    spawn_topology_poke(Some(repo));
    let _ = app.emit("sidebar-changed", ());
    Ok(())
}
