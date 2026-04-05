use shard_core::workspaces::{Workspace, WorkspaceStore};
use shard_core::ShardPaths;

#[tauri::command]
pub fn list_workspaces(repo: String) -> Result<Vec<Workspace>, String> {
    let store = WorkspaceStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    store.list(&repo).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn create_workspace(
    repo: String,
    name: Option<String>,
    branch: Option<String>,
) -> Result<Workspace, String> {
    let store = WorkspaceStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    store
        .create(&repo, name.as_deref(), branch.as_deref())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn remove_workspace(repo: String, name: String) -> Result<(), String> {
    let store = WorkspaceStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    store.remove(&repo, &name).map_err(|e| e.to_string())
}
