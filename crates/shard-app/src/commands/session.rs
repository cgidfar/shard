use std::collections::hash_map::Entry;

use tauri::ipc::{Channel, Response};
use tauri::{Emitter, Manager};

use shard_core::default_command;
use shard_core::repos::RepositoryStore;
use shard_core::sessions::{Session, SessionStore};
use shard_core::workspaces::WorkspaceStore;
use shard_core::{Harness, ShardPaths};
use shard_transport::protocol::{self, ActivityState, ClientKind, Frame, OwnerSummary};
use shard_transport::transport_windows::NamedPipeTransport;
use shard_transport::SessionTransport;

use std::sync::Arc;

use crate::daemon_ipc;
use crate::state::{AppState, AttachmentHandle, ConnectionToken, MonitorHandle};

#[derive(Clone, serde::Serialize)]
pub struct SessionInfo {
    repo: String,
    session: Session,
}

#[derive(Clone, serde::Serialize)]
struct SessionActivityEvent {
    id: String,
    state: &'static str, // "active" | "idle" | "blocked"
}

#[derive(Clone, serde::Serialize)]
struct TerminalEndedEvent {
    id: String,
    status: &'static str,
    code: u8,
}

#[derive(Clone, serde::Serialize)]
pub struct SessionInputStateEvent {
    pub id: String,
    pub owner_label: Option<String>,
    pub owner_kind: Option<&'static str>,
    pub owner_client_id: Option<u64>,
}

#[derive(Clone, serde::Serialize)]
struct SessionTitleChangedEvent {
    id: String,
    title: String,
}

#[derive(Clone, serde::Serialize)]
pub struct SessionTitleEntry {
    pub id: String,
    pub title: String,
}

#[derive(Clone, serde::Serialize)]
pub struct SessionActivityEntry {
    pub id: String,
    pub state: &'static str,
}

fn activity_state_str(s: ActivityState) -> &'static str {
    match s {
        ActivityState::Active => "active",
        ActivityState::Idle => "idle",
        ActivityState::Blocked => "blocked",
    }
}

fn owner_kind_str(k: ClientKind) -> &'static str {
    match k {
        ClientKind::GuiWindow => "gui",
        ClientKind::CliAgent => "cli",
        ClientKind::MonitorOnly => "monitor",
    }
}

fn emit_input_state(app: &tauri::AppHandle, session_id: &str, owner: Option<&OwnerSummary>) {
    let event = SessionInputStateEvent {
        id: session_id.to_string(),
        owner_label: owner.map(|o| o.label.clone()),
        owner_kind: owner.map(|o| owner_kind_str(o.kind)),
        owner_client_id: owner.map(|o| o.client_id),
    };
    let _ = app.emit("session-input-state", event);
}

async fn record_and_emit_input_state(
    app: &tauri::AppHandle,
    session_id: &str,
    owner: Option<&OwnerSummary>,
) {
    let state = app.state::<AppState>();
    state
        .input_owners
        .lock()
        .await
        .insert(session_id.to_string(), owner.cloned());
    emit_input_state(app, session_id, owner);
}

/// Handle ActivityUpdate / Status / InputOwnerChanged frames common to both
/// monitors and attach readers. Returns `true` if the caller should break
/// its read loop (session ended).
async fn handle_supervisor_frame(app: &tauri::AppHandle, session_id: &str, frame: &Frame) -> bool {
    match frame {
        Frame::ActivityUpdate { state } => {
            let state_str = activity_state_str(*state);
            // Cache so windows opened later can hydrate without waiting for
            // the next ActivityUpdate.
            app.state::<AppState>()
                .activity_states
                .lock()
                .await
                .insert(session_id.to_string(), *state);
            let _ = app.emit(
                "session-activity",
                SessionActivityEvent {
                    id: session_id.to_string(),
                    state: state_str,
                },
            );
            false
        }
        Frame::InputOwnerChanged { owner } => {
            record_and_emit_input_state(app, session_id, owner.as_ref()).await;
            false
        }
        Frame::Status { code } => {
            // Lifecycle termination — update DB and notify sidebar
            let status = match code {
                0 => "exited",
                1 => "stopped",
                _ => "failed",
            };
            if let Ok(paths) = ShardPaths::new() {
                let store = SessionStore::new(paths);
                if let Ok((repo, _)) = store.find_by_id(session_id) {
                    let _ = store.update_status(&repo, session_id, status, Some(*code as i32));
                }
            }
            let app_state = app.state::<AppState>();
            app_state.input_owners.lock().await.remove(session_id);
            app_state.dynamic_titles.lock().await.remove(session_id);
            app_state.activity_states.lock().await.remove(session_id);
            let _ = app.emit("sidebar-changed", ());
            true
        }
        _ => false,
    }
}

#[tauri::command]
pub async fn list_session_input_owners(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<SessionInputStateEvent>, String> {
    let owners = state.input_owners.lock().await;
    Ok(owners
        .iter()
        .map(|(id, owner)| SessionInputStateEvent {
            id: id.clone(),
            owner_label: owner.as_ref().map(|o| o.label.clone()),
            owner_kind: owner.as_ref().map(|o| owner_kind_str(o.kind)),
            owner_client_id: owner.as_ref().map(|o| o.client_id),
        })
        .collect())
}

/// Record an OSC terminal title observed by one window's xterm.js and
/// broadcast it to every window so their sidebars stay in sync. The cache
/// also lets newly opened windows hydrate via `list_session_titles`.
#[tauri::command]
pub async fn notify_session_title(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
    title: String,
) -> Result<(), String> {
    {
        let mut titles = state.dynamic_titles.lock().await;
        match titles.get(&id) {
            Some(existing) if existing == &title => return Ok(()),
            _ => titles.insert(id.clone(), title.clone()),
        };
    }
    let _ = app.emit("session-title-changed", SessionTitleChangedEvent { id, title });
    Ok(())
}

#[tauri::command]
pub async fn list_session_titles(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<SessionTitleEntry>, String> {
    let titles = state.dynamic_titles.lock().await;
    Ok(titles
        .iter()
        .map(|(id, title)| SessionTitleEntry {
            id: id.clone(),
            title: title.clone(),
        })
        .collect())
}

#[tauri::command]
pub async fn list_session_activities(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<SessionActivityEntry>, String> {
    let activities = state.activity_states.lock().await;
    Ok(activities
        .iter()
        .map(|(id, s)| SessionActivityEntry {
            id: id.clone(),
            state: activity_state_str(*s),
        })
        .collect())
}

/// Start the process-global monitor connection for a session. Sends
/// `Hello { kind: MonitorOnly }` + `Resume { u64::MAX }`, then relays
/// activity / status / input-owner frames as Tauri events.
pub fn start_monitor(
    app: tauri::AppHandle,
    session_id: String,
    transport_addr: String,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        let client = match NamedPipeTransport::connect(&transport_addr).await {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(
                    "monitor connect failed for {}: {e}",
                    &session_id[..8.min(session_id.len())]
                );
                return;
            }
        };

        let (mut reader, mut writer) = tokio::io::split(client);
        let token = ConnectionToken::new();

        // Hello first (mandatory under SHA-21 protocol).
        if protocol::write_frame(
            &mut writer,
            &Frame::Hello {
                client_id: token.as_u64(),
                kind: ClientKind::MonitorOnly,
                label: "tauri:monitor".to_string(),
            },
        )
        .await
        .is_err()
        {
            return;
        }

        // u64::MAX still means "skip replay" — preserved for symmetry with
        // the streaming-attach path, even though MonitorOnly already short-
        // circuits replay supervisor-side.
        let _ = protocol::write_frame(
            &mut writer,
            &Frame::Resume {
                last_seen_offset: u64::MAX,
            },
        )
        .await;

        loop {
            match protocol::read_frame(&mut reader).await {
                Ok(Some(ref frame @ Frame::ActivityUpdate { .. }))
                | Ok(Some(ref frame @ Frame::Status { .. }))
                | Ok(Some(ref frame @ Frame::InputOwnerChanged { .. })) => {
                    if handle_supervisor_frame(&app, &session_id, frame).await {
                        break;
                    }
                }
                Ok(Some(_)) => {} // discard TerminalOutput etc.
                Ok(None) => break,
                Err(_) => break,
            }
        }
        tracing::debug!(
            "monitor ended for session {}",
            &session_id[..8.min(session_id.len())]
        );
    })
}

// ── IPC Commands ──

#[tauri::command]
pub fn list_sessions(
    repo: Option<String>,
    workspace: Option<String>,
) -> Result<Vec<SessionInfo>, String> {
    let paths = ShardPaths::new().map_err(|e| e.to_string())?;
    let session_store = SessionStore::new(ShardPaths::new().map_err(|e| e.to_string())?);

    let repos = if let Some(alias) = &repo {
        vec![alias.clone()]
    } else {
        let repo_store = RepositoryStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
        repo_store
            .list()
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|r| r.alias)
            .collect()
    };

    let mut all = Vec::new();
    for alias in &repos {
        let repo_db = paths.repo_db(alias);
        if !repo_db.exists() {
            continue;
        }
        let sessions = session_store
            .list(alias, workspace.as_deref())
            .map_err(|e| e.to_string())?;
        for s in sessions {
            all.push(SessionInfo {
                repo: alias.clone(),
                session: s,
            });
        }
    }

    Ok(all)
}

#[tauri::command]
pub fn create_session(
    app: tauri::AppHandle,
    repo: String,
    workspace_name: String,
    command: Option<Vec<String>>,
    harness: Option<Harness>,
) -> Result<Session, String> {
    let ws_store = WorkspaceStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    let _ws = ws_store
        .get(&repo, &workspace_name)
        .map_err(|e| e.to_string())?;

    let command = command.unwrap_or_else(default_command);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;

    let (session_id, transport_addr) = rt.block_on(async {
        use shard_transport::control_protocol::ControlFrame;

        let daemon_start_timeout = std::time::Duration::from_secs(5);
        let mut conn = daemon_ipc::connect_or_spawn(daemon_start_timeout)
            .await
            .map_err(|e| e.to_string())?;

        conn.handshake()
            .await
            .map_err(|e| format!("daemon handshake failed: {e}"))?;

        // Hooks install round-trip — non-fatal. A hooks failure must
        // not block session spawn (today's behavior preserved).
        let hooks_result: Result<(bool, Option<String>), String> = async {
            let mut hook_conn = daemon_ipc::connect_or_spawn(daemon_start_timeout)
                .await
                .map_err(|e| e.to_string())?;

            hook_conn
                .handshake()
                .await
                .map_err(|e| format!("daemon handshake failed: {e}"))?;

            hook_conn
                .request_typed(
                    &ControlFrame::InstallHarnessHooks {
                        harness: "claude-code".to_string(),
                    },
                    |f| match f {
                        ControlFrame::InstallHarnessHooksAck {
                            installed,
                            skipped_reason,
                        } => Some((installed, skipped_reason)),
                        _ => None,
                    },
                )
                .await
                .map_err(|e| e.to_string())
        }
        .await;

        match hooks_result {
            Ok((installed, skipped_reason)) => {
                tracing::info!(
                    installed,
                    skipped_reason = skipped_reason.as_deref(),
                    "hooks install ack",
                );
            }
            Err(e) => {
                tracing::warn!("hooks install failed: {e}");
            }
        }

        let response = conn
            .request(&ControlFrame::SpawnSession {
                repo: repo.clone(),
                workspace: workspace_name.clone(),
                command: command.clone(),
                harness: harness.map(|h| h.to_string()),
            })
            .await
            .map_err(|e| format!("daemon request failed: {e}"))?;

        match response {
            ControlFrame::SpawnAck {
                session_id,
                transport_addr,
                ..
            } => Ok((session_id, transport_addr)),
            ControlFrame::Error { message } => Err(format!("daemon: {message}")),
            other => Err(format!("unexpected daemon response: {other:?}")),
        }
    })?;

    drop(rt);

    let session_store = SessionStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    let result = session_store
        .get(&repo, &session_id)
        .map_err(|e| e.to_string())?;

    // Start the process-global monitor for the new session so every window
    // sees activity / status / ownership updates without being attached.
    let task = start_monitor(app.clone(), session_id.clone(), transport_addr);
    {
        let state: tauri::State<'_, AppState> = app.state();
        let mut monitors = state.monitors.blocking_lock();
        monitors.insert(session_id.clone(), MonitorHandle { task });
    }

    let _ = app.emit("sidebar-changed", ());
    Ok(result)
}

#[tauri::command]
pub async fn stop_session(
    app: tauri::AppHandle,
    id: String,
    force: bool,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let (_repo, session) = daemon_ipc::find_session_by_id(&id).await?;

    if session.status != "running" && session.status != "starting" {
        return Ok(());
    }

    // Abort everything tracking this session in this process before the
    // daemon's drain runs — every attachment across every window, plus the
    // monitor.
    {
        let mut attachments = state.attachments.lock().await;
        let keys_to_remove: Vec<(String, String)> = attachments
            .keys()
            .filter(|(_w, sid)| sid == &id)
            .cloned()
            .collect();
        for key in keys_to_remove {
            if let Some(handle) = attachments.remove(&key) {
                handle.task.abort();
            }
        }
    }
    {
        let mut session_windows = state.session_windows.lock().await;
        session_windows.remove(&id);
    }
    {
        let mut monitors = state.monitors.lock().await;
        if let Some(handle) = monitors.remove(&id) {
            handle.task.abort();
        }
    }

    daemon_ipc::stop_session(&id, force).await?;
    let _ = app.emit("sidebar-changed", ());
    Ok(())
}

#[tauri::command]
pub async fn remove_session(app: tauri::AppHandle, id: String) -> Result<(), String> {
    let (repo, _) = daemon_ipc::find_session_by_id(&id).await?;
    daemon_ipc::remove_session(&repo, &id).await?;
    let _ = app.emit("sidebar-changed", ());
    Ok(())
}

async fn await_claim_confirmation<R: tokio::io::AsyncRead + Unpin>(
    app: &tauri::AppHandle,
    session_id: &str,
    reader: &mut R,
    channel: &Channel<Response>,
    client_id: u64,
) -> Result<(), String> {
    let wait = async {
        loop {
            match protocol::read_frame(reader).await {
                Ok(Some(Frame::TerminalOutput { data, .. })) => {
                    channel
                        .send(Response::new(data))
                        .map_err(|e| format!("attach channel send failed: {e}"))?;
                }
                Ok(Some(ref frame @ Frame::ActivityUpdate { .. })) => {
                    handle_supervisor_frame(app, session_id, frame).await;
                }
                Ok(Some(ref frame @ Frame::InputOwnerChanged { ref owner })) => {
                    handle_supervisor_frame(app, session_id, frame).await;
                    if matches!(owner, Some(o) if o.client_id == client_id) {
                        return Ok(());
                    }
                }
                Ok(Some(Frame::ClaimRejected { owner })) => {
                    record_and_emit_input_state(app, session_id, owner.as_ref()).await;
                    return match owner {
                        Some(o) if o.kind == ClientKind::GuiWindow => {
                            Err(format!("owned-by:{}", o.label))
                        }
                        Some(o) => Err(format!(
                            "input owned by {} client '{}'",
                            owner_kind_str(o.kind),
                            o.label
                        )),
                        None => Err("input claim rejected".to_string()),
                    };
                }
                Ok(Some(ref frame @ Frame::Status { .. })) => {
                    handle_supervisor_frame(app, session_id, frame).await;
                    return Err("session ended while attaching".to_string());
                }
                Ok(Some(_)) => {}
                Ok(None) => return Err("session pipe closed while attaching".to_string()),
                Err(e) => return Err(e.to_string()),
            }
        }
    };

    tokio::time::timeout(std::time::Duration::from_secs(5), wait)
        .await
        .map_err(|_| "timed out waiting for input ownership".to_string())?
}

#[tauri::command]
pub async fn attach_session(
    app: tauri::AppHandle,
    window: tauri::Window,
    id: String,
    channel: Channel<Response>,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let window_label = window.label().to_string();
    let (_repo, session) = daemon_ipc::find_session_by_id(&id).await?;

    if session.status != "running" {
        return Err(format!(
            "session {} is '{}', not 'running'",
            id, session.status
        ));
    }

    // Re-attach within the same window: tear down the old reader. The
    // process-global monitor is left alone — it does not conflict with
    // window attachments and aborting it here would race the start-monitor
    // path on stop_session.
    {
        let mut attachments = state.attachments.lock().await;
        if let Some(prev) = attachments.remove(&(window_label.clone(), id.clone())) {
            prev.task.abort();
        }
    }

    let client = match NamedPipeTransport::connect(&session.transport_addr).await {
        Ok(client) => client,
        Err(e) => return Err(e.to_string()),
    };

    let (mut reader, writer) = tokio::io::split(client);
    let token = ConnectionToken::new();
    let client_id = token.as_u64();

    // Hello → Resume → ClaimInput. The supervisor is the single authority
    // for cross-window ownership, so we wait for either an owner broadcast
    // that names this connection or a ClaimRejected response before
    // returning success to the frontend.
    let mut writer = writer;
    if let Err(e) = protocol::write_frame(
        &mut writer,
        &Frame::Hello {
            client_id,
            kind: ClientKind::GuiWindow,
            label: window_label.clone(),
        },
    )
    .await
    {
        return Err(e.to_string());
    }
    if let Err(e) = protocol::write_frame(
        &mut writer,
        &Frame::Resume {
            last_seen_offset: 0,
        },
    )
    .await
    {
        return Err(e.to_string());
    }
    if let Err(e) = protocol::write_frame(&mut writer, &Frame::ClaimInput).await {
        return Err(e.to_string());
    }
    await_claim_confirmation(&app, &id, &mut reader, &channel, client_id).await?;

    let shared_writer: crate::state::SharedSessionWriter =
        Arc::new(tokio::sync::Mutex::new(writer));
    let session_id = id.clone();
    let app_clone = app.clone();
    let window_label_for_task = window_label.clone();
    let task = tauri::async_runtime::spawn(async move {
        let mut terminal_status: Option<(&'static str, u8)> = None;
        loop {
            match protocol::read_frame(&mut reader).await {
                Ok(Some(Frame::TerminalOutput { data, .. })) => {
                    if let Err(e) = channel.send(Response::new(data)) {
                        tracing::warn!("attach channel send failed for session {session_id}: {e}");
                        if terminal_status.is_none() {
                            terminal_status = Some(("failed", 255));
                        }
                        break;
                    }
                }
                Ok(Some(ref frame @ Frame::ActivityUpdate { .. })) => {
                    handle_supervisor_frame(&app_clone, &session_id, frame).await;
                }
                Ok(Some(ref frame @ Frame::InputOwnerChanged { .. })) => {
                    handle_supervisor_frame(&app_clone, &session_id, frame).await;
                }
                Ok(Some(ref frame @ Frame::Status { .. })) => {
                    if let Frame::Status { code } = frame {
                        let status = match code {
                            0 => "exited",
                            1 => "stopped",
                            _ => "failed",
                        };
                        terminal_status = Some((status, *code));
                    }
                    handle_supervisor_frame(&app_clone, &session_id, frame).await;
                    let deadline =
                        tokio::time::Instant::now() + std::time::Duration::from_millis(500);
                    while let Ok(Ok(Some(Frame::TerminalOutput { data, .. }))) =
                        tokio::time::timeout_at(deadline, protocol::read_frame(&mut reader)).await
                    {
                        let _ = channel.send(Response::new(data));
                    }
                    break;
                }
                Ok(None) => {
                    if terminal_status.is_none() {
                        terminal_status = Some(("failed", 255));
                    }
                    break;
                }
                Ok(Some(_)) => {}
                Err(_) => {
                    if terminal_status.is_none() {
                        terminal_status = Some(("failed", 255));
                    }
                    break;
                }
            }
        }
        if let Some((status, code)) = terminal_status {
            let _ = app_clone.emit(
                "terminal-ended",
                TerminalEndedEvent {
                    id: session_id.clone(),
                    status,
                    code,
                },
            );
        }
        // Self-cleanup. Use the token to guard against an interleaved
        // re-attach having already replaced this entry.
        let state = app_clone.state::<AppState>();
        {
            let mut attachments = state.attachments.lock().await;
            let key = (window_label_for_task.clone(), session_id.clone());
            let should_remove = matches!(
                attachments.get(&key),
                Some(handle) if handle.token == token
            );
            if should_remove {
                attachments.remove(&key);
            }
        }
        {
            let mut session_windows = state.session_windows.lock().await;
            if matches!(session_windows.get(&session_id), Some(w) if w == &window_label_for_task) {
                session_windows.remove(&session_id);
            }
        }
        tracing::debug!("attach reader ended for {window_label_for_task}/{session_id}");
    });

    {
        let mut attachments = state.attachments.lock().await;
        match attachments.entry((window_label.clone(), id.clone())) {
            Entry::Vacant(entry) => {
                entry.insert(AttachmentHandle {
                    token,
                    writer: shared_writer,
                    task,
                    client_id,
                });
            }
            Entry::Occupied(_) => {
                task.abort();
                return Err("attachment changed while attaching".into());
            }
        }
    }
    {
        let mut session_windows = state.session_windows.lock().await;
        session_windows.insert(id, window_label);
    }

    Ok(())
}

#[tauri::command]
pub async fn write_to_session(
    window: tauri::Window,
    id: String,
    data: Vec<u8>,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let window_label = window.label().to_string();
    // Clone the Arc out from under the attachments lock so the lock is
    // released before we await the pipe write — otherwise a slow / blocked
    // pipe would freeze every other session command.
    let writer = {
        let attachments = state.attachments.lock().await;
        attachments
            .get(&(window_label, id))
            .ok_or("session not attached in this window")?
            .writer
            .clone()
    };
    let mut w = writer.lock().await;
    let frame = Frame::TerminalInput { data };
    protocol::write_frame(&mut *w, &frame)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn resize_session(
    window: tauri::Window,
    id: String,
    rows: u16,
    cols: u16,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let window_label = window.label().to_string();
    let writer = {
        let attachments = state.attachments.lock().await;
        attachments
            .get(&(window_label, id))
            .ok_or("session not attached in this window")?
            .writer
            .clone()
    };
    let mut w = writer.lock().await;
    let frame = Frame::Resize { rows, cols };
    protocol::write_frame(&mut *w, &frame)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn rename_session(
    app: tauri::AppHandle,
    id: String,
    label: Option<String>,
) -> Result<(), String> {
    let (repo, _) = daemon_ipc::find_session_by_id(&id).await?;
    daemon_ipc::rename_session(&repo, &id, label).await?;
    let _ = app.emit("sidebar-changed", ());
    Ok(())
}

#[tauri::command]
pub async fn detach_session(
    app: tauri::AppHandle,
    window: tauri::Window,
    id: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let window_label = window.label().to_string();
    {
        let mut attachments = state.attachments.lock().await;
        if let Some(handle) = attachments.remove(&(window_label.clone(), id.clone())) {
            handle.task.abort();
        }
    }
    {
        let mut session_windows = state.session_windows.lock().await;
        if matches!(session_windows.get(&id), Some(w) if w == &window_label) {
            session_windows.remove(&id);
        }
    }

    // If the session is still running and no monitor exists, restart one
    // so the sidebar keeps getting activity / status updates.
    if let Ok((_repo, session)) = daemon_ipc::find_session_by_id(&id).await {
        if session.status == "running" {
            let mut monitors = state.monitors.lock().await;
            if !monitors.contains_key(&id) {
                let task = start_monitor(app, id.clone(), session.transport_addr);
                monitors.insert(id, MonitorHandle { task });
            }
        }
    }

    Ok(())
}

/// Release every attachment held by `window_label` (used by the per-window
/// `CloseRequested` handler). Synchronous lock on `attachments` so the
/// caller can run from a Tauri runtime callback that cannot await.
pub fn release_attachments_for_window(state: &AppState, window_label: &str) {
    {
        let mut attachments = state.attachments.blocking_lock();
        attachments.retain(|(w, _), handle| {
            if w == window_label {
                handle.task.abort();
                false
            } else {
                true
            }
        });
    }
    state
        .session_windows
        .blocking_lock()
        .retain(|_, owner| owner != window_label);
}
