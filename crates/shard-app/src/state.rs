use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use shard_core::state::RepoState;
use shard_transport::protocol::{ActivityState, OwnerSummary};
use shard_transport::PlatformClient;
use tokio::io::WriteHalf;
use tokio::sync::Mutex;

/// Shared write-half of an attached session pipe. Wrapped in `Arc<Mutex>`
/// so per-window IPC commands (`write_to_session`, `resize_session`) can
/// serialize writes.
pub type SharedSessionWriter = Arc<Mutex<WriteHalf<PlatformClient>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionToken(u64);

impl ConnectionToken {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    /// Use the token's monotonic value as the `client_id` we send in `Hello`.
    /// The supervisor identifies clients by its own server-side `conn_id` for
    /// ownership accounting; this value is informational + lets a window
    /// match `InputOwnerChanged` payloads back to its own connection.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// One per running session, process-global. Receives `ActivityUpdate`,
/// `Status`, and `InputOwnerChanged` frames; relays each as a Tauri event so
/// every window's sidebar / overlay state stays current. Independent of
/// which (if any) window has the session attached.
pub struct MonitorHandle {
    pub task: tauri::async_runtime::JoinHandle<()>,
}

/// One per (window, session) where a window has the session open in its
/// terminal pane. Holds the pipe writer and the reader task that fans
/// `TerminalOutput` into the per-window Tauri channel.
pub struct AttachmentHandle {
    pub token: ConnectionToken,
    pub writer: SharedSessionWriter,
    pub task: tauri::async_runtime::JoinHandle<()>,
    /// Value sent in our `Hello`. Currently only consumed inside the reader
    /// task closure (captured by value); kept on the struct for diagnostics
    /// and a future "list active attachments" RPC.
    #[allow(dead_code)]
    pub client_id: u64,
}

pub struct AppState {
    /// One monitor pipe per running session, process-global.
    pub monitors: Mutex<HashMap<String, MonitorHandle>>,
    /// Per-window full attachments, keyed by `(window_label, session_id)`.
    /// Multiple windows can have monitors for the same session. Cross-GUI
    /// input exclusivity is enforced by the supervisor; this map only tracks
    /// local pipes that were accepted by that policy.
    pub attachments: Mutex<HashMap<(String, String), AttachmentHandle>>,
    /// Advisory cache of "which window has this session open" populated
    /// from `attach_session` / `detach_session`. Used by `focus_window`
    /// when another window clicks a session that is already attached. Not
    /// the source of truth — the `attachments` map is — but allows O(1)
    /// lookup without scanning every window.
    pub session_windows: Mutex<HashMap<String, String>>,
    /// Last owner snapshot observed from each session supervisor. This
    /// lets newly opened windows hydrate their sidebar state without
    /// waiting for the next ownership transition.
    pub input_owners: Mutex<HashMap<String, Option<OwnerSummary>>>,
    /// Latest OSC terminal title broadcast for each session. Populated by
    /// `notify_session_title` whenever any window's xterm.js parses an
    /// OSC title change, so windows that don't have the terminal mounted
    /// (and newly opened windows) can show the same dynamic label without
    /// needing to attach. Cleared when the session ends.
    pub dynamic_titles: Mutex<HashMap<String, String>>,
    /// Latest supervisor-reported activity state per session. Populated by
    /// `handle_supervisor_frame` so newly opened windows can hydrate their
    /// activity indicators without waiting for the next ActivityUpdate.
    /// Cleared when the session ends.
    pub activity_states: Mutex<HashMap<String, ActivityState>>,
    /// Last-known `RepoState` per alias, populated by the daemon-subscribe
    /// task in `daemon_ipc::run_state_subscriber`.
    pub repo_states: Mutex<HashMap<String, RepoState>>,
    /// Monotonic counter for new window labels. Always increments —
    /// reusing labels invites stale-event bugs.
    pub next_window_id: AtomicU64,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            monitors: Mutex::new(HashMap::new()),
            attachments: Mutex::new(HashMap::new()),
            session_windows: Mutex::new(HashMap::new()),
            input_owners: Mutex::new(HashMap::new()),
            dynamic_titles: Mutex::new(HashMap::new()),
            activity_states: Mutex::new(HashMap::new()),
            repo_states: Mutex::new(HashMap::new()),
            next_window_id: AtomicU64::new(2), // 1 is reserved; first secondary window is "window-2"
        }
    }

    pub fn allocate_window_label(&self) -> String {
        let n = self.next_window_id.fetch_add(1, Ordering::Relaxed);
        format!("window-{n}")
    }
}
