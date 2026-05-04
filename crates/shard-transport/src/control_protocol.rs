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
/// v6 adds `RemoveSession`/`RenameSession`/`FindSessionById`
/// (type bytes 0xA2–0xA5 and 0xA8–0xA9) for Phase 4 of the
/// daemon-broker migration.
/// v7 adds `InstallHarnessHooks` (type bytes 0xAA–0xAB) for Phase 5.
/// v8 removes the unused `DetachSession` control frames; type bytes
/// 0xA6–0xA7 remain reserved.
/// v9 adds `AdoptWorkspace` (type bytes 0xAC–0xAD), appends `is_external`
/// to the `Workspace` wire layout, and adds `external_path` to
/// `BranchInfo` (SHA-50: track external worktrees).
pub const PROTOCOL_VERSION: u16 = 9;

/// Maximum accepted daemon control frame size, including the type byte.
pub const MAX_CONTROL_FRAME_LEN: usize = 8 * 1024 * 1024;

const MAX_CONTROL_COLLECTION_COUNT: usize = 16_384;
const MAX_SPAWN_COMMAND_COUNT: usize = 1_024;

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
    AddRepo { url: String, alias: Option<String> },

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

    /// Client → Daemon: resolve a full-or-prefix session id against the
    /// daemon's global session index. Walks every repo DB and returns
    /// the unique match, or an `Error` frame on zero / ambiguous matches.
    FindSessionById { prefix: String },

    /// Daemon → Client: resolved session, tagged with its repo alias.
    FoundSession { repo: String, session: Session },

    // --- Workspace Adopt (SHA-50) ---
    /// Client → Daemon: adopt a pre-existing external git worktree into Shard
    /// tracking. `path` must be a directory that is already a registered,
    /// non-prunable worktree of the repo. The daemon writes a row with
    /// `is_external=1` so subsequent removes only untrack — they never delete
    /// the directory or git admin entry. Local repos only in v1.
    AdoptWorkspace {
        repo: String,
        path: String,
        name: Option<String>,
    },

    /// Daemon → Client: adopt succeeded; returns the persisted row.
    AdoptWorkspaceAck { workspace: Workspace },

    // --- Harness Hooks (Phase 5 of daemon-broker migration) ---
    /// Client → Daemon: install hook integration for `harness` (currently
    /// `"claude-code"` or `"codex"`). Centralizes today's best-effort
    /// per-session install so concurrent installs serialize against a
    /// single global mutex inside the daemon. The `installed` bool in
    /// the ack is a **postcondition** — `true` means hooks are in place
    /// after this call, not that bytes were written.
    InstallHarnessHooks { harness: String },

    /// Daemon → Client: hooks-install outcome. See the ack matrix in
    /// `docs/daemon-broker-migration.md` Phase 5 section.
    InstallHarnessHooksAck {
        installed: bool,
        skipped_reason: Option<String>,
    },

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
// 0xA6 and 0xA7 were the removed DetachSession request/ack frames.
const TYPE_FIND_SESSION_BY_ID: u8 = 0xA8;
const TYPE_FOUND_SESSION: u8 = 0xA9;
const TYPE_INSTALL_HARNESS_HOOKS: u8 = 0xAA;
const TYPE_INSTALL_HARNESS_HOOKS_ACK: u8 = 0xAB;
const TYPE_ADOPT_WORKSPACE: u8 = 0xAC;
const TYPE_ADOPT_WORKSPACE_ACK: u8 = 0xAD;

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
            write_str(&mut payload, daemon_version)?;
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
            write_str(&mut payload, repo)?;
            write_str(&mut payload, workspace)?;
            write_count_u16(
                &mut payload,
                "SpawnSession command",
                command.len(),
                MAX_SPAWN_COMMAND_COUNT,
            )?;
            for cmd in command {
                write_str(&mut payload, cmd)?;
            }
            match harness {
                Some(h) => {
                    payload.push(1);
                    write_str(&mut payload, h)?;
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
            write_str(&mut payload, session_id)?;
            payload.extend_from_slice(&supervisor_pid.to_be_bytes());
            write_str(&mut payload, transport_addr)?;
            TYPE_SPAWN_ACK
        }
        ControlFrame::StopSession { session_id, force } => {
            write_str(&mut payload, session_id)?;
            payload.push(if *force { 1 } else { 0 });
            TYPE_STOP_SESSION
        }
        ControlFrame::StopAck => TYPE_STOP_ACK,
        ControlFrame::ListSessions => TYPE_LIST_SESSIONS,
        ControlFrame::SessionList { sessions } => {
            write_count_u32(&mut payload, "SessionList sessions", sessions.len())?;
            for s in sessions {
                write_str(&mut payload, &s.session_id)?;
                payload.extend_from_slice(&s.supervisor_pid.to_be_bytes());
                write_str(&mut payload, &s.transport_addr)?;
                write_str(&mut payload, &s.repo)?;
                write_str(&mut payload, &s.workspace)?;
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
            write_str(&mut payload, &state.repo_alias)?;
            payload.extend_from_slice(&state.version.to_be_bytes());

            // Workspaces — sort for deterministic encoding so roundtrip tests
            // are stable. Order is not semantically meaningful: consumers
            // diff against a map.
            let mut ws: Vec<(&String, &WorkspaceStatus)> = state.workspaces.iter().collect();
            ws.sort_by(|a, b| a.0.cmp(b.0));
            write_count_u32(&mut payload, "StateSnapshot workspaces", ws.len())?;
            for (name, status) in ws {
                write_str(&mut payload, name)?;
                write_workspace_status(&mut payload, status)?;
            }

            TYPE_STATE_SNAPSHOT
        }
        ControlFrame::TopologyChanged { repo_alias } => {
            match repo_alias {
                Some(alias) => {
                    payload.push(1);
                    write_str(&mut payload, alias)?;
                }
                None => payload.push(0),
            }
            TYPE_TOPOLOGY_CHANGED
        }
        ControlFrame::SessionsChanged { repo } => {
            write_str(&mut payload, repo)?;
            TYPE_SESSIONS_CHANGED
        }
        ControlFrame::RemoveWorkspace { repo, name } => {
            write_str(&mut payload, repo)?;
            write_str(&mut payload, name)?;
            TYPE_REMOVE_WORKSPACE
        }
        ControlFrame::RemoveWorkspaceAck => TYPE_REMOVE_WORKSPACE_ACK,
        ControlFrame::WorkspaceRemoved { repo, name } => {
            write_str(&mut payload, repo)?;
            write_str(&mut payload, name)?;
            TYPE_WORKSPACE_REMOVED
        }
        ControlFrame::CreateWorkspace {
            repo,
            name,
            mode,
            branch,
        } => {
            write_str(&mut payload, repo)?;
            write_opt_str(&mut payload, name.as_deref())?;
            payload.push(mode_to_byte(*mode));
            write_opt_str(&mut payload, branch.as_deref())?;
            TYPE_CREATE_WORKSPACE
        }
        ControlFrame::CreateWorkspaceAck { workspace } => {
            write_workspace(&mut payload, workspace)?;
            TYPE_CREATE_WORKSPACE_ACK
        }
        ControlFrame::ListWorkspaces { repo } => {
            write_str(&mut payload, repo)?;
            TYPE_LIST_WORKSPACES
        }
        ControlFrame::WorkspaceList { items } => {
            write_count_u32(&mut payload, "WorkspaceList items", items.len())?;
            for item in items {
                write_workspace(&mut payload, &item.workspace)?;
                match &item.status {
                    Some(status) => {
                        payload.push(1);
                        write_workspace_status(&mut payload, status)?;
                    }
                    None => payload.push(0),
                }
            }
            TYPE_WORKSPACE_LIST
        }
        ControlFrame::ListBranchInfo { repo } => {
            write_str(&mut payload, repo)?;
            TYPE_LIST_BRANCH_INFO
        }
        ControlFrame::BranchInfoList { branches } => {
            write_count_u32(&mut payload, "BranchInfoList branches", branches.len())?;
            for b in branches {
                write_str(&mut payload, &b.name)?;
                payload.push(if b.is_head { 1 } else { 0 });
                write_opt_str(&mut payload, b.checked_out_by.as_deref())?;
                write_opt_str(&mut payload, b.external_path.as_deref())?;
            }
            TYPE_BRANCH_INFO_LIST
        }
        ControlFrame::AddRepo { url, alias } => {
            write_str(&mut payload, url)?;
            write_opt_str(&mut payload, alias.as_deref())?;
            TYPE_ADD_REPO
        }
        ControlFrame::AddRepoAck { repo } => {
            write_repository(&mut payload, repo)?;
            TYPE_ADD_REPO_ACK
        }
        ControlFrame::RemoveRepo { alias } => {
            write_str(&mut payload, alias)?;
            TYPE_REMOVE_REPO
        }
        ControlFrame::RemoveRepoAck => TYPE_REMOVE_REPO_ACK,
        ControlFrame::SyncRepo { alias } => {
            write_str(&mut payload, alias)?;
            TYPE_SYNC_REPO
        }
        ControlFrame::SyncRepoAck => TYPE_SYNC_REPO_ACK,
        ControlFrame::ListRepos => TYPE_LIST_REPOS,
        ControlFrame::RepoList { repos } => {
            write_count_u32(&mut payload, "RepoList repos", repos.len())?;
            for r in repos {
                write_repository(&mut payload, r)?;
            }
            TYPE_REPO_LIST
        }
        ControlFrame::RemoveSession { repo, id } => {
            write_str(&mut payload, repo)?;
            write_str(&mut payload, id)?;
            TYPE_REMOVE_SESSION
        }
        ControlFrame::RemoveSessionAck => TYPE_REMOVE_SESSION_ACK,
        ControlFrame::RenameSession { repo, id, label } => {
            write_str(&mut payload, repo)?;
            write_str(&mut payload, id)?;
            write_opt_str(&mut payload, label.as_deref())?;
            TYPE_RENAME_SESSION
        }
        ControlFrame::RenameSessionAck => TYPE_RENAME_SESSION_ACK,
        ControlFrame::FindSessionById { prefix } => {
            write_str(&mut payload, prefix)?;
            TYPE_FIND_SESSION_BY_ID
        }
        ControlFrame::FoundSession { repo, session } => {
            write_str(&mut payload, repo)?;
            write_session(&mut payload, session)?;
            TYPE_FOUND_SESSION
        }
        ControlFrame::InstallHarnessHooks { harness } => {
            write_str(&mut payload, harness)?;
            TYPE_INSTALL_HARNESS_HOOKS
        }
        ControlFrame::InstallHarnessHooksAck {
            installed,
            skipped_reason,
        } => {
            payload.push(if *installed { 1 } else { 0 });
            write_opt_str(&mut payload, skipped_reason.as_deref())?;
            TYPE_INSTALL_HARNESS_HOOKS_ACK
        }
        ControlFrame::AdoptWorkspace { repo, path, name } => {
            write_str(&mut payload, repo)?;
            write_str(&mut payload, path)?;
            write_opt_str(&mut payload, name.as_deref())?;
            TYPE_ADOPT_WORKSPACE
        }
        ControlFrame::AdoptWorkspaceAck { workspace } => {
            write_workspace(&mut payload, workspace)?;
            TYPE_ADOPT_WORKSPACE_ACK
        }
        ControlFrame::Error { message } => {
            write_str(&mut payload, message)?;
            TYPE_ERROR
        }
    };

    let length = u32::try_from(1 + payload.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("control frame payload too large: {} bytes", payload.len()),
        )
    })?;
    if (length as usize) > MAX_CONTROL_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("control frame exceeds MAX_CONTROL_FRAME_LEN: {length}"),
        ));
    }
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
    let Some(length) = read_frame_len(reader, MAX_CONTROL_FRAME_LEN, "control").await? else {
        return Ok(None);
    };

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

    let (frame, consumed) = match type_byte {
        TYPE_HELLO => {
            ensure_len(payload, 2, "Hello")?;
            let protocol_version = u16::from_be_bytes(payload[..2].try_into().unwrap());
            (ControlFrame::Hello { protocol_version }, 2)
        }
        TYPE_HELLO_ACK => {
            let mut offset = 0;
            ensure_len(payload, 2, "HelloAck")?;
            let protocol_version = u16::from_be_bytes(payload[..2].try_into().unwrap());
            offset += 2;
            let (daemon_version, n) = read_str(&payload[offset..])?;
            offset += n;
            (
                ControlFrame::HelloAck {
                    protocol_version,
                    daemon_version,
                },
                offset,
            )
        }
        TYPE_PING => (ControlFrame::Ping, 0),
        TYPE_PONG => (ControlFrame::Pong, 0),
        TYPE_SPAWN_SESSION => {
            let mut offset = 0;
            let (repo, n) = read_str(&payload[offset..])?;
            offset += n;
            let (workspace, n) = read_str(&payload[offset..])?;
            offset += n;
            ensure_len(&payload[offset..], 2, "SpawnSession cmd_count")?;
            let cmd_count = bounded_count(
                u16::from_be_bytes(payload[offset..offset + 2].try_into().unwrap()) as usize,
                MAX_SPAWN_COMMAND_COUNT,
                "SpawnSession command count",
            )?;
            offset += 2;
            let mut command = Vec::with_capacity(cmd_count);
            for _ in 0..cmd_count {
                let (cmd, n) = read_str(&payload[offset..])?;
                offset += n;
                command.push(cmd);
            }
            let (harness, n) = read_opt_str(&payload[offset..])?;
            offset += n;
            (
                ControlFrame::SpawnSession {
                    repo,
                    workspace,
                    command,
                    harness,
                },
                offset,
            )
        }
        TYPE_SPAWN_ACK => {
            let mut offset = 0;
            let (session_id, n) = read_str(&payload[offset..])?;
            offset += n;
            ensure_len(&payload[offset..], 4, "SpawnAck pid")?;
            let supervisor_pid =
                u32::from_be_bytes(payload[offset..offset + 4].try_into().unwrap());
            offset += 4;
            let (transport_addr, n) = read_str(&payload[offset..])?;
            offset += n;
            (
                ControlFrame::SpawnAck {
                    session_id,
                    supervisor_pid,
                    transport_addr,
                },
                offset,
            )
        }
        TYPE_STOP_SESSION => {
            let (session_id, n) = read_str(payload)?;
            ensure_len(&payload[n..], 1, "StopSession force flag")?;
            let force = read_bool_flag(payload[n], "StopSession force flag")?;
            (ControlFrame::StopSession { session_id, force }, n + 1)
        }
        TYPE_STOP_ACK => (ControlFrame::StopAck, 0),
        TYPE_LIST_SESSIONS => (ControlFrame::ListSessions, 0),
        TYPE_SESSION_LIST => {
            ensure_len(payload, 4, "SessionList count")?;
            let count = read_u32_count(&payload[..4], "SessionList count")?;
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
            (ControlFrame::SessionList { sessions }, offset)
        }
        TYPE_SHUTDOWN => {
            ensure_len(payload, 1, "Shutdown")?;
            let graceful = read_bool_flag(payload[0], "Shutdown graceful flag")?;
            (ControlFrame::Shutdown { graceful }, 1)
        }
        TYPE_SHUTDOWN_ACK => (ControlFrame::ShutdownAck, 0),
        TYPE_SUBSCRIBE => (ControlFrame::Subscribe, 0),
        TYPE_STATE_SNAPSHOT => {
            let mut offset = 0;
            let (repo_alias, n) = read_str(&payload[offset..])?;
            offset += n;

            ensure_len(&payload[offset..], 8, "StateSnapshot version")?;
            let version = u64::from_be_bytes(payload[offset..offset + 8].try_into().unwrap());
            offset += 8;

            ensure_len(&payload[offset..], 4, "StateSnapshot ws count")?;
            let ws_count = read_u32_count(&payload[offset..offset + 4], "StateSnapshot ws count")?;
            offset += 4;

            let mut workspaces = std::collections::HashMap::with_capacity(ws_count);
            for _ in 0..ws_count {
                let (name, n) = read_str(&payload[offset..])?;
                offset += n;
                let (status, n) = read_workspace_status(&payload[offset..])?;
                offset += n;
                workspaces.insert(name, status);
            }

            (
                ControlFrame::StateSnapshot {
                    state: RepoState {
                        repo_alias,
                        version,
                        workspaces,
                    },
                },
                offset,
            )
        }
        TYPE_TOPOLOGY_CHANGED => {
            let (repo_alias, n) = read_opt_str(payload)?;
            (ControlFrame::TopologyChanged { repo_alias }, n)
        }
        TYPE_SESSIONS_CHANGED => {
            let (repo, n) = read_str(payload)?;
            (ControlFrame::SessionsChanged { repo }, n)
        }
        TYPE_REMOVE_WORKSPACE => {
            let (repo, n) = read_str(payload)?;
            let (name, m) = read_str(&payload[n..])?;
            (ControlFrame::RemoveWorkspace { repo, name }, n + m)
        }
        TYPE_REMOVE_WORKSPACE_ACK => (ControlFrame::RemoveWorkspaceAck, 0),
        TYPE_WORKSPACE_REMOVED => {
            let (repo, n) = read_str(payload)?;
            let (name, m) = read_str(&payload[n..])?;
            (ControlFrame::WorkspaceRemoved { repo, name }, n + m)
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
            let (branch, n) = read_opt_str(&payload[offset..])?;
            offset += n;
            (
                ControlFrame::CreateWorkspace {
                    repo,
                    name,
                    mode,
                    branch,
                },
                offset,
            )
        }
        TYPE_CREATE_WORKSPACE_ACK => {
            let (workspace, n) = read_workspace(payload)?;
            (ControlFrame::CreateWorkspaceAck { workspace }, n)
        }
        TYPE_LIST_WORKSPACES => {
            let (repo, n) = read_str(payload)?;
            (ControlFrame::ListWorkspaces { repo }, n)
        }
        TYPE_WORKSPACE_LIST => {
            ensure_len(payload, 4, "WorkspaceList count")?;
            let count = read_u32_count(&payload[..4], "WorkspaceList count")?;
            let mut offset = 4;
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                let (workspace, n) = read_workspace(&payload[offset..])?;
                offset += n;
                ensure_len(&payload[offset..], 1, "WorkspaceList status tag")?;
                let status = if read_option_tag(payload[offset], "WorkspaceList status tag")? {
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
            (ControlFrame::WorkspaceList { items }, offset)
        }
        TYPE_LIST_BRANCH_INFO => {
            let (repo, n) = read_str(payload)?;
            (ControlFrame::ListBranchInfo { repo }, n)
        }
        TYPE_BRANCH_INFO_LIST => {
            ensure_len(payload, 4, "BranchInfoList count")?;
            let count = read_u32_count(&payload[..4], "BranchInfoList count")?;
            let mut offset = 4;
            let mut branches = Vec::with_capacity(count);
            for _ in 0..count {
                let (name, n) = read_str(&payload[offset..])?;
                offset += n;
                ensure_len(&payload[offset..], 1, "BranchInfo is_head")?;
                let is_head = read_bool_flag(payload[offset], "BranchInfo is_head")?;
                offset += 1;
                let (checked_out_by, n) = read_opt_str(&payload[offset..])?;
                offset += n;
                let (external_path, n) = read_opt_str(&payload[offset..])?;
                offset += n;
                branches.push(BranchInfo {
                    name,
                    is_head,
                    checked_out_by,
                    external_path,
                });
            }
            (ControlFrame::BranchInfoList { branches }, offset)
        }
        TYPE_ADD_REPO => {
            let (url, n) = read_str(payload)?;
            let (alias, m) = read_opt_str(&payload[n..])?;
            (ControlFrame::AddRepo { url, alias }, n + m)
        }
        TYPE_ADD_REPO_ACK => {
            let (repo, n) = read_repository(payload)?;
            (ControlFrame::AddRepoAck { repo }, n)
        }
        TYPE_REMOVE_REPO => {
            let (alias, n) = read_str(payload)?;
            (ControlFrame::RemoveRepo { alias }, n)
        }
        TYPE_REMOVE_REPO_ACK => (ControlFrame::RemoveRepoAck, 0),
        TYPE_SYNC_REPO => {
            let (alias, n) = read_str(payload)?;
            (ControlFrame::SyncRepo { alias }, n)
        }
        TYPE_SYNC_REPO_ACK => (ControlFrame::SyncRepoAck, 0),
        TYPE_LIST_REPOS => (ControlFrame::ListRepos, 0),
        TYPE_REPO_LIST => {
            ensure_len(payload, 4, "RepoList count")?;
            let count = read_u32_count(&payload[..4], "RepoList count")?;
            let mut offset = 4;
            let mut repos = Vec::with_capacity(count);
            for _ in 0..count {
                let (repo, n) = read_repository(&payload[offset..])?;
                offset += n;
                repos.push(repo);
            }
            (ControlFrame::RepoList { repos }, offset)
        }
        TYPE_REMOVE_SESSION => {
            let (repo, n) = read_str(payload)?;
            let (id, m) = read_str(&payload[n..])?;
            (ControlFrame::RemoveSession { repo, id }, n + m)
        }
        TYPE_REMOVE_SESSION_ACK => (ControlFrame::RemoveSessionAck, 0),
        TYPE_RENAME_SESSION => {
            let mut offset = 0;
            let (repo, n) = read_str(&payload[offset..])?;
            offset += n;
            let (id, n) = read_str(&payload[offset..])?;
            offset += n;
            let (label, n) = read_opt_str(&payload[offset..])?;
            offset += n;
            (ControlFrame::RenameSession { repo, id, label }, offset)
        }
        TYPE_RENAME_SESSION_ACK => (ControlFrame::RenameSessionAck, 0),
        TYPE_FIND_SESSION_BY_ID => {
            let (prefix, n) = read_str(payload)?;
            (ControlFrame::FindSessionById { prefix }, n)
        }
        TYPE_FOUND_SESSION => {
            let (repo, n) = read_str(payload)?;
            let (session, m) = read_session(&payload[n..])?;
            (ControlFrame::FoundSession { repo, session }, n + m)
        }
        TYPE_INSTALL_HARNESS_HOOKS => {
            let (harness, n) = read_str(payload)?;
            (ControlFrame::InstallHarnessHooks { harness }, n)
        }
        TYPE_INSTALL_HARNESS_HOOKS_ACK => {
            ensure_len(payload, 1, "InstallHarnessHooksAck installed")?;
            let installed = read_bool_flag(payload[0], "InstallHarnessHooksAck installed")?;
            let (skipped_reason, n) = read_opt_str(&payload[1..])?;
            (
                ControlFrame::InstallHarnessHooksAck {
                    installed,
                    skipped_reason,
                },
                1 + n,
            )
        }
        TYPE_ADOPT_WORKSPACE => {
            let mut offset = 0;
            let (repo, n) = read_str(&payload[offset..])?;
            offset += n;
            let (path, n) = read_str(&payload[offset..])?;
            offset += n;
            let (name, n) = read_opt_str(&payload[offset..])?;
            offset += n;
            (ControlFrame::AdoptWorkspace { repo, path, name }, offset)
        }
        TYPE_ADOPT_WORKSPACE_ACK => {
            let (workspace, n) = read_workspace(payload)?;
            (ControlFrame::AdoptWorkspaceAck { workspace }, n)
        }
        TYPE_ERROR => {
            let (message, n) = read_str(payload)?;
            (ControlFrame::Error { message }, n)
        }
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown control frame type: 0x{type_byte:02x}"),
            ));
        }
    };

    ensure_consumed(payload, consumed, "control frame payload")?;

    Ok(Some(frame))
}

// --- WorkspaceStatus wire encoding ---
//
// Layout: [branch_tag u8][branch_str?][sha_tag u8][sha_str?][detached u8][health u8]

const HEALTH_HEALTHY: u8 = 0;
const HEALTH_MISSING: u8 = 1;
const HEALTH_BROKEN: u8 = 2;

fn write_workspace_status(buf: &mut Vec<u8>, status: &WorkspaceStatus) -> std::io::Result<()> {
    match &status.current_branch {
        Some(b) => {
            buf.push(1);
            write_str(buf, b)?;
        }
        None => buf.push(0),
    }
    match &status.head_sha {
        Some(s) => {
            buf.push(1);
            write_str(buf, s)?;
        }
        None => buf.push(0),
    }
    buf.push(if status.detached { 1 } else { 0 });
    buf.push(match status.health {
        WorkspaceHealth::Healthy => HEALTH_HEALTHY,
        WorkspaceHealth::Missing => HEALTH_MISSING,
        WorkspaceHealth::Broken => HEALTH_BROKEN,
    });
    Ok(())
}

fn read_workspace_status(buf: &[u8]) -> std::io::Result<(WorkspaceStatus, usize)> {
    let mut offset = 0;
    ensure_len(&buf[offset..], 1, "WorkspaceStatus branch tag")?;
    let branch = if read_option_tag(buf[offset], "WorkspaceStatus branch tag")? {
        offset += 1;
        let (s, n) = read_str(&buf[offset..])?;
        offset += n;
        Some(s)
    } else {
        offset += 1;
        None
    };

    ensure_len(&buf[offset..], 1, "WorkspaceStatus sha tag")?;
    let sha = if read_option_tag(buf[offset], "WorkspaceStatus sha tag")? {
        offset += 1;
        let (s, n) = read_str(&buf[offset..])?;
        offset += n;
        Some(s)
    } else {
        offset += 1;
        None
    };

    ensure_len(&buf[offset..], 2, "WorkspaceStatus detached + health")?;
    let detached = read_bool_flag(buf[offset], "WorkspaceStatus detached")?;
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
//   [name str][branch str][path str][is_base u8][is_external u8][created_at u64]
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

fn write_opt_str(buf: &mut Vec<u8>, s: Option<&str>) -> std::io::Result<()> {
    match s {
        Some(value) => {
            buf.push(1);
            write_str(buf, value)?;
        }
        None => buf.push(0),
    }
    Ok(())
}

fn read_opt_str(buf: &[u8]) -> std::io::Result<(Option<String>, usize)> {
    ensure_len(buf, 1, "Option<String> tag")?;
    if read_option_tag(buf[0], "Option<String> tag")? {
        let (s, n) = read_str(&buf[1..])?;
        Ok((Some(s), 1 + n))
    } else {
        Ok((None, 1))
    }
}

fn write_workspace(buf: &mut Vec<u8>, ws: &Workspace) -> std::io::Result<()> {
    write_str(buf, &ws.name)?;
    write_str(buf, &ws.branch)?;
    write_str(buf, &ws.path)?;
    buf.push(if ws.is_base { 1 } else { 0 });
    buf.push(if ws.is_external { 1 } else { 0 });
    buf.extend_from_slice(&ws.created_at.to_be_bytes());
    Ok(())
}

fn read_workspace(buf: &[u8]) -> std::io::Result<(Workspace, usize)> {
    let mut offset = 0;
    let (name, n) = read_str(&buf[offset..])?;
    offset += n;
    let (branch, n) = read_str(&buf[offset..])?;
    offset += n;
    let (path, n) = read_str(&buf[offset..])?;
    offset += n;
    ensure_len(
        &buf[offset..],
        10,
        "Workspace is_base + is_external + created_at",
    )?;
    let is_base = read_bool_flag(buf[offset], "Workspace is_base")?;
    offset += 1;
    let is_external = read_bool_flag(buf[offset], "Workspace is_external")?;
    offset += 1;
    let created_at = u64::from_be_bytes(buf[offset..offset + 8].try_into().unwrap());
    offset += 8;
    Ok((
        Workspace {
            name,
            branch,
            path,
            is_base,
            is_external,
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

fn write_repository(buf: &mut Vec<u8>, repo: &Repository) -> std::io::Result<()> {
    write_str(buf, &repo.id)?;
    write_str(buf, &repo.url)?;
    write_str(buf, &repo.alias)?;
    write_opt_str(buf, repo.host.as_deref())?;
    write_opt_str(buf, repo.owner.as_deref())?;
    write_opt_str(buf, repo.name.as_deref())?;
    write_opt_str(buf, repo.local_path.as_deref())?;
    buf.extend_from_slice(&repo.created_at.to_be_bytes());
    Ok(())
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
    if read_option_tag(buf[0], "Option<u32> tag")? {
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
    if read_option_tag(buf[0], "Option<i32> tag")? {
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
    if read_option_tag(buf[0], "Option<u64> tag")? {
        ensure_len(&buf[1..], 8, "Option<u64> body")?;
        let v = u64::from_be_bytes(buf[1..9].try_into().unwrap());
        Ok((Some(v), 9))
    } else {
        Ok((None, 1))
    }
}

fn write_session(buf: &mut Vec<u8>, session: &Session) -> std::io::Result<()> {
    write_str(buf, &session.id)?;
    write_str(buf, &session.workspace_name)?;
    write_str(buf, &session.command_json)?;
    write_str(buf, &session.transport_addr)?;
    write_str(buf, &session.log_path)?;
    write_opt_u32(buf, session.supervisor_pid);
    write_opt_u32(buf, session.child_pid);
    write_str(buf, &session.status)?;
    write_opt_i32(buf, session.exit_code);
    buf.extend_from_slice(&session.created_at.to_be_bytes());
    write_opt_u64(buf, session.stopped_at);
    write_opt_str(buf, session.label.as_deref())?;
    write_opt_str(buf, session.harness.map(|h| h.to_string()).as_deref())?;
    Ok(())
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

fn write_str(buf: &mut Vec<u8>, s: &str) -> std::io::Result<()> {
    let bytes = s.as_bytes();
    let len = u16::try_from(bytes.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("string too long for u16 prefix: {} bytes", bytes.len()),
        )
    })?;
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(bytes);
    Ok(())
}

fn write_count_u32(buf: &mut Vec<u8>, label: &str, count: usize) -> std::io::Result<()> {
    if count > MAX_CONTROL_COLLECTION_COUNT {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{label} count {count} exceeds MAX_CONTROL_COLLECTION_COUNT ({})",
                MAX_CONTROL_COLLECTION_COUNT
            ),
        ));
    }
    buf.extend_from_slice(&(count as u32).to_be_bytes());
    Ok(())
}

fn write_count_u16(buf: &mut Vec<u8>, label: &str, count: usize, max: usize) -> std::io::Result<()> {
    if count > max {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{label} count {count} exceeds max ({max})"),
        ));
    }
    buf.extend_from_slice(&(count as u16).to_be_bytes());
    Ok(())
}

/// Read a length-prefixed string. Returns (string, bytes_consumed).
fn read_str(buf: &[u8]) -> std::io::Result<(String, usize)> {
    ensure_len(buf, 2, "string length prefix")?;
    let len = u16::from_be_bytes(buf[..2].try_into().unwrap()) as usize;
    ensure_len(&buf[2..], len, "string body")?;
    let s = String::from_utf8(buf[2..2 + len].to_vec()).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid UTF-8: {e}"),
        )
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

fn ensure_consumed(buf: &[u8], consumed: usize, context: &str) -> std::io::Result<()> {
    if consumed == buf.len() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{context}: {} trailing bytes",
                buf.len().saturating_sub(consumed)
            ),
        ))
    }
}

fn read_u32_count(buf: &[u8], context: &str) -> std::io::Result<usize> {
    bounded_count(
        usize::try_from(u32::from_be_bytes(buf.try_into().unwrap())).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{context} does not fit usize"),
            )
        })?,
        MAX_CONTROL_COLLECTION_COUNT,
        context,
    )
}

fn bounded_count(count: usize, max: usize, context: &str) -> std::io::Result<usize> {
    if count > max {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{context} {count} exceeds max {max}"),
        ))
    } else {
        Ok(count)
    }
}

fn read_bool_flag(byte: u8, context: &str) -> std::io::Result<bool> {
    match byte {
        0 => Ok(false),
        1 => Ok(true),
        other => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{context}: invalid boolean tag {other}"),
        )),
    }
}

fn read_option_tag(byte: u8, context: &str) -> std::io::Result<bool> {
    match byte {
        0 => Ok(false),
        1 => Ok(true),
        other => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{context}: invalid option tag {other}"),
        )),
    }
}

async fn read_frame_len<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_len: usize,
    label: &str,
) -> std::io::Result<Option<usize>> {
    let mut len_buf = [0u8; 4];
    let mut read = 0;
    while read < len_buf.len() {
        let n = reader.read(&mut len_buf[read..]).await?;
        if n == 0 {
            if read == 0 {
                return Ok(None);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{label} frame length prefix ended after {read} bytes"),
            ));
        }
        read += n;
    }

    let length = usize::try_from(u32::from_be_bytes(len_buf)).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{label} frame length does not fit usize"),
        )
    })?;
    if length > max_len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{label} frame length {length} exceeds max {max_len}"),
        ));
    }

    Ok(Some(length))
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
        assert_eq!(
            roundtrip(ControlFrame::StopAck).await,
            ControlFrame::StopAck
        );
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

    fn framed(type_byte: u8, payload: Vec<u8>) -> Vec<u8> {
        let mut framed = Vec::new();
        framed.extend_from_slice(&((1 + payload.len()) as u32).to_be_bytes());
        framed.push(type_byte);
        framed.extend_from_slice(&payload);
        framed
    }

    #[tokio::test]
    async fn rejects_oversized_control_frame_before_allocation() {
        let mut cursor =
            std::io::Cursor::new(((MAX_CONTROL_FRAME_LEN as u32) + 1).to_be_bytes().to_vec());
        let err = read_control_frame(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn partial_control_length_prefix_is_invalid_data() {
        let mut cursor = std::io::Cursor::new(vec![0, 0, 0]);
        let err = read_control_frame(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn rejects_unknown_control_type() {
        let mut cursor = std::io::Cursor::new(framed(0xff, Vec::new()));
        let err = read_control_frame(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn rejects_retired_detach_session_types() {
        for type_byte in [0xA6, 0xA7] {
            let mut cursor = std::io::Cursor::new(framed(type_byte, Vec::new()));
            let err = read_control_frame(&mut cursor).await.unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        }
    }

    #[tokio::test]
    async fn rejects_string_length_past_payload() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&5u16.to_be_bytes());
        payload.extend_from_slice(b"abc");

        let mut cursor = std::io::Cursor::new(framed(TYPE_REMOVE_REPO, payload));
        let err = read_control_frame(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn rejects_excessive_collection_count_before_allocation() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&((MAX_CONTROL_COLLECTION_COUNT as u32) + 1).to_be_bytes());

        let mut cursor = std::io::Cursor::new(framed(TYPE_REPO_LIST, payload));
        let err = read_control_frame(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn rejects_invalid_option_tag() {
        let mut cursor = std::io::Cursor::new(framed(TYPE_TOPOLOGY_CHANGED, vec![2]));
        let err = read_control_frame(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn rejects_invalid_boolean_tag() {
        let mut cursor = std::io::Cursor::new(framed(TYPE_SHUTDOWN, vec![2]));
        let err = read_control_frame(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn rejects_control_trailing_bytes() {
        let mut cursor = std::io::Cursor::new(framed(TYPE_PING, vec![0]));
        let err = read_control_frame(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
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
            is_external: false,
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
                    external_path: None,
                },
                BranchInfo {
                    name: "feature".to_string(),
                    is_head: false,
                    checked_out_by: Some("feature-workspace".to_string()),
                    external_path: None,
                },
                BranchInfo {
                    name: "external-feat".to_string(),
                    is_head: false,
                    checked_out_by: Some("(external: external-feat)".to_string()),
                    external_path: Some(r"D:\elsewhere\external-feat".to_string()),
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
            owner: if local {
                None
            } else {
                Some("demo".to_string())
            },
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
        write_str(&mut payload, "id").unwrap(); // id
        write_str(&mut payload, "ws").unwrap(); // workspace_name
        write_str(&mut payload, "[]").unwrap(); // command_json
        write_str(&mut payload, "").unwrap(); // transport_addr
        write_str(&mut payload, "").unwrap(); // log_path
        payload.push(0); // supervisor_pid: None
        payload.push(0); // child_pid: None
        write_str(&mut payload, "running").unwrap(); // status
        payload.push(0); // exit_code: None
        payload.extend_from_slice(&0u64.to_be_bytes()); // created_at
        payload.push(0); // stopped_at: None
        payload.push(0); // label: None
                         // harness: Some("unknown-harness") — expected to decode as None
        payload.push(1);
        write_str(&mut payload, "unknown-harness").unwrap();

        let (session, _) = read_session(&payload).unwrap();
        assert!(
            session.harness.is_none(),
            "unknown harness string should decode as None"
        );
    }

    #[tokio::test]
    async fn roundtrip_install_harness_hooks_request() {
        let frame = ControlFrame::InstallHarnessHooks {
            harness: "claude-code".to_string(),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_install_harness_hooks_ack_installed_no_reason() {
        let frame = ControlFrame::InstallHarnessHooksAck {
            installed: true,
            skipped_reason: None,
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_install_harness_hooks_ack_already_configured() {
        let frame = ControlFrame::InstallHarnessHooksAck {
            installed: true,
            skipped_reason: Some("already configured".to_string()),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_install_harness_hooks_ack_skipped() {
        let frame = ControlFrame::InstallHarnessHooksAck {
            installed: false,
            skipped_reason: Some("claude code not installed".to_string()),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_adopt_workspace_full() {
        let frame = ControlFrame::AdoptWorkspace {
            repo: "demo".to_string(),
            path: r"D:\elsewhere\feat-x".to_string(),
            name: Some("feat-x".to_string()),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_adopt_workspace_default_name() {
        let frame = ControlFrame::AdoptWorkspace {
            repo: "demo".to_string(),
            path: r"D:\elsewhere\feat-x".to_string(),
            name: None,
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_adopt_workspace_ack() {
        let frame = ControlFrame::AdoptWorkspaceAck {
            workspace: Workspace {
                name: "feat-x".to_string(),
                branch: "feat-x".to_string(),
                path: r"D:\elsewhere\feat-x".to_string(),
                is_base: false,
                is_external: true,
                created_at: 1_700_000_321,
            },
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn mode_byte_rejects_unknown() {
        let mut bytes = Vec::new();
        write_str(&mut bytes, "demo").unwrap();
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
