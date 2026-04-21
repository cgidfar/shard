//! Control protocol for daemon ↔ client communication.
//!
//! Uses the same wire format as the session protocol: `[u32 len][u8 type][payload]`.
//! Type bytes are in the `0x80+` range to avoid collision with session frame types.

use shard_core::harness::Harness;
use shard_core::repos::Repository;
use shard_core::sessions::Session;
use shard_core::state::{RepoState, WorkspaceHealth, WorkspaceStatus};
use shard_core::workspaces::{BranchInfo, Workspace, WorkspaceMode, WorkspaceWithStatus};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Current control protocol version. Bumped on breaking wire changes.
///
/// v6 adds `RemoveSession`/`RenameSession`/`DetachSession`/`FindSessionById`
/// (type bytes 0xA2–0xA9) for Phase 4 of the daemon-broker migration.
pub const PROTOCOL_VERSION: u16 = 6;

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
    /// WorkspaceStatus set for one repo, tagged with a monotonic version.
    /// Snapshots are idempotent — subscribers may safely drop or reorder
    /// them; the latest version wins.
    StateSnapshot { state: RepoState },

    /// Daemon → Client: one or more sessions in this repo just transitioned
    /// to a terminal status in the DB (the daemon's `fast_tick` detected a
    /// dead supervisor PID and marked the row `exited`). Payload is just the
    /// affected repo alias — the client's job is to re-query the sessions
    /// table for that repo. Fire-and-forget; no reply.
    SessionsChanged { repo: String },

    /// Client → Daemon: one-shot poke telling the daemon that the repo /
    /// workspace topology has changed (add/remove) and it should reload its
    /// in-memory set and adjust watchers. `repo_alias = None` requests a
    /// full reload; `Some(alias)` scopes the reload to one repo.
    ///
    /// Sent by Tauri commands after DB mutations commit. Fire-and-forget:
    /// the daemon does not reply.
    TopologyChanged { repo_alias: Option<String> },

    // --- Workspace Lifecycle (Phase 1 of daemon-broker migration) ---
    /// Client → Daemon: atomically remove a workspace. The daemon stops
    /// any bound sessions (graceful → force), drops watchers, runs
    /// `git worktree remove`, deletes the DB row, and broadcasts
    /// `WorkspaceRemoved`. Replaces the old split path (Tauri backend +
    /// CLI calling `WorkspaceStore::remove` directly then firing a
    /// topology poke) which caused SHA-55.
    RemoveWorkspace { repo: String, name: String },

    /// Daemon → Client: remove succeeded.
    RemoveWorkspaceAck,

    /// Daemon → Subscribers: a workspace has been removed. Emitted once
    /// per successful `RemoveWorkspace`, after all side effects have
    /// landed. Subscribers should drop any cached state for this
    /// `(repo, name)` and refresh their sidebar / tree.
    WorkspaceRemoved { repo: String, name: String },

    // --- Workspace Create + Reads (Phase 2 of daemon-broker migration) ---
    /// Client → Daemon: create a new workspace under `repo`. Always
    /// `is_base=false`; base workspaces are only created during
    /// `AddRepo`. On success the daemon registers the workspace in the
    /// lifecycle map (Active) and fires a topology poke so subscribers
    /// see the new row in the next StateSnapshot.
    CreateWorkspace {
        repo: String,
        name: Option<String>,
        mode: WorkspaceMode,
        branch: Option<String>,
    },

    /// Daemon → Client: create succeeded; returns the persisted row.
    CreateWorkspaceAck { workspace: Workspace },

    /// Client → Daemon: list workspaces for `repo` enriched with the
    /// monitor's live `WorkspaceStatus` snapshot. Routed through the
    /// daemon so readers and the event stream see a consistent view.
    ListWorkspaces { repo: String },

    /// Daemon → Client: enriched workspace list for a repo.
    WorkspaceList { items: Vec<WorkspaceWithStatus> },

    /// Client → Daemon: list branch-occupancy info for `repo`. Git-backed;
    /// routed through the daemon because the monitor already walks the
    /// worktree list on its tick and we want readers to agree with the
    /// monitor's view.
    ListBranchInfo { repo: String },

    /// Daemon → Client: branches + occupancy.
    BranchInfoList { branches: Vec<BranchInfo> },

    // --- Repo Lifecycle (Phase 3 of daemon-broker migration) ---
    /// Client → Daemon: register a new repository by URL or local path.
    /// Remote URLs are bare-cloned into the data dir; local paths are
    /// referenced in place. Auto-creates a base workspace for the
    /// detected default branch.
    AddRepo {
        url: String,
        alias: Option<String>,
    },

    /// Daemon → Client: add succeeded; returns the persisted row.
    AddRepoAck { repo: Repository },

    /// Client → Daemon: tear down a repository. Stops every live
    /// session bound to it, drops the monitor's watcher, removes
    /// worktrees + `.shard/` for local repos (the original checkout is
    /// preserved), or the entire repo directory for remote repos, then
    /// deletes the DB row.
    RemoveRepo { alias: String },

    /// Daemon → Client: remove succeeded.
    RemoveRepoAck,

    /// Client → Daemon: `git fetch --all --prune` against the repo's
    /// source (bare clone or local checkout). No DB mutation.
    SyncRepo { alias: String },

    /// Daemon → Client: sync completed.
    SyncRepoAck,

    /// Client → Daemon: list registered repositories.
    ListRepos,

    /// Daemon → Client: the full repo list, ordered by alias.
    RepoList { repos: Vec<Repository> },

    // --- Session Lifecycle Tail (Phase 4 of daemon-broker migration) ---
    /// Client → Daemon: remove a terminal-status session from the DB and
    /// clean up its session directory. Guards inside the daemon reject
    /// `running` / `starting` sessions with a typed error — the caller is
    /// expected to `StopSession` first.
    RemoveSession { repo: String, id: String },

    /// Daemon → Client: remove succeeded.
    RemoveSessionAck,

    /// Client → Daemon: set or clear a session's label. Pure DB update;
    /// no watcher / supervisor coordination.
    RenameSession {
        repo: String,
        id: String,
        label: Option<String>,
    },

    /// Daemon → Client: rename succeeded.
    RenameSessionAck,

    /// Client → Daemon: validate a session id. The daemon's handler is
    /// effectively a "does this session still exist" probe — the actual
    /// attach/detach connection management stays in the Tauri backend
    /// (per migration non-goals: terminal I/O stays direct). Provides a
    /// symmetric RPC surface so CLI and GUI detach flows run the same
    /// daemon round-trip for telemetry and future multi-window work.
    DetachSession { id: String },

    /// Daemon → Client: detach acknowledged.
    DetachSessionAck,

    /// Client → Daemon: resolve a full-or-prefix session id against the
    /// daemon's global session index. Walks every repo DB and returns
    /// the unique match, or an `Error` frame on zero / ambiguous matches.
    FindSessionById { prefix: String },

    /// Daemon → Client: resolved session, tagged with its repo alias.
    FoundSession { repo: String, session: Session },

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
const TYPE_SESSIONS_CHANGED: u8 = 0x90;
const TYPE_REMOVE_WORKSPACE: u8 = 0x91;
const TYPE_REMOVE_WORKSPACE_ACK: u8 = 0x92;
const TYPE_WORKSPACE_REMOVED: u8 = 0x93;
const TYPE_CREATE_WORKSPACE: u8 = 0x94;
const TYPE_CREATE_WORKSPACE_ACK: u8 = 0x95;
const TYPE_LIST_WORKSPACES: u8 = 0x96;
const TYPE_WORKSPACE_LIST: u8 = 0x97;
const TYPE_LIST_BRANCH_INFO: u8 = 0x98;
const TYPE_BRANCH_INFO_LIST: u8 = 0x99;
const TYPE_ADD_REPO: u8 = 0x9A;
const TYPE_ADD_REPO_ACK: u8 = 0x9B;
const TYPE_REMOVE_REPO: u8 = 0x9C;
const TYPE_REMOVE_REPO_ACK: u8 = 0x9D;
const TYPE_SYNC_REPO: u8 = 0x9E;
const TYPE_SYNC_REPO_ACK: u8 = 0x9F;
const TYPE_LIST_REPOS: u8 = 0xA0;
const TYPE_REPO_LIST: u8 = 0xA1;
const TYPE_REMOVE_SESSION: u8 = 0xA2;
const TYPE_REMOVE_SESSION_ACK: u8 = 0xA3;
const TYPE_RENAME_SESSION: u8 = 0xA4;
const TYPE_RENAME_SESSION_ACK: u8 = 0xA5;
const TYPE_DETACH_SESSION: u8 = 0xA6;
const TYPE_DETACH_SESSION_ACK: u8 = 0xA7;
const TYPE_FIND_SESSION_BY_ID: u8 = 0xA8;
const TYPE_FOUND_SESSION: u8 = 0xA9;

// WorkspaceMode wire tag
const MODE_NEW_BRANCH: u8 = 0;
const MODE_EXISTING_BRANCH: u8 = 1;

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
        ControlFrame::SessionsChanged { repo } => {
            write_str(&mut payload, repo);
            TYPE_SESSIONS_CHANGED
        }
        ControlFrame::RemoveWorkspace { repo, name } => {
            write_str(&mut payload, repo);
            write_str(&mut payload, name);
            TYPE_REMOVE_WORKSPACE
        }
        ControlFrame::RemoveWorkspaceAck => TYPE_REMOVE_WORKSPACE_ACK,
        ControlFrame::WorkspaceRemoved { repo, name } => {
            write_str(&mut payload, repo);
            write_str(&mut payload, name);
            TYPE_WORKSPACE_REMOVED
        }
        ControlFrame::CreateWorkspace {
            repo,
            name,
            mode,
            branch,
        } => {
            write_str(&mut payload, repo);
            write_opt_str(&mut payload, name.as_deref());
            payload.push(mode_to_byte(*mode));
            write_opt_str(&mut payload, branch.as_deref());
            TYPE_CREATE_WORKSPACE
        }
        ControlFrame::CreateWorkspaceAck { workspace } => {
            write_workspace(&mut payload, workspace);
            TYPE_CREATE_WORKSPACE_ACK
        }
        ControlFrame::ListWorkspaces { repo } => {
            write_str(&mut payload, repo);
            TYPE_LIST_WORKSPACES
        }
        ControlFrame::WorkspaceList { items } => {
            payload.extend_from_slice(&(items.len() as u32).to_be_bytes());
            for item in items {
                write_workspace(&mut payload, &item.workspace);
                match &item.status {
                    Some(status) => {
                        payload.push(1);
                        write_workspace_status(&mut payload, status);
                    }
                    None => payload.push(0),
                }
            }
            TYPE_WORKSPACE_LIST
        }
        ControlFrame::ListBranchInfo { repo } => {
            write_str(&mut payload, repo);
            TYPE_LIST_BRANCH_INFO
        }
        ControlFrame::BranchInfoList { branches } => {
            payload.extend_from_slice(&(branches.len() as u32).to_be_bytes());
            for b in branches {
                write_str(&mut payload, &b.name);
                payload.push(if b.is_head { 1 } else { 0 });
                write_opt_str(&mut payload, b.checked_out_by.as_deref());
            }
            TYPE_BRANCH_INFO_LIST
        }
        ControlFrame::AddRepo { url, alias } => {
            write_str(&mut payload, url);
            write_opt_str(&mut payload, alias.as_deref());
            TYPE_ADD_REPO
        }
        ControlFrame::AddRepoAck { repo } => {
            write_repository(&mut payload, repo);
            TYPE_ADD_REPO_ACK
        }
        ControlFrame::RemoveRepo { alias } => {
            write_str(&mut payload, alias);
            TYPE_REMOVE_REPO
        }
        ControlFrame::RemoveRepoAck => TYPE_REMOVE_REPO_ACK,
        ControlFrame::SyncRepo { alias } => {
            write_str(&mut payload, alias);
            TYPE_SYNC_REPO
        }
        ControlFrame::SyncRepoAck => TYPE_SYNC_REPO_ACK,
        ControlFrame::ListRepos => TYPE_LIST_REPOS,
        ControlFrame::RepoList { repos } => {
            payload.extend_from_slice(&(repos.len() as u32).to_be_bytes());
            for r in repos {
                write_repository(&mut payload, r);
            }
            TYPE_REPO_LIST
        }
        ControlFrame::RemoveSession { repo, id } => {
            write_str(&mut payload, repo);
            write_str(&mut payload, id);
            TYPE_REMOVE_SESSION
        }
        ControlFrame::RemoveSessionAck => TYPE_REMOVE_SESSION_ACK,
        ControlFrame::RenameSession { repo, id, label } => {
            write_str(&mut payload, repo);
            write_str(&mut payload, id);
            write_opt_str(&mut payload, label.as_deref());
            TYPE_RENAME_SESSION
        }
        ControlFrame::RenameSessionAck => TYPE_RENAME_SESSION_ACK,
        ControlFrame::DetachSession { id } => {
            write_str(&mut payload, id);
            TYPE_DETACH_SESSION
        }
        ControlFrame::DetachSessionAck => TYPE_DETACH_SESSION_ACK,
        ControlFrame::FindSessionById { prefix } => {
            write_str(&mut payload, prefix);
            TYPE_FIND_SESSION_BY_ID
        }
        ControlFrame::FoundSession { repo, session } => {
            write_str(&mut payload, repo);
            write_session(&mut payload, session);
            TYPE_FOUND_SESSION
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

            ControlFrame::StateSnapshot {
                state: RepoState {
                    repo_alias,
                    version,
                    workspaces,
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
        TYPE_SESSIONS_CHANGED => {
            let (repo, _) = read_str(payload)?;
            ControlFrame::SessionsChanged { repo }
        }
        TYPE_REMOVE_WORKSPACE => {
            let (repo, n) = read_str(payload)?;
            let (name, _) = read_str(&payload[n..])?;
            ControlFrame::RemoveWorkspace { repo, name }
        }
        TYPE_REMOVE_WORKSPACE_ACK => ControlFrame::RemoveWorkspaceAck,
        TYPE_WORKSPACE_REMOVED => {
            let (repo, n) = read_str(payload)?;
            let (name, _) = read_str(&payload[n..])?;
            ControlFrame::WorkspaceRemoved { repo, name }
        }
        TYPE_CREATE_WORKSPACE => {
            let mut offset = 0;
            let (repo, n) = read_str(&payload[offset..])?;
            offset += n;
            let (name, n) = read_opt_str(&payload[offset..])?;
            offset += n;
            ensure_len(&payload[offset..], 1, "CreateWorkspace mode")?;
            let mode = mode_from_byte(payload[offset])?;
            offset += 1;
            let (branch, _) = read_opt_str(&payload[offset..])?;
            ControlFrame::CreateWorkspace {
                repo,
                name,
                mode,
                branch,
            }
        }
        TYPE_CREATE_WORKSPACE_ACK => {
            let (workspace, _) = read_workspace(payload)?;
            ControlFrame::CreateWorkspaceAck { workspace }
        }
        TYPE_LIST_WORKSPACES => {
            let (repo, _) = read_str(payload)?;
            ControlFrame::ListWorkspaces { repo }
        }
        TYPE_WORKSPACE_LIST => {
            ensure_len(payload, 4, "WorkspaceList count")?;
            let count = u32::from_be_bytes(payload[..4].try_into().unwrap()) as usize;
            let mut offset = 4;
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                let (workspace, n) = read_workspace(&payload[offset..])?;
                offset += n;
                ensure_len(&payload[offset..], 1, "WorkspaceList status tag")?;
                let status = if payload[offset] == 1 {
                    offset += 1;
                    let (s, n) = read_workspace_status(&payload[offset..])?;
                    offset += n;
                    Some(s)
                } else {
                    offset += 1;
                    None
                };
                items.push(WorkspaceWithStatus { workspace, status });
            }
            ControlFrame::WorkspaceList { items }
        }
        TYPE_LIST_BRANCH_INFO => {
            let (repo, _) = read_str(payload)?;
            ControlFrame::ListBranchInfo { repo }
        }
        TYPE_BRANCH_INFO_LIST => {
            ensure_len(payload, 4, "BranchInfoList count")?;
            let count = u32::from_be_bytes(payload[..4].try_into().unwrap()) as usize;
            let mut offset = 4;
            let mut branches = Vec::with_capacity(count);
            for _ in 0..count {
                let (name, n) = read_str(&payload[offset..])?;
                offset += n;
                ensure_len(&payload[offset..], 1, "BranchInfo is_head")?;
                let is_head = payload[offset] == 1;
                offset += 1;
                let (checked_out_by, n) = read_opt_str(&payload[offset..])?;
                offset += n;
                branches.push(BranchInfo {
                    name,
                    is_head,
                    checked_out_by,
                });
            }
            ControlFrame::BranchInfoList { branches }
        }
        TYPE_ADD_REPO => {
            let (url, n) = read_str(payload)?;
            let (alias, _) = read_opt_str(&payload[n..])?;
            ControlFrame::AddRepo { url, alias }
        }
        TYPE_ADD_REPO_ACK => {
            let (repo, _) = read_repository(payload)?;
            ControlFrame::AddRepoAck { repo }
        }
        TYPE_REMOVE_REPO => {
            let (alias, _) = read_str(payload)?;
            ControlFrame::RemoveRepo { alias }
        }
        TYPE_REMOVE_REPO_ACK => ControlFrame::RemoveRepoAck,
        TYPE_SYNC_REPO => {
            let (alias, _) = read_str(payload)?;
            ControlFrame::SyncRepo { alias }
        }
        TYPE_SYNC_REPO_ACK => ControlFrame::SyncRepoAck,
        TYPE_LIST_REPOS => ControlFrame::ListRepos,
        TYPE_REPO_LIST => {
            ensure_len(payload, 4, "RepoList count")?;
            let count = u32::from_be_bytes(payload[..4].try_into().unwrap()) as usize;
            let mut offset = 4;
            let mut repos = Vec::with_capacity(count);
            for _ in 0..count {
                let (repo, n) = read_repository(&payload[offset..])?;
                offset += n;
                repos.push(repo);
            }
            ControlFrame::RepoList { repos }
        }
        TYPE_REMOVE_SESSION => {
            let (repo, n) = read_str(payload)?;
            let (id, _) = read_str(&payload[n..])?;
            ControlFrame::RemoveSession { repo, id }
        }
        TYPE_REMOVE_SESSION_ACK => ControlFrame::RemoveSessionAck,
        TYPE_RENAME_SESSION => {
            let mut offset = 0;
            let (repo, n) = read_str(&payload[offset..])?;
            offset += n;
            let (id, n) = read_str(&payload[offset..])?;
            offset += n;
            let (label, _) = read_opt_str(&payload[offset..])?;
            ControlFrame::RenameSession { repo, id, label }
        }
        TYPE_RENAME_SESSION_ACK => ControlFrame::RenameSessionAck,
        TYPE_DETACH_SESSION => {
            let (id, _) = read_str(payload)?;
            ControlFrame::DetachSession { id }
        }
        TYPE_DETACH_SESSION_ACK => ControlFrame::DetachSessionAck,
        TYPE_FIND_SESSION_BY_ID => {
            let (prefix, _) = read_str(payload)?;
            ControlFrame::FindSessionById { prefix }
        }
        TYPE_FOUND_SESSION => {
            let (repo, n) = read_str(payload)?;
            let (session, _) = read_session(&payload[n..])?;
            ControlFrame::FoundSession { repo, session }
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

// --- Workspace + BranchInfo + WorkspaceMode wire encoding ---
//
// Layout for Workspace:
//   [name str][branch str][path str][is_base u8][created_at u64]
//
// Option<String> is encoded with a 1-byte tag (0 = None, 1 = Some) followed by
// a length-prefixed string in the Some case.

fn mode_to_byte(mode: WorkspaceMode) -> u8 {
    match mode {
        WorkspaceMode::NewBranch => MODE_NEW_BRANCH,
        WorkspaceMode::ExistingBranch => MODE_EXISTING_BRANCH,
    }
}

fn mode_from_byte(byte: u8) -> std::io::Result<WorkspaceMode> {
    match byte {
        MODE_NEW_BRANCH => Ok(WorkspaceMode::NewBranch),
        MODE_EXISTING_BRANCH => Ok(WorkspaceMode::ExistingBranch),
        other => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unknown WorkspaceMode byte: {other}"),
        )),
    }
}

fn write_opt_str(buf: &mut Vec<u8>, s: Option<&str>) {
    match s {
        Some(value) => {
            buf.push(1);
            write_str(buf, value);
        }
        None => buf.push(0),
    }
}

fn read_opt_str(buf: &[u8]) -> std::io::Result<(Option<String>, usize)> {
    ensure_len(buf, 1, "Option<String> tag")?;
    if buf[0] == 1 {
        let (s, n) = read_str(&buf[1..])?;
        Ok((Some(s), 1 + n))
    } else {
        Ok((None, 1))
    }
}

fn write_workspace(buf: &mut Vec<u8>, ws: &Workspace) {
    write_str(buf, &ws.name);
    write_str(buf, &ws.branch);
    write_str(buf, &ws.path);
    buf.push(if ws.is_base { 1 } else { 0 });
    buf.extend_from_slice(&ws.created_at.to_be_bytes());
}

fn read_workspace(buf: &[u8]) -> std::io::Result<(Workspace, usize)> {
    let mut offset = 0;
    let (name, n) = read_str(&buf[offset..])?;
    offset += n;
    let (branch, n) = read_str(&buf[offset..])?;
    offset += n;
    let (path, n) = read_str(&buf[offset..])?;
    offset += n;
    ensure_len(&buf[offset..], 9, "Workspace is_base + created_at")?;
    let is_base = buf[offset] == 1;
    offset += 1;
    let created_at = u64::from_be_bytes(buf[offset..offset + 8].try_into().unwrap());
    offset += 8;
    Ok((
        Workspace {
            name,
            branch,
            path,
            is_base,
            created_at,
        },
        offset,
    ))
}

// --- Repository wire encoding ---
//
// Layout:
//   [id str][url str][alias str][host opt_str][owner opt_str]
//   [name opt_str][local_path opt_str][created_at u64]

fn write_repository(buf: &mut Vec<u8>, repo: &Repository) {
    write_str(buf, &repo.id);
    write_str(buf, &repo.url);
    write_str(buf, &repo.alias);
    write_opt_str(buf, repo.host.as_deref());
    write_opt_str(buf, repo.owner.as_deref());
    write_opt_str(buf, repo.name.as_deref());
    write_opt_str(buf, repo.local_path.as_deref());
    buf.extend_from_slice(&repo.created_at.to_be_bytes());
}

fn read_repository(buf: &[u8]) -> std::io::Result<(Repository, usize)> {
    let mut offset = 0;
    let (id, n) = read_str(&buf[offset..])?;
    offset += n;
    let (url, n) = read_str(&buf[offset..])?;
    offset += n;
    let (alias, n) = read_str(&buf[offset..])?;
    offset += n;
    let (host, n) = read_opt_str(&buf[offset..])?;
    offset += n;
    let (owner, n) = read_opt_str(&buf[offset..])?;
    offset += n;
    let (name, n) = read_opt_str(&buf[offset..])?;
    offset += n;
    let (local_path, n) = read_opt_str(&buf[offset..])?;
    offset += n;
    ensure_len(&buf[offset..], 8, "Repository created_at")?;
    let created_at = u64::from_be_bytes(buf[offset..offset + 8].try_into().unwrap());
    offset += 8;
    Ok((
        Repository {
            id,
            url,
            alias,
            host,
            owner,
            name,
            local_path,
            created_at,
        },
        offset,
    ))
}

// --- Session wire encoding ---
//
// Layout (matches the DB schema order in shard-core::sessions::Session):
//   [id str][workspace_name str][command_json str][transport_addr str]
//   [log_path str][supervisor_pid opt_u32][child_pid opt_u32]
//   [status str][exit_code opt_i32][created_at u64][stopped_at opt_u64]
//   [label opt_str][harness opt_str]
//
// `harness` is encoded via its Display impl ("claude-code" / "codex"); the
// receiver parses it back through `FromStr`. Unknown strings become `None`
// on the wire side, matching the DB's tolerance in `row_to_session`.

fn write_opt_u32(buf: &mut Vec<u8>, value: Option<u32>) {
    match value {
        Some(v) => {
            buf.push(1);
            buf.extend_from_slice(&v.to_be_bytes());
        }
        None => buf.push(0),
    }
}

fn read_opt_u32(buf: &[u8]) -> std::io::Result<(Option<u32>, usize)> {
    ensure_len(buf, 1, "Option<u32> tag")?;
    if buf[0] == 1 {
        ensure_len(&buf[1..], 4, "Option<u32> body")?;
        let v = u32::from_be_bytes(buf[1..5].try_into().unwrap());
        Ok((Some(v), 5))
    } else {
        Ok((None, 1))
    }
}

fn write_opt_i32(buf: &mut Vec<u8>, value: Option<i32>) {
    match value {
        Some(v) => {
            buf.push(1);
            buf.extend_from_slice(&v.to_be_bytes());
        }
        None => buf.push(0),
    }
}

fn read_opt_i32(buf: &[u8]) -> std::io::Result<(Option<i32>, usize)> {
    ensure_len(buf, 1, "Option<i32> tag")?;
    if buf[0] == 1 {
        ensure_len(&buf[1..], 4, "Option<i32> body")?;
        let v = i32::from_be_bytes(buf[1..5].try_into().unwrap());
        Ok((Some(v), 5))
    } else {
        Ok((None, 1))
    }
}

fn write_opt_u64(buf: &mut Vec<u8>, value: Option<u64>) {
    match value {
        Some(v) => {
            buf.push(1);
            buf.extend_from_slice(&v.to_be_bytes());
        }
        None => buf.push(0),
    }
}

fn read_opt_u64(buf: &[u8]) -> std::io::Result<(Option<u64>, usize)> {
    ensure_len(buf, 1, "Option<u64> tag")?;
    if buf[0] == 1 {
        ensure_len(&buf[1..], 8, "Option<u64> body")?;
        let v = u64::from_be_bytes(buf[1..9].try_into().unwrap());
        Ok((Some(v), 9))
    } else {
        Ok((None, 1))
    }
}

fn write_session(buf: &mut Vec<u8>, session: &Session) {
    write_str(buf, &session.id);
    write_str(buf, &session.workspace_name);
    write_str(buf, &session.command_json);
    write_str(buf, &session.transport_addr);
    write_str(buf, &session.log_path);
    write_opt_u32(buf, session.supervisor_pid);
    write_opt_u32(buf, session.child_pid);
    write_str(buf, &session.status);
    write_opt_i32(buf, session.exit_code);
    buf.extend_from_slice(&session.created_at.to_be_bytes());
    write_opt_u64(buf, session.stopped_at);
    write_opt_str(buf, session.label.as_deref());
    write_opt_str(buf, session.harness.map(|h| h.to_string()).as_deref());
}

fn read_session(buf: &[u8]) -> std::io::Result<(Session, usize)> {
    let mut offset = 0;
    let (id, n) = read_str(&buf[offset..])?;
    offset += n;
    let (workspace_name, n) = read_str(&buf[offset..])?;
    offset += n;
    let (command_json, n) = read_str(&buf[offset..])?;
    offset += n;
    let (transport_addr, n) = read_str(&buf[offset..])?;
    offset += n;
    let (log_path, n) = read_str(&buf[offset..])?;
    offset += n;
    let (supervisor_pid, n) = read_opt_u32(&buf[offset..])?;
    offset += n;
    let (child_pid, n) = read_opt_u32(&buf[offset..])?;
    offset += n;
    let (status, n) = read_str(&buf[offset..])?;
    offset += n;
    let (exit_code, n) = read_opt_i32(&buf[offset..])?;
    offset += n;
    ensure_len(&buf[offset..], 8, "Session created_at")?;
    let created_at = u64::from_be_bytes(buf[offset..offset + 8].try_into().unwrap());
    offset += 8;
    let (stopped_at, n) = read_opt_u64(&buf[offset..])?;
    offset += n;
    let (label, n) = read_opt_str(&buf[offset..])?;
    offset += n;
    let (harness_str, n) = read_opt_str(&buf[offset..])?;
    offset += n;
    let harness: Option<Harness> = harness_str.and_then(|s| s.parse().ok());
    Ok((
        Session {
            id,
            workspace_name,
            command_json,
            transport_addr,
            log_path,
            supervisor_pid,
            child_pid,
            status,
            exit_code,
            created_at,
            stopped_at,
            label,
            harness,
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
    async fn roundtrip_sessions_changed() {
        let f = ControlFrame::SessionsChanged {
            repo: "my-repo".to_string(),
        };
        assert_eq!(roundtrip(f.clone()).await, f);
    }

    #[tokio::test]
    async fn roundtrip_remove_workspace() {
        let f = ControlFrame::RemoveWorkspace {
            repo: "my-repo".to_string(),
            name: "feature-a".to_string(),
        };
        assert_eq!(roundtrip(f.clone()).await, f);
    }

    #[tokio::test]
    async fn roundtrip_remove_workspace_ack() {
        assert_eq!(
            roundtrip(ControlFrame::RemoveWorkspaceAck).await,
            ControlFrame::RemoveWorkspaceAck
        );
    }

    #[tokio::test]
    async fn roundtrip_workspace_removed() {
        let f = ControlFrame::WorkspaceRemoved {
            repo: "my-repo".to_string(),
            name: "feature-a".to_string(),
        };
        assert_eq!(roundtrip(f.clone()).await, f);
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

    fn sample_workspace(name: &str, is_base: bool) -> Workspace {
        Workspace {
            name: name.to_string(),
            branch: format!("branch-{name}"),
            path: format!(r"C:\tmp\{name}"),
            is_base,
            created_at: 1_700_000_000,
        }
    }

    #[tokio::test]
    async fn roundtrip_create_workspace_full() {
        let frame = ControlFrame::CreateWorkspace {
            repo: "demo".to_string(),
            name: Some("feature".to_string()),
            mode: WorkspaceMode::ExistingBranch,
            branch: Some("main".to_string()),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_create_workspace_minimal() {
        let frame = ControlFrame::CreateWorkspace {
            repo: "demo".to_string(),
            name: None,
            mode: WorkspaceMode::NewBranch,
            branch: None,
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_create_workspace_ack() {
        let frame = ControlFrame::CreateWorkspaceAck {
            workspace: sample_workspace("feature-a", false),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_create_workspace_ack_base() {
        let frame = ControlFrame::CreateWorkspaceAck {
            workspace: sample_workspace("base", true),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_list_workspaces() {
        let frame = ControlFrame::ListWorkspaces {
            repo: "demo".to_string(),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_workspace_list_full() {
        let frame = ControlFrame::WorkspaceList {
            items: vec![
                WorkspaceWithStatus {
                    workspace: sample_workspace("main", true),
                    status: Some(WorkspaceStatus {
                        current_branch: Some("main".to_string()),
                        head_sha: Some("a".repeat(40)),
                        detached: false,
                        health: WorkspaceHealth::Healthy,
                    }),
                },
                WorkspaceWithStatus {
                    workspace: sample_workspace("feature", false),
                    status: None,
                },
            ],
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_workspace_list_empty() {
        let frame = ControlFrame::WorkspaceList { items: vec![] };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_list_branch_info() {
        let frame = ControlFrame::ListBranchInfo {
            repo: "demo".to_string(),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_branch_info_list() {
        let frame = ControlFrame::BranchInfoList {
            branches: vec![
                BranchInfo {
                    name: "main".to_string(),
                    is_head: true,
                    checked_out_by: None,
                },
                BranchInfo {
                    name: "feature".to_string(),
                    is_head: false,
                    checked_out_by: Some("feature-workspace".to_string()),
                },
            ],
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_branch_info_list_empty() {
        let frame = ControlFrame::BranchInfoList { branches: vec![] };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    fn sample_repository(alias: &str, local: bool) -> Repository {
        Repository {
            id: format!("id-{alias}"),
            url: if local {
                format!(r"C:\repos\{alias}")
            } else {
                format!("https://github.com/demo/{alias}.git")
            },
            alias: alias.to_string(),
            host: if local {
                None
            } else {
                Some("github.com".to_string())
            },
            owner: if local { None } else { Some("demo".to_string()) },
            name: Some(alias.to_string()),
            local_path: if local {
                Some(format!(r"C:\repos\{alias}"))
            } else {
                None
            },
            created_at: 1_700_000_123,
        }
    }

    #[tokio::test]
    async fn roundtrip_add_repo_with_alias() {
        let frame = ControlFrame::AddRepo {
            url: "https://github.com/demo/x.git".to_string(),
            alias: Some("demo".to_string()),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_add_repo_no_alias() {
        let frame = ControlFrame::AddRepo {
            url: r"C:\repos\demo".to_string(),
            alias: None,
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_add_repo_ack_remote() {
        let frame = ControlFrame::AddRepoAck {
            repo: sample_repository("demo", false),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_add_repo_ack_local() {
        let frame = ControlFrame::AddRepoAck {
            repo: sample_repository("local-demo", true),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_remove_repo_and_ack() {
        let req = ControlFrame::RemoveRepo {
            alias: "demo".to_string(),
        };
        assert_eq!(roundtrip(req.clone()).await, req);
        assert_eq!(
            roundtrip(ControlFrame::RemoveRepoAck).await,
            ControlFrame::RemoveRepoAck
        );
    }

    #[tokio::test]
    async fn roundtrip_sync_repo_and_ack() {
        let req = ControlFrame::SyncRepo {
            alias: "demo".to_string(),
        };
        assert_eq!(roundtrip(req.clone()).await, req);
        assert_eq!(
            roundtrip(ControlFrame::SyncRepoAck).await,
            ControlFrame::SyncRepoAck
        );
    }

    #[tokio::test]
    async fn roundtrip_list_repos_and_response() {
        assert_eq!(
            roundtrip(ControlFrame::ListRepos).await,
            ControlFrame::ListRepos
        );
        let frame = ControlFrame::RepoList {
            repos: vec![
                sample_repository("alpha", false),
                sample_repository("beta", true),
            ],
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_repo_list_empty() {
        let frame = ControlFrame::RepoList { repos: vec![] };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    fn sample_session(id: &str, status: &str) -> Session {
        Session {
            id: id.to_string(),
            workspace_name: "feature".to_string(),
            command_json: r#"["pwsh","-NoLogo"]"#.to_string(),
            transport_addr: format!(r"\\.\pipe\shard-session-{id}"),
            log_path: format!(r"C:\tmp\{id}\session.log"),
            supervisor_pid: Some(1234),
            child_pid: Some(5678),
            status: status.to_string(),
            exit_code: Some(0),
            created_at: 1_700_000_321,
            stopped_at: Some(1_700_000_999),
            label: Some("my-agent".to_string()),
            harness: Some(Harness::ClaudeCode),
        }
    }

    fn minimal_session(id: &str) -> Session {
        Session {
            id: id.to_string(),
            workspace_name: "main".to_string(),
            command_json: "[]".to_string(),
            transport_addr: String::new(),
            log_path: String::new(),
            supervisor_pid: None,
            child_pid: None,
            status: "starting".to_string(),
            exit_code: None,
            created_at: 1_700_000_100,
            stopped_at: None,
            label: None,
            harness: None,
        }
    }

    #[tokio::test]
    async fn roundtrip_remove_session() {
        let frame = ControlFrame::RemoveSession {
            repo: "demo".to_string(),
            id: "019d5a15-session-01".to_string(),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
        assert_eq!(
            roundtrip(ControlFrame::RemoveSessionAck).await,
            ControlFrame::RemoveSessionAck
        );
    }

    #[tokio::test]
    async fn roundtrip_rename_session() {
        let frame = ControlFrame::RenameSession {
            repo: "demo".to_string(),
            id: "abc".to_string(),
            label: Some("my-agent".to_string()),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);

        let clear = ControlFrame::RenameSession {
            repo: "demo".to_string(),
            id: "abc".to_string(),
            label: None,
        };
        assert_eq!(roundtrip(clear.clone()).await, clear);

        assert_eq!(
            roundtrip(ControlFrame::RenameSessionAck).await,
            ControlFrame::RenameSessionAck
        );
    }

    #[tokio::test]
    async fn roundtrip_detach_session() {
        let frame = ControlFrame::DetachSession {
            id: "019d5a15".to_string(),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
        assert_eq!(
            roundtrip(ControlFrame::DetachSessionAck).await,
            ControlFrame::DetachSessionAck
        );
    }

    #[tokio::test]
    async fn roundtrip_find_session_by_id() {
        let req = ControlFrame::FindSessionById {
            prefix: "019d5a15".to_string(),
        };
        assert_eq!(roundtrip(req.clone()).await, req);
    }

    #[tokio::test]
    async fn roundtrip_found_session_full() {
        let frame = ControlFrame::FoundSession {
            repo: "demo".to_string(),
            session: sample_session("019d5a15-0001", "running"),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_found_session_minimal() {
        let frame = ControlFrame::FoundSession {
            repo: "demo".to_string(),
            session: minimal_session("019d5a15-0002"),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_found_session_unknown_harness_drops() {
        // If a future daemon sends a harness string the client doesn't
        // recognize, we decode it as `None` — matches the DB's tolerance.
        let mut payload = Vec::new();
        // Session fields, matching write_session layout:
        write_str(&mut payload, "id"); // id
        write_str(&mut payload, "ws"); // workspace_name
        write_str(&mut payload, "[]"); // command_json
        write_str(&mut payload, ""); // transport_addr
        write_str(&mut payload, ""); // log_path
        payload.push(0); // supervisor_pid: None
        payload.push(0); // child_pid: None
        write_str(&mut payload, "running"); // status
        payload.push(0); // exit_code: None
        payload.extend_from_slice(&0u64.to_be_bytes()); // created_at
        payload.push(0); // stopped_at: None
        payload.push(0); // label: None
        // harness: Some("unknown-harness") — expected to decode as None
        payload.push(1);
        write_str(&mut payload, "unknown-harness");

        let (session, _) = read_session(&payload).unwrap();
        assert!(
            session.harness.is_none(),
            "unknown harness string should decode as None"
        );
    }

    #[tokio::test]
    async fn mode_byte_rejects_unknown() {
        let mut bytes = Vec::new();
        write_str(&mut bytes, "demo");
        bytes.push(0); // name tag: None
        bytes.push(0x7f); // invalid mode
        bytes.push(0); // branch tag: None

        let mut framed = Vec::new();
        framed.extend_from_slice(&((1 + bytes.len()) as u32).to_be_bytes());
        framed.push(TYPE_CREATE_WORKSPACE);
        framed.extend_from_slice(&bytes);

        let mut cursor = std::io::Cursor::new(framed);
        let err = read_control_frame(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
