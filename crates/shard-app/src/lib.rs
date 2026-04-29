mod commands;
mod daemon_ipc;
mod state;

use state::AppState;
use tauri::Manager;

/// Ensure the daemon is running before the app starts.
/// Spawns it if not already alive. Non-fatal on failure (app still works,
/// just won't have daemon features until first create_session).
fn ensure_daemon_running() {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => return,
    };

    rt.block_on(async {
        let _ = daemon_ipc::connect_or_spawn(std::time::Duration::from_secs(3)).await;
    });
}

/// Start monitors for all currently running sessions.
fn start_monitors_for_running_sessions(app: &tauri::AppHandle) {
    use shard_core::repos::RepositoryStore;
    use shard_core::sessions::SessionStore;
    use shard_core::ShardPaths;

    let Ok(paths) = ShardPaths::new() else { return };
    let repo_store = RepositoryStore::new(ShardPaths::new().unwrap());
    let session_store = SessionStore::new(ShardPaths::new().unwrap());

    let Ok(repos) = repo_store.list() else { return };

    // Collect running sessions first (DB I/O) without holding the lock
    let mut running: Vec<(String, String, String)> = Vec::new(); // (id, transport_addr, label)
    for repo in &repos {
        if !paths.repo_db(&repo.alias).exists() {
            continue;
        }
        let Ok(sessions) = session_store.list(&repo.alias, None) else {
            continue;
        };
        for session in &sessions {
            if session.status != "running" {
                continue;
            }
            running.push((
                session.id.clone(),
                session.transport_addr.clone(),
                format!("{}:{}", repo.alias, session.workspace_name),
            ));
        }
    }

    if running.is_empty() {
        return;
    }

    // Spawn monitors and batch-insert under a single short lock
    let state: tauri::State<'_, AppState> = app.state();
    let mut conns = state.connections.blocking_lock();
    for (id, addr, label) in running {
        let task = commands::session::start_monitor(app.clone(), id.clone(), addr);
        conns.insert(id.clone(), state::SessionConnection::Monitored { task });
        tracing::info!("Started monitor for session {} [{label}]", &id[..8]);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Initialize tracing to file so logs work even when spawned with Stdio::null()
    let paths = shard_core::ShardPaths::new().ok();
    if let Some(ref p) = paths {
        let log_dir = p.data_dir();
        let _ = std::fs::create_dir_all(log_dir);
        if let Ok(log_file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_dir.join("app.log"))
        {
            tracing_subscriber::fmt()
                .with_writer(std::sync::Mutex::new(log_file))
                .with_env_filter(
                    tracing_subscriber::EnvFilter::from_default_env()
                        .add_directive(tracing::Level::INFO.into()),
                )
                .init();
        }
    }

    // Ensure daemon is running before anything else.
    // This handles session pruning/adoption and makes the tray icon visible.
    ensure_daemon_running();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::new())
        .setup(|app| {
            start_monitors_for_running_sessions(app.handle());

            // Long-lived daemon state subscription. Runs for the lifetime
            // of the app, reconnects with backoff on daemon drop, keeps
            // the last-known RepoState cache warm in AppState.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                daemon_ipc::run_state_subscriber(handle).await;
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::repo::list_repos,
            commands::repo::add_repo,
            commands::repo::sync_repo,
            commands::repo::remove_repo,
            commands::workspace::list_workspaces,
            commands::workspace::create_workspace,
            commands::workspace::remove_workspace,
            commands::workspace::list_repo_branches,
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
