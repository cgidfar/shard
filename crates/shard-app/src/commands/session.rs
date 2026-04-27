use tauri::ipc::{Channel, Response};
use tauri::{Emitter, Manager};

use shard_core::default_command;
use shard_core::repos::RepositoryStore;
use shard_core::sessions::{Session, SessionStore};
use shard_core::workspaces::WorkspaceStore;
use shard_core::{Harness, ShardPaths};
use shard_transport::daemon_client::NamedPipeDaemonConnection;
use shard_transport::protocol::{self, ActivityState, Frame};
use shard_transport::transport_windows::NamedPipeTransport;
use shard_transport::SessionTransport;

use crate::daemon_ipc;
use crate::state::{AppState, ConnectionToken, SessionConnection, SessionWriter};

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
    let ws_store = WorkspaceStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    let _ws = ws_store
        .get(&repo, &workspace_name)
        .map_err(|e| e.to_string())?;

    let command = command.unwrap_or_else(default_command);

    // Route through daemon. Harness-hook installation is a separate
    // RPC so the read-modify-write on `~/.claude/settings.json` is
    // serialized across CLI and GUI spawns (Phase 5).
    //
    // We always request the Claude Code installer regardless of the
    // session's selected harness — the RPC's `harness` arg is the
    // install target, not the session's harness. This preserves the
    // pre-Phase-5 opportunistic-install behavior where Codex sessions
    // still get Claude hooks bootstrapped. Changing that is a UX
    // decision, not a plumbing change.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;

    let (session_id, transport_addr) = rt.block_on(async {
        use shard_transport::control_protocol::ControlFrame;

        let mut conn = connect_or_spawn_daemon_app()
            .await
            .map_err(|e| e.to_string())?;

        conn.handshake()
            .await
            .map_err(|e| format!("daemon handshake failed: {e}"))?;

        // Hooks install round-trip — non-fatal. A hooks failure must
        // not block session spawn (today's behavior preserved).
        match crate::daemon_ipc::install_harness_hooks("claude-code").await {
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

    // Drop runtime before blocking_lock to avoid deadlock
    drop(rt);

    // Read back the full session record from DB
    let session_store = SessionStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    let result = session_store
        .get(&repo, &session_id)
        .map_err(|e| e.to_string())?;

    // Start a monitor for the new session so sidebar gets activity updates
    let task = start_monitor(app.clone(), session_id.clone(), transport_addr);
    {
        let state: tauri::State<'_, AppState> = app.state();
        let mut conns = state.connections.blocking_lock();
        conns.insert(session_id.clone(), SessionConnection::Monitored { task });
    }

    let _ = app.emit("sidebar-changed", ());
    Ok(result)
}

/// Connect to daemon, spawning it if not running. For use from app context.
async fn connect_or_spawn_daemon_app() -> Result<NamedPipeDaemonConnection, std::io::Error> {
    use shard_transport::daemon_client;

    daemon_client::connect_or_spawn(
        || {
            // Spawn daemon — find shardctl.exe relative to app exe.
            let exe_dir = std::env::current_exe()?
                .parent()
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no exe dir"))?
                .to_path_buf();
            let shardctl = exe_dir.join("shardctl.exe");
            if !shardctl.exists() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("shardctl.exe not found at {}", shardctl.display()),
                ));
            }

            use shard_supervisor::process::{PlatformProcessControl, ProcessControl};
            let args = vec!["daemon".to_string(), "start".to_string()];
            PlatformProcessControl::spawn_detached(&shardctl, &args).map(|_| ())
        },
        std::time::Duration::from_secs(5),
    )
    .await
}

#[tauri::command]
pub async fn stop_session(
    app: tauri::AppHandle,
    id: String,
    force: bool,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    // Route through the daemon's `StopSession` RPC so the daemon can drain
    // the supervisor, clear its live-registry entry, and write the DB
    // status in one workflow. The previous direct-pipe path hit SHA-43
    // (supervisor silently discards stop frames sent as the first frame)
    // and left the daemon's in-memory registry populated, which then
    // caused `RemoveSession` to reject with "still live — stop it first".
    let (_repo, session) = daemon_ipc::find_session_by_id(&id).await?;

    if session.status != "running" && session.status != "starting" {
        return Ok(());
    }

    // Abort any existing monitor/attach task before sending the stop so
    // the daemon's drain doesn't race the Tauri reader on the same pipe.
    {
        let mut conns = state.connections.lock().await;
        if let Some(conn) = conns.remove(&id) {
            conn.abort();
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

#[tauri::command]
pub async fn attach_session(
    app: tauri::AppHandle,
    id: String,
    channel: Channel<Response>,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let (_repo, session) = daemon_ipc::find_session_by_id(&id).await?;

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

    let client = match NamedPipeTransport::connect(&session.transport_addr).await {
        Ok(client) => client,
        Err(e) => {
            restore_monitor_if_absent(
                &app,
                &state,
                id.clone(),
                session.transport_addr.clone(),
            )
            .await;
            return Err(e.to_string());
        }
    };

    let (mut reader, writer) = tokio::io::split(client);
    let token = ConnectionToken::new();

    // Send Resume frame
    let mut writer = writer;
    if let Err(e) = protocol::write_frame(
        &mut writer,
        &Frame::Resume {
            last_seen_offset: 0,
        },
    )
    .await
    {
        restore_monitor_if_absent(&app, &state, id.clone(), session.transport_addr.clone()).await;
        return Err(e.to_string());
    }

    // Spawn reader task that forwards terminal output AND relays activity/status events
    let session_id = id.clone();
    let app_clone = app.clone();
    let task = tauri::async_runtime::spawn(async move {
        let mut terminal_status: Option<(&'static str, u8)> = None;
        loop {
            match protocol::read_frame(&mut reader).await {
                Ok(Some(Frame::TerminalOutput { data, .. })) => {
                    if channel.send(Response::new(data)).is_err() {
                        break;
                    }
                }
                Ok(Some(ref frame @ Frame::ActivityUpdate { .. })) => {
                    handle_supervisor_frame(&app_clone, &session_id, frame);
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
                    handle_supervisor_frame(&app_clone, &session_id, frame);
                    // Drain any trailing TerminalOutput (defensive)
                    let deadline =
                        tokio::time::Instant::now() + std::time::Duration::from_millis(500);
                    loop {
                        match tokio::time::timeout_at(deadline, protocol::read_frame(&mut reader))
                            .await
                        {
                            Ok(Ok(Some(Frame::TerminalOutput { data, .. }))) => {
                                let _ = channel.send(Response::new(data));
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
        let state = app_clone.state::<AppState>();
        let mut conns = state.connections.lock().await;
        let should_remove = matches!(
            conns.get(&session_id),
            Some(SessionConnection::Attached {
                token: current, ..
            }) if *current == token
        );
        if should_remove {
            conns.remove(&session_id);
        }
        tracing::debug!("attach reader ended for session {session_id}");
    });

    // Store writer + reader task as Attached connection
    {
        let mut conns = state.connections.lock().await;
        if conns.contains_key(&id) {
            task.abort();
            return Err("session connection changed while attaching".into());
        }
        conns.insert(
            id.clone(),
            SessionConnection::Attached {
                token,
                writer: SessionWriter { writer },
                task,
            },
        );
    }

    Ok(())
}

async fn restore_monitor_if_absent(
    app: &tauri::AppHandle,
    state: &tauri::State<'_, AppState>,
    id: String,
    transport_addr: String,
) {
    let task = start_monitor(app.clone(), id.clone(), transport_addr);
    let mut conns = state.connections.lock().await;
    if conns.contains_key(&id) {
        task.abort();
    } else {
        conns.insert(id, SessionConnection::Monitored { task });
    }
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
    // Lock released — safe to do IPC without blocking write_to_session/resize_session

    // Resolve through the daemon. The detach probe and the monitor-restart
    // lookup are the same operation: a single FindSessionById tells us both
    // whether the id is still valid and gives us the transport addr we'd
    // need to re-monitor. Drop the redundant DetachSession probe — it
    // returned the same answer find_session_by_id already gives us.
    if let Ok((_repo, session)) = daemon_ipc::find_session_by_id(&id).await {
        if session.status == "running" {
            let task = start_monitor(app, id.clone(), session.transport_addr);
            let mut conns = state.connections.lock().await;
            if conns.contains_key(&id) {
                task.abort();
            } else {
                conns.insert(id, SessionConnection::Monitored { task });
            }
        }
    }

    Ok(())
}
