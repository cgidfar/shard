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
use shard_core::workspaces::{
    self as workspaces, default_git_ops, WorkspaceGitOps, WorkspaceStore,
};
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
#[doc(hidden)]
pub struct LiveSession {
    pub session_id: String,
    pub supervisor_pid: u32,
    pub transport_addr: String,
    pub repo: String,
    pub workspace: String,
    pub creation_time: u64,
}

/// Snapshot of the minimum session info the tray quit path needs,
/// captured under the `DaemonState.sessions` lock and then released so the
/// lock is not held across the graceful-stop RPCs or the confirmation dialog.
#[cfg(windows)]
#[derive(Clone)]
struct LiveSessionSnapshot {
    session_id: String,
    repo: String,
    transport_addr: String,
}

/// Shutdown mode for the daemon.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShutdownMode {
    Running,
    Graceful,
    Force,
}

// ── Tray Icon (Windows) ──────────────────────────────────────────────────────

/// Events sent from the tokio runtime to the winit event loop.
#[cfg(windows)]
#[derive(Debug)]
pub(crate) enum TrayEvent {
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
    /// Shared daemon state, used by the quit handler to take a fresh snapshot
    /// of live sessions and to reach `paths` for DB status updates.
    state: Arc<DaemonState>,
    /// Handle to the daemon's tokio runtime. The tray runs on the winit main
    /// thread, so any async work (locking `state.sessions`, parallel stop RPCs)
    /// is dispatched here via `block_on`.
    tokio_handle: tokio::runtime::Handle,
}

#[cfg(windows)]
impl TrayApp {
    fn new(
        shutdown_tx: watch::Sender<ShutdownMode>,
        exe_dir: PathBuf,
        state: Arc<DaemonState>,
        tokio_handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            tray: None,
            session_count: 0,
            open_id: tray_icon::menu::MenuId::new("open"),
            quit_id: tray_icon::menu::MenuId::new("quit"),
            shutdown_tx,
            exe_dir,
            state,
            tokio_handle,
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

        let icon = Self::load_tray_icon().unwrap_or_else(|e| {
            warn!("Failed to load embedded tray icon, using fallback square: {e}");
            // Fallback: a small Windows-blue square so the daemon still has a
            // visible tray entry even if the embedded asset is malformed.
            let rgba: Vec<u8> = std::iter::repeat([0x00u8, 0x78, 0xD4, 0xFF])
                .take(16 * 16)
                .flatten()
                .collect();
            Icon::from_rgba(rgba, 16, 16).expect("valid icon dimensions")
        });

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

    /// Decode the embedded shard tray-icon PNG into a `tray_icon::Icon`.
    /// The PNG ships with the binary via `include_bytes!`, so the daemon
    /// has no runtime file dependency.
    fn load_tray_icon() -> Result<Icon, String> {
        // 64×64 RGBA PNG generated from the master icon SVG (see
        // tools/icon-gen). 64px gives clean rendering at any tray DPI.
        const TRAY_ICON_PNG: &[u8] = include_bytes!("../../assets/tray-icon.png");

        let decoder = png::Decoder::new(std::io::Cursor::new(TRAY_ICON_PNG));
        let mut reader = decoder
            .read_info()
            .map_err(|e| format!("png header: {e}"))?;
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader
            .next_frame(&mut buf)
            .map_err(|e| format!("png frame: {e}"))?;

        if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
            return Err(format!(
                "expected RGBA8 PNG, got {:?} at {:?}",
                info.color_type, info.bit_depth
            ));
        }

        let bytes = &buf[..info.buffer_size()];
        Icon::from_rgba(bytes.to_vec(), info.width, info.height)
            .map_err(|e| format!("icon construction: {e}"))
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

    /// Take a fresh snapshot of sessions whose supervisor processes are
    /// *actually alive right now*. Runs under the `state.sessions` lock and
    /// probes each supervisor PID synchronously via `is_alive`, so stale
    /// entries (supervisor crashed but the 5s heartbeat hasn't pruned yet)
    /// are filtered out at click time rather than relying on the pruned map.
    fn collect_live_active_sessions(&self) -> Vec<LiveSessionSnapshot> {
        self.tokio_handle.block_on(async {
            let sessions = self.state.sessions.lock().await;
            sessions
                .values()
                .filter(|s| PlatformProcessControl::is_alive(s.supervisor_pid))
                .map(|s| LiveSessionSnapshot {
                    session_id: s.session_id.clone(),
                    repo: s.repo.clone(),
                    transport_addr: s.transport_addr.clone(),
                })
                .collect()
        })
    }

    /// Show a Yes/No native confirmation dialog via Win32 `MessageBoxW`.
    /// Blocks the winit thread until the user dismisses it; `MessageBoxW`
    /// runs its own internal message loop, so the OS continues pumping
    /// messages and the tray icon stays responsive. Returns true only for
    /// an explicit Yes click.
    fn confirm_quit(&self, count: usize) -> bool {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            MessageBoxW, IDYES, MB_ICONWARNING, MB_TOPMOST, MB_YESNO,
        };

        let plural = if count == 1 { "" } else { "s" };
        let title = format!("Quit {APP_NAME}?");
        let text = format!("{count} active session{plural} will be closed. Continue?");

        let to_wide = |s: &str| -> Vec<u16> {
            std::ffi::OsStr::new(s)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect()
        };
        let title_w = to_wide(&title);
        let text_w = to_wide(&text);

        let result = unsafe {
            MessageBoxW(
                0, // no owner window — the tray has no HWND
                text_w.as_ptr(),
                title_w.as_ptr(),
                MB_YESNO | MB_ICONWARNING | MB_TOPMOST,
            )
        };

        result == IDYES as i32
    }

    /// Signal the tokio side to finish and exit the winit event loop. This is
    /// the terminal step of every quit path (dialog-confirmed or zero-session
    /// fast path). After this returns, `run_control_loop` drops the
    /// `DaemonJobGuard`, which force-kills any supervisors still alive.
    fn force_quit(&self, event_loop: &ActiveEventLoop) {
        let _ = self.shutdown_tx.send(ShutdownMode::Force);
        event_loop.exit();
    }
}

// ── Tray quit: daemon-layer orchestration ───────────────────────────────────
//
// These live outside `TrayApp` because daemon lifecycle policy (quiesce the
// control pipe, flip DB rows, kill the GUI, drain supervisors) is not UI
// code. The tray is a thin initiator: it gathers the snapshot, shows the
// confirmation dialog, calls `execute_quit`, then exits the event loop.

/// Orchestrate a tray-initiated daemon quit. Called once the user has
/// confirmed — must run on the tokio runtime. Steps:
///
/// 1. Flip `state.quitting` so any control-pipe request landing during the
///    drain window is rejected for mutating frames. Prevents a relaunched
///    GUI from spawning a new supervisor that would die when the Job
///    Object drops a moment later.
/// 2. Mark rows `stopped` in SQLite so a relaunched GUI's
///    `start_monitors_for_running_sessions` finds nothing to attach to.
/// 3. Kill the GUI process immediately so the user cannot interact with
///    a frozen window during the drain.
/// 4. Send StopGraceful to every live supervisor in parallel (≤3s).
///
/// The caller (the tray thread) is expected to follow this with
/// `force_quit`, which drops the `DaemonJobGuard` and terminates any
/// supervisors that did not shut down cleanly within the deadline.
#[cfg(windows)]
async fn execute_quit(
    state: Arc<DaemonState>,
    exe_dir: std::path::PathBuf,
    active: Vec<LiveSessionSnapshot>,
) {
    state
        .quitting
        .store(true, std::sync::atomic::Ordering::Release);

    if !active.is_empty() {
        mark_stopped_in_db(&state.paths, &active);
    }

    terminate_app_processes(&exe_dir);

    if !active.is_empty() {
        stop_all_graceful(active, Duration::from_secs(3)).await;
    }
}

/// Persist `stopped` status for every session we're about to kill, so the
/// next app launch does not spin up monitors against dead pipes. Without
/// this the daemon heartbeat (which normally handles status cleanup) is
/// aborted before it has a chance to run, and stale `running` rows leak
/// into the next session.
#[cfg(windows)]
fn mark_stopped_in_db(paths: &ShardPaths, snapshots: &[LiveSessionSnapshot]) {
    let store = SessionStore::new(paths.clone());
    for snap in snapshots {
        let short = &snap.session_id[..8.min(snap.session_id.len())];
        if let Err(e) = store.update_status(&snap.repo, &snap.session_id, "stopped", None) {
            warn!("Failed to mark session {short} stopped: {e}");
        }
    }
}

/// Enumerate running processes and `TerminateProcess` any `APP_EXE`
/// whose **full image path** matches the sibling binary next to this
/// daemon. Used on tray quit to honour the "close any open GUI windows"
/// requirement — the shard-app process is independent of the daemon's
/// Job Object, so dropping the job guard does not reach it.
///
/// Full-path matching (not basename) is deliberate: developer machines
/// may have multiple `shard-app.exe` instances from different checkouts
/// or an installed build. Only the one that this daemon would have
/// launched (same `exe_dir`) should be killed.
#[cfg(windows)]
fn terminate_app_processes(exe_dir: &std::path::Path) {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    };

    // Canonicalize the expected path once. If we don't know where our
    // sibling app binary lives, bail — killing arbitrary processes by
    // basename is not acceptable.
    let expected_canon = match std::fs::canonicalize(exe_dir.join(APP_EXE)) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                "Cannot canonicalize expected {APP_EXE} path at {:?}: {e}; skipping GUI termination",
                exe_dir.join(APP_EXE)
            );
            return;
        }
    };

    let self_pid = std::process::id();

    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        warn!("CreateToolhelp32Snapshot failed; cannot enumerate app processes");
        return;
    }

    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

    let mut ok = unsafe { Process32FirstW(snap, &mut entry) };
    while ok != 0 {
        let pid = entry.th32ProcessID;

        // Never suicide — we're the daemon, not the app.
        if pid != self_pid {
            // Fast basename prefilter: skip anything that isn't even
            // named shard-app.exe before paying for OpenProcess.
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = OsString::from_wide(&entry.szExeFile[..len]);

            if name.to_string_lossy().eq_ignore_ascii_case(APP_EXE) {
                let handle = unsafe {
                    OpenProcess(
                        PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                        0,
                        pid,
                    )
                };
                if handle != 0 {
                    let mut buf = [0u16; 1024];
                    let mut buf_len: u32 = buf.len() as u32;
                    let rc = unsafe {
                        QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut buf_len)
                    };
                    if rc != 0 {
                        let full_path =
                            PathBuf::from(OsString::from_wide(&buf[..buf_len as usize]));
                        let candidate_canon = std::fs::canonicalize(&full_path).ok();
                        if candidate_canon.as_ref() == Some(&expected_canon) {
                            let terminated = unsafe { TerminateProcess(handle, 1) };
                            if terminated != 0 {
                                info!("Terminated {APP_EXE} (pid={pid})");
                            } else {
                                warn!("TerminateProcess failed for {APP_EXE} (pid={pid})");
                            }
                        } else {
                            info!("Skipping unrelated {APP_EXE} at {:?} (pid={pid})", full_path);
                        }
                    } else {
                        warn!("QueryFullProcessImageNameW failed for pid={pid}; leaving alive");
                    }
                    unsafe { CloseHandle(handle) };
                } else {
                    warn!("OpenProcess failed for {APP_EXE} (pid={pid})");
                }
            }
        }

        ok = unsafe { Process32NextW(snap, &mut entry) };
    }

    unsafe { CloseHandle(snap) };
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

                let active = self.collect_live_active_sessions();

                if !active.is_empty() && !self.confirm_quit(active.len()) {
                    info!(
                        "Quit cancelled by user; {} active session(s) preserved",
                        active.len()
                    );
                    return;
                }
                info!("Quit confirmed; stopping {} active session(s)", active.len());

                self.tokio_handle.block_on(execute_quit(
                    self.state.clone(),
                    self.exe_dir.clone(),
                    active,
                ));

                self.force_quit(event_loop);
            }
        }
    }
}

/// Send `StopGraceful` to every live supervisor in parallel, bounded by
/// `overall_deadline`. Any session that hasn't acknowledged within the
/// deadline will be logged at `warn!` and left for the `DaemonJobGuard`
/// drop to kill via `KILL_ON_JOB_CLOSE` on the way out of `run_daemon`.
#[cfg(windows)]
async fn stop_all_graceful(snapshots: Vec<LiveSessionSnapshot>, overall_deadline: Duration) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let total = snapshots.len();
    let completed = Arc::new(AtomicUsize::new(0));
    let mut set = tokio::task::JoinSet::new();

    for snap in snapshots {
        let completed = completed.clone();
        set.spawn(async move {
            let short = &snap.session_id[..8.min(snap.session_id.len())].to_string();
            match stop_one_graceful(&snap.transport_addr).await {
                Ok(()) => {
                    completed.fetch_add(1, Ordering::Relaxed);
                    info!("Stopped session {short} gracefully");
                }
                Err(e) => {
                    warn!("Failed to stop session {short}: {e}");
                }
            }
        });
    }

    let drain = async {
        while set.join_next().await.is_some() {}
    };

    match tokio::time::timeout(overall_deadline, drain).await {
        Ok(()) => {
            let done = completed.load(Ordering::Relaxed);
            if done == total {
                info!("All {total} session(s) stopped gracefully");
            } else {
                warn!(
                    "Graceful stop finished with errors: {done}/{total} session(s) stopped cleanly; remainder will be force-killed by job object"
                );
            }
        }
        Err(_) => {
            let done = completed.load(Ordering::Relaxed);
            warn!(
                "Graceful stop deadline ({:?}) exceeded: {}/{} session(s) stopped cleanly; remaining will be force-killed by job object",
                overall_deadline, done, total
            );
            set.abort_all();
        }
    }
}

/// Outcome of a drain-only stop attempt.
#[derive(Debug)]
#[cfg(windows)]
pub(crate) enum DrainOutcome {
    /// Supervisor responded with a lifecycle `Status` frame (or EOF'd) before
    /// the timeout fired.
    Exited,
    /// Timeout fired before the supervisor exited.
    TimedOut,
}

/// Outcome of a stop-and-wait attempt.
#[derive(Debug)]
#[cfg(windows)]
#[allow(dead_code)] // consumed by RemoveWorkspace handler in Phase 1
pub(crate) enum StopOutcome {
    /// Supervisor exited gracefully within the timeout window.
    Exited,
    /// Graceful window expired and we had to force-kill the supervisor PID
    /// via `TerminateProcess`. The process is no longer live.
    ForceKilled,
}

/// Connect to a session pipe, send the standard Resume+StopGraceful probe,
/// and drain until a lifecycle `Status` frame or pipe EOF — or the timeout
/// fires. Returns `DrainOutcome::TimedOut` on timeout; does NOT force-kill.
///
/// The initial `Resume` frame is load-bearing: the supervisor's accept
/// handler only dispatches stop frames via the *post-first-frame* recv loop
/// (see `crates/shard-supervisor/src/event_loop.rs:148-289`), so sending
/// `StopGraceful` as the first frame would be silently discarded. See
/// SHA-43 for the matching bug in the existing non-tray callers.
#[cfg(windows)]
async fn stop_and_drain(
    transport_addr: &str,
    graceful_timeout: Duration,
) -> shard_core::Result<DrainOutcome> {
    use shard_core::ShardError;
    use shard_transport::protocol::{read_frame, write_frame, Frame};

    let mut client = shard_transport::PlatformTransport::connect(transport_addr)
        .await
        .map_err(|e| ShardError::Other(format!("connect: {e}")))?;

    write_frame(
        &mut client,
        &Frame::Resume {
            last_seen_offset: u64::MAX,
        },
    )
    .await
    .map_err(|e| ShardError::Other(format!("resume handshake: {e}")))?;

    write_frame(&mut client, &Frame::StopGraceful)
        .await
        .map_err(|e| ShardError::Other(format!("stop-graceful write: {e}")))?;

    let drain = async {
        loop {
            match read_frame(&mut client).await {
                Ok(Some(Frame::Status { code })) if code <= 2 => return Ok::<(), ShardError>(()),
                Ok(None) => return Ok(()),
                Ok(Some(_)) => continue,
                Err(e) => return Err(ShardError::Other(format!("read: {e}"))),
            }
        }
    };

    match tokio::time::timeout(graceful_timeout, drain).await {
        Ok(Ok(())) => Ok(DrainOutcome::Exited),
        Ok(Err(e)) => Err(e),
        Err(_) => Ok(DrainOutcome::TimedOut),
    }
}

/// Drain a supervisor's graceful stop and, on timeout, force-kill the PID.
///
/// Used by mutation workflows (e.g., `RemoveWorkspace`) that need the
/// supervisor's child shell to actually relinquish its CWD — the open
/// handle otherwise blocks `RemoveDirectoryW` and produces the SHA-55
/// class of failure.
///
/// `creation_time` is the FILETIME captured when the daemon originally
/// spawned (or adopted) this supervisor. It's compared against the live
/// process's creation time before `TerminateProcess` fires, so a recycled
/// PID is never killed. Pass `0` only when the caller explicitly trusts
/// the PID (no recycle check) — not recommended outside legacy paths.
#[cfg(windows)]
#[allow(dead_code)] // consumed by RemoveWorkspace handler in Phase 1
pub(crate) async fn stop_session_and_wait(
    transport_addr: &str,
    supervisor_pid: u32,
    creation_time: u64,
    graceful_timeout: Duration,
) -> shard_core::Result<StopOutcome> {
    match stop_and_drain(transport_addr, graceful_timeout).await? {
        DrainOutcome::Exited => Ok(StopOutcome::Exited),
        DrainOutcome::TimedOut => {
            warn!(
                "stop_session_and_wait: pid={supervisor_pid} did not exit within {:?}, force-killing",
                graceful_timeout
            );
            shard_supervisor::process_windows::force_kill_pid_checked(
                supervisor_pid,
                creation_time,
            )
            .map_err(|e| {
                shard_core::ShardError::Other(format!(
                    "force-kill pid {supervisor_pid} failed: {e}"
                ))
            })?;
            Ok(StopOutcome::ForceKilled)
        }
    }
}

/// Legacy wrapper for the tray-initiated parallel-graceful-stop path. The
/// tray quit uses a 3s global budget and relies on the daemon's Job Object
/// to clean up stragglers, so no per-session force-kill is issued here.
#[cfg(windows)]
async fn stop_one_graceful(transport_addr: &str) -> shard_core::Result<()> {
    match stop_and_drain(transport_addr, Duration::from_secs(3)).await? {
        DrainOutcome::Exited => Ok(()),
        DrainOutcome::TimedOut => Ok(()),
    }
}

/// Runtime configuration for a daemon instance. Production uses
/// `DaemonConfig::production()` (real ShardPaths, well-known pipe name,
/// real git). Integration tests construct a config with a `TempDir`-backed
/// `ShardPaths`, a unique pipe name, and (optionally) a fault-injecting
/// git-ops impl.
#[derive(Clone)]
pub struct DaemonConfig {
    pub paths: ShardPaths,
    pub control_pipe_name: String,
    /// Git operations layer used by mutation handlers. Tests can substitute
    /// a stub to force `worktree_remove` failures and exercise the
    /// `broken` state transition in `RemoveWorkspace`.
    pub git_ops: Arc<dyn WorkspaceGitOps>,
}

impl DaemonConfig {
    /// Config that matches the production daemon: env-resolved ShardPaths +
    /// the well-known `CONTROL_PIPE_NAME` + real git.
    pub fn production() -> shard_core::Result<Self> {
        Ok(Self {
            paths: ShardPaths::new()?,
            control_pipe_name: CONTROL_PIPE_NAME.to_string(),
            git_ops: default_git_ops(),
        })
    }
}

/// Shared daemon state.
///
/// Public-but-hidden so the integration-test harness can inject fake
/// sessions and inspect internal maps. Not part of the supported API
/// surface — external callers should use the RPC layer.
#[doc(hidden)]
pub struct DaemonState {
    pub(crate) sessions: Mutex<HashMap<String, LiveSession>>,
    pub(crate) job_guard: DaemonJobGuard,
    pub(crate) paths: ShardPaths,
    /// Named-pipe address the control server listens on. In production this
    /// is the compile-time `CONTROL_PIPE_NAME`; tests override it with a
    /// per-test unique name so the in-process daemon can run alongside a
    /// real one on the developer's machine.
    pub(crate) control_pipe_name: String,
    exe_path: PathBuf,
    shutdown_tx: watch::Sender<ShutdownMode>,
    /// Set to `true` the moment the user confirms a tray quit. The control
    /// pipe `dispatch_request` checks this and rejects mutating frames
    /// (`SpawnSession`, `StopSession`) so a GUI relaunched during the ≤3s
    /// graceful drain window cannot spawn a new supervisor under a daemon
    /// that is about to drop its Job Object and kill everything.
    quitting: std::sync::atomic::AtomicBool,
    /// Proxy to send events to the tray icon event loop. `None` in the
    /// headless test daemon — callers must null-check before sending.
    #[cfg(windows)]
    pub(crate) tray_proxy: Option<EventLoopProxy<TrayEvent>>,
    /// Handle to the WorkspaceMonitor task. Set once, during
    /// `run_control_loop` startup; read by control-pipe clients that
    /// subscribe to state updates or send topology pokes.
    monitor: std::sync::OnceLock<crate::cmd::workspace_monitor::MonitorHandle>,
    /// Per-workspace lifecycle gate (D12). Mutation RPCs use this to
    /// serialize against concurrent operations on the same workspace and
    /// to expose a typed "being deleted" error for spawn/create. Kept as
    /// an `Arc` so `DeleteGuard`s can outlive the handler frame.
    #[doc(hidden)]
    pub lifecycle: Arc<crate::cmd::lifecycle::LifecycleRegistry>,
    /// Per-repo mutation mutex. Serializes `CreateWorkspace` and
    /// `RemoveWorkspace` against each other on the same repo so their
    /// critical sections cannot interleave (Codex round-2 finding). The
    /// lifecycle gate alone isn't enough because
    /// `begin_delete`/`check_can_mutate` are independent atomic steps
    /// around a non-atomic DB-plus-git workflow — a Remove for an
    /// as-yet-absent name can still Ack while a concurrent Create is
    /// partway through committing the row.
    ///
    /// Per-repo (not per-workspace) is intentional: in a single-user
    /// app, coarse-grained blocking is cheap and keeps the state
    /// machine simple. If concurrent mutations on different workspaces
    /// of the same repo become a measured bottleneck, narrow later.
    repo_mutation_locks: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Git operations used by mutation handlers (test seam).
    pub(crate) git_ops: Arc<dyn WorkspaceGitOps>,
}

impl DaemonState {
    /// Return (creating if absent) the per-repo mutation lock. Held by
    /// `handle_create_workspace` and `handle_remove_workspace` across their
    /// critical sections.
    pub(crate) async fn acquire_repo_mutation_lock(
        self: &Arc<Self>,
        repo: &str,
    ) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.repo_mutation_locks.lock().await;
        map.entry(repo.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
}

pub fn run(command: DaemonCommands) -> shard_core::Result<()> {
    match command {
        DaemonCommands::Start => run_daemon(),
        DaemonCommands::Stop => stop_daemon(),
        DaemonCommands::Status => status_daemon(),
    }
}

/// Build a headless `DaemonState` for the integration test harness.
///
/// Separate from [`run_headless_daemon`] so tests can keep a handle to the
/// state while the control loop runs on a spawned task — lets them inject
/// fake sessions or inspect the lifecycle registry mid-test.
#[cfg(windows)]
#[doc(hidden)]
pub fn build_headless_state(
    config: DaemonConfig,
    shutdown_tx: watch::Sender<ShutdownMode>,
) -> shard_core::Result<Arc<DaemonState>> {
    config.paths.ensure_dirs()?;

    let job_guard = DaemonJobGuard::new().map_err(|e| {
        shard_core::ShardError::Other(format!("Job Object creation failed: {e}"))
    })?;

    let exe_path = std::env::current_exe()?;

    Ok(Arc::new(DaemonState {
        sessions: Mutex::new(HashMap::new()),
        job_guard,
        paths: config.paths,
        control_pipe_name: config.control_pipe_name,
        exe_path,
        shutdown_tx,
        quitting: std::sync::atomic::AtomicBool::new(false),
        tray_proxy: None,
        monitor: std::sync::OnceLock::new(),
        lifecycle: Arc::new(crate::cmd::lifecycle::LifecycleRegistry::new()),
        repo_mutation_locks: tokio::sync::Mutex::new(HashMap::new()),
        git_ops: config.git_ops,
    }))
}

/// Run the headless daemon given a pre-built state.
#[cfg(windows)]
#[doc(hidden)]
pub async fn run_headless_daemon_with_state(
    state: Arc<DaemonState>,
    shutdown_rx: watch::Receiver<ShutdownMode>,
) -> shard_core::Result<()> {
    adopt_orphans(state.clone()).await;
    run_control_loop(state, shutdown_rx).await;
    Ok(())
}

/// Headless entry point for the integration test harness.
///
/// Runs `adopt_orphans` + `run_control_loop` on the caller's tokio runtime,
/// with no tray and no winit event loop. Exits cleanly once `shutdown_rx`
/// observes a non-`Running` value.
#[cfg(windows)]
pub async fn run_headless_daemon(
    config: DaemonConfig,
    shutdown_tx: watch::Sender<ShutdownMode>,
    shutdown_rx: watch::Receiver<ShutdownMode>,
) -> shard_core::Result<()> {
    let state = build_headless_state(config, shutdown_tx)?;
    run_headless_daemon_with_state(state, shutdown_rx).await
}

// ── Test-support surface ────────────────────────────────────────────────────
//
// These helpers are public-but-hidden to keep the surface small. Production
// callers never need them.

/// Inject a fake `LiveSession` record into the daemon's live registry. Used
/// by integration tests to simulate a session-bound workspace without
/// actually spawning a supervisor child process.
#[cfg(windows)]
#[doc(hidden)]
pub async fn test_inject_live_session(
    state: &Arc<DaemonState>,
    session_id: String,
    supervisor_pid: u32,
    transport_addr: String,
    repo: String,
    workspace: String,
    creation_time: u64,
) {
    let mut sessions = state.sessions.lock().await;
    sessions.insert(
        session_id.clone(),
        LiveSession {
            session_id,
            supervisor_pid,
            transport_addr,
            repo,
            workspace,
            creation_time,
        },
    );
}

/// Observe the current number of live sessions known to the daemon.
#[cfg(windows)]
#[doc(hidden)]
pub async fn test_live_session_count(state: &Arc<DaemonState>) -> usize {
    state.sessions.lock().await.len()
}

/// Check whether the lifecycle registry believes the given workspace is
/// currently accepting mutations. Returns the error if blocked.
#[cfg(windows)]
#[doc(hidden)]
pub fn test_lifecycle_check(
    state: &Arc<DaemonState>,
    repo: &str,
    name: &str,
) -> Result<(), String> {
    state
        .lifecycle
        .check_can_mutate(repo, name)
        .map_err(|e| e.to_string())
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
        control_pipe_name: CONTROL_PIPE_NAME.to_string(),
        exe_path,
        shutdown_tx: shutdown_tx.clone(),
        quitting: std::sync::atomic::AtomicBool::new(false),
        tray_proxy: Some(tray_proxy),
        monitor: std::sync::OnceLock::new(),
        lifecycle: Arc::new(crate::cmd::lifecycle::LifecycleRegistry::new()),
        repo_mutation_locks: tokio::sync::Mutex::new(HashMap::new()),
        git_ops: default_git_ops(),
    });

    // Build the tokio runtime on the main thread and share it via Arc. The
    // runtime is only dropped when the last Arc drops — which happens at the
    // end of `run_daemon` on the main thread, after `event_loop.run_app`
    // returns and after the tokio thread joins. This keeps the tray's
    // `tokio_handle` safe to use for the entire lifetime of the event loop:
    // even if `run_control_loop` exits early (e.g. via `shardctl daemon
    // stop`), the runtime stays alive until the tray itself tears down.
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| shard_core::ShardError::Other(format!("tokio runtime: {e}")))?,
    );
    let tokio_handle = rt.handle().clone();

    let tokio_state = state.clone();
    let tokio_shutdown_rx = shutdown_rx.clone();
    let tokio_rt = rt.clone();
    let tokio_thread = std::thread::Builder::new()
        .name("daemon-tokio".into())
        .spawn(move || {
            // Adopt orphaned supervisors from a previous daemon instance
            tokio_rt.block_on(adopt_orphans(tokio_state.clone()));

            info!("Daemon ready, entering control pipe loop");
            tokio_rt.block_on(run_control_loop(tokio_state, tokio_shutdown_rx));

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
                    if let Some(proxy) = &watcher_proxy {
                        let _ = proxy.send_event(TrayEvent::Quit);
                    }
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });

    // Run the tray icon on the main thread (blocks until quit)
    let mut app = TrayApp::new(shutdown_tx, exe_dir, state.clone(), tokio_handle);
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

/// Main async loop: control pipe server + WorkspaceMonitor.
async fn run_control_loop(state: Arc<DaemonState>, mut shutdown_rx: watch::Receiver<ShutdownMode>) {
    // Populate the lifecycle registry with every known workspace so
    // mutation RPCs can look them up. Cheap: a single DB scan at startup.
    // Best-effort — if the DB is unavailable the registry stays empty and
    // handlers fall back to on-demand registration.
    {
        let repo_store = RepositoryStore::new(state.paths.clone());
        let ws_store = WorkspaceStore::new(state.paths.clone());
        if let Ok(repos) = repo_store.list() {
            for repo in &repos {
                if let Ok(workspaces) = ws_store.list(&repo.alias) {
                    for ws in &workspaces {
                        state.lifecycle.register_active(&repo.alias, &ws.name);
                    }
                }
            }
        }
    }

    // Create the first control pipe instance
    let server = match create_pipe_instance(&state.control_pipe_name, true) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create control pipe: {e}");
            return;
        }
    };

    // Spawn WorkspaceMonitor. This replaces the former 5s heartbeat task —
    // PID liveness, DB exit transitions, and tray session-count updates now
    // live in the monitor's 1s fast tick alongside git-state syncing.
    let (monitor_handle, monitor_task) =
        crate::cmd::workspace_monitor::spawn(state.clone(), shutdown_rx.clone());
    if state.monitor.set(monitor_handle).is_err() {
        warn!("WorkspaceMonitor handle already set — ignoring");
    }

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

    // Cancel accept loop and monitor task
    accept_handle.abort();
    monitor_task.abort();

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
                        server = match create_pipe_instance(&state.control_pipe_name, false) {
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

    // Request/response loop. Two special frames change the loop's shape:
    //   - `Subscribe` converts this connection into a long-lived state push
    //     stream. After that point, nothing more is read from the client —
    //     closing the pipe is the only way to unsubscribe.
    //   - `TopologyChanged` is a fire-and-forget poke with no response.
    loop {
        let frame = match read_control_frame(&mut stream).await? {
            Some(f) => f,
            None => return Ok(()), // Client disconnected
        };

        match frame {
            ControlFrame::Subscribe => {
                if let Some(monitor) = state.monitor.get() {
                    let shutdown_rx = state.shutdown_tx.subscribe();
                    return run_subscribe_loop(monitor.clone(), stream, shutdown_rx).await;
                } else {
                    write_control_frame(
                        &mut stream,
                        &ControlFrame::Error {
                            message: "monitor not ready".to_string(),
                        },
                    )
                    .await?;
                    continue;
                }
            }
            ControlFrame::TopologyChanged { repo_alias } => {
                if let Some(monitor) = state.monitor.get() {
                    monitor.poke_topology(repo_alias);
                }
                // No response — fire-and-forget.
                continue;
            }
            other => {
                // Capture shutdown mode before dispatching.
                let shutdown_mode = match &other {
                    ControlFrame::Shutdown { graceful } => Some(if *graceful {
                        ShutdownMode::Graceful
                    } else {
                        ShutdownMode::Force
                    }),
                    _ => None,
                };

                let response = dispatch_request(&state, other).await;
                write_control_frame(&mut stream, &response).await?;

                // If we just acked a shutdown, signal with the correct mode and exit
                if let Some(mode) = shutdown_mode {
                    let _ = state.shutdown_tx.send(mode);
                    return Ok(());
                }
            }
        }
    }
}

/// Long-lived subscribe loop. The client has already handshaked and sent
/// `Subscribe`; from here on the daemon pushes `StateSnapshot` frames for
/// every repo whose state changes. There is no explicit unsubscribe —
/// closing the pipe terminates the loop on the next send attempt.
///
/// On subscriber lag (`broadcast::RecvError::Lagged`) the daemon resyncs by
/// re-sending every repo's current state. Snapshots are idempotent and
/// versioned, so this is always safe.
async fn run_subscribe_loop(
    monitor: crate::cmd::workspace_monitor::MonitorHandle,
    mut stream: tokio::net::windows::named_pipe::NamedPipeServer,
    mut shutdown_rx: watch::Receiver<ShutdownMode>,
) -> std::io::Result<()> {
    use crate::cmd::workspace_monitor::ChangeKind;
    use tokio::sync::broadcast::error::RecvError;

    // Subscribe BEFORE snapshotting. If this order were reversed any repo
    // updates that landed between the snapshot read and the subscribe
    // registration would be lost. Subscribing first means we may get
    // duplicate notifications for the initial set, but snapshots are
    // versioned and idempotent so duplicates are harmless.
    let mut change_rx = monitor.subscribe();

    // Initial snapshot — one frame per known repo.
    for state in monitor.snapshot().await {
        let frame = ControlFrame::StateSnapshot {
            state: (*state).clone(),
        };
        if let Err(e) = write_control_frame(&mut stream, &frame).await {
            info!("subscribe: initial snapshot write failed: {e}");
            return Ok(());
        }
    }

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                return Ok(());
            }
            recv = change_rx.recv() => {
                match recv {
                    Ok(ChangeKind::State(alias)) => {
                        if let Some(state) = monitor.get(&alias).await {
                            let frame = ControlFrame::StateSnapshot {
                                state: (*state).clone(),
                            };
                            if let Err(e) = write_control_frame(&mut stream, &frame).await {
                                info!("subscribe: client disconnected ({e})");
                                return Ok(());
                            }
                        }
                    }
                    Ok(ChangeKind::Sessions(repo)) => {
                        let frame = ControlFrame::SessionsChanged { repo };
                        if let Err(e) = write_control_frame(&mut stream, &frame).await {
                            info!("subscribe: client disconnected ({e})");
                            return Ok(());
                        }
                    }
                    Ok(ChangeKind::WorkspaceRemoved { repo, name }) => {
                        let frame = ControlFrame::WorkspaceRemoved { repo, name };
                        if let Err(e) = write_control_frame(&mut stream, &frame).await {
                            info!("subscribe: client disconnected ({e})");
                            return Ok(());
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        // We may have dropped both State and Sessions events.
                        // Re-send a full snapshot per repo AND a SessionsChanged
                        // per repo so the client refreshes both axes of state.
                        info!("subscribe: lagged by {n}, resyncing");
                        let states = monitor.snapshot().await;
                        for state in &states {
                            let frame = ControlFrame::StateSnapshot {
                                state: (**state).clone(),
                            };
                            if let Err(e) = write_control_frame(&mut stream, &frame).await {
                                info!("subscribe: resync write failed: {e}");
                                return Ok(());
                            }
                        }
                        for state in &states {
                            let frame = ControlFrame::SessionsChanged {
                                repo: state.repo_alias.clone(),
                            };
                            if let Err(e) = write_control_frame(&mut stream, &frame).await {
                                info!("subscribe: resync write failed: {e}");
                                return Ok(());
                            }
                        }
                    }
                    Err(RecvError::Closed) => {
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Dispatch a control request to the appropriate handler.
async fn dispatch_request(state: &Arc<DaemonState>, frame: ControlFrame) -> ControlFrame {
    // Reject mutating frames if the daemon is in the middle of a tray-
    // initiated quit. Read-only frames (Ping, ListSessions, Shutdown) are
    // still serviced so observers and `shardctl daemon stop` continue to
    // behave sensibly during the ≤3s drain window.
    if state
        .quitting
        .load(std::sync::atomic::Ordering::Acquire)
        && matches!(
            frame,
            ControlFrame::SpawnSession { .. }
                | ControlFrame::StopSession { .. }
                | ControlFrame::CreateWorkspace { .. }
                | ControlFrame::RemoveWorkspace { .. }
        )
    {
        return ControlFrame::Error {
            message: "daemon is shutting down".to_string(),
        };
    }

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

        ControlFrame::RemoveWorkspace { repo, name } => {
            handle_remove_workspace(state, repo, name).await
        }

        ControlFrame::CreateWorkspace {
            repo,
            name,
            mode,
            branch,
        } => handle_create_workspace(state, repo, name, mode, branch).await,

        ControlFrame::ListWorkspaces { repo } => handle_list_workspaces(state, repo).await,

        ControlFrame::ListBranchInfo { repo } => handle_list_branch_info(state, repo).await,

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
    // Gate check per D5: reject spawn when the target workspace is being
    // deleted or is in Broken state. This closes the race where a new
    // session could land after RemoveWorkspace acquired `Deleting` but
    // before it snapshotted the bound-session list.
    if let Err(e) = state.lifecycle.check_can_mutate(&repo, &workspace) {
        return ControlFrame::Error {
            message: e.to_string(),
        };
    }

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

/// Handle `RemoveWorkspace` — the D5 atomic workflow.
///
///  1. Acquire the per-workspace lifecycle gate (`Active`/`Broken` →
///     `Deleting`). Concurrent `RemoveWorkspace` on the same target joins
///     via the lifecycle notifier; `NotFound` means the row is already
///     gone and we return success idempotently.
///  2. Stop every session currently bound to `repo:name` via
///     `stop_session_and_wait` with a 5s graceful budget (force-kill on
///     timeout). The supervisor's child shell holds the workspace CWD
///     open; its exit is what releases the handle.
///  3. `MonitorCommand::DropRepoWorkspace` — the monitor drops its
///     `ReadDirectoryChangesW` handle for this workspace and acks BEFORE
///     we proceed to `RemoveDirectoryW`. This closes the SHA-55 race.
///  4. `remove_worktree_fs` via the injected git-ops — `git worktree
///     remove --force` with prune + `remove_dir_all` fallback for broken
///     rows. On failure we transition to `broken` and preserve the DB
///     row so a retried `RemoveWorkspace` can resume.
///  5. `WorkspaceStore::delete_row` — DB side. If this fails after the
///     filesystem delete succeeded, the row becomes an orphan; reconcile
///     will re-surface it and the user can retry.
///  6. `commit_gone()` releases the lifecycle gate, fires any joiners.
///  7. `poke_topology` + broadcast `WorkspaceRemoved` so subscribers
///     invalidate cached state and the UI drops the row.
///
/// `is_base=true` workspaces skip the git step entirely (see D11 /
/// SHA-56): they refer to the user's own checkout, not a managed
/// worktree. The DB row is still deleted.
async fn handle_remove_workspace(
    state: &Arc<DaemonState>,
    repo: String,
    name: String,
) -> ControlFrame {
    use crate::cmd::lifecycle::BeginDelete;

    // Serialize against concurrent `CreateWorkspace` on this repo (Codex
    // round-2 finding). The lifecycle gate alone can't prevent the race
    // where an absent name triggers `commit_gone` while a Create is
    // partway through committing the same name to the DB.
    let repo_lock = state.acquire_repo_mutation_lock(&repo).await;
    let _guard = repo_lock.lock().await;

    // 1. Lifecycle gate. `begin_delete` is atomic over absent/Active/Broken
    //    → Deleting. If another delete is already in flight, wait on its
    //    completion notifier and then re-loop: if the first caller
    //    rolled back the workspace is back to Active and we retry; if it
    //    committed Gone (absent from the map), `begin_delete` will insert
    //    Deleting for us and we own the (now best-effort) cleanup; if it
    //    committed Broken, we pick up the retry per D12.
    let guard = loop {
        match state.lifecycle.begin_delete(&repo, &name) {
            BeginDelete::Started(g) => break g,
            BeginDelete::AlreadyDeleting(notifier) => {
                notifier.notified().await;
                // Loop: the joiner either owns the retry now, or the
                // original succeeded and the workspace is Gone — which
                // `begin_delete` will treat as absent and re-Start. The
                // subsequent `ws_store.get` call returns WorkspaceNotFound
                // and we commit_gone idempotently.
            }
        }
    };

    // Fetch workspace info up front. If the row doesn't exist, release the
    // gate and return success idempotently.
    let ws_store = WorkspaceStore::new(state.paths.clone());
    let workspace = match ws_store.get(&repo, &name) {
        Ok(w) => w,
        Err(shard_core::ShardError::WorkspaceNotFound(_)) => {
            guard.commit_gone();
            return ControlFrame::RemoveWorkspaceAck;
        }
        Err(e) => {
            guard.rollback();
            return ControlFrame::Error {
                message: format!("failed to look up workspace: {e}"),
            };
        }
    };

    // 2. Stop every bound session.
    let bound_sessions: Vec<(String, String, u32, u64)> = {
        let sessions = state.sessions.lock().await;
        sessions
            .values()
            .filter(|s| s.repo == repo && s.workspace == name)
            .map(|s| {
                (
                    s.session_id.clone(),
                    s.transport_addr.clone(),
                    s.supervisor_pid,
                    s.creation_time,
                )
            })
            .collect()
    };

    for (session_id, transport_addr, pid, creation_time) in &bound_sessions {
        match stop_session_and_wait(
            transport_addr,
            *pid,
            *creation_time,
            Duration::from_secs(5),
        )
        .await
        {
            Ok(_) => {
                info!(
                    "RemoveWorkspace: stopped session {} [{}:{}]",
                    &session_id[..8.min(session_id.len())],
                    repo,
                    name
                );
                // Only drop from the registry when the process is confirmed
                // gone. If stop failed (including the PID-reuse refusal or
                // OpenProcess failure), the supervisor may still be alive;
                // leaving the entry in keeps fast_tick + retry handlers
                // able to see and act on it.
                let mut sessions = state.sessions.lock().await;
                sessions.remove(session_id);
            }
            Err(e) => {
                warn!(
                    "RemoveWorkspace: failed to stop session {} cleanly ({e}); aborting delete",
                    &session_id[..8.min(session_id.len())]
                );
                // Give up on this delete — the directory is still pinned
                // by a live shell's CWD, so RemoveDirectoryW would fail
                // anyway. Commit Broken so a retry can pick up once the
                // session is dealt with.
                guard.commit_broken();
                return ControlFrame::Error {
                    message: format!(
                        "failed to stop bound session {}: {e}",
                        &session_id[..8.min(session_id.len())]
                    ),
                };
            }
        }
    }

    // 3. Drop watcher ack-after-drop.
    if !workspace.is_base {
        if let Some(monitor) = state.monitor.get() {
            if let Err(e) = monitor.drop_repo_workspace(&repo, &name).await {
                warn!(
                    "RemoveWorkspace: monitor watcher drop failed ({e}); proceeding",
                );
                // Fall through. Worst case the fs delete fails because
                // the watcher still holds the handle; we'll surface that
                // as a Broken state below.
            }
        }
    }

    // 4. Filesystem side.
    if !workspace.is_base {
        let repo_store = RepositoryStore::new(state.paths.clone());
        let repo_rec = match repo_store.get(&repo) {
            Ok(r) => r,
            Err(e) => {
                guard.commit_broken();
                return ControlFrame::Error {
                    message: format!("repo lookup failed mid-delete: {e}"),
                };
            }
        };

        let source_dir = state
            .paths
            .repo_source_for_repo(&repo, repo_rec.local_path.as_deref());
        let ws_dir = std::path::PathBuf::from(&workspace.path);

        if let Err(e) = workspaces::remove_worktree_fs(
            state.git_ops.as_ref(),
            &source_dir,
            &ws_dir,
        ) {
            warn!(
                "RemoveWorkspace: filesystem delete failed ({}:{}): {e}",
                repo, name
            );
            guard.commit_broken();
            return ControlFrame::Error {
                message: format!("workspace removal failed, marked broken: {e}"),
            };
        }
    }

    // 5. DB row delete.
    if let Err(e) = ws_store.delete_row(&repo, &name) {
        warn!(
            "RemoveWorkspace: DB row delete failed after fs delete ({}:{}): {e}",
            repo, name
        );
        guard.commit_broken();
        return ControlFrame::Error {
            message: format!("DB delete failed after fs removal: {e}"),
        };
    }

    // 6. Release the gate.
    guard.commit_gone();

    // 7. Refresh monitor state + broadcast fine-grained event.
    if let Some(monitor) = state.monitor.get() {
        monitor.poke_topology(Some(repo.clone()));
        monitor.broadcast(crate::cmd::workspace_monitor::ChangeKind::WorkspaceRemoved {
            repo: repo.clone(),
            name: name.clone(),
        });
    }

    info!("RemoveWorkspace: {}:{} removed", repo, name);
    ControlFrame::RemoveWorkspaceAck
}

/// Handle `CreateWorkspace` (Phase 2 of the daemon-broker migration).
///
/// Mirrors the D5 pattern but is far simpler than remove because create has
/// no cleanup steps to serialize. Resolves the effective workspace name
/// up-front via `WorkspaceStore::resolve_workspace_name` so the lifecycle
/// gate check applies uniformly — otherwise an implicit name (e.g. from
/// `WorkspaceMode::NewBranch` + no explicit `name`) that happens to match
/// a workspace currently in `Deleting`/`Broken` would bypass the gate and
/// fail deep inside `create` with a generic DB error.
async fn handle_create_workspace(
    state: &Arc<DaemonState>,
    repo: String,
    name: Option<String>,
    mode: shard_core::workspaces::WorkspaceMode,
    branch: Option<String>,
) -> ControlFrame {
    // Serialize against concurrent `RemoveWorkspace` on this repo so the
    // two handlers' critical sections can't interleave. Without this, a
    // Remove of an as-yet-absent name could slip in between our gate
    // check and the DB INSERT and return a vacuous Ack to its caller.
    let repo_lock = state.acquire_repo_mutation_lock(&repo).await;
    let _guard = repo_lock.lock().await;

    let ws_store = WorkspaceStore::new(state.paths.clone());

    // Resolve the effective name before any side effects so the gate check
    // covers both explicit and implicit-name callers. Resolution mirrors
    // the logic in `WorkspaceStore::create` — if the two diverge we'll
    // gate-check one name and write a different one, so the helper is
    // deliberately narrow.
    let resolved_name = match ws_store.resolve_workspace_name(
        &repo,
        name.as_deref(),
        mode,
        branch.as_deref(),
    ) {
        Ok(n) => n,
        Err(e) => {
            return ControlFrame::Error {
                message: format!("resolve workspace name failed: {e}"),
            }
        }
    };

    // Pre-mutation gate check — mirrors `handle_spawn`. Rejects
    // `Deleting`/`Broken` targets regardless of whether the caller
    // spelled the name or let it default.
    if let Err(e) = state.lifecycle.check_can_mutate(&repo, &resolved_name) {
        return ControlFrame::Error {
            message: e.to_string(),
        };
    }

    let ws = match ws_store.create(&repo, name.as_deref(), mode, branch.as_deref(), false) {
        Ok(w) => w,
        Err(e) => {
            return ControlFrame::Error {
                message: format!("create workspace failed: {e}"),
            }
        }
    };

    // Register the resolved name in the lifecycle map so subsequent
    // `RemoveWorkspace` / `SpawnSession` calls see a concrete `Active`
    // entry rather than falling through the "absent" branch. See plan
    // starter-kit note for Phase 2.
    state.lifecycle.register_active(&repo, &ws.name);

    // Poke the monitor so the repo's next StateSnapshot includes the new
    // workspace. Subscribers pick this up through the existing
    // `ChangeKind::State(repo)` path — no new fine-grained event needed
    // per D10.
    if let Some(monitor) = state.monitor.get() {
        monitor.poke_topology(Some(repo.clone()));
    }

    info!(
        "CreateWorkspace: {}:{} on branch '{}'",
        repo, ws.name, ws.branch
    );
    ControlFrame::CreateWorkspaceAck { workspace: ws }
}

/// Handle `ListWorkspaces` — enriched list of workspaces for a repo.
///
/// Reads the DB through `WorkspaceStore::list`, then joins each row against
/// the monitor's cached `RepoState` so the caller gets both halves in one
/// round trip. `status` is `None` when the monitor has not yet produced a
/// snapshot (e.g. immediately after `AddRepo`) or when a workspace is newer
/// than the last monitor tick.
async fn handle_list_workspaces(state: &Arc<DaemonState>, repo: String) -> ControlFrame {
    let ws_store = WorkspaceStore::new(state.paths.clone());
    let workspaces = match ws_store.list(&repo) {
        Ok(w) => w,
        Err(e) => {
            return ControlFrame::Error {
                message: format!("list workspaces failed: {e}"),
            }
        }
    };

    let repo_state = match state.monitor.get() {
        Some(monitor) => monitor.get(&repo).await,
        None => None,
    };

    let items = workspaces
        .into_iter()
        .map(|ws| {
            let status = repo_state
                .as_ref()
                .and_then(|s| s.workspaces.get(&ws.name).cloned());
            shard_core::workspaces::WorkspaceWithStatus {
                workspace: ws,
                status,
            }
        })
        .collect();

    ControlFrame::WorkspaceList { items }
}

/// Handle `ListBranchInfo` — branch/worktree occupancy for a repo.
///
/// On-demand git read (`git branch`, `git worktree list --porcelain`)
/// cross-referenced with the DB's workspace rows. Routed through the
/// daemon so all callers agree on a single serialization point; not
/// cached against the monitor's snapshot because the monitor walks the
/// worktree list on its own cadence for a different purpose and callers
/// of this RPC want fresh data (e.g. the new-workspace wizard picking a
/// branch). Per D4: this is a "live view" read that migrates alongside
/// its corresponding mutation in the same batch.
async fn handle_list_branch_info(state: &Arc<DaemonState>, repo: String) -> ControlFrame {
    let ws_store = WorkspaceStore::new(state.paths.clone());
    match ws_store.list_branch_info(&repo) {
        Ok(branches) => ControlFrame::BranchInfoList { branches },
        Err(e) => ControlFrame::Error {
            message: format!("list branch info failed: {e}"),
        },
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
        if let Some(proxy) = &state.tray_proxy {
            let _ = proxy.send_event(TrayEvent::SessionCount(count));
        }
    }
}
