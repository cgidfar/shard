//! Control protocol for daemon ↔ client communication.
//!
//! Uses the same wire format as the session protocol: `[u32 len][u8 type][payload]`.
//! Type bytes are in the `0x80+` range to avoid collision with session frame types.

use shard_core::state::{RepoState, WorkspaceHealth, WorkspaceStatus};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Current control protocol version. Bumped on breaking wire changes.
pub const PROTOCOL_VERSION: u16 = 2;

/// Well-known named pipe address for the daemon control channel.
#[cfg(windows)]
pub const CONTROL_PIPE_NAME: &str = r"\\.\pipe\shard-control";

/// Control frames exchanged between daemon and clients (CLI/app).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlFrame {
    // --- Handshake ---
    /// Client → Daemon: initiate connection with protocol version.
    Hello { protocol_version: u16 },

    /// Daemon → Client: acknowledge connection, report daemon version.
    HelloAck {
        protocol_version: u16,
        daemon_version: String,
    },

    // --- Health ---
    /// Client → Daemon: health check.
    Ping,

    /// Daemon → Client: health response.
    Pong,

    // --- Session Lifecycle ---
    /// Client → Daemon: spawn a new session supervisor.
    SpawnSession {
        repo: String,
        workspace: String,
        command: Vec<String>,
        harness: Option<String>,
    },

    /// Daemon → Client: session spawned successfully.
    SpawnAck {
        session_id: String,
        supervisor_pid: u32,
        transport_addr: String,
    },

    /// Client → Daemon: stop a session.
    StopSession { session_id: String, force: bool },

    /// Daemon → Client: session stop initiated.
    StopAck,

    /// Client → Daemon: request list of live sessions.
    ListSessions,

    /// Daemon → Client: live session list.
    SessionList { sessions: Vec<LiveSessionInfo> },

    // --- Daemon Lifecycle ---
    /// Client → Daemon: request daemon shutdown.
    Shutdown { graceful: bool },

    /// Daemon → Client: shutdown acknowledged.
    ShutdownAck,

    // --- State Subscription ---
    /// Client → Daemon: switch this connection into long-lived subscribe
    /// mode. The daemon will start pushing `StateSnapshot` frames (one per
    /// repo) and stop accepting request/response frames on this connection.
    /// Sent once, after `Hello`/`HelloAck`. Closing the pipe is the only way
    /// to unsubscribe — there is no explicit Unsubscribe frame.
    Subscribe,

    /// Daemon → Client: full per-repo state snapshot. Carries the complete
    /// WorkspaceStatus set plus session liveness for one repo, tagged with a
    /// monotonic version. Snapshots are idempotent — subscribers may safely
    /// drop or reorder them; the latest version wins.
    StateSnapshot { state: RepoState },

    /// Client → Daemon: one-shot poke telling the daemon that the repo /
    /// workspace topology has changed (add/remove) and it should reload its
    /// in-memory set and adjust watchers. `repo_alias = None` requests a
    /// full reload; `Some(alias)` scopes the reload to one repo.
    ///
    /// Sent by Tauri commands after DB mutations commit. Fire-and-forget:
    /// the daemon does not reply.
    TopologyChanged { repo_alias: Option<String> },

    // --- Errors ---
    /// Daemon → Client: request failed.
    Error { message: String },
}

/// Minimal session info returned by the daemon's live registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSessionInfo {
    pub session_id: String,
    pub supervisor_pid: u32,
    pub transport_addr: String,
    pub repo: String,
    pub workspace: String,
}

// Frame type bytes (0x80+ range to avoid session protocol collision)
const TYPE_HELLO: u8 = 0x80;
const TYPE_HELLO_ACK: u8 = 0x81;
const TYPE_PING: u8 = 0x82;
const TYPE_PONG: u8 = 0x83;
const TYPE_SPAWN_SESSION: u8 = 0x84;
const TYPE_SPAWN_ACK: u8 = 0x85;
const TYPE_STOP_SESSION: u8 = 0x86;
const TYPE_STOP_ACK: u8 = 0x87;
const TYPE_LIST_SESSIONS: u8 = 0x88;
const TYPE_SESSION_LIST: u8 = 0x89;
const TYPE_SHUTDOWN: u8 = 0x8A;
const TYPE_SHUTDOWN_ACK: u8 = 0x8B;
const TYPE_SUBSCRIBE: u8 = 0x8C;
const TYPE_STATE_SNAPSHOT: u8 = 0x8D;
const TYPE_TOPOLOGY_CHANGED: u8 = 0x8E;
const TYPE_ERROR: u8 = 0x8F;

/// Write a control frame to an async writer.
pub async fn write_control_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &ControlFrame,
) -> std::io::Result<()> {
    let mut payload = Vec::new();

    let type_byte = match frame {
        ControlFrame::Hello { protocol_version } => {
            payload.extend_from_slice(&protocol_version.to_be_bytes());
            TYPE_HELLO
        }
        ControlFrame::HelloAck {
            protocol_version,
            daemon_version,
        } => {
            payload.extend_from_slice(&protocol_version.to_be_bytes());
            write_str(&mut payload, daemon_version);
            TYPE_HELLO_ACK
        }
        ControlFrame::Ping => TYPE_PING,
        ControlFrame::Pong => TYPE_PONG,
        ControlFrame::SpawnSession {
            repo,
            workspace,
            command,
            harness,
        } => {
            write_str(&mut payload, repo);
            write_str(&mut payload, workspace);
            payload.extend_from_slice(&(command.len() as u16).to_be_bytes());
            for cmd in command {
                write_str(&mut payload, cmd);
            }
            match harness {
                Some(h) => {
                    payload.push(1);
                    write_str(&mut payload, h);
                }
                None => payload.push(0),
            }
            TYPE_SPAWN_SESSION
        }
        ControlFrame::SpawnAck {
            session_id,
            supervisor_pid,
            transport_addr,
        } => {
            write_str(&mut payload, session_id);
            payload.extend_from_slice(&supervisor_pid.to_be_bytes());
            write_str(&mut payload, transport_addr);
            TYPE_SPAWN_ACK
        }
        ControlFrame::StopSession { session_id, force } => {
            write_str(&mut payload, session_id);
            payload.push(if *force { 1 } else { 0 });
            TYPE_STOP_SESSION
        }
        ControlFrame::StopAck => TYPE_STOP_ACK,
        ControlFrame::ListSessions => TYPE_LIST_SESSIONS,
        ControlFrame::SessionList { sessions } => {
            payload.extend_from_slice(&(sessions.len() as u32).to_be_bytes());
            for s in sessions {
                write_str(&mut payload, &s.session_id);
                payload.extend_from_slice(&s.supervisor_pid.to_be_bytes());
                write_str(&mut payload, &s.transport_addr);
                write_str(&mut payload, &s.repo);
                write_str(&mut payload, &s.workspace);
            }
            TYPE_SESSION_LIST
        }
        ControlFrame::Shutdown { graceful } => {
            payload.push(if *graceful { 1 } else { 0 });
            TYPE_SHUTDOWN
        }
        ControlFrame::ShutdownAck => TYPE_SHUTDOWN_ACK,
        ControlFrame::Subscribe => TYPE_SUBSCRIBE,
        ControlFrame::StateSnapshot { state } => {
            write_str(&mut payload, &state.repo_alias);
            payload.extend_from_slice(&state.version.to_be_bytes());

            // Workspaces — sort for deterministic encoding so roundtrip tests
            // are stable. Order is not semantically meaningful: consumers
            // diff against a map.
            let mut ws: Vec<(&String, &WorkspaceStatus)> = state.workspaces.iter().collect();
            ws.sort_by(|a, b| a.0.cmp(b.0));
            payload.extend_from_slice(&(ws.len() as u32).to_be_bytes());
            for (name, status) in ws {
                write_str(&mut payload, name);
                write_workspace_status(&mut payload, status);
            }

            // Session liveness
            let mut sess: Vec<(&String, &bool)> = state.sessions_alive.iter().collect();
            sess.sort_by(|a, b| a.0.cmp(b.0));
            payload.extend_from_slice(&(sess.len() as u32).to_be_bytes());
            for (session_id, alive) in sess {
                write_str(&mut payload, session_id);
                payload.push(if *alive { 1 } else { 0 });
            }

            TYPE_STATE_SNAPSHOT
        }
        ControlFrame::TopologyChanged { repo_alias } => {
            match repo_alias {
                Some(alias) => {
                    payload.push(1);
                    write_str(&mut payload, alias);
                }
                None => payload.push(0),
            }
            TYPE_TOPOLOGY_CHANGED
        }
        ControlFrame::Error { message } => {
            write_str(&mut payload, message);
            TYPE_ERROR
        }
    };

    let length = 1 + payload.len() as u32;
    writer.write_all(&length.to_be_bytes()).await?;
    writer.write_all(&[type_byte]).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Read a control frame from an async reader.
///
/// Returns None on clean EOF (reader closed).
pub async fn read_control_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<ControlFrame>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let length = u32::from_be_bytes(len_buf) as usize;

    if length == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "control frame length is zero",
        ));
    }

    let mut buf = vec![0u8; length];
    reader.read_exact(&mut buf).await?;

    let type_byte = buf[0];
    let payload = &buf[1..];

    let frame = match type_byte {
        TYPE_HELLO => {
            ensure_len(payload, 2, "Hello")?;
            let protocol_version = u16::from_be_bytes(payload[..2].try_into().unwrap());
            ControlFrame::Hello { protocol_version }
        }
        TYPE_HELLO_ACK => {
            ensure_len(payload, 2, "HelloAck")?;
            let protocol_version = u16::from_be_bytes(payload[..2].try_into().unwrap());
            let (daemon_version, _) = read_str(&payload[2..])?;
            ControlFrame::HelloAck {
                protocol_version,
                daemon_version,
            }
        }
        TYPE_PING => ControlFrame::Ping,
        TYPE_PONG => ControlFrame::Pong,
        TYPE_SPAWN_SESSION => {
            let mut offset = 0;
            let (repo, n) = read_str(&payload[offset..])?;
            offset += n;
            let (workspace, n) = read_str(&payload[offset..])?;
            offset += n;
            ensure_len(&payload[offset..], 2, "SpawnSession cmd_count")?;
            let cmd_count = u16::from_be_bytes(payload[offset..offset + 2].try_into().unwrap());
            offset += 2;
            let mut command = Vec::with_capacity(cmd_count as usize);
            for _ in 0..cmd_count {
                let (cmd, n) = read_str(&payload[offset..])?;
                offset += n;
                command.push(cmd);
            }
            ensure_len(&payload[offset..], 1, "SpawnSession harness flag")?;
            let harness = if payload[offset] == 1 {
                offset += 1;
                let (h, _) = read_str(&payload[offset..])?;
                Some(h)
            } else {
                None
            };
            ControlFrame::SpawnSession {
                repo,
                workspace,
                command,
                harness,
            }
        }
        TYPE_SPAWN_ACK => {
            let mut offset = 0;
            let (session_id, n) = read_str(&payload[offset..])?;
            offset += n;
            ensure_len(&payload[offset..], 4, "SpawnAck pid")?;
            let supervisor_pid =
                u32::from_be_bytes(payload[offset..offset + 4].try_into().unwrap());
            offset += 4;
            let (transport_addr, _) = read_str(&payload[offset..])?;
            ControlFrame::SpawnAck {
                session_id,
                supervisor_pid,
                transport_addr,
            }
        }
        TYPE_STOP_SESSION => {
            let (session_id, n) = read_str(payload)?;
            ensure_len(&payload[n..], 1, "StopSession force flag")?;
            let force = payload[n] == 1;
            ControlFrame::StopSession { session_id, force }
        }
        TYPE_STOP_ACK => ControlFrame::StopAck,
        TYPE_LIST_SESSIONS => ControlFrame::ListSessions,
        TYPE_SESSION_LIST => {
            ensure_len(payload, 4, "SessionList count")?;
            let count = u32::from_be_bytes(payload[..4].try_into().unwrap()) as usize;
            let mut offset = 4;
            let mut sessions = Vec::with_capacity(count);
            for _ in 0..count {
                let (session_id, n) = read_str(&payload[offset..])?;
                offset += n;
                ensure_len(&payload[offset..], 4, "SessionList pid")?;
                let supervisor_pid =
                    u32::from_be_bytes(payload[offset..offset + 4].try_into().unwrap());
                offset += 4;
                let (transport_addr, n) = read_str(&payload[offset..])?;
                offset += n;
                let (repo, n) = read_str(&payload[offset..])?;
                offset += n;
                let (workspace, n) = read_str(&payload[offset..])?;
                offset += n;
                sessions.push(LiveSessionInfo {
                    session_id,
                    supervisor_pid,
                    transport_addr,
                    repo,
                    workspace,
                });
            }
            ControlFrame::SessionList { sessions }
        }
        TYPE_SHUTDOWN => {
            ensure_len(payload, 1, "Shutdown")?;
            let graceful = payload[0] == 1;
            ControlFrame::Shutdown { graceful }
        }
        TYPE_SHUTDOWN_ACK => ControlFrame::ShutdownAck,
        TYPE_SUBSCRIBE => ControlFrame::Subscribe,
        TYPE_STATE_SNAPSHOT => {
            let mut offset = 0;
            let (repo_alias, n) = read_str(&payload[offset..])?;
            offset += n;

            ensure_len(&payload[offset..], 8, "StateSnapshot version")?;
            let version =
                u64::from_be_bytes(payload[offset..offset + 8].try_into().unwrap());
            offset += 8;

            ensure_len(&payload[offset..], 4, "StateSnapshot ws count")?;
            let ws_count =
                u32::from_be_bytes(payload[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;

            let mut workspaces = std::collections::HashMap::with_capacity(ws_count);
            for _ in 0..ws_count {
                let (name, n) = read_str(&payload[offset..])?;
                offset += n;
                let (status, n) = read_workspace_status(&payload[offset..])?;
                offset += n;
                workspaces.insert(name, status);
            }

            ensure_len(&payload[offset..], 4, "StateSnapshot sess count")?;
            let sess_count =
                u32::from_be_bytes(payload[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;

            let mut sessions_alive = std::collections::HashMap::with_capacity(sess_count);
            for _ in 0..sess_count {
                let (session_id, n) = read_str(&payload[offset..])?;
                offset += n;
                ensure_len(&payload[offset..], 1, "StateSnapshot alive byte")?;
                let alive = payload[offset] == 1;
                offset += 1;
                sessions_alive.insert(session_id, alive);
            }

            ControlFrame::StateSnapshot {
                state: RepoState {
                    repo_alias,
                    version,
                    workspaces,
                    sessions_alive,
                },
            }
        }
        TYPE_TOPOLOGY_CHANGED => {
            ensure_len(payload, 1, "TopologyChanged tag")?;
            let repo_alias = if payload[0] == 1 {
                let (alias, _) = read_str(&payload[1..])?;
                Some(alias)
            } else {
                None
            };
            ControlFrame::TopologyChanged { repo_alias }
        }
        TYPE_ERROR => {
            let (message, _) = read_str(payload)?;
            ControlFrame::Error { message }
        }
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown control frame type: 0x{type_byte:02x}"),
            ));
        }
    };

    Ok(Some(frame))
}

// --- WorkspaceStatus wire encoding ---
//
// Layout: [branch_tag u8][branch_str?][sha_tag u8][sha_str?][detached u8][health u8]

const HEALTH_HEALTHY: u8 = 0;
const HEALTH_MISSING: u8 = 1;
const HEALTH_BROKEN: u8 = 2;

fn write_workspace_status(buf: &mut Vec<u8>, status: &WorkspaceStatus) {
    match &status.current_branch {
        Some(b) => {
            buf.push(1);
            write_str(buf, b);
        }
        None => buf.push(0),
    }
    match &status.head_sha {
        Some(s) => {
            buf.push(1);
            write_str(buf, s);
        }
        None => buf.push(0),
    }
    buf.push(if status.detached { 1 } else { 0 });
    buf.push(match status.health {
        WorkspaceHealth::Healthy => HEALTH_HEALTHY,
        WorkspaceHealth::Missing => HEALTH_MISSING,
        WorkspaceHealth::Broken => HEALTH_BROKEN,
    });
}

fn read_workspace_status(buf: &[u8]) -> std::io::Result<(WorkspaceStatus, usize)> {
    let mut offset = 0;
    ensure_len(&buf[offset..], 1, "WorkspaceStatus branch tag")?;
    let branch = if buf[offset] == 1 {
        offset += 1;
        let (s, n) = read_str(&buf[offset..])?;
        offset += n;
        Some(s)
    } else {
        offset += 1;
        None
    };

    ensure_len(&buf[offset..], 1, "WorkspaceStatus sha tag")?;
    let sha = if buf[offset] == 1 {
        offset += 1;
        let (s, n) = read_str(&buf[offset..])?;
        offset += n;
        Some(s)
    } else {
        offset += 1;
        None
    };

    ensure_len(&buf[offset..], 2, "WorkspaceStatus detached + health")?;
    let detached = buf[offset] == 1;
    offset += 1;
    let health = match buf[offset] {
        HEALTH_HEALTHY => WorkspaceHealth::Healthy,
        HEALTH_MISSING => WorkspaceHealth::Missing,
        HEALTH_BROKEN => WorkspaceHealth::Broken,
        other => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown WorkspaceHealth code: {other}"),
            ))
        }
    };
    offset += 1;

    Ok((
        WorkspaceStatus {
            current_branch: branch,
            head_sha: sha,
            detached,
            health,
        },
        offset,
    ))
}

// --- Helpers for length-prefixed string encoding ---

fn write_str(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    buf.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    buf.extend_from_slice(bytes);
}

/// Read a length-prefixed string. Returns (string, bytes_consumed).
fn read_str(buf: &[u8]) -> std::io::Result<(String, usize)> {
    ensure_len(buf, 2, "string length prefix")?;
    let len = u16::from_be_bytes(buf[..2].try_into().unwrap()) as usize;
    ensure_len(&buf[2..], len, "string body")?;
    let s = String::from_utf8(buf[2..2 + len].to_vec()).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("invalid UTF-8: {e}"))
    })?;
    Ok((s, 2 + len))
}

fn ensure_len(buf: &[u8], needed: usize, context: &str) -> std::io::Result<()> {
    if buf.len() < needed {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{context}: need {needed} bytes, have {}", buf.len()),
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn roundtrip(frame: ControlFrame) -> ControlFrame {
        let mut buf = Vec::new();
        write_control_frame(&mut buf, &frame).await.unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        read_control_frame(&mut cursor).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn roundtrip_hello() {
        let frame = ControlFrame::Hello {
            protocol_version: PROTOCOL_VERSION,
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_hello_ack() {
        let frame = ControlFrame::HelloAck {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: "0.1.0".to_string(),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_ping_pong() {
        assert_eq!(roundtrip(ControlFrame::Ping).await, ControlFrame::Ping);
        assert_eq!(roundtrip(ControlFrame::Pong).await, ControlFrame::Pong);
    }

    #[tokio::test]
    async fn roundtrip_spawn_session() {
        let frame = ControlFrame::SpawnSession {
            repo: "my-repo".to_string(),
            workspace: "main".to_string(),
            command: vec!["pwsh".to_string(), "-NoLogo".to_string()],
            harness: Some("claude-code".to_string()),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_spawn_session_no_harness() {
        let frame = ControlFrame::SpawnSession {
            repo: "repo".to_string(),
            workspace: "ws".to_string(),
            command: vec!["cmd.exe".to_string()],
            harness: None,
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_spawn_ack() {
        let frame = ControlFrame::SpawnAck {
            session_id: "019d5a15-abcd-7000-8000-000000000001".to_string(),
            supervisor_pid: 12345,
            transport_addr: r"\\.\pipe\shard-session-019d5a15".to_string(),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_stop_session() {
        let frame = ControlFrame::StopSession {
            session_id: "abc123".to_string(),
            force: true,
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_stop_ack() {
        assert_eq!(roundtrip(ControlFrame::StopAck).await, ControlFrame::StopAck);
    }

    #[tokio::test]
    async fn roundtrip_list_sessions() {
        assert_eq!(
            roundtrip(ControlFrame::ListSessions).await,
            ControlFrame::ListSessions
        );
    }

    #[tokio::test]
    async fn roundtrip_session_list() {
        let frame = ControlFrame::SessionList {
            sessions: vec![
                LiveSessionInfo {
                    session_id: "id-1".to_string(),
                    supervisor_pid: 100,
                    transport_addr: r"\\.\pipe\shard-session-id-1".to_string(),
                    repo: "repo-a".to_string(),
                    workspace: "main".to_string(),
                },
                LiveSessionInfo {
                    session_id: "id-2".to_string(),
                    supervisor_pid: 200,
                    transport_addr: r"\\.\pipe\shard-session-id-2".to_string(),
                    repo: "repo-b".to_string(),
                    workspace: "feature".to_string(),
                },
            ],
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_session_list_empty() {
        let frame = ControlFrame::SessionList { sessions: vec![] };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_shutdown() {
        let frame = ControlFrame::Shutdown { graceful: true };
        assert_eq!(roundtrip(frame.clone()).await, frame);
        let frame = ControlFrame::Shutdown { graceful: false };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_shutdown_ack() {
        assert_eq!(
            roundtrip(ControlFrame::ShutdownAck).await,
            ControlFrame::ShutdownAck
        );
    }

    #[tokio::test]
    async fn roundtrip_error() {
        let frame = ControlFrame::Error {
            message: "something went wrong".to_string(),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn read_eof_returns_none() {
        let mut cursor = std::io::Cursor::new(Vec::new());
        assert!(read_control_frame(&mut cursor).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn roundtrip_subscribe() {
        assert_eq!(
            roundtrip(ControlFrame::Subscribe).await,
            ControlFrame::Subscribe
        );
    }

    #[tokio::test]
    async fn roundtrip_topology_changed() {
        let f1 = ControlFrame::TopologyChanged { repo_alias: None };
        assert_eq!(roundtrip(f1.clone()).await, f1);

        let f2 = ControlFrame::TopologyChanged {
            repo_alias: Some("my-repo".to_string()),
        };
        assert_eq!(roundtrip(f2.clone()).await, f2);
    }

    #[tokio::test]
    async fn roundtrip_state_snapshot_full() {
        let mut state = RepoState::new("my-repo");
        state.version = 42;
        state.workspaces.insert(
            "main".to_string(),
            WorkspaceStatus {
                current_branch: Some("main".to_string()),
                head_sha: Some("abc123def4567890abc123def4567890abc12345".to_string()),
                detached: false,
                health: WorkspaceHealth::Healthy,
            },
        );
        state.workspaces.insert(
            "feature".to_string(),
            WorkspaceStatus {
                current_branch: None,
                head_sha: Some("def4567890abc123def4567890abc123def45678".to_string()),
                detached: true,
                health: WorkspaceHealth::Broken,
            },
        );
        state.workspaces.insert(
            "stale".to_string(),
            WorkspaceStatus {
                current_branch: None,
                head_sha: None,
                detached: false,
                health: WorkspaceHealth::Missing,
            },
        );
        state
            .sessions_alive
            .insert("sess-1".to_string(), true);
        state
            .sessions_alive
            .insert("sess-2".to_string(), false);

        let frame = ControlFrame::StateSnapshot {
            state: state.clone(),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_state_snapshot_empty() {
        let state = RepoState::new("empty-repo");
        let frame = ControlFrame::StateSnapshot { state };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }
}
