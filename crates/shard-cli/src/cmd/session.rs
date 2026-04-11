use std::path::PathBuf;

use shard_core::sessions::SessionStore;
use shard_core::workspaces::WorkspaceStore;
use shard_core::ShardPaths;
use shard_supervisor::process::{PlatformProcessControl, ProcessControl};
use shard_transport::transport_windows::NamedPipeTransport;
use shard_transport::SessionTransport;

use crate::opts::{parse_target, SessionCommands};

/// Default shell command for new sessions.
fn default_command() -> Vec<String> {
    // Prefer PowerShell 7 if available
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

pub fn run(command: SessionCommands) -> shard_core::Result<()> {
    match command {
        SessionCommands::Create { target, harness, command } => create(target, harness, command),
        SessionCommands::List { target, json } => list(target, json),
        SessionCommands::Attach { id } => attach(id),
        SessionCommands::Stop { id, force } => stop(id, force),
        SessionCommands::Remove { id } => remove(id),
        SessionCommands::Serve {
            repo,
            workspace,
            session_id,
            transport_addr,
            command,
        } => serve(repo, workspace, session_id, transport_addr, command),
    }
}

fn create(target: String, harness: Option<shard_core::Harness>, command: Vec<String>) -> shard_core::Result<()> {
    let (repo, ws_name) =
        parse_target(&target).map_err(|e| shard_core::ShardError::Other(e))?;

    // Verify workspace exists before asking daemon to spawn
    let ws_store = WorkspaceStore::new(ShardPaths::new()?);
    let _ws = ws_store.get(repo, ws_name)?;

    let command = if command.is_empty() {
        default_command()
    } else {
        command
    };

    // Best-effort: install harness hooks so agents can report activity state
    let exe = std::env::current_exe()?;
    if !shard_core::hooks::claude_code_hooks_installed() {
        if let Err(e) = shard_core::hooks::install_claude_code_hooks(&exe) {
            tracing::warn!("failed to install Claude Code hooks: {e}");
        }
    }

    // Route through daemon: connect-or-spawn, then send SpawnSession RPC
    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(async {
        use shard_transport::control_protocol::ControlFrame;

        let mut conn = connect_or_spawn_daemon().await?;
        conn.handshake().await.map_err(|e| {
            shard_core::ShardError::Other(format!("daemon handshake failed: {e}"))
        })?;

        let response = conn
            .request(&ControlFrame::SpawnSession {
                repo: repo.to_string(),
                workspace: ws_name.to_string(),
                command: command.clone(),
                harness: harness.map(|h| h.to_string()),
            })
            .await
            .map_err(|e| shard_core::ShardError::Other(format!("daemon request failed: {e}")))?;

        match response {
            ControlFrame::SpawnAck {
                session_id,
                supervisor_pid,
                transport_addr,
            } => Ok((session_id, supervisor_pid, transport_addr)),
            ControlFrame::Error { message } => {
                Err(shard_core::ShardError::Other(format!("daemon: {message}")))
            }
            other => Err(shard_core::ShardError::Other(format!(
                "unexpected daemon response: {other:?}"
            ))),
        }
    })?;

    let (session_id, _supervisor_pid, _transport_addr) = result;

    println!("Created session {}", session_id);
    println!("  Workspace: {}:{}", repo, ws_name);
    println!("  Command:   {}", command.join(" "));
    println!("  Attach:    shardctl session attach {}", session_id);

    Ok(())
}

/// Connect to the daemon, spawning it if not running.
async fn connect_or_spawn_daemon() -> shard_core::Result<
    shard_transport::daemon_client::DaemonConnection<tokio::net::windows::named_pipe::NamedPipeClient>,
> {
    use shard_transport::daemon_client;

    // Try connecting first
    match daemon_client::connect().await {
        Ok(conn) => return Ok(conn),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Daemon not running — spawn it
        }
        Err(e) if e.raw_os_error() == Some(231) => {
            // ERROR_PIPE_BUSY — daemon exists but all instances busy, retry
            let conn = daemon_client::connect_with_retry(std::time::Duration::from_secs(5))
                .await
                .map_err(|e| {
                    shard_core::ShardError::Other(format!("daemon pipe busy, retry failed: {e}"))
                })?;
            return Ok(conn);
        }
        Err(e) => {
            return Err(shard_core::ShardError::Other(format!(
                "cannot connect to daemon: {e}"
            )));
        }
    }

    // Spawn daemon
    let exe = std::env::current_exe()?;
    let args = vec!["daemon".to_string(), "start".to_string()];
    PlatformProcessControl::spawn_detached(&exe, &args)?;

    // Wait for it to become ready
    let conn = daemon_client::connect_with_retry(std::time::Duration::from_secs(5))
        .await
        .map_err(|e| {
            shard_core::ShardError::Other(format!("daemon did not start: {e}"))
        })?;

    Ok(conn)
}

fn list(target: Option<String>, json: bool) -> shard_core::Result<()> {
    let session_store = SessionStore::new(ShardPaths::new()?);

    // Parse target — could be "repo" or "repo:workspace" or nothing
    let (repo_alias, ws_filter) = match &target {
        Some(t) => {
            if let Ok((repo, ws)) = parse_target(t) {
                (Some(repo.to_string()), Some(ws.to_string()))
            } else {
                (Some(t.clone()), None)
            }
        }
        None => (None, None),
    };

    // If no repo specified, list across all repos
    let paths = ShardPaths::new()?;
    let repos = if let Some(alias) = &repo_alias {
        vec![alias.clone()]
    } else {
        let repo_store = shard_core::repos::RepositoryStore::new(ShardPaths::new()?);
        repo_store.list()?.into_iter().map(|r| r.alias).collect()
    };

    let mut all_sessions = Vec::new();
    for alias in &repos {
        let repo_db = paths.repo_db(alias);
        if !repo_db.exists() {
            continue;
        }
        let sessions = session_store.list(alias, ws_filter.as_deref())?;
        for s in sessions {
            all_sessions.push((alias.clone(), s));
        }
    }

    if json {
        let display: Vec<_> = all_sessions
            .iter()
            .map(|(alias, s)| {
                serde_json::json!({
                    "repo": alias,
                    "session": s,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&display).unwrap());
    } else if all_sessions.is_empty() {
        println!("No sessions.");
    } else {
        for (alias, s) in &all_sessions {
            let cmd: Vec<String> = serde_json::from_str(&s.command_json).unwrap_or_default();
            println!(
                "  {} [{}] {}:{} — {}",
                &s.id[..8],
                s.status,
                alias,
                s.workspace_name,
                cmd.join(" "),
            );
        }
    }

    Ok(())
}

fn attach(id: String) -> shard_core::Result<()> {
    let session_store = SessionStore::new(ShardPaths::new()?);
    let (_repo, session) = session_store.find_by_id(&id)?;

    if session.status != "running" {
        return Err(shard_core::ShardError::Other(format!(
            "session {} is '{}', not 'running'",
            id, session.status
        )));
    }

    // Run the attach in a tokio runtime
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(crate::attach::attach_to_session(&session.transport_addr))?;

    Ok(())
}

fn stop(id: String, force: bool) -> shard_core::Result<()> {
    let session_store = SessionStore::new(ShardPaths::new()?);
    let (repo, session) = session_store.find_by_id(&id)?;

    if session.status != "running" && session.status != "starting" {
        println!("Session {} is already '{}'", id, session.status);
        return Ok(());
    }

    let rt = tokio::runtime::Runtime::new()?;
    let stopped_via_daemon = rt.block_on(async {
        use shard_transport::control_protocol::ControlFrame;

        // Try routing through daemon first
        if let Ok(mut conn) = connect_or_spawn_daemon().await {
            if conn.handshake().await.is_ok() {
                match conn
                    .request(&ControlFrame::StopSession {
                        session_id: session.id.clone(),
                        force,
                    })
                    .await
                {
                    Ok(ControlFrame::StopAck) => return true,
                    _ => {}
                }
            }
        }
        false
    });

    if !stopped_via_daemon {
        // Fallback: direct stop via session pipe (daemon might not be running)
        let frame = if force {
            shard_transport::protocol::Frame::StopForce
        } else {
            shard_transport::protocol::Frame::StopGraceful
        };

        let rpc_result = rt.block_on(async {
            use shard_transport::protocol;
            use tokio::time::timeout;
            use std::time::Duration;

            match NamedPipeTransport::connect(&session.transport_addr).await {
                Ok(mut client) => {
                    let _ = protocol::write_frame(&mut client, &frame).await;
                    let _ = timeout(Duration::from_secs(5), async {
                        loop {
                            match protocol::read_frame(&mut client).await {
                                Ok(Some(protocol::Frame::Status { .. })) => return,
                                Ok(None) => return,
                                _ => tokio::time::sleep(Duration::from_millis(100)).await,
                            }
                        }
                    }).await;
                    true
                }
                Err(_) => false,
            }
        });

        if !rpc_result {
            if let Some(child_pid) = session.child_pid {
                let _ = PlatformProcessControl::terminate(child_pid);
            }
            if let Some(sup_pid) = session.supervisor_pid {
                let _ = PlatformProcessControl::terminate(sup_pid);
            }
        }
    }

    // Wait for supervisor to die, then update DB
    if let Some(sup_pid) = session.supervisor_pid {
        let start = std::time::Instant::now();
        while PlatformProcessControl::is_alive(sup_pid)
            && start.elapsed() < std::time::Duration::from_secs(5)
        {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        if PlatformProcessControl::is_alive(sup_pid) {
            let _ = PlatformProcessControl::terminate(sup_pid);
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    session_store.update_status(&repo, &session.id, "stopped", None)?;
    println!("Stopped session {}", &session.id[..8]);

    Ok(())
}

fn remove(id: String) -> shard_core::Result<()> {
    let session_store = SessionStore::new(ShardPaths::new()?);
    let (repo, _session) = session_store.find_by_id(&id)?;
    session_store.remove(&repo, &id)?;
    println!("Removed session {}", &id[..8]);
    Ok(())
}

fn serve(
    repo: String,
    workspace: String,
    session_id: String,
    transport_addr: String,
    command: Vec<String>,
) -> shard_core::Result<()> {
    let paths = ShardPaths::new()?;
    let session_store = SessionStore::new(ShardPaths::new()?);
    let ws_store = WorkspaceStore::new(ShardPaths::new()?);

    // Redirect stdout/stderr to supervisor log
    let session_dir = paths.session_dir(&repo, &session_id);
    std::fs::create_dir_all(&session_dir)?;
    let log_path = session_dir.join("session.log");
    let supervisor_log = session_dir.join("supervisor.log");

    // Set up logging to supervisor.log
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&supervisor_log)?;
    tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(log_file))
        .with_ansi(false)
        .init();

    tracing::info!("supervisor starting for session {session_id}");

    // Create Job Object BEFORE spawning the child so it inherits the job.
    // If the supervisor dies, the OS kills the child automatically.
    // Non-fatal: some environments already have the process in a job.
    #[cfg(windows)]
    let _job_guard = match shard_supervisor::job_object::JobGuard::new() {
        Ok(guard) => Some(guard),
        Err(e) => {
            tracing::warn!("could not create job object (orphan prevention disabled): {e}");
            None
        }
    };

    // Get workspace path for working directory
    let ws = ws_store.get(&repo, &workspace)?;
    let working_dir = PathBuf::from(&ws.path);

    // Spawn PTY with env vars so hook scripts can reach the supervisor.
    // TERM/COLORTERM tell TUI apps what sequences the emulator supports.
    // TERM_PROGRAM overrides any inherited value (e.g. WarpTerminal) so
    // that TUI frameworks detect xterm.js capabilities correctly — this
    // affects whether apps like Claude Code enable alternate-screen mode.
    let pty_envs: Vec<(&str, &str)> = vec![
        ("SHARD_PIPE_ADDR", &transport_addr),
        ("SHARD_SESSION", "1"),
        ("TERM", "xterm-256color"),
        ("COLORTERM", "truecolor"),
        ("TERM_PROGRAM", "xterm"),
    ];
    let pty_session =
        shard_supervisor::pty::PtySession::spawn(&command, &working_dir, 24, 80, &pty_envs)
            .map_err(|e| shard_core::ShardError::Other(format!("failed to spawn PTY: {e}")))?;

    // Record child PID
    if let Some(pid) = pty_session.child_pid() {
        session_store.set_child_pid(&repo, &session_id, pid)?;
    }

    // Create the tokio runtime first — needed for named pipe creation
    let rt = tokio::runtime::Runtime::new()?;

    // Create the named pipe BEFORE writing the ready file.
    // This ensures clients can connect as soon as they see the ready signal.
    // Must be inside the runtime context because tokio registers with the reactor.
    let initial_server = rt.block_on(async {
        shard_transport::transport_windows::create_pipe_instance(&transport_addr, true)
    }).map_err(|e| shard_core::ShardError::Other(format!("failed to create pipe: {e}")))?;

    // Update status to running
    session_store.update_status(&repo, &session_id, "running", None)?;

    // Write the ready file — only after pipe is listening
    let ready_path = session_dir.join("ready");
    std::fs::write(&ready_path, "ok")?;
    tracing::info!("supervisor ready, transport at {transport_addr}");

    // Run the event loop (pipe server is passed in, already listening)
    let exit_code = rt
        .block_on(shard_supervisor::event_loop::run(
            &transport_addr,
            pty_session,
            &log_path,
            initial_server,
        ))
        .map_err(|e| shard_core::ShardError::Other(format!("event loop error: {e}")))?;

    // Update DB with exit status
    // exit_code -1 with shutdown means it was stopped via RPC
    let final_status = if exit_code == -1 { "stopped" } else { "exited" };
    tracing::info!("session finished: status={final_status}, exit_code={exit_code}");
    session_store.update_status(&repo, &session_id, final_status, Some(exit_code))?;

    Ok(())
}
