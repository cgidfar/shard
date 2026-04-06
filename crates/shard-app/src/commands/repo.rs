use shard_core::repos::{Repository, RepositoryStore};
use shard_core::workspaces::WorkspaceStore;
use shard_core::ShardPaths;
use tauri::Emitter;

#[tauri::command]
pub fn list_repos() -> Result<Vec<Repository>, String> {
    let store = RepositoryStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    store.list().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn add_repo(app: tauri::AppHandle, url: String, alias: Option<String>) -> Result<Repository, String> {
    let paths = ShardPaths::new().map_err(|e| e.to_string())?;
    let store = RepositoryStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    let repo = store.add(&url, alias.as_deref()).map_err(|e| e.to_string())?;

    // Auto-create a workspace for the default branch
    let ws_store = WorkspaceStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    let is_local = repo.local_path.is_some();
    let source_dir = paths.repo_source_for_repo(&repo.alias, repo.local_path.as_deref());
    match shard_core::git::default_branch(&source_dir) {
        Ok(branch) => {
            if let Err(e) = ws_store.create(&repo.alias, Some(&branch), Some(&branch), is_local) {
                tracing::warn!("auto-create default workspace failed: {e}");
            }
        }
        Err(e) => {
            tracing::warn!("could not detect default branch: {e}");
        }
    }

    let _ = app.emit("sidebar-changed", ());
    Ok(repo)
}

#[tauri::command]
pub fn sync_repo(alias: String) -> Result<(), String> {
    let store = RepositoryStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    store.sync(&alias).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn remove_repo(app: tauri::AppHandle, alias: String) -> Result<(), String> {
    let store = RepositoryStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    store.remove(&alias).map_err(|e| e.to_string())?;
    let _ = app.emit("sidebar-changed", ());
    Ok(())
}
