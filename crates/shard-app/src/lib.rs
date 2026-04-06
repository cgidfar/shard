mod commands;
mod state;

use state::AppState;

/// Prune sessions whose supervisor process is no longer alive.
/// Runs at app startup to reconcile stale DB records from prior crashes.
fn prune_stale_sessions() {
    use shard_core::db;
    use shard_core::repos::RepositoryStore;
    use shard_core::sessions::SessionStore;
    use shard_core::ShardPaths;
    use shard_supervisor::process::{PlatformProcessControl, ProcessControl};

    let Ok(paths) = ShardPaths::new() else { return };
    let repo_store = RepositoryStore::new(ShardPaths::new().unwrap());
    let session_store = SessionStore::new(ShardPaths::new().unwrap());

    let Ok(repos) = repo_store.list() else { return };

    for repo in &repos {
        if !paths.repo_db(&repo.alias).exists() {
            continue;
        }
        // Run migrations on existing repo DBs
        if let Ok(conn) = db::open_connection(&paths.repo_db(&repo.alias)) {
            let _ = db::init_repo_db(&conn);
        }
        let Ok(sessions) = session_store.list(&repo.alias, None) else {
            continue;
        };
        for session in &sessions {
            if session.status != "running" && session.status != "starting" {
                continue;
            }
            let alive = session
                .supervisor_pid
                .map(|pid| PlatformProcessControl::is_alive(pid))
                .unwrap_or(false);
            if !alive {
                let _ = session_store.update_status(
                    &repo.alias,
                    &session.id,
                    "failed",
                    None,
                );
                tracing::info!(
                    "Pruned stale session {} [{}:{}]",
                    &session.id[..8],
                    repo.alias,
                    session.workspace_name,
                );
            }
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    prune_stale_sessions();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            commands::repo::list_repos,
            commands::repo::add_repo,
            commands::repo::sync_repo,
            commands::repo::remove_repo,
            commands::workspace::list_workspaces,
            commands::workspace::create_workspace,
            commands::workspace::remove_workspace,
            commands::session::list_sessions,
            commands::session::create_session,
            commands::session::stop_session,
            commands::session::remove_session,
            commands::session::rename_session,
            commands::session::attach_session,
            commands::session::write_to_session,
            commands::session::resize_session,
            commands::session::detach_session,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
