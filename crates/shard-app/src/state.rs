use std::collections::HashMap;

use shard_core::state::RepoState;
use tokio::io::WriteHalf;
use tokio::net::windows::named_pipe::NamedPipeClient;
use tokio::sync::Mutex;

pub struct SessionWriter {
    pub writer: WriteHalf<NamedPipeClient>,
}

/// Per-session connection state. Each running session has exactly one pipe
/// connection at a time — either a lightweight monitor or a full terminal attach.
pub enum SessionConnection {
    /// Background reader that only cares about ActivityUpdate and Status frames.
    /// Used for sessions visible in the sidebar but without an open terminal tab.
    Monitored { task: tauri::async_runtime::JoinHandle<()> },
    /// Full terminal connection with input forwarding.
    Attached {
        writer: SessionWriter,
        task: tauri::async_runtime::JoinHandle<()>,
    },
}

impl SessionConnection {
    pub fn abort(self) {
        match self {
            SessionConnection::Monitored { task } => task.abort(),
            SessionConnection::Attached { task, .. } => task.abort(),
        }
    }
}

pub struct AppState {
    pub connections: Mutex<HashMap<String, SessionConnection>>,
    /// Last-known `RepoState` per alias, populated by the daemon-subscribe
    /// task in `daemon_ipc::run_state_subscriber`. Read by
    /// `list_workspaces` to enrich the workspace list with live status
    /// without doing another daemon round-trip.
    pub repo_states: Mutex<HashMap<String, RepoState>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            connections: Mutex::new(HashMap::new()),
            repo_states: Mutex::new(HashMap::new()),
        }
    }
}
