use shard_core::repos::Repository;
use tauri::Emitter;

use crate::daemon_ipc;

#[tauri::command]
pub async fn list_repos() -> Result<Vec<Repository>, String> {
    daemon_ipc::list_repos().await
}

#[tauri::command]
pub async fn add_repo(
    app: tauri::AppHandle,
    url: String,
    alias: Option<String>,
) -> Result<Repository, String> {
    let repo = daemon_ipc::add_repo(&url, alias).await?;
    let _ = app.emit("sidebar-changed", ());
    Ok(repo)
}

#[tauri::command]
pub async fn sync_repo(alias: String) -> Result<(), String> {
    daemon_ipc::sync_repo(&alias).await
}

#[tauri::command]
pub async fn remove_repo(app: tauri::AppHandle, alias: String) -> Result<(), String> {
    daemon_ipc::remove_repo(&alias).await?;
    let _ = app.emit("sidebar-changed", ());
    Ok(())
}
