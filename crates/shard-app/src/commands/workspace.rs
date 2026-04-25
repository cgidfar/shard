use shard_core::workspaces::{BranchInfo, Workspace, WorkspaceMode, WorkspaceWithStatus};
use tauri::Emitter;

use crate::daemon_ipc;

#[tauri::command]
pub async fn list_workspaces(
    _app: tauri::AppHandle,
    repo: String,
) -> Result<Vec<WorkspaceWithStatus>, String> {
    daemon_ipc::list_workspaces(&repo).await
}

#[tauri::command]
pub async fn create_workspace(
    app: tauri::AppHandle,
    repo: String,
    name: Option<String>,
    mode: WorkspaceMode,
    branch: Option<String>,
) -> Result<Workspace, String> {
    let ws = daemon_ipc::create_workspace(&repo, name, mode, branch).await?;
    let _ = app.emit("sidebar-changed", ());
    Ok(ws)
}

#[tauri::command]
pub async fn list_repo_branches(repo: String) -> Result<Vec<BranchInfo>, String> {
    daemon_ipc::list_branch_info(&repo).await
}

#[tauri::command]
pub async fn remove_workspace(
    app: tauri::AppHandle,
    repo: String,
    name: String,
) -> Result<(), String> {
    daemon_ipc::remove_workspace(&repo, &name).await?;
    let _ = app.emit("sidebar-changed", ());
    Ok(())
}
