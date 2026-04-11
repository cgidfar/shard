use std::io::{Read, Seek};
use std::path::Path;
use std::sync::{Arc, Mutex};

use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch};

use shard_transport::protocol::{self, ActivityState, Frame};
#[cfg(windows)]
use shard_transport::transport_windows::create_pipe_instance;
#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeServer;

use crate::pty::PtySession;

struct Client {
    tx: mpsc::Sender<Vec<u8>>,
    /// Skip TerminalOutput fan-out for monitor-only clients.
    monitor_only: bool,
}

#[derive(Clone, Copy, Debug)]
enum Shutdown {
    None,
    Graceful,
    Force,
}

/// Run the session supervisor event loop.
///
/// `initial_server` is a pre-created named pipe server instance. The caller
/// creates this *before* signaling readiness, ensuring clients can connect
/// as soon as the ready file appears.
///
/// Returns the child's exit code.
pub async fn run(
    transport_addr: &str,
    mut pty_session: PtySession,
    log_path: &Path,
    initial_server: NamedPipeServer,
) -> std::io::Result<i32> {
    let child_pid = pty_session.child_pid();
    let byte_offset = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let clients: Arc<Mutex<Vec<Client>>> = Arc::new(Mutex::new(Vec::new()));
    let pty_writer = Arc::new(Mutex::new(pty_session.writer));

    let (resize_tx, mut resize_rx) = mpsc::channel::<(u16, u16)>(16);
    let (shutdown_tx, shutdown_rx) = watch::channel(Shutdown::None);

    // Latest activity state reported by harness hooks (0xFF = unset).
    // Updated by fire-and-forget hook connections, read for snapshot on Resume.
    let activity_state = Arc::new(std::sync::atomic::AtomicU8::new(0xFF));

    let log_file = Arc::new(Mutex::new(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?,
    ));

    // Shared log path for resume/replay
    let log_path_shared = Arc::new(log_path.to_path_buf());

    // === Task 1: Read PTY output and fan out ===
    let clients_clone = clients.clone();
    let byte_offset_clone = byte_offset.clone();
    let log_file_clone = log_file.clone();
    let mut pty_reader = pty_session.reader;

    let pty_read_task = tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 4096];
        loop {
            match pty_reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let data = buf[..n].to_vec();

                    if let Ok(mut log) = log_file_clone.lock() {
                        let _ = std::io::Write::write_all(&mut *log, &data);
                    }

                    let offset = byte_offset_clone
                        .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);

                    let frame = Frame::TerminalOutput {
                        offset,
                        data: data.clone(),
                    };
                    let mut frame_buf = Vec::new();
                    let rt = tokio::runtime::Handle::current();
                    rt.block_on(async {
                        let _ = protocol::write_frame(&mut frame_buf, &frame).await;
                    });

                    if let Ok(mut clients) = clients_clone.lock() {
                        clients.retain(|client| {
                            if client.monitor_only { return true; }
                            client.tx.try_send(frame_buf.clone()).is_ok()
                        });
                    }
                }
                Err(e) => {
                    tracing::debug!("pty read error: {e}");
                    break;
                }
            }
        }
    });

    // === Task 2: Accept transport clients ===
    let clients_clone2 = clients.clone();
    let pty_writer_clone = pty_writer.clone();
    let byte_offset_for_accept = byte_offset.clone();
    let activity_state_for_accept = activity_state.clone();
    let addr = transport_addr.to_string();

    let accept_task = tokio::spawn(async move {
        let mut server = initial_server;

        loop {
            if let Err(e) = server.connect().await {
                tracing::error!("pipe connect error: {e}");
                break;
            }

            let connected = server;
            server = match create_pipe_instance(&addr, false) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("failed to create next pipe instance: {e}");
                    break;
                }
            };

            let (tx, mut rx) = mpsc::channel::<Vec<u8>>(1024);

            let writer = pty_writer_clone.clone();
            let resize = resize_tx.clone();
            let shutdown = shutdown_tx.clone();
            let log_for_replay = log_path_shared.clone();
            let current_offset = byte_offset_for_accept.clone();
            let clients_for_register = clients_clone2.clone();
            let activity = activity_state_for_accept.clone();

            tokio::spawn(async move {
                let (mut reader, mut writer_half) = tokio::io::split(connected);

                // Read the first frame to determine connection type
                let first_frame = match protocol::read_frame(&mut reader).await {
                    Ok(Some(frame)) => frame,
                    _ => return,
                };

                // --- Fire-and-forget path: harness hook pushing state ---
                if let Frame::ActivityUpdate { state } = first_frame {
                    activity.store(state as u8, std::sync::atomic::Ordering::Relaxed);
                    let mut buf = Vec::new();
                    if protocol::write_frame(&mut buf, &Frame::ActivityUpdate { state })
                        .await
                        .is_ok()
                    {
                        if let Ok(clients) = clients_for_register.lock() {
                            for client in clients.iter() {
                                let _ = client.tx.try_send(buf.clone());
                            }
                        }
                    }
                    return;
                }

                // --- Streaming client path: Resume → register → replay → stream ---
                let resume_offset = match first_frame {
                    Frame::Resume { last_seen_offset } => last_seen_offset,
                    _ => 0,
                };

                // Register for live updates BEFORE replay so no bytes are
                // lost between the offset snapshot and registration. Live
                // fan-out buffers in the mpsc channel while replay writes
                // directly to the pipe, preserving ordering.
                let is_monitor = resume_offset == u64::MAX;
                if let Ok(mut clients) = clients_for_register.lock() {
                    clients.push(Client { tx, monitor_only: is_monitor });
                }

                // Replay log from resume_offset to current position, streamed
                // from disk in small chunks to avoid loading the entire file
                // into memory and to keep IPC messages small.
                let live_offset = current_offset.load(std::sync::atomic::Ordering::Relaxed);
                if resume_offset < live_offset {
                    const REPLAY_CHUNK: usize = 4096;
                    match std::fs::File::open(&*log_for_replay) {
                        Ok(mut file) => {
                            let start = resume_offset as u64;
                            let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
                            let end = std::cmp::min(live_offset, file_len);
                            if start < end {
                                if let Err(e) = file.seek(std::io::SeekFrom::Start(start)) {
                                    tracing::warn!("replay seek failed: {e}");
                                } else {
                                    let mut remaining = (end - start) as usize;
                                    let mut offset = start;
                                    let mut chunk_buf = vec![0u8; REPLAY_CHUNK];
                                    while remaining > 0 {
                                        let to_read = std::cmp::min(remaining, REPLAY_CHUNK);
                                        match file.read(&mut chunk_buf[..to_read]) {
                                            Ok(0) => break,
                                            Ok(n) => {
                                                let frame = Frame::TerminalOutput {
                                                    offset,
                                                    data: chunk_buf[..n].to_vec(),
                                                };
                                                let mut buf = Vec::new();
                                                if let Err(e) = protocol::write_frame(&mut buf, &frame).await {
                                                    tracing::warn!("replay frame serialize failed: {e}");
                                                    break;
                                                }
                                                if let Err(e) = writer_half.write_all(&buf).await {
                                                    tracing::warn!("replay pipe write failed: {e}");
                                                    break;
                                                }
                                                offset += n as u64;
                                                remaining -= n;
                                            }
                                            Err(e) => {
                                                tracing::warn!("replay log read failed: {e}");
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("failed to open log for replay: {e}");
                        }
                    }
                }

                // Send activity state snapshot so late-connecting monitors
                // get the current state even if the hook fired before they joined.
                let state_val = activity.load(std::sync::atomic::Ordering::Relaxed);
                if let Ok(snap_state) = ActivityState::try_from(state_val) {
                    let snap = Frame::ActivityUpdate { state: snap_state };
                    let mut buf = Vec::new();
                    if protocol::write_frame(&mut buf, &snap).await.is_ok() {
                        let _ = writer_half.write_all(&buf).await;
                    }
                }

                // Forward live PTY output to client
                let send_task = tokio::spawn(async move {
                    while let Some(data) = rx.recv().await {
                        if writer_half.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                });

                // Read frames from client and dispatch
                let recv_task = tokio::spawn(async move {
                    loop {
                        match protocol::read_frame(&mut reader).await {
                            Ok(Some(Frame::TerminalInput { data })) => {
                                if let Ok(mut w) = writer.lock() {
                                    let _ = std::io::Write::write_all(&mut *w, &data);
                                }
                            }
                            Ok(Some(Frame::Resize { rows, cols })) => {
                                let _ = resize.send((rows, cols)).await;
                            }
                            Ok(Some(Frame::StopGraceful)) => {
                                tracing::info!("received stop-graceful");
                                let _ = shutdown.send(Shutdown::Graceful);
                                break;
                            }
                            Ok(Some(Frame::StopForce)) => {
                                tracing::info!("received stop-force");
                                let _ = shutdown.send(Shutdown::Force);
                                break;
                            }
                            Ok(None) => break,
                            Ok(Some(_)) => {}
                            Err(e) => {
                                tracing::debug!("client read error: {e}");
                                break;
                            }
                        }
                    }
                });

                let _ = tokio::join!(send_task, recv_task);
            });
        }
    });

    // === Task 3: Process resize requests ===
    let master_for_resize = pty_session.master;
    let resize_task = tokio::spawn(async move {
        while let Some((rows, cols)) = resize_rx.recv().await {
            tracing::debug!("resizing PTY to {rows}x{cols}");
            if let Err(e) = master_for_resize.resize(portable_pty::PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            }) {
                tracing::warn!("resize failed: {e}");
            }
        }
    });

    // === Task 4: Wait for child to exit OR shutdown signal ===
    let mut shutdown_watch = shutdown_rx.clone();

    let exit_code = tokio::select! {
        result = tokio::task::spawn_blocking(move || pty_session.child.wait()) => {
            match result {
                Ok(Ok(status)) => {
                    tracing::info!("child exited: {:?}", status);
                    status.exit_code() as i32
                }
                Ok(Err(e)) => {
                    tracing::error!("child wait error: {e}");
                    -1
                }
                Err(e) => {
                    tracing::error!("join error: {e}");
                    -1
                }
            }
        }
        shutdown_kind = async {
            loop {
                shutdown_watch.changed().await.ok();
                let val = *shutdown_watch.borrow();
                match val {
                    Shutdown::Graceful | Shutdown::Force => break val,
                    Shutdown::None => continue,
                }
            }
        } => {
            match shutdown_kind {
                Shutdown::Graceful => {
                    tracing::info!("graceful shutdown: closing PTY master");
                    // Drop the PTY writer to send EOF to the child
                    drop(pty_writer);
                    // Wait up to 3 seconds for the child to exit naturally
                    let _graceful_wait = tokio::time::timeout(
                        std::time::Duration::from_secs(3),
                        tokio::task::spawn_blocking({
                            let pid = child_pid;
                            move || {
                                if let Some(pid) = pid {
                                    let start = std::time::Instant::now();
                                    while start.elapsed() < std::time::Duration::from_secs(3) {
                                        #[cfg(windows)]
                                        {
                                            use crate::process::{PlatformProcessControl, ProcessControl};
                                            if !PlatformProcessControl::is_alive(pid) {
                                                return;
                                            }
                                        }
                                        std::thread::sleep(std::time::Duration::from_millis(100));
                                    }
                                }
                            }
                        }),
                    ).await;

                    // If child didn't exit gracefully, force-kill
                    if let Some(pid) = child_pid {
                        #[cfg(windows)]
                        {
                            use crate::process::{PlatformProcessControl, ProcessControl};
                            if PlatformProcessControl::is_alive(pid) {
                                tracing::info!("child didn't exit gracefully, force-killing");
                                let _ = PlatformProcessControl::terminate(pid);
                            }
                        }
                    }
                    -1
                }
                Shutdown::Force => {
                    tracing::info!("force shutdown: killing child immediately");
                    if let Some(pid) = child_pid {
                        #[cfg(windows)]
                        {
                            use crate::process::{PlatformProcessControl, ProcessControl};
                            let _ = PlatformProcessControl::terminate(pid);
                        }
                    }
                    -1
                }
                Shutdown::None => unreachable!(),
            }
        }
    };

    // 1. Drain PTY reader — child already exited, so ConPTY pipe will EOF
    //    once buffered data (including alt-screen restore) is consumed.
    //    Keep resize_task alive so the PtyMaster isn't dropped prematurely.
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        pty_read_task,
    )
    .await;

    // 2. NOW send Status frame — all TerminalOutput has been flushed
    let status_frame = Frame::Status {
        code: if exit_code == 0 { 0 } else if exit_code == -1 { 1 } else { 2 },
    };
    let mut status_buf = Vec::new();
    let _ = protocol::write_frame(&mut status_buf, &status_frame).await;
    if let Ok(clients) = clients.lock() {
        for client in clients.iter() {
            let _ = client.tx.try_send(status_buf.clone());
        }
    }

    // 3. Brief delay for delivery, then clean up
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    accept_task.abort();
    resize_task.abort();

    Ok(exit_code)
}
