use tauri::ipc::Channel;
use tauri::{Emitter, Manager};

use shard_core::repos::RepositoryStore;
use shard_core::sessions::{Session, SessionStore};
use shard_core::workspaces::WorkspaceStore;
use shard_core::{Harness, ShardPaths};
use shard_transport::protocol::{self, ActivityState, Frame};
use shard_transport::transport_windows::NamedPipeTransport;
use shard_transport::SessionTransport;

use crate::state::{AppState, SessionConnection, SessionWriter};

// ── Shared types & helpers ──

/// Default shell command for new sessions.
fn default_command() -> Vec<String> {
    if which_exists("pwsh.exe") {
        vec!["pwsh.exe".into(), "-NoLogo".into()]
    } else {
        let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        vec![shell]
    }
}

fn which_exists(name: &str) -> bool {
    std::process::Command::new("where")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

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

/// Handle ActivityUpdate and Status frames — shared by monitors and attach readers.
/// Returns `true` if the caller should break its read loop (session ended).
fn handle_supervisor_frame(app: &tauri::AppHandle, session_id: &str, frame: &Frame) -> bool {
    match frame {
        Frame::ActivityUpdate { state } => {
            let state_str = match state {
                ActivityState::Active => "active",
                ActivityState::Idle => "idle",
                ActivityState::Blocked => "blocked",
            };
            let _ = app.emit(
                "session-activity",
                SessionActivityEvent {
                    id: session_id.to_string(),
                    state: state_str,
                },
            );
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
            let _ = app.emit("sidebar-changed", ());
            true
        }
        _ => false,
    }
}

/// Start a lightweight monitor connection for a running session.
/// The monitor discards terminal output but relays ActivityUpdate and Status
/// frames as Tauri events. Returns the spawned task handle.
pub fn start_monitor(app: tauri::AppHandle, session_id: String, transport_addr: String) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        let client = match NamedPipeTransport::connect(&transport_addr).await {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!("monitor connect failed for {}: {e}", &session_id[..8.min(session_id.len())]);
                return;
            }
        };

        let (mut reader, mut writer) = tokio::io::split(client);

        // Send Resume with u64::MAX sentinel — skip replay, live-only
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
                | Ok(Some(ref frame @ Frame::Status { .. })) => {
                    if handle_supervisor_frame(&app, &session_id, frame) {
                        break;
                    }
                }
                Ok(Some(_)) => {} // discard TerminalOutput etc.
                Ok(None) => break,
                Err(_) => break,
            }
        }
        tracing::debug!("monitor ended for session {}", &session_id[..8.min(session_id.len())]);
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
    let paths = ShardPaths::new().map_err(|e| e.to_string())?;

    let ws_store = WorkspaceStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    let _ws = ws_store
        .get(&repo, &workspace_name)
        .map_err(|e| e.to_string())?;

    let command = command.unwrap_or_else(default_command);

    let session_store = SessionStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    let session = session_store
        .create(&repo, &workspace_name, &command, "pending", harness)
        .map_err(|e| e.to_string())?;
    let transport_addr = NamedPipeTransport::session_address(&session.id);
    session_store
        .update_transport_addr(&repo, &session.id, &transport_addr)
        .map_err(|e| e.to_string())?;

    let session_dir = paths.session_dir(&repo, &session.id);
    let ready_path = session_dir.join("ready");

    // Find shardctl.exe — same directory as this app's exe
    let exe_dir = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .ok_or("cannot find exe directory")?
        .to_path_buf();

    let shardctl = exe_dir.join("shardctl.exe");
    if !shardctl.exists() {
        return Err(format!(
            "shardctl.exe not found at {}",
            shardctl.display()
        ));
    }

    // Best-effort: install harness hooks so agents can report activity state
    if !shard_core::hooks::claude_code_hooks_installed() {
        if let Err(e) = shard_core::hooks::install_claude_code_hooks(&shardctl) {
            tracing::warn!("failed to install Claude Code hooks: {e}");
        }
    }

    let args: Vec<String> = vec![
        "session".into(),
        "serve".into(),
        "--repo".into(),
        repo.to_string(),
        "--workspace".into(),
        workspace_name.to_string(),
        "--session-id".into(),
        session.id.clone(),
        "--transport-addr".into(),
        transport_addr.clone(),
        "--".into(),
    ]
    .into_iter()
    .chain(command.iter().cloned())
    .collect();

    use shard_supervisor::process::{PlatformProcessControl, ProcessControl};
    let supervisor_pid = PlatformProcessControl::spawn_detached(&shardctl, &args)
        .map_err(|e| e.to_string())?;
    session_store
        .set_supervisor_pid(&repo, &session.id, supervisor_pid)
        .map_err(|e| e.to_string())?;

    // Wait for readiness
    let start = std::time::Instant::now();
    loop {
        if ready_path.exists() {
            break;
        }
        if !PlatformProcessControl::is_alive(supervisor_pid) {
            let _ = session_store.update_status(&repo, &session.id, "failed", None);
            return Err("supervisor process exited during startup".into());
        }
        if start.elapsed() > std::time::Duration::from_secs(10) {
            let _ = session_store.update_status(&repo, &session.id, "failed", None);
            let _ = PlatformProcessControl::terminate(supervisor_pid);
            return Err("supervisor did not start within 10 seconds".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let result = session_store
        .get(&repo, &session.id)
        .map_err(|e| e.to_string())?;

    // Start a monitor for the new session so sidebar gets activity updates
    let task = start_monitor(app.clone(), session.id.clone(), transport_addr);
    {
        let state: tauri::State<'_, AppState> = app.state();
        let mut conns = state.connections.blocking_lock();
        conns.insert(session.id.clone(), SessionConnection::Monitored { task });
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
    let session_store = SessionStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    let (repo, session) = session_store.find_by_id(&id).map_err(|e| e.to_string())?;

    if session.status != "running" && session.status != "starting" {
        return Ok(());
    }

    // Abort any existing monitor/attach task FIRST to avoid DB update races
    {
        let mut conns = state.connections.lock().await;
        if let Some(conn) = conns.remove(&id) {
            conn.abort();
        }
    }

    let frame = if force {
        Frame::StopForce
    } else {
        Frame::StopGraceful
    };

    let transport_addr = session.transport_addr.clone();

    let rpc_ok = match NamedPipeTransport::connect(&transport_addr).await {
        Ok(mut client) => {
            let _ = protocol::write_frame(&mut client, &frame).await;
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    match protocol::read_frame(&mut client).await {
                        // Only break on lifecycle Status (codes 0-2), not activity frames
                        Ok(Some(Frame::Status { code })) if code <= 2 => return,
                        Ok(None) => return,
                        _ => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
                    }
                }
            })
            .await;
            true
        }
        Err(_) => false,
    };

    if !rpc_ok {
        use shard_supervisor::process::{PlatformProcessControl, ProcessControl};
        if let Some(pid) = session.child_pid {
            let _ = PlatformProcessControl::terminate(pid);
        }
        if let Some(pid) = session.supervisor_pid {
            let _ = PlatformProcessControl::terminate(pid);
        }
    }

    session_store
        .update_status(&repo, &id, "stopped", None)
        .map_err(|e| e.to_string())?;
    let _ = app.emit("sidebar-changed", ());
    Ok(())
}

#[tauri::command]
pub fn remove_session(app: tauri::AppHandle, id: String) -> Result<(), String> {
    let session_store = SessionStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    let (repo, _) = session_store.find_by_id(&id).map_err(|e| e.to_string())?;
    session_store
        .remove(&repo, &id)
        .map_err(|e| e.to_string())?;
    let _ = app.emit("sidebar-changed", ());
    Ok(())
}

#[tauri::command]
pub async fn attach_session(
    app: tauri::AppHandle,
    id: String,
    channel: Channel<Vec<u8>>,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let session_store = SessionStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    let (_repo, session) = session_store.find_by_id(&id).map_err(|e| e.to_string())?;

    if session.status != "running" {
        return Err(format!(
            "session {} is '{}', not 'running'",
            id, session.status
        ));
    }

    // Abort existing monitor if present — we're taking over the connection
    {
        let mut conns = state.connections.lock().await;
        if let Some(conn) = conns.remove(&id) {
            conn.abort();
        }
    }

    let client = NamedPipeTransport::connect(&session.transport_addr)
        .await
        .map_err(|e| e.to_string())?;

    let (mut reader, writer) = tokio::io::split(client);

    // Send Resume frame
    let mut writer = writer;
    protocol::write_frame(&mut writer, &Frame::Resume { last_seen_offset: 0 })
        .await
        .map_err(|e| e.to_string())?;

    // Spawn reader task that forwards terminal output AND relays activity/status events
    let session_id = id.clone();
    let app_clone = app.clone();
    let task = tauri::async_runtime::spawn(async move {
        loop {
            match protocol::read_frame(&mut reader).await {
                Ok(Some(Frame::TerminalOutput { data, .. })) => {
                    if channel.send(data).is_err() {
                        break;
                    }
                }
                Ok(Some(ref frame @ Frame::ActivityUpdate { .. })) => {
                    handle_supervisor_frame(&app_clone, &session_id, frame);
                }
                Ok(Some(ref frame @ Frame::Status { .. })) => {
                    handle_supervisor_frame(&app_clone, &session_id, frame);
                    // Drain any trailing TerminalOutput (defensive)
                    let deadline = tokio::time::Instant::now()
                        + std::time::Duration::from_millis(500);
                    loop {
                        match tokio::time::timeout_at(
                            deadline,
                            protocol::read_frame(&mut reader),
                        )
                        .await
                        {
                            Ok(Ok(Some(Frame::TerminalOutput { data, .. }))) => {
                                let _ = channel.send(data);
                            }
                            _ => break,
                        }
                    }
                    break;
                }
                Ok(None) => break,
                Ok(Some(_)) => {}
                Err(_) => break,
            }
        }
        tracing::debug!("attach reader ended for session {session_id}");
    });

    // Store writer + reader task as Attached connection
    {
        let mut conns = state.connections.lock().await;
        conns.insert(
            id.clone(),
            SessionConnection::Attached {
                writer: SessionWriter { writer },
                task,
            },
        );
    }

    Ok(())
}

#[tauri::command]
pub async fn write_to_session(
    id: String,
    data: Vec<u8>,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let mut conns = state.connections.lock().await;
    let conn = conns.get_mut(&id).ok_or("session not attached")?;

    let writer = match conn {
        SessionConnection::Attached { ref mut writer, .. } => &mut writer.writer,
        SessionConnection::Monitored { .. } => return Err("session not attached".into()),
    };

    let frame = Frame::TerminalInput { data };
    protocol::write_frame(writer, &frame)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn resize_session(
    id: String,
    rows: u16,
    cols: u16,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let mut conns = state.connections.lock().await;
    let conn = conns.get_mut(&id).ok_or("session not attached")?;

    let writer = match conn {
        SessionConnection::Attached { ref mut writer, .. } => &mut writer.writer,
        SessionConnection::Monitored { .. } => return Err("session not attached".into()),
    };

    let frame = Frame::Resize { rows, cols };
    protocol::write_frame(writer, &frame)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn rename_session(
    app: tauri::AppHandle,
    id: String,
    label: Option<String>,
) -> Result<(), String> {
    let session_store = SessionStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    let (repo, _) = session_store.find_by_id(&id).map_err(|e| e.to_string())?;
    session_store
        .rename(&repo, &id, label.as_deref())
        .map_err(|e| e.to_string())?;
    let _ = app.emit("sidebar-changed", ());
    Ok(())
}

#[tauri::command]
pub async fn detach_session(
    app: tauri::AppHandle,
    id: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    // Remove and abort existing connection
    {
        let mut conns = state.connections.lock().await;
        if let Some(conn) = conns.remove(&id) {
            conn.abort();
        }
    }
    // Lock released — safe to do DB I/O without blocking write_to_session/resize_session

    // If session is still running, start a monitor so sidebar keeps getting updates
    let session_store = SessionStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    if let Ok((_repo, session)) = session_store.find_by_id(&id) {
        if session.status == "running" {
            let task = start_monitor(app, id.clone(), session.transport_addr);
            let mut conns = state.connections.lock().await;
            conns.insert(id, SessionConnection::Monitored { task });
        }
    }

    Ok(())
}
