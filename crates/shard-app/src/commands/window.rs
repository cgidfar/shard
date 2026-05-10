//! Window-management IPC commands. Backs the "Shard +" titlebar button,
//! `Ctrl+N`, and the focus-other-window UX when a session is already
//! attached elsewhere.

use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::state::AppState;

/// Build a new app window with our standard chrome (no native decorations,
/// 1200×800, centered). Shared between the `open_new_window` IPC command
/// and daemon-driven new-window requests so window defaults don't drift
/// between the two paths.
pub fn build_new_window(app: &tauri::AppHandle, label: &str) -> Result<(), String> {
    let url = WebviewUrl::App("index.html".into());
    WebviewWindowBuilder::new(app, label, url)
        .title("Shard")
        .inner_size(1200.0, 800.0)
        .resizable(true)
        .decorations(false)
        .center()
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Returns the calling window's Tauri label. Each frontend WebView calls
/// this once on init so it can attach the label to subsequent
/// `attach_session` / `write_to_session` / etc. invocations.
#[tauri::command]
pub fn get_window_label(window: tauri::Window) -> String {
    window.label().to_string()
}

/// Open a fresh empty window. Returns the new window's label.
#[tauri::command]
pub async fn open_new_window(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let label = state.allocate_window_label();
    build_new_window(&app, &label)?;
    Ok(label)
}

/// Bring the named window to the foreground. Used when a session click
/// targets a session already attached in another window — instead of
/// fighting for ownership, focus the owner.
#[tauri::command]
pub fn focus_window(app: tauri::AppHandle, label: String) -> Result<(), String> {
    focus_window_by_label(&app, &label)
}

#[derive(Clone, serde::Serialize)]
struct FocusSessionEvent {
    id: String,
}

/// Bring the named window forward and ask its frontend to activate the
/// already-owned session so the terminal receives keyboard focus.
#[tauri::command]
pub fn focus_session_window(
    app: tauri::AppHandle,
    label: String,
    session_id: String,
) -> Result<(), String> {
    focus_window_by_label(&app, &label)?;
    app.emit_to(
        &label,
        "focus-session",
        FocusSessionEvent { id: session_id },
    )
    .map_err(|e| e.to_string())
}

fn focus_window_by_label(app: &tauri::AppHandle, label: &str) -> Result<(), String> {
    let window = app
        .get_webview_window(label)
        .ok_or_else(|| format!("no window with label {label}"))?;
    window.unminimize().ok();
    window.show().map_err(|e| e.to_string())?;
    window.set_focus().map_err(|e| e.to_string())
}
