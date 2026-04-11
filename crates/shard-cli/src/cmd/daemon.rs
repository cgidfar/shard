//! Daemon process: owns session supervisor lifecycle, control pipe, and tray icon.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{watch, Mutex};
use tracing::{error, info, warn};

use shard_core::sessions::SessionStore;
use shard_core::repos::RepositoryStore;
use shard_core::ShardPaths;
use shard_supervisor::job_object::DaemonJobGuard;
use shard_supervisor::process::{PlatformProcessControl, ProcessControl};
use shard_supervisor::process_windows::{
    get_process_creation_time, open_process_for_job, spawn_detached_with_handle,
};
use shard_transport::control_protocol::*;
use shard_transport::transport_windows::create_pipe_instance;
use shard_transport::SessionTransport;

use crate::opts::DaemonCommands;

/// In-memory record of a live supervised session.
struct LiveSession {
    pub session_id: String,
    pub supervisor_pid: u32,
    pub transport_addr: String,
    pub repo: String,
    pub workspace: String,
    pub creation_time: u64,
}

/// Shutdown mode for the daemon.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ShutdownMode {
    Running,
    Graceful,
    Force,
}

/// Shared daemon state.
struct DaemonState {
    sessions: Mutex<HashMap<String, LiveSession>>,
    job_guard: DaemonJobGuard,
    paths: ShardPaths,
    exe_path: PathBuf,
    shutdown_tx: watch::Sender<ShutdownMode>,
}

pub fn run(command: DaemonCommands) -> shard_core::Result<()> {
    match command {
        DaemonCommands::Start => run_daemon(),
        DaemonCommands::Stop => stop_daemon(),
        DaemonCommands::Status => status_daemon(),
    }
}

/// Entry point for the daemon process. Sets up logging, control pipe, and tray.
fn run_daemon() -> shard_core::Result<()> {
    let paths = ShardPaths::new()?;

    // Set up file-based logging
    let log_dir = paths.data_dir();
    std::fs::create_dir_all(&log_dir)?;
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("daemon.log"))?;

    tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(log_file))
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    info!("Shard daemon starting (pid={})", std::process::id());

    // Check if daemon is already running by trying to open the control pipe
    if is_daemon_running() {
        info!("Another daemon instance is already running, exiting");
        return Ok(());
    }

    // Create Job Object for supervisors
    let job_guard = match DaemonJobGuard::new() {
        Ok(guard) => {
            info!("Created daemon Job Object");
            guard
        }
        Err(e) => {
            error!("Failed to create Job Object: {e}");
            return Err(shard_core::ShardError::Other(format!(
                "Job Object creation failed: {e}"
            )));
        }
    };

    let exe_path = std::env::current_exe()?;
    let (shutdown_tx, shutdown_rx) = watch::channel(ShutdownMode::Running);

    let state = Arc::new(DaemonState {
        sessions: Mutex::new(HashMap::new()),
        job_guard,
        paths: paths.clone(),
        exe_path,
        shutdown_tx,
    });

    // Adopt any orphaned supervisors from a previous daemon instance
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| shard_core::ShardError::Other(format!("tokio runtime: {e}")))?;

    rt.block_on(adopt_orphans(state.clone()));

    // Run the control pipe server + heartbeat on the tokio runtime
    // Tray icon will be added in a follow-up (requires winit event loop on main thread)
    info!("Daemon ready, entering control pipe loop");
    rt.block_on(run_control_loop(state, shutdown_rx));

    info!("Daemon shutting down");
    Ok(())
}

/// Send a shutdown request to the running daemon.
fn stop_daemon() -> shard_core::Result<()> {
    let rt = tokio::runtime::Runtime::new()
        .map_err(|e| shard_core::ShardError::Other(e.to_string()))?;

    rt.block_on(async {
        let mut conn = shard_transport::daemon_client::connect()
            .await
            .map_err(|e| shard_core::ShardError::Other(format!("cannot connect to daemon: {e}")))?;

        conn.handshake()
            .await
            .map_err(|e| shard_core::ShardError::Other(format!("handshake failed: {e}")))?;

        let response = conn
            .request(&ControlFrame::Shutdown { graceful: true })
            .await
            .map_err(|e| shard_core::ShardError::Other(format!("shutdown request failed: {e}")))?;

        match response {
            ControlFrame::ShutdownAck => {
                println!("Daemon shutdown initiated");
                Ok(())
            }
            ControlFrame::Error { message } => {
                Err(shard_core::ShardError::Other(format!("daemon error: {message}")))
            }
            _ => Err(shard_core::ShardError::Other(
                "unexpected response to shutdown".to_string(),
            )),
        }
    })
}

/// Check if the daemon is running and print status.
fn status_daemon() -> shard_core::Result<()> {
    let rt = tokio::runtime::Runtime::new()
        .map_err(|e| shard_core::ShardError::Other(e.to_string()))?;

    rt.block_on(async {
        match shard_transport::daemon_client::connect().await {
            Ok(mut conn) => match conn.handshake().await {
                Ok(version) => {
                    println!("Daemon running (version: {version})");

                    match conn.request(&ControlFrame::ListSessions).await {
                        Ok(ControlFrame::SessionList { sessions }) => {
                            println!("{} active session(s)", sessions.len());
                            for s in &sessions {
                                println!(
                                    "  {} [{}:{}] pid={}",
                                    &s.session_id[..8],
                                    s.repo,
                                    s.workspace,
                                    s.supervisor_pid
                                );
                            }
                        }
                        _ => {}
                    }
                    Ok(())
                }
                Err(e) => {
                    println!("Daemon pipe exists but handshake failed: {e}");
                    Ok(())
                }
            },
            Err(_) => {
                println!("Daemon is not running");
                Ok(())
            }
        }
    })
}

/// Check if the daemon is already running (synchronous, no tokio required).
fn is_daemon_running() -> bool {
    // Use raw Win32 CreateFileW to probe the pipe without needing a tokio runtime.
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL,
    };

    let pipe_name: Vec<u16> = std::ffi::OsStr::new(CONTROL_PIPE_NAME)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let handle = CreateFileW(
            pipe_name.as_ptr(),
            0x80000000 | 0x40000000, // GENERIC_READ | GENERIC_WRITE
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            0,
        );
        if handle == INVALID_HANDLE_VALUE {
            false
        } else {
            CloseHandle(handle);
            true
        }
    }
}

/// Main async loop: control pipe server + heartbeat.
async fn run_control_loop(state: Arc<DaemonState>, mut shutdown_rx: watch::Receiver<ShutdownMode>) {
    // Create the first control pipe instance
    let server = match create_pipe_instance(CONTROL_PIPE_NAME, true) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create control pipe: {e}");
            return;
        }
    };

    // Spawn heartbeat task
    let heartbeat_state = state.clone();
    let heartbeat_handle = tokio::spawn(heartbeat_task(heartbeat_state));

    // Accept loop
    let accept_state = state.clone();
    let shutdown_rx_accept = shutdown_rx.clone();
    let accept_handle = tokio::spawn(async move {
        accept_loop(accept_state, server, shutdown_rx_accept).await;
    });

    // Wait for shutdown signal
    let _ = shutdown_rx.changed().await;
    let mode = *shutdown_rx.borrow();
    info!("Shutdown signal received (mode={:?})", mode as u8);

    // Cancel accept loop and heartbeat
    accept_handle.abort();
    heartbeat_handle.abort();

    match mode {
        ShutdownMode::Graceful => {
            // Remove kill-on-close so supervisors survive daemon exit
            if let Err(e) = state.job_guard.remove_kill_on_close() {
                warn!("Failed to remove kill-on-close: {e}");
            } else {
                info!("Removed KILL_ON_JOB_CLOSE — supervisors will survive daemon exit");
            }
        }
        ShutdownMode::Force => {
            // Do NOT remove kill-on-close — dropping the job guard will kill all supervisors
            info!("Force shutdown — KILL_ON_JOB_CLOSE remains active, supervisors will be terminated");
        }
        ShutdownMode::Running => unreachable!(),
    }
}

/// Accept control pipe connections and spawn per-client handlers.
async fn accept_loop(
    state: Arc<DaemonState>,
    initial_server: tokio::net::windows::named_pipe::NamedPipeServer,
    mut shutdown_rx: watch::Receiver<ShutdownMode>,
) {
    let mut server = initial_server;

    loop {
        tokio::select! {
            result = server.connect() => {
                match result {
                    Ok(()) => {
                        // Client connected to this instance. Create next instance before handling.
                        let connected = server;
                        server = match create_pipe_instance(CONTROL_PIPE_NAME, false) {
                            Ok(s) => s,
                            Err(e) => {
                                error!("Failed to create next pipe instance: {e}");
                                return;
                            }
                        };

                        let client_state = state.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(client_state, connected).await {
                                warn!("Client handler error: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        error!("Pipe accept error: {e}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
            _ = shutdown_rx.changed() => {
                info!("Accept loop shutting down");
                return;
            }
        }
    }
}

/// Handle a single control pipe client connection.
async fn handle_client(
    state: Arc<DaemonState>,
    mut stream: tokio::net::windows::named_pipe::NamedPipeServer,
) -> std::io::Result<()> {
    // Expect Hello first
    let frame = read_control_frame(&mut stream).await?;
    match frame {
        Some(ControlFrame::Hello { protocol_version }) => {
            if protocol_version != PROTOCOL_VERSION {
                write_control_frame(
                    &mut stream,
                    &ControlFrame::Error {
                        message: format!(
                            "protocol version mismatch: daemon={PROTOCOL_VERSION}, client={protocol_version}"
                        ),
                    },
                )
                .await?;
                return Ok(());
            }
            write_control_frame(
                &mut stream,
                &ControlFrame::HelloAck {
                    protocol_version: PROTOCOL_VERSION,
                    daemon_version: env!("CARGO_PKG_VERSION").to_string(),
                },
            )
            .await?;
        }
        Some(other) => {
            write_control_frame(
                &mut stream,
                &ControlFrame::Error {
                    message: format!("expected Hello, got {other:?}"),
                },
            )
            .await?;
            return Ok(());
        }
        None => return Ok(()),
    }

    // Request/response loop
    loop {
        let frame = match read_control_frame(&mut stream).await? {
            Some(f) => f,
            None => return Ok(()), // Client disconnected
        };

        // Capture shutdown mode before dispatching
        let shutdown_mode = match &frame {
            ControlFrame::Shutdown { graceful } => {
                Some(if *graceful { ShutdownMode::Graceful } else { ShutdownMode::Force })
            }
            _ => None,
        };

        let response = dispatch_request(&state, frame).await;
        write_control_frame(&mut stream, &response).await?;

        // If we just acked a shutdown, signal with the correct mode and exit
        if let Some(mode) = shutdown_mode {
            let _ = state.shutdown_tx.send(mode);
            return Ok(());
        }
    }
}

/// Dispatch a control request to the appropriate handler.
async fn dispatch_request(state: &Arc<DaemonState>, frame: ControlFrame) -> ControlFrame {
    match frame {
        ControlFrame::Ping => ControlFrame::Pong,

        ControlFrame::SpawnSession {
            repo,
            workspace,
            command,
            harness,
        } => handle_spawn(state, repo, workspace, command, harness).await,

        ControlFrame::StopSession { session_id, force } => {
            handle_stop(state, session_id, force).await
        }

        ControlFrame::ListSessions => {
            let sessions = state.sessions.lock().await;
            let list: Vec<LiveSessionInfo> = sessions
                .values()
                .map(|s| LiveSessionInfo {
                    session_id: s.session_id.clone(),
                    supervisor_pid: s.supervisor_pid,
                    transport_addr: s.transport_addr.clone(),
                    repo: s.repo.clone(),
                    workspace: s.workspace.clone(),
                })
                .collect();
            ControlFrame::SessionList { sessions: list }
        }

        ControlFrame::Shutdown { graceful } => {
            info!("Shutdown requested (graceful={graceful})");
            ControlFrame::ShutdownAck
        }

        _ => ControlFrame::Error {
            message: "unexpected frame type in request position".to_string(),
        },
    }
}

/// Handle SpawnSession: create DB record, spawn supervisor, assign to job, wait for ready.
async fn handle_spawn(
    state: &Arc<DaemonState>,
    repo: String,
    workspace: String,
    command: Vec<String>,
    harness: Option<String>,
) -> ControlFrame {
    let harness_parsed = harness.as_deref().and_then(|h| h.parse().ok());
    let session_store = SessionStore::new(state.paths.clone());

    // Derive transport addr and create DB record
    let session = match session_store.create(&repo, &workspace, &command, "", harness_parsed) {
        Ok(s) => s,
        Err(e) => {
            return ControlFrame::Error {
                message: format!("failed to create session record: {e}"),
            }
        }
    };

    let transport_addr = shard_transport::transport_windows::session_pipe_name(&session.id);

    // Update transport addr in DB
    if let Err(e) = session_store.update_transport_addr(&repo, &session.id, &transport_addr) {
        return ControlFrame::Error {
            message: format!("failed to update transport addr: {e}"),
        };
    }

    // Build supervisor args
    let mut args: Vec<String> = vec![
        "session".to_string(),
        "serve".to_string(),
        "--repo".to_string(),
        repo.clone(),
        "--workspace".to_string(),
        workspace.clone(),
        "--session-id".to_string(),
        session.id.clone(),
        "--transport-addr".to_string(),
        transport_addr.clone(),
        "--".to_string(),
    ];
    args.extend(command);

    // Spawn supervisor and assign to job
    let (pid, handle) = match spawn_detached_with_handle(&state.exe_path, &args) {
        Ok(result) => result,
        Err(e) => {
            return ControlFrame::Error {
                message: format!("failed to spawn supervisor: {e}"),
            }
        }
    };

    // Assign to daemon's Job Object
    if let Err(e) = state.job_guard.assign_process(handle) {
        warn!("Failed to assign supervisor pid={pid} to job: {e}");
        // Non-fatal — supervisor still works, just won't be killed on daemon crash
    }

    // Get creation time for PID reuse detection
    let creation_time = get_process_creation_time(handle).unwrap_or(0);

    // Close the handle now that we've assigned it
    unsafe {
        windows_sys::Win32::Foundation::CloseHandle(handle);
    }

    // Update DB with supervisor PID
    let _ = session_store.set_supervisor_pid(&repo, &session.id, pid);

    // Wait for ready file
    let ready_path = state.paths.session_dir(&repo, &session.id).join("ready");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if ready_path.exists() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return ControlFrame::Error {
                message: format!("supervisor did not become ready within 10s (pid={pid})"),
            };
        }
        if !PlatformProcessControl::is_alive(pid) {
            return ControlFrame::Error {
                message: "supervisor died before becoming ready".to_string(),
            };
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Register in live sessions
    {
        let mut sessions = state.sessions.lock().await;
        sessions.insert(
            session.id.clone(),
            LiveSession {
                session_id: session.id.clone(),
                supervisor_pid: pid,
                transport_addr: transport_addr.clone(),
                repo: repo.clone(),
                workspace: workspace.clone(),
                creation_time,
            },
        );
    }

    info!(
        "Spawned session {} [{}:{}] pid={}",
        &session.id[..8],
        repo,
        workspace,
        pid
    );

    ControlFrame::SpawnAck {
        session_id: session.id,
        supervisor_pid: pid,
        transport_addr,
    }
}

/// Handle StopSession: connect to the session pipe and send stop frame.
async fn handle_stop(state: &Arc<DaemonState>, session_id: String, force: bool) -> ControlFrame {
    let transport_addr = {
        let sessions = state.sessions.lock().await;
        match sessions.get(&session_id) {
            Some(s) => s.transport_addr.clone(),
            None => {
                // Try prefix match
                let matches: Vec<_> = sessions
                    .values()
                    .filter(|s| s.session_id.starts_with(&session_id))
                    .collect();
                match matches.len() {
                    0 => {
                        return ControlFrame::Error {
                            message: format!("session '{session_id}' not found in live registry"),
                        }
                    }
                    1 => matches[0].transport_addr.clone(),
                    n => {
                        return ControlFrame::Error {
                            message: format!(
                                "ambiguous session prefix '{session_id}' matches {n} sessions"
                            ),
                        }
                    }
                }
            }
        }
    };

    // Connect to the session's transport pipe and send stop
    match shard_transport::PlatformTransport::connect(&transport_addr).await {
        Ok(mut client) => {
            use shard_transport::protocol::{write_frame, Frame};
            let frame = if force {
                Frame::StopForce
            } else {
                Frame::StopGraceful
            };
            if let Err(e) = write_frame(&mut client, &frame).await {
                return ControlFrame::Error {
                    message: format!("failed to send stop frame: {e}"),
                };
            }
            info!("Sent stop to session {}", &session_id[..8.min(session_id.len())]);
            ControlFrame::StopAck
        }
        Err(e) => ControlFrame::Error {
            message: format!("failed to connect to session pipe: {e}"),
        },
    }
}

/// Periodic heartbeat: check if supervised processes are still alive.
async fn heartbeat_task(state: Arc<DaemonState>) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        interval.tick().await;

        let mut dead_ids = Vec::new();
        {
            let sessions = state.sessions.lock().await;
            for (id, session) in sessions.iter() {
                if !PlatformProcessControl::is_alive(session.supervisor_pid) {
                    dead_ids.push((id.clone(), session.repo.clone()));
                }
            }
        }

        if !dead_ids.is_empty() {
            let session_store = SessionStore::new(state.paths.clone());
            let mut sessions = state.sessions.lock().await;
            for (id, repo) in &dead_ids {
                sessions.remove(id);
                let _ = session_store.update_status(repo, id, "exited", None);
                info!("Heartbeat: session {} supervisor died, marked exited", &id[..8]);
            }
        }
    }
}

/// Adopt orphaned supervisors from a previous daemon instance.
async fn adopt_orphans(state: Arc<DaemonState>) {
    let repo_store = RepositoryStore::new(state.paths.clone());
    let session_store = SessionStore::new(state.paths.clone());

    let repos = match repo_store.list() {
        Ok(r) => r,
        Err(_) => return,
    };

    let mut adopted = 0u32;
    let mut pruned = 0u32;

    for repo in &repos {
        if !state.paths.repo_db(&repo.alias).exists() {
            continue;
        }
        let sessions = match session_store.list(&repo.alias, None) {
            Ok(s) => s,
            Err(_) => continue,
        };

        for session in &sessions {
            if session.status != "running" && session.status != "starting" {
                continue;
            }
            let pid = match session.supervisor_pid {
                Some(p) => p,
                None => {
                    let _ = session_store.update_status(&repo.alias, &session.id, "failed", None);
                    pruned += 1;
                    continue;
                }
            };

            if !PlatformProcessControl::is_alive(pid) {
                let _ = session_store.update_status(&repo.alias, &session.id, "failed", None);
                pruned += 1;
                continue;
            }

            // Process is alive — verify via creation time if possible and adopt
            match open_process_for_job(pid) {
                Ok(handle) => {
                    let creation_time = get_process_creation_time(handle).unwrap_or(0);

                    // Assign to our Job Object
                    if let Err(e) = state.job_guard.assign_process(handle) {
                        warn!(
                            "Failed to adopt session {} pid={}: {e}",
                            &session.id[..8],
                            pid
                        );
                        unsafe {
                            windows_sys::Win32::Foundation::CloseHandle(handle);
                        }
                        continue;
                    }
                    unsafe {
                        windows_sys::Win32::Foundation::CloseHandle(handle);
                    }

                    let mut sessions = state.sessions.lock().await;
                    sessions.insert(
                        session.id.clone(),
                        LiveSession {
                            session_id: session.id.clone(),
                            supervisor_pid: pid,
                            transport_addr: session.transport_addr.clone(),
                            repo: repo.alias.clone(),
                            workspace: session.workspace_name.clone(),
                            creation_time,
                        },
                    );
                    adopted += 1;
                }
                Err(e) => {
                    warn!(
                        "Cannot open process for session {} pid={}: {e}",
                        &session.id[..8],
                        pid
                    );
                    let _ = session_store.update_status(&repo.alias, &session.id, "failed", None);
                    pruned += 1;
                }
            }
        }
    }

    if adopted > 0 || pruned > 0 {
        info!("Orphan adoption: {adopted} adopted, {pruned} pruned");
    }
}
