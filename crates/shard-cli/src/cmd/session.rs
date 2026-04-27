use std::path::PathBuf;

use shard_core::default_command;
use shard_core::sessions::SessionStore;
use shard_core::workspaces::WorkspaceStore;
use shard_core::ShardPaths;
use shard_supervisor::process::{PlatformProcessControl, ProcessControl};
use shard_transport::daemon_client::NamedPipeDaemonConnection;

use crate::opts::{parse_target, SessionCommands};

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

    // Route through daemon: connect-or-spawn, install harness hooks
    // (daemon-centralized per Phase 5), then send SpawnSession RPC.
    //
    // Phase 5 note: callers always request the Claude Code installer
    // regardless of the session's selected harness. The RPC's `harness`
    // arg is the install *target*, not the session's harness — this
    // preserves today's opportunistic-install behavior where Codex
    // sessions still get Claude hooks bootstrapped, so switching
    // harnesses mid-session doesn't strand the user.
    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(async {
        use shard_transport::control_protocol::ControlFrame;

        let mut conn = connect_or_spawn_daemon().await?;
        conn.handshake().await.map_err(|e| {
            shard_core::ShardError::Other(format!("daemon handshake failed: {e}"))
        })?;

        // Hooks install round-trip — non-fatal. A hooks failure must
        // not block the session from spawning; the daemon centralizes
        // the serialized read-modify-write, so there's no cleanup to
        // unwind here. Print a one-line summary mapped from the ack
        // matrix so the operator can tell whether hooks are in place.
        match conn
            .request(&ControlFrame::InstallHarnessHooks {
                harness: "claude-code".to_string(),
            })
            .await
        {
            Ok(ControlFrame::InstallHarnessHooksAck {
                installed,
                skipped_reason,
            }) => match (installed, skipped_reason.as_deref()) {
                (true, None) => println!("Installed Claude Code hooks."),
                (true, Some(_already_configured)) => {
                    tracing::debug!("Claude Code hooks already configured");
                }
                (false, Some(reason)) => {
                    eprintln!("Claude Code hooks skipped: {reason}");
                }
                (false, None) => {
                    tracing::debug!("Claude Code hooks skipped with no reason");
                }
            },
            Ok(ControlFrame::Error { message }) => {
                eprintln!("warning: hooks install failed: {message}");
            }
            Ok(other) => {
                tracing::warn!("hooks install: unexpected response {other:?}");
            }
            Err(e) => {
                tracing::warn!("hooks install: request failed: {e}");
            }
        }

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
async fn connect_or_spawn_daemon() -> shard_core::Result<NamedPipeDaemonConnection> {
    use shard_transport::daemon_client;

    daemon_client::connect_or_spawn(
        || {
            let exe = std::env::current_exe()?;
            let args = vec!["daemon".to_string(), "start".to_string()];
            PlatformProcessControl::spawn_detached(&exe, &args).map(|_| ())
        },
        std::time::Duration::from_secs(5),
    )
    .await
    .map_err(|e| {
        shard_core::ShardError::Other(format!("daemon unavailable or did not start: {e}"))
    })
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
    let (_repo, session) = find_session_via_daemon(&id)?;

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
    match stop_via_daemon(&id, force, None)? {
        StopCommandOutcome::AlreadyStopped { id, status } => {
            println!("Session {} is already '{}'", id, status);
        }
        StopCommandOutcome::Stopped { id } => {
            println!("Stopped session {}", &id[..8.min(id.len())]);
        }
    }

    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum StopCommandOutcome {
    AlreadyStopped { id: String, status: String },
    Stopped { id: String },
}

fn stop_via_daemon(
    id: &str,
    force: bool,
    control_pipe_name: Option<&str>,
) -> shard_core::Result<StopCommandOutcome> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(stop_via_daemon_async(id, force, control_pipe_name))
}

async fn stop_via_daemon_async(
    id: &str,
    force: bool,
    control_pipe_name: Option<&str>,
) -> shard_core::Result<StopCommandOutcome> {
    use shard_transport::control_protocol::ControlFrame;
    use shard_transport::daemon_client;

    let mut conn = connect_to_daemon_for_stop(control_pipe_name).await?;
    let (_repo, session) = conn
        .request_typed(
            &ControlFrame::FindSessionById {
                prefix: id.to_string(),
            },
            |f| match f {
                ControlFrame::FoundSession { repo, session } => Ok((repo, session)),
                other => Err(other),
            },
        )
        .await
        .map_err(|e| shard_core::ShardError::Other(e.to_string()))?;

    if session.status != "running" && session.status != "starting" {
        return Ok(StopCommandOutcome::AlreadyStopped {
            id: session.id,
            status: session.status,
        });
    }

    conn.request_typed(
        &ControlFrame::StopSession {
            session_id: session.id.clone(),
            force,
        },
        |f| match f {
            ControlFrame::StopAck => Ok(()),
            other => Err(other),
        },
    )
    .await
    .map_err(|e| match e {
        daemon_client::DaemonError::Reported(message) => {
            shard_core::ShardError::Other(format!("daemon failed to stop session: {message}"))
        }
        daemon_client::DaemonError::Transport(e) => {
            shard_core::ShardError::Other(format!("daemon stop request failed: {e}"))
        }
    })?;

    Ok(StopCommandOutcome::Stopped { id: session.id })
}

async fn connect_to_daemon_for_stop(
    control_pipe_name: Option<&str>,
) -> shard_core::Result<
    shard_transport::daemon_client::DaemonConnection<tokio::net::windows::named_pipe::NamedPipeClient>,
> {
    use shard_transport::daemon_client;

    let mut conn = match control_pipe_name {
        Some(pipe) => daemon_client::connect_to(pipe).await,
        None => daemon_client::connect().await,
    }
    .map_err(|e| {
        shard_core::ShardError::Other(format!(
            "daemon not running or unreachable: {e}. Start it with `shardctl daemon start` and retry; session stop does not perform direct supervisor cleanup."
        ))
    })?;

    conn.handshake().await.map_err(|e| {
        shard_core::ShardError::Other(format!(
            "daemon handshake failed: {e}. Restart the daemon and retry; session stop does not perform direct supervisor cleanup."
        ))
    })?;

    Ok(conn)
}

#[cfg(windows)]
#[doc(hidden)]
pub async fn test_stop_with_control_pipe(
    id: &str,
    force: bool,
    control_pipe_name: &str,
) -> shard_core::Result<()> {
    match stop_via_daemon_async(id, force, Some(control_pipe_name)).await? {
        StopCommandOutcome::AlreadyStopped { .. } | StopCommandOutcome::Stopped { .. } => {
            Ok(())
        }
    }
}

fn remove(id: String) -> shard_core::Result<()> {
    use shard_transport::control_protocol::ControlFrame;

    let (repo, session) = find_session_via_daemon(&id)?;

    crate::cmd::daemon_rpc::run(
        ControlFrame::RemoveSession {
            repo: repo.clone(),
            id: session.id.clone(),
        },
        |f| match f {
            ControlFrame::RemoveSessionAck => Ok(()),
            other => Err(other),
        },
    )?;

    println!("Removed session {}", &session.id[..8.min(session.id.len())]);
    Ok(())
}

/// Resolve a session id (or prefix) through the daemon's global session
/// index. Same shape CLI repo / workspace subcommands use for their
/// daemon round-trips (see `cmd/repo.rs::run_daemon_rpc`). Per Phase 4
/// D4, all session lookups route through the daemon so CLI and GUI
/// agree on the source of truth.
fn find_session_via_daemon(id: &str) -> shard_core::Result<(String, shard_core::sessions::Session)> {
    use shard_transport::control_protocol::ControlFrame;

    crate::cmd::daemon_rpc::run(
        ControlFrame::FindSessionById {
            prefix: id.to_string(),
        },
        |f| match f {
            ControlFrame::FoundSession { repo, session } => Ok((repo, session)),
            other => Err(other),
        },
    )
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
