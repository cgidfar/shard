mod commands;
mod state;

use state::AppState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
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
            commands::session::attach_session,
            commands::session::write_to_session,
            commands::session::resize_session,
            commands::session::detach_session,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
