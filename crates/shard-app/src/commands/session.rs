use tauri::ipc::Channel;

use shard_core::repos::RepositoryStore;
use shard_core::sessions::{Session, SessionStore};
use shard_core::workspaces::WorkspaceStore;
use shard_core::ShardPaths;
use shard_transport::protocol::{self, Frame};
use shard_transport::transport_windows::NamedPipeTransport;
use shard_transport::SessionTransport;

use crate::state::{AppState, SessionWriter};

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
    repo: String,
    workspace_name: String,
    command: Option<Vec<String>>,
) -> Result<Session, String> {
    let paths = ShardPaths::new().map_err(|e| e.to_string())?;

    let ws_store = WorkspaceStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    let _ws = ws_store
        .get(&repo, &workspace_name)
        .map_err(|e| e.to_string())?;

    let command = command.unwrap_or_else(default_command);

    let session_store = SessionStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    let session = session_store
        .create(&repo, &workspace_name, &command, "pending")
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

    session_store
        .get(&repo, &session.id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn stop_session(id: String, force: bool) -> Result<(), String> {
    let session_store = SessionStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    let (repo, session) = session_store.find_by_id(&id).map_err(|e| e.to_string())?;

    if session.status != "running" && session.status != "starting" {
        return Ok(());
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
                        Ok(Some(Frame::Status { .. })) | Ok(None) => return,
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
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn remove_session(id: String) -> Result<(), String> {
    let session_store = SessionStore::new(ShardPaths::new().map_err(|e| e.to_string())?);
    let (repo, _) = session_store.find_by_id(&id).map_err(|e| e.to_string())?;
    session_store
        .remove(&repo, &id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn attach_session(
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

    let client = NamedPipeTransport::connect(&session.transport_addr)
        .await
        .map_err(|e| e.to_string())?;

    let (mut reader, writer) = tokio::io::split(client);

    // Send Resume frame — need mutable access, so do it before storing
    let mut writer = writer;
    protocol::write_frame(&mut writer, &Frame::Resume { last_seen_offset: 0 })
        .await
        .map_err(|e| e.to_string())?;

    // Store writer for input forwarding
    {
        let mut sessions = state.sessions.lock().await;
        sessions.insert(id.clone(), SessionWriter { writer });
    }

    // Spawn reader task that forwards terminal output to the Tauri Channel
    let session_id = id.clone();
    tokio::spawn(async move {
        loop {
            match protocol::read_frame(&mut reader).await {
                Ok(Some(Frame::TerminalOutput { data, .. })) => {
                    if channel.send(data).is_err() {
                        break;
                    }
                }
                Ok(Some(Frame::Status { .. })) | Ok(None) => break,
                Ok(Some(_)) => {}
                Err(_) => break,
            }
        }
        tracing::debug!("reader task for session {session_id} ended");
    });

    Ok(())
}

#[tauri::command]
pub async fn write_to_session(
    id: String,
    data: Vec<u8>,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let mut sessions = state.sessions.lock().await;
    let entry = sessions.get_mut(&id).ok_or("session not attached")?;

    let frame = Frame::TerminalInput { data };
    protocol::write_frame(&mut entry.writer, &frame)
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
    let mut sessions = state.sessions.lock().await;
    let entry = sessions.get_mut(&id).ok_or("session not attached")?;

    let frame = Frame::Resize { rows, cols };
    protocol::write_frame(&mut entry.writer, &frame)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn detach_session(id: String, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut sessions = state.sessions.lock().await;
    sessions.remove(&id);
    Ok(())
}
