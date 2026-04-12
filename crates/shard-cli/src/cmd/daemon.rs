//! Daemon process: owns session supervisor lifecycle, control pipe, and tray icon.
//!
//! # Threading Model
//!
//! On Windows, the main thread runs the winit event loop (Win32 message pump) which
//! hosts the system tray icon. The tokio runtime runs on a background thread spawned
//! by `run_daemon()`. Communication between them uses `EventLoopProxy<TrayEvent>`.
//!
//! # TODO(macos): Extract to TrayBackend trait
//!
//! The tray implementation below is Windows-specific. For macOS portability, extract
//! to a `TrayBackend` trait following the pattern of `SessionTransport` and `ProcessControl`.
//! See Linear issue SHA-34 for details.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{watch, Mutex};
use tracing::{error, info, warn};

use shard_core::repos::RepositoryStore;
use shard_core::sessions::SessionStore;
use shard_core::{ShardPaths, APP_EXE, APP_NAME};
use shard_supervisor::job_object::DaemonJobGuard;
use shard_supervisor::process::{PlatformProcessControl, ProcessControl};
use shard_supervisor::process_windows::{
    get_process_creation_time, open_process_for_job, spawn_detached_with_handle,
};
use shard_transport::control_protocol::*;
use shard_transport::transport_windows::create_pipe_instance;
use shard_transport::SessionTransport;

use crate::opts::DaemonCommands;

// Tray icon imports (Windows-only)
#[cfg(windows)]
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
#[cfg(windows)]
use tray_icon::{Icon, TrayIconBuilder};
#[cfg(windows)]
use winit::application::ApplicationHandler;
#[cfg(windows)]
use winit::event::WindowEvent;
#[cfg(windows)]
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
#[cfg(windows)]
use winit::window::WindowId;

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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ShutdownMode {
    Running,
    Graceful,
    Force,
}

// ── Tray Icon (Windows) ──────────────────────────────────────────────────────

/// Events sent from the tokio runtime to the winit event loop.
#[cfg(windows)]
#[derive(Debug)]
enum TrayEvent {
    /// Update the session count label in the tray menu.
    SessionCount(usize),
    /// Signal the event loop to exit (daemon is shutting down).
    Quit,
}

/// System tray application state.
#[cfg(windows)]
struct TrayApp {
    tray: Option<tray_icon::TrayIcon>,
    session_count: usize,
    open_id: tray_icon::menu::MenuId,
    quit_id: tray_icon::menu::MenuId,
    shutdown_tx: watch::Sender<ShutdownMode>,
    exe_dir: PathBuf,
}

#[cfg(windows)]
impl TrayApp {
    fn new(shutdown_tx: watch::Sender<ShutdownMode>, exe_dir: PathBuf) -> Self {
        Self {
            tray: None,
            session_count: 0,
            open_id: tray_icon::menu::MenuId::new("open"),
            quit_id: tray_icon::menu::MenuId::new("quit"),
            shutdown_tx,
            exe_dir,
        }
    }

    /// Build or rebuild the tray menu with current session count.
    fn build_menu(&self) -> Menu {
        let open_label = format!("Open {APP_NAME}");
        let quit_label = format!("Quit {APP_NAME}");
        let open_item = MenuItem::with_id(self.open_id.clone(), &open_label, true, None);
        let separator = PredefinedMenuItem::separator();
        let count_label = if self.session_count == 1 {
            "1 active session".to_string()
        } else {
            format!("{} active sessions", self.session_count)
        };
        let count_item = MenuItem::new(count_label, false, None);
        let quit_item = MenuItem::with_id(self.quit_id.clone(), &quit_label, true, None);

        let menu = Menu::new();
        let _ = menu.append_items(&[&open_item, &separator, &count_item, &quit_item]);
        menu
    }

    /// Create the tray icon. Must be called after the event loop starts.
    fn create_tray(&mut self) {
        let menu = self.build_menu();

        // 16x16 placeholder icon (Windows blue #0078D4)
        let rgba: Vec<u8> = std::iter::repeat([0x00u8, 0x78, 0xD4, 0xFF])
            .take(16 * 16)
            .flatten()
            .collect();
        let icon = Icon::from_rgba(rgba, 16, 16).expect("valid icon dimensions");

        match TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(APP_NAME)
            .with_icon(icon)
            .build()
        {
            Ok(tray) => {
                self.tray = Some(tray);
                info!("Tray icon created");
            }
            Err(e) => {
                error!("Failed to create tray icon: {e}");
            }
        }
    }

    /// Update the menu with a new session count.
    fn update_session_count(&mut self, count: usize) {
        if count == self.session_count {
            return;
        }
        self.session_count = count;
        if let Some(ref tray) = self.tray {
            let menu = self.build_menu();
            let _ = tray.set_menu(Some(Box::new(menu)));
        }
    }

    /// Open the Shard app: find existing window and focus, or spawn new instance.
    fn open_shard_app(&self) {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::HWND;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE,
        };

        // Search by window title (pass null for class to match any class)
        let title: Vec<u16> = std::ffi::OsStr::new(APP_NAME)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        unsafe {
            let hwnd: HWND = FindWindowW(std::ptr::null(), title.as_ptr());
            if hwnd != 0 {
                // Window found — restore (if minimized) and bring to foreground
                ShowWindow(hwnd, SW_RESTORE);
                SetForegroundWindow(hwnd);
                info!("Focused existing Shard window");
                return;
            }
        }

        // No existing window — spawn the app
        let app_path = self.exe_dir.join(APP_EXE);
        if !app_path.exists() {
            warn!("{} not found at {:?}", APP_EXE, app_path);
            return;
        }

        use std::os::windows::process::CommandExt;
        use std::process::Stdio;
        const DETACHED_PROCESS: u32 = 0x00000008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x01000000;

        // Try with BREAKAWAY first (avoids inheriting daemon's job object limits),
        // fall back without it if the job doesn't allow breakaway.
        let spawn_result = std::process::Command::new(&app_path)
            .current_dir(&self.exe_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB)
            .spawn()
            .or_else(|_| {
                std::process::Command::new(&app_path)
                    .current_dir(&self.exe_dir)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
                    .spawn()
            });

        match spawn_result {
            Ok(child) => {
                info!("Spawned {} (pid={})", APP_EXE, child.id());
            }
            Err(e) => {
                error!("Failed to spawn {}: {e}", APP_EXE);
            }
        }
    }
}

#[cfg(windows)]
impl ApplicationHandler<TrayEvent> for TrayApp {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        // Create tray on first resume (required by tray-icon)
        if self.tray.is_none() {
            self.create_tray();
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        _event: WindowEvent,
    ) {
        // No windows — nothing to handle
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: TrayEvent) {
        match event {
            TrayEvent::SessionCount(count) => {
                self.update_session_count(count);
            }
            TrayEvent::Quit => {
                info!("Tray received quit signal");
                event_loop.exit();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Set Wait control flow to avoid busy-loop CPU burn
        event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait);

        // Poll for menu events (non-blocking)
        if let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == self.open_id {
                info!("Open {} menu item clicked", APP_NAME);
                self.open_shard_app();
            } else if event.id == self.quit_id {
                info!("Quit menu item clicked");
                let _ = self.shutdown_tx.send(ShutdownMode::Force);
                event_loop.exit();
            }
        }
    }
}

/// Shared daemon state.
struct DaemonState {
    sessions: Mutex<HashMap<String, LiveSession>>,
    job_guard: DaemonJobGuard,
    paths: ShardPaths,
    exe_path: PathBuf,
    shutdown_tx: watch::Sender<ShutdownMode>,
    /// Proxy to send events to the tray icon event loop.
    #[cfg(windows)]
    tray_proxy: EventLoopProxy<TrayEvent>,
}

pub fn run(command: DaemonCommands) -> shard_core::Result<()> {
    match command {
        DaemonCommands::Start => run_daemon(),
        DaemonCommands::Stop => stop_daemon(),
        DaemonCommands::Status => status_daemon(),
    }
}

/// Entry point for the daemon process. Sets up logging, control pipe, and tray.
#[cfg(windows)]
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
    let exe_dir = exe_path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let (shutdown_tx, shutdown_rx) = watch::channel(ShutdownMode::Running);

    // Build the winit event loop FIRST — must be created on the main thread.
    // The EventLoopProxy is Send+Sync and can be passed to the tokio thread.
    let event_loop = EventLoop::<TrayEvent>::with_user_event()
        .build()
        .map_err(|e| shard_core::ShardError::Other(format!("event loop: {e}")))?;
    let tray_proxy = event_loop.create_proxy();

    let state = Arc::new(DaemonState {
        sessions: Mutex::new(HashMap::new()),
        job_guard,
        paths: paths.clone(),
        exe_path,
        shutdown_tx: shutdown_tx.clone(),
        tray_proxy,
    });

    // Spawn the tokio runtime on a background thread.
    // Main thread will run the winit event loop for the tray icon.
    let tokio_state = state.clone();
    let tokio_shutdown_rx = shutdown_rx.clone();
    let tokio_thread = std::thread::Builder::new()
        .name("daemon-tokio".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    error!("Failed to create tokio runtime: {e}");
                    return;
                }
            };

            // Adopt orphaned supervisors from a previous daemon instance
            rt.block_on(adopt_orphans(tokio_state.clone()));

            info!("Daemon ready, entering control pipe loop");
            rt.block_on(run_control_loop(tokio_state, tokio_shutdown_rx));

            info!("Daemon control loop exited");
        })
        .map_err(|e| shard_core::ShardError::Other(format!("thread spawn: {e}")))?;

    // Spawn a watcher that signals the tray to quit when shutdown is triggered
    // (e.g., from `shardctl daemon stop` via control pipe)
    let watcher_proxy = state.tray_proxy.clone();
    let watcher_rx = shutdown_rx;
    let watcher_thread = std::thread::Builder::new()
        .name("shutdown-watcher".into())
        .spawn(move || {
            // Poll until shutdown mode changes from Running
            // Note: Using polling because watch::Receiver doesn't have blocking_recv.
            // At 100ms intervals this is acceptable for a background watcher.
            loop {
                if *watcher_rx.borrow() != ShutdownMode::Running {
                    let _ = watcher_proxy.send_event(TrayEvent::Quit);
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });

    // Run the tray icon on the main thread (blocks until quit)
    let mut app = TrayApp::new(shutdown_tx, exe_dir);
    if let Err(e) = event_loop.run_app(&mut app) {
        error!("Event loop error: {e}");
    }

    // Wait for background threads to finish cleanup
    let _ = tokio_thread.join();
    if let Ok(handle) = watcher_thread {
        let _ = handle.join();
    }

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

        // Scan for dead sessions and remove from HashMap under lock
        let (dead_ids, count) = {
            let mut sessions = state.sessions.lock().await;
            let mut dead = Vec::new();
            for (id, session) in sessions.iter() {
                if !PlatformProcessControl::is_alive(session.supervisor_pid) {
                    dead.push((id.clone(), session.repo.clone()));
                }
            }
            for (id, _) in &dead {
                sessions.remove(id);
            }
            (dead, sessions.len())
        };

        // Update DB outside lock to avoid blocking other operations
        if !dead_ids.is_empty() {
            let session_store = SessionStore::new(state.paths.clone());
            for (id, repo) in &dead_ids {
                let _ = session_store.update_status(repo, id, "exited", None);
                info!("Heartbeat: session {} supervisor died, marked exited", &id[..8]);
            }
        }

        #[cfg(windows)]
        {
            let _ = state.tray_proxy.send_event(TrayEvent::SessionCount(count));
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

    // Send initial session count to tray
    #[cfg(windows)]
    {
        let count = state.sessions.lock().await.len();
        let _ = state.tray_proxy.send_event(TrayEvent::SessionCount(count));
    }
}
