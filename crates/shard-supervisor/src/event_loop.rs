use std::collections::HashMap;
use std::io::{Read, Seek};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch};

use shard_transport::protocol::{self, ActivityState, ClientKind, Frame, OwnerSummary};
use shard_transport::PlatformServer;
#[cfg(windows)]
use shard_transport::transport_windows::create_pipe_instance;

use crate::pty::PtySession;

#[allow(dead_code)] // identity fields read via the per-conn local captures, but kept here for diagnostics + future "list clients" RPC
struct Client {
    /// Server-assigned, monotonic per supervisor process. Used as the
    /// authoritative key for the clients map and for owner-release checks
    /// (a stale `client_id` can collide with a later connection; `conn_id`
    /// cannot).
    conn_id: u64,
    /// Client-supplied id from `Hello`. Surfaced in `OwnerSummary` so other
    /// clients can identify the owner.
    client_id: u64,
    kind: ClientKind,
    label: String,
    tx: mpsc::Sender<Vec<u8>>,
    /// Skip TerminalOutput fan-out for monitor-only clients.
    monitor_only: bool,
}

struct Owner {
    conn_id: u64,
    client_id: u64,
    kind: ClientKind,
    label: String,
}

impl Owner {
    fn summary(&self) -> OwnerSummary {
        OwnerSummary {
            client_id: self.client_id,
            kind: self.kind,
            label: self.label.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Shutdown {
    None,
    Graceful,
    Force,
}

#[derive(Debug, PartialEq, Eq)]
enum ClaimResult {
    Accepted(OwnerSummary),
    Rejected(Option<OwnerSummary>),
    Ignored,
}

/// Serialize an `InputOwnerChanged` frame and broadcast it to every connected
/// client. Used both on transitions (claim/release) and as a connect-time
/// snapshot for late joiners.
async fn broadcast_input_owner(clients: &Mutex<HashMap<u64, Client>>, owner: Option<OwnerSummary>) {
    let frame = Frame::InputOwnerChanged { owner };
    let mut buf = Vec::new();
    if protocol::write_frame(&mut buf, &frame).await.is_err() {
        return;
    }
    if let Ok(mut clients) = clients.lock() {
        clients.retain(|_conn_id, client| client.tx.try_send(buf.clone()).is_ok());
    }
}

async fn send_claim_rejected(
    clients: &Mutex<HashMap<u64, Client>>,
    conn_id: u64,
    owner: Option<OwnerSummary>,
) {
    let frame = Frame::ClaimRejected { owner };
    let mut buf = Vec::new();
    if protocol::write_frame(&mut buf, &frame).await.is_err() {
        return;
    }
    if let Ok(mut clients) = clients.lock() {
        let should_remove = clients
            .get(&conn_id)
            .map(|client| client.tx.try_send(buf).is_err())
            .unwrap_or(false);
        if should_remove {
            clients.remove(&conn_id);
        }
    }
}

fn apply_claim_input(
    owner_slot: &Mutex<Option<Owner>>,
    conn_id: u64,
    client_id: u64,
    kind: ClientKind,
    label: &str,
) -> ClaimResult {
    if matches!(kind, ClientKind::MonitorOnly) {
        return ClaimResult::Ignored;
    }

    let Ok(mut owner) = owner_slot.lock() else {
        return ClaimResult::Rejected(None);
    };

    if owner.as_ref().map(|o| o.conn_id) == Some(conn_id) {
        return ClaimResult::Accepted(owner.as_ref().unwrap().summary());
    }

    if matches!(kind, ClientKind::GuiWindow) {
        if let Some(current) = owner.as_ref() {
            let same_window = current.kind == ClientKind::GuiWindow && current.label == label;
            if !same_window {
                return ClaimResult::Rejected(Some(current.summary()));
            }
        }
    }

    *owner = Some(Owner {
        conn_id,
        client_id,
        kind,
        label: label.to_string(),
    });

    ClaimResult::Accepted(owner.as_ref().unwrap().summary())
}

fn release_owner_if_conn_matches(owner_slot: &Mutex<Option<Owner>>, conn_id: u64) -> bool {
    let Ok(mut owner) = owner_slot.lock() else {
        return false;
    };
    if owner.as_ref().map(|o| o.conn_id) == Some(conn_id) {
        *owner = None;
        true
    } else {
        false
    }
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
    initial_server: PlatformServer,
) -> std::io::Result<i32> {
    let child_pid = pty_session.child_pid();
    let byte_offset = Arc::new(AtomicU64::new(0));
    let clients: Arc<Mutex<HashMap<u64, Client>>> = Arc::new(Mutex::new(HashMap::new()));
    let pty_writer = Arc::new(Mutex::new(pty_session.writer));
    let next_conn_id = Arc::new(AtomicU64::new(1));
    let input_owner: Arc<Mutex<Option<Owner>>> = Arc::new(Mutex::new(None));

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

                    let offset = byte_offset_clone.fetch_add(n as u64, Ordering::Relaxed);

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
                        clients.retain(|_conn_id, client| {
                            if client.monitor_only {
                                return true;
                            }
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
    let next_conn_id_for_accept = next_conn_id.clone();
    let input_owner_for_accept = input_owner.clone();
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
            let next_conn_id = next_conn_id_for_accept.clone();
            let owner_slot = input_owner_for_accept.clone();

            tokio::spawn(async move {
                let (mut reader, mut writer_half) = tokio::io::split(connected);

                // Read the first frame to determine connection type
                let first_frame = match protocol::read_frame(&mut reader).await {
                    Ok(Some(frame)) => frame,
                    _ => return,
                };

                // --- Fire-and-forget path: harness hook pushing state ---
                //
                // Hooks (e.g. shard-cli's `notify` subcommand) open a fresh
                // pipe and send a single ActivityUpdate. They do not send
                // Hello — preserve this special case so harness scripts keep
                // working without a protocol upgrade.
                if let Frame::ActivityUpdate { state } = first_frame {
                    activity.store(state as u8, Ordering::Relaxed);
                    let mut buf = Vec::new();
                    if protocol::write_frame(&mut buf, &Frame::ActivityUpdate { state })
                        .await
                        .is_ok()
                    {
                        if let Ok(clients) = clients_for_register.lock() {
                            for client in clients.values() {
                                let _ = client.tx.try_send(buf.clone());
                            }
                        }
                    }
                    return;
                }

                // --- Streaming client path: Hello → Resume → register → replay → stream ---
                let (client_id, kind, label) = match first_frame {
                    Frame::Hello {
                        client_id,
                        kind,
                        label,
                    } => (client_id, kind, label),
                    other => {
                        tracing::warn!(
                            "expected Hello as first frame, got {:?}; closing connection",
                            std::mem::discriminant(&other)
                        );
                        return;
                    }
                };

                let resume_offset = match protocol::read_frame(&mut reader).await {
                    Ok(Some(Frame::Resume { last_seen_offset })) => last_seen_offset,
                    other => {
                        tracing::warn!(
                            "expected Resume after Hello, got {:?}; closing connection",
                            other.as_ref().map(std::mem::discriminant)
                        );
                        return;
                    }
                };

                let conn_id = next_conn_id.fetch_add(1, Ordering::Relaxed);
                let is_monitor = matches!(kind, ClientKind::MonitorOnly);

                // Register for live updates BEFORE replay so no bytes are
                // lost between the offset snapshot and registration. Live
                // fan-out buffers in the mpsc channel while replay writes
                // directly to the pipe, preserving ordering.
                if let Ok(mut clients) = clients_for_register.lock() {
                    clients.insert(
                        conn_id,
                        Client {
                            conn_id,
                            client_id,
                            kind,
                            label: label.clone(),
                            tx,
                            monitor_only: is_monitor,
                        },
                    );
                }

                // Replay log from resume_offset to current position. Skipped
                // entirely for monitor-only clients (they only care about
                // ActivityUpdate / Status / InputOwnerChanged).
                //
                // The frontend's `attach_session` always sends
                // `last_seen_offset = 0`, so without a cap every reconnect
                // would replay the entire log. Cap the window to
                // MAX_REPLAY_BYTES of recent history.
                if !is_monitor {
                    let live_offset = current_offset.load(Ordering::Relaxed);
                    const REPLAY_CHUNK: usize = 4096;
                    const MAX_REPLAY_BYTES: u64 = 1024 * 1024;
                    let capped_start =
                        std::cmp::max(resume_offset, live_offset.saturating_sub(MAX_REPLAY_BYTES));
                    if capped_start < live_offset {
                        match std::fs::File::open(&*log_for_replay) {
                            Ok(mut file) => {
                                let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
                                let end = std::cmp::min(live_offset, file_len);
                                let start = std::cmp::max(
                                    capped_start,
                                    end.saturating_sub(MAX_REPLAY_BYTES),
                                );
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
                                                    if let Err(e) =
                                                        protocol::write_frame(&mut buf, &frame)
                                                            .await
                                                    {
                                                        tracing::warn!(
                                                            "replay frame serialize failed: {e}"
                                                        );
                                                        break;
                                                    }
                                                    if let Err(e) =
                                                        writer_half.write_all(&buf).await
                                                    {
                                                        tracing::warn!(
                                                            "replay pipe write failed: {e}"
                                                        );
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
                }

                // Send activity state snapshot so late-connecting clients
                // get the current state even if the hook fired before they joined.
                let state_val = activity.load(Ordering::Relaxed);
                if let Ok(snap_state) = ActivityState::try_from(state_val) {
                    let snap = Frame::ActivityUpdate { state: snap_state };
                    let mut buf = Vec::new();
                    if protocol::write_frame(&mut buf, &snap).await.is_ok() {
                        let _ = writer_half.write_all(&buf).await;
                    }
                }

                // Send input-owner snapshot so the new client immediately
                // knows whether someone else owns input. Direct write rather
                // than fan-out: only this client needs the snapshot.
                let owner_snap = Frame::InputOwnerChanged {
                    owner: owner_slot
                        .lock()
                        .ok()
                        .and_then(|g| g.as_ref().map(Owner::summary)),
                };
                let mut buf = Vec::new();
                if protocol::write_frame(&mut buf, &owner_snap).await.is_ok() {
                    let _ = writer_half.write_all(&buf).await;
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
                let owner_for_recv = owner_slot.clone();
                let clients_for_recv = clients_for_register.clone();
                let recv_task = tokio::spawn(async move {
                    // Gate predicate for `TerminalInput` and `Resize`. Stop
                    // frames are intentionally NOT gated — see SHA-43.
                    // `conn_id` is server-assigned and unique even under
                    // `client_id` reuse; comparing on it avoids cross-
                    // connection confusion.
                    let i_own_input = || {
                        matches!(
                            owner_for_recv.lock().ok().and_then(|g| g.as_ref().map(|o| o.conn_id)),
                            Some(id) if id == conn_id
                        )
                    };
                    loop {
                        match protocol::read_frame(&mut reader).await {
                            Ok(Some(Frame::TerminalInput { data })) => {
                                if i_own_input() {
                                    if let Ok(mut w) = writer.lock() {
                                        let _ = std::io::Write::write_all(&mut *w, &data);
                                    }
                                }
                            }
                            Ok(Some(Frame::Resize { rows, cols })) => {
                                if i_own_input() {
                                    let _ = resize.send((rows, cols)).await;
                                }
                            }
                            Ok(Some(Frame::ClaimInput)) => {
                                match apply_claim_input(
                                    &owner_for_recv,
                                    conn_id,
                                    client_id,
                                    kind,
                                    &label,
                                ) {
                                    ClaimResult::Accepted(owner) => {
                                        broadcast_input_owner(&clients_for_recv, Some(owner)).await;
                                    }
                                    ClaimResult::Rejected(owner) => {
                                        send_claim_rejected(&clients_for_recv, conn_id, owner)
                                            .await;
                                    }
                                    ClaimResult::Ignored => {
                                        tracing::debug!(
                                            "monitor-only client {conn_id} sent ClaimInput; ignoring"
                                        );
                                    }
                                }
                            }
                            // Stop frames are intentionally NOT gated on
                            // ownership. Any client (CLI, monitor, daemon
                            // drain path) must be able to terminate the
                            // session — the inverse caused SHA-43.
                            Ok(Some(Frame::StopGraceful)) => {
                                tracing::info!("received stop-graceful from conn {conn_id}");
                                let _ = shutdown.send(Shutdown::Graceful);
                                break;
                            }
                            Ok(Some(Frame::StopForce)) => {
                                tracing::info!("received stop-force from conn {conn_id}");
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

                // When either task ends (EOF on read, or write error after
                // pipe close), abort the other so cleanup can run promptly.
                // The original `join!` pattern would deadlock here on idle
                // sessions: a Tauri-side detach closes the pipe, recv_task
                // sees EOF and ends, but send_task is blocked on
                // `rx.recv().await` and only wakes when the supervisor next
                // tries to send something — which on a quiescent PTY never
                // happens, leaving ownership stuck.
                let mut send_task = send_task;
                let mut recv_task = recv_task;
                tokio::select! {
                    _ = &mut send_task => {
                        recv_task.abort();
                        let _ = recv_task.await;
                    }
                    _ = &mut recv_task => {
                        send_task.abort();
                        let _ = send_task.await;
                    }
                }

                // Cleanup: remove from clients map and release ownership if
                // we held it. Use conn_id (not client_id) for the release
                // check — defends against client_id collision/reuse.
                //
                // The check-and-clear must happen under a single lock hold:
                // a separate read-then-write would let a new client claim
                // ownership between the two, and our cleanup would then
                // wrongly clear *their* fresh ownership.
                if let Ok(mut clients) = clients_for_register.lock() {
                    clients.remove(&conn_id);
                }
                let was_owner = release_owner_if_conn_matches(&owner_slot, conn_id);
                if was_owner {
                    broadcast_input_owner(&clients_for_register, None).await;
                }
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
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), pty_read_task).await;

    // 2. NOW send Status frame — all TerminalOutput has been flushed
    let status_frame = Frame::Status {
        code: if exit_code == 0 {
            0
        } else if exit_code == -1 {
            1
        } else {
            2
        },
    };
    let mut status_buf = Vec::new();
    let _ = protocol::write_frame(&mut status_buf, &status_frame).await;
    if let Ok(clients) = clients.lock() {
        for client in clients.values() {
            let _ = client.tx.try_send(status_buf.clone());
        }
    }

    // 3. Brief delay for delivery, then clean up
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    accept_task.abort();
    resize_task.abort();

    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(conn_id: u64, client_id: u64, kind: ClientKind, label: &str) -> Owner {
        Owner {
            conn_id,
            client_id,
            kind,
            label: label.to_string(),
        }
    }

    fn summary(client_id: u64, kind: ClientKind, label: &str) -> OwnerSummary {
        OwnerSummary {
            client_id,
            kind,
            label: label.to_string(),
        }
    }

    fn client(tx: mpsc::Sender<Vec<u8>>, monitor_only: bool) -> Client {
        Client {
            conn_id: 1,
            client_id: 1,
            kind: if monitor_only {
                ClientKind::MonitorOnly
            } else {
                ClientKind::GuiWindow
            },
            label: "window-1".to_string(),
            tx,
            monitor_only,
        }
    }

    #[test]
    fn gui_claim_rejects_different_gui_owner() {
        let owner_slot = Mutex::new(Some(owner(1, 10, ClientKind::GuiWindow, "window-1")));

        let result = apply_claim_input(&owner_slot, 2, 20, ClientKind::GuiWindow, "window-2");

        assert_eq!(
            result,
            ClaimResult::Rejected(Some(summary(10, ClientKind::GuiWindow, "window-1")))
        );
        assert_eq!(
            owner_slot.lock().unwrap().as_ref().map(Owner::summary),
            Some(summary(10, ClientKind::GuiWindow, "window-1"))
        );
    }

    #[test]
    fn gui_claim_accepts_same_window_reconnect() {
        let owner_slot = Mutex::new(Some(owner(1, 10, ClientKind::GuiWindow, "window-1")));

        let result = apply_claim_input(&owner_slot, 2, 20, ClientKind::GuiWindow, "window-1");

        assert_eq!(
            result,
            ClaimResult::Accepted(summary(20, ClientKind::GuiWindow, "window-1"))
        );
        assert_eq!(
            owner_slot.lock().unwrap().as_ref().map(Owner::summary),
            Some(summary(20, ClientKind::GuiWindow, "window-1"))
        );
    }

    #[test]
    fn cli_claim_can_displace_gui_owner() {
        let owner_slot = Mutex::new(Some(owner(1, 10, ClientKind::GuiWindow, "window-1")));

        let result = apply_claim_input(&owner_slot, 2, 20, ClientKind::CliAgent, "shardctl");

        assert_eq!(
            result,
            ClaimResult::Accepted(summary(20, ClientKind::CliAgent, "shardctl"))
        );
        assert_eq!(
            owner_slot.lock().unwrap().as_ref().map(Owner::summary),
            Some(summary(20, ClientKind::CliAgent, "shardctl"))
        );
    }

    #[test]
    fn gui_claim_rejects_cli_owner() {
        let owner_slot = Mutex::new(Some(owner(1, 10, ClientKind::CliAgent, "shardctl")));

        let result = apply_claim_input(&owner_slot, 2, 20, ClientKind::GuiWindow, "window-1");

        assert_eq!(
            result,
            ClaimResult::Rejected(Some(summary(10, ClientKind::CliAgent, "shardctl")))
        );
    }

    #[test]
    fn owner_release_uses_server_connection_id() {
        let owner_slot = Mutex::new(Some(owner(1, 10, ClientKind::GuiWindow, "window-1")));

        assert!(!release_owner_if_conn_matches(&owner_slot, 2));
        assert!(owner_slot.lock().unwrap().is_some());

        assert!(release_owner_if_conn_matches(&owner_slot, 1));
        assert!(owner_slot.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn input_owner_broadcast_evicts_backlogged_clients() {
        let clients = Mutex::new(HashMap::new());
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(1);
        tx.try_send(vec![0xAA]).unwrap();
        clients.lock().unwrap().insert(1, client(tx, false));

        broadcast_input_owner(
            &clients,
            Some(summary(10, ClientKind::GuiWindow, "window-1")),
        )
        .await;

        assert!(clients.lock().unwrap().is_empty());
        assert_eq!(rx.recv().await, Some(vec![0xAA]));
    }
}
