//! Daemon-owned monitor that keeps `RepoState` in sync with live git state.
//!
//! One task per daemon. Unified `tokio::select!` over:
//!   1. Debounced filesystem events (notify + notify-debouncer-full)
//!   2. Fast tick (1s) — supervisor PID liveness + re-check workspaces flagged by notify
//!   3. Reconcile tick (30s) — full walk, `git worktree list --porcelain`, broken/missing classification
//!   4. Topology poke channel — repos / workspaces added or removed by a Tauri command
//!   5. Shutdown signal — graceful exit
//!
//! Absorbs the former 5s heartbeat task's responsibilities (supervisor liveness,
//! DB exit transitions, tray session-count updates). There is no other periodic
//! task in the daemon; this is it.
//!
//! Writes nothing to the DB for workspace status — `WorkspaceStatus` is pure
//! derived state kept in memory and broadcast to subscribers via a shared map
//! + broadcast notification.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, RecommendedCache};
use tokio::sync::{broadcast, mpsc, oneshot, watch, RwLock};
use tracing::{debug, info, warn};

use shard_core::repos::{Repository, RepositoryStore};
use shard_core::sessions::SessionStore;
use shard_core::state::{RepoState, WorkspaceHealth, WorkspaceStatus};
use shard_core::workspaces::{Workspace, WorkspaceStore};
use shard_supervisor::process::{PlatformProcessControl, ProcessControl};

use crate::cmd::daemon::{DaemonState, ShutdownMode, TrayEvent};

/// Debounce window for notify events. Git operations touch multiple files in
/// bursts (HEAD, index, logs, …) so we coalesce before reacting.
const NOTIFY_DEBOUNCE: Duration = Duration::from_millis(150);

/// PID liveness + notify reconcile cadence.
const FAST_TICK: Duration = Duration::from_secs(1);

/// Full-walk reconcile cadence. Catches events notify missed
/// (network drives, permissions, debouncer overflow).
const RECONCILE_TICK: Duration = Duration::from_secs(30);

/// Capacity of the change broadcast. One slot per queued "alias X updated"
/// signal; subscribers that lag past this will see `broadcast::RecvError::Lagged`
/// and recover by reading the shared map. We size for burstiness during bulk
/// git operations across many repos.
const CHANGE_CHANNEL_CAPACITY: usize = 1024;

/// Handle shared between the monitor task and subscribers / IPC callers.
///
/// Holds:
///   - the latest `RepoState` for every repo the daemon knows about
///   - a broadcast channel that emits a repo alias whenever that repo's state
///     changes (subscribers look up the fresh value in `repos`)
///   - a sender to the monitor's topology poke channel
#[derive(Clone)]
pub struct MonitorHandle {
    inner: Arc<MonitorInner>,
}

/// What kind of change a subscriber is being notified about.
///
/// The two kinds map to two different wire frames: `State` triggers a
/// `StateSnapshot` (re-read `RepoState` and push it), `Sessions` triggers
/// a `SessionsChanged` (hint the client to re-query the sessions table).
/// Both carry only the affected repo alias; the receiver is responsible
/// for fetching whatever it needs.
#[derive(Clone, Debug)]
pub enum ChangeKind {
    State(String),
    Sessions(String),
    /// Fine-grained "a workspace was removed" signal. Emitted by
    /// `RemoveWorkspace` after the full D5 workflow commits. Subscribers
    /// translate this into a `ControlFrame::WorkspaceRemoved` frame.
    WorkspaceRemoved {
        repo: String,
        name: String,
    },
}

/// Typed commands sent from daemon mutation handlers to the monitor task.
///
/// Previously the daemon poked the monitor via a bare
/// `mpsc::UnboundedSender<Option<String>>` with no back-channel. That's
/// fine for fire-and-forget reloads but unsafe for operations where the
/// monitor's `ReadDirectoryChangesW` handle must be dropped *before* a
/// mutation handler's `RemoveDirectoryW` fires (SHA-55). Command variants
/// that carry a oneshot ack solve this.
#[derive(Debug)]
pub enum MonitorCommand {
    /// Reload the given repo (or every repo if `None`) from DB and
    /// reconfigure watchers. Fire-and-forget; no ack.
    PokeTopology { repo_alias: Option<String> },

    /// Drop the repo's current debouncer (releasing all its
    /// `ReadDirectoryChangesW` handles, including the one on
    /// `workspace_name`), then rebuild a new debouncer over the remaining
    /// workspaces, then send the ack.
    ///
    /// **Load-bearing ack semantics:** the ack must fire only *after* the
    /// old `Debouncer` has been dropped on this task. Mutation handlers
    /// rely on that guarantee to sequence `RemoveDirectoryW` safely.
    DropRepoWorkspace {
        alias: String,
        /// Name of the workspace being deleted. It's filtered out of the
        /// rebuilt watcher set so the new debouncer doesn't reopen a
        /// handle on the directory we're about to remove.
        workspace_name: String,
        ack: oneshot::Sender<()>,
    },

    /// Drop the repo's entire debouncer + RepoState entry, releasing
    /// every `ReadDirectoryChangesW` handle it owned. Used by
    /// `RemoveRepo` before tearing down the on-disk repo tree, since
    /// every workspace under the repo is going away at once.
    ///
    /// **Load-bearing ack semantics:** same contract as
    /// `DropRepoWorkspace` — the ack must fire only after the
    /// `Debouncer` has been dropped on this task so the mutation
    /// handler can safely call `RemoveDirectoryW`.
    DropRepo {
        alias: String,
        ack: oneshot::Sender<()>,
    },
}

struct MonitorInner {
    repos: RwLock<HashMap<String, Arc<RepoState>>>,
    change_tx: broadcast::Sender<ChangeKind>,
    /// Typed command channel (replaces the former topology-only poke).
    /// Fire-and-forget pokes are encoded as `MonitorCommand::PokeTopology`.
    command_tx: mpsc::UnboundedSender<MonitorCommand>,
}

impl MonitorHandle {
    /// Read the current state for every repo. Used when a subscriber first
    /// connects and needs an initial snapshot before switching to deltas.
    pub async fn snapshot(&self) -> Vec<Arc<RepoState>> {
        self.inner.repos.read().await.values().cloned().collect()
    }

    /// Read the current state for a single repo, if known.
    pub async fn get(&self, alias: &str) -> Option<Arc<RepoState>> {
        self.inner.repos.read().await.get(alias).cloned()
    }

    /// Subscribe to change notifications. Returns `ChangeKind` events scoped
    /// to one repo at a time — `State` when `RepoState` changes (call
    /// `snapshot`/`get` to read the fresh value), `Sessions` when the sessions
    /// table for that repo has transitioned (re-query the DB).
    pub fn subscribe(&self) -> broadcast::Receiver<ChangeKind> {
        self.inner.change_tx.subscribe()
    }

    /// Fire-and-forget topology poke. Called by daemon IPC handlers after a
    /// client has committed a repo/workspace mutation. `None` requests a
    /// full reload; `Some(alias)` scopes it to one repo.
    pub fn poke_topology(&self, repo_alias: Option<String>) {
        let _ = self
            .inner
            .command_tx
            .send(MonitorCommand::PokeTopology { repo_alias });
    }

    /// Broadcast a change to subscribers. Used by mutation handlers that
    /// have already committed side effects and want to fan out the
    /// resulting event (e.g. `WorkspaceRemoved`) to connected clients.
    pub(crate) fn broadcast(&self, change: ChangeKind) {
        let _ = self.inner.change_tx.send(change);
    }

    /// Drop the watcher for a specific workspace (so its
    /// `ReadDirectoryChangesW` handle is released) and wait for the
    /// confirmation ack before returning. Used by `RemoveWorkspace` before
    /// `git worktree remove` / `fs::remove_dir_all`.
    ///
    /// If the monitor task has gone away (shutdown in progress) the
    /// returned error is informational — the caller can proceed since
    /// there is no watcher to block the delete.
    pub(crate) async fn drop_repo_workspace(
        &self,
        alias: &str,
        workspace_name: &str,
    ) -> Result<(), &'static str> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.inner
            .command_tx
            .send(MonitorCommand::DropRepoWorkspace {
                alias: alias.to_string(),
                workspace_name: workspace_name.to_string(),
                ack: ack_tx,
            })
            .map_err(|_| "monitor task is not running")?;
        ack_rx
            .await
            .map_err(|_| "monitor dropped ack before replying")
    }

    /// Drop the repo's entire watcher (and its `RepoState` entry) and
    /// wait for the confirmation ack before returning. Used by
    /// `RemoveRepo` before the on-disk teardown.
    pub(crate) async fn drop_repo(&self, alias: &str) -> Result<(), &'static str> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.inner
            .command_tx
            .send(MonitorCommand::DropRepo {
                alias: alias.to_string(),
                ack: ack_tx,
            })
            .map_err(|_| "monitor task is not running")?;
        ack_rx
            .await
            .map_err(|_| "monitor dropped ack before replying")
    }
}

/// Spawn the WorkspaceMonitor as a detached tokio task and return its handle.
///
/// The task owns all filesystem watchers and ticks; the handle provides
/// read-only access to the latest state plus the topology-poke channel.
pub(crate) fn spawn(
    daemon_state: Arc<DaemonState>,
    shutdown_rx: watch::Receiver<ShutdownMode>,
) -> (MonitorHandle, tokio::task::JoinHandle<()>) {
    let (change_tx, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
    let (command_tx, command_rx) = mpsc::unbounded_channel::<MonitorCommand>();

    let inner = Arc::new(MonitorInner {
        repos: RwLock::new(HashMap::new()),
        change_tx,
        command_tx,
    });

    let handle = MonitorHandle {
        inner: inner.clone(),
    };

    let task_state = daemon_state.clone();
    let task_inner = inner.clone();
    let task = tokio::spawn(async move {
        let mut monitor = WorkspaceMonitor::new(task_state, task_inner, command_rx, shutdown_rx);
        if let Err(e) = monitor.initial_load().await {
            warn!("WorkspaceMonitor initial load failed: {e}");
        }
        monitor.run().await;
        info!("WorkspaceMonitor exited");
    });

    (handle, task)
}

// ── Internal state ──────────────────────────────────────────────────────────

struct WorkspaceMonitor {
    daemon: Arc<DaemonState>,
    inner: Arc<MonitorInner>,
    command_rx: mpsc::UnboundedReceiver<MonitorCommand>,
    shutdown_rx: watch::Receiver<ShutdownMode>,

    repo_store: RepositoryStore,
    ws_store: WorkspaceStore,

    /// One debouncer per repo — keeps lifetimes bounded to the repo's
    /// existence in the topology. Dropping a debouncer unwatches its paths.
    debouncers: HashMap<String, Debouncer<notify::RecommendedWatcher, RecommendedCache>>,

    /// Watched paths per repo, used to unwatch on topology changes.
    watched_paths: HashMap<String, Vec<PathBuf>>,

    /// Unified notify event stream — all debouncers push into this channel.
    notify_rx: mpsc::UnboundedReceiver<NotifyBatch>,
    notify_tx: mpsc::UnboundedSender<NotifyBatch>,
}

/// One debounced batch of notify events from a single repo's debouncer.
#[derive(Debug)]
struct NotifyBatch {
    repo_alias: String,
    /// Paths that were touched. Empty means "something changed, we don't
    /// know what" — reconcile the whole repo.
    paths: HashSet<PathBuf>,
}

impl WorkspaceMonitor {
    fn new(
        daemon: Arc<DaemonState>,
        inner: Arc<MonitorInner>,
        command_rx: mpsc::UnboundedReceiver<MonitorCommand>,
        shutdown_rx: watch::Receiver<ShutdownMode>,
    ) -> Self {
        let (notify_tx, notify_rx) = mpsc::unbounded_channel();
        let repo_store = RepositoryStore::new(daemon.paths.clone());
        let ws_store = WorkspaceStore::new(daemon.paths.clone());
        Self {
            daemon,
            inner,
            command_rx,
            shutdown_rx,
            repo_store,
            ws_store,
            debouncers: HashMap::new(),
            watched_paths: HashMap::new(),
            notify_rx,
            notify_tx,
        }
    }

    /// Build the initial RepoState for every known repo and set up watchers.
    async fn initial_load(&mut self) -> shard_core::Result<()> {
        let repos = self.repo_store.list()?;
        info!("WorkspaceMonitor: initial load, {} repo(s)", repos.len());

        for repo in repos {
            self.load_repo(&repo).await;
        }

        self.broadcast_all().await;
        Ok(())
    }

    /// Build RepoState + filesystem watchers for a single repo.
    /// Leaves any existing entry untouched — call `drop_repo` first if you
    /// want a clean rebuild.
    async fn load_repo(&mut self, repo: &Repository) {
        let alias = repo.alias.clone();
        let workspaces = match self.ws_store.list(&alias) {
            Ok(ws) => ws,
            Err(e) => {
                debug!("load_repo({alias}): no workspaces ({e})");
                Vec::new()
            }
        };

        let mut state = RepoState::new(alias.clone());
        state.version = 1;
        for ws in &workspaces {
            let status = classify_workspace(ws, repo);
            state.workspaces.insert(ws.name.clone(), status);
        }

        self.inner
            .repos
            .write()
            .await
            .insert(alias.clone(), Arc::new(state));

        if let Err(e) = self.start_watcher(&alias, repo, &workspaces) {
            warn!("load_repo({alias}): watcher setup failed: {e}");
        }
    }

    /// Tear down a repo's state and watchers.
    async fn drop_repo(&mut self, alias: &str) {
        self.inner.repos.write().await.remove(alias);
        self.debouncers.remove(alias);
        self.watched_paths.remove(alias);
    }

    fn start_watcher(
        &mut self,
        alias: &str,
        _repo: &Repository,
        workspaces: &[Workspace],
    ) -> notify::Result<()> {
        // Clear any existing watcher for this alias first.
        self.debouncers.remove(alias);
        self.watched_paths.remove(alias);

        if workspaces.is_empty() {
            return Ok(());
        }

        let notify_tx = self.notify_tx.clone();
        let alias_for_cb = alias.to_string();
        let mut debouncer = new_debouncer(
            NOTIFY_DEBOUNCE,
            None,
            move |result: DebounceEventResult| match result {
                Ok(events) => {
                    let mut paths: HashSet<PathBuf> = HashSet::new();
                    for ev in events {
                        for p in &ev.paths {
                            paths.insert(p.clone());
                        }
                    }
                    let _ = notify_tx.send(NotifyBatch {
                        repo_alias: alias_for_cb.clone(),
                        paths,
                    });
                }
                Err(errors) => {
                    for e in errors {
                        debug!("notify error in {}: {e}", alias_for_cb);
                    }
                    // Push an empty batch so the main loop still reconciles
                    // the repo defensively.
                    let _ = notify_tx.send(NotifyBatch {
                        repo_alias: alias_for_cb.clone(),
                        paths: HashSet::new(),
                    });
                }
            },
        )?;

        let mut watched: Vec<PathBuf> = Vec::new();
        for ws in workspaces {
            let ws_path = PathBuf::from(&ws.path);

            // Watch the gitdir (containing HEAD) non-recursively. We get
            // events for HEAD flips, index writes, and anything else in that
            // directory; the fast-tick reconcile filters for what matters.
            match shard_core::git::resolve_gitdir(&ws_path) {
                Ok(gitdir) => {
                    match debouncer.watch(gitdir.as_path(), RecursiveMode::NonRecursive) {
                        Ok(()) => watched.push(gitdir),
                        Err(e) => debug!(
                            "watch gitdir {:?} failed for {alias}/{}: {e}",
                            gitdir, ws.name
                        ),
                    }
                }
                Err(e) => {
                    debug!(
                        "resolve_gitdir({:?}) failed for {alias}/{}: {e}",
                        ws_path, ws.name
                    );
                }
            }

            // Watch the workspace directory itself non-recursively so we
            // notice external `rm -rf`. On Windows notify surfaces the
            // remove via the parent's directory handle being invalidated;
            // the next reconcile re-classifies the workspace as Missing.
            match debouncer.watch(ws_path.as_path(), RecursiveMode::NonRecursive) {
                Ok(()) => watched.push(ws_path),
                Err(e) => {
                    debug!("watch worktree {:?} failed for {alias}: {e}", ws_path);
                }
            }
        }

        self.debouncers.insert(alias.to_string(), debouncer);
        self.watched_paths.insert(alias.to_string(), watched);
        Ok(())
    }

    /// Emit a change notification for every repo currently in the map.
    /// Used after the initial load so any subscriber that connected before
    /// load finished gets a refresh.
    async fn broadcast_all(&self) {
        let aliases: Vec<String> = self.inner.repos.read().await.keys().cloned().collect();
        for alias in aliases {
            let _ = self.inner.change_tx.send(ChangeKind::State(alias));
        }
    }

    async fn run(&mut self) {
        let mut fast = tokio::time::interval(FAST_TICK);
        fast.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut reconcile = tokio::time::interval(RECONCILE_TICK);
        reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;

                // Shutdown — exit immediately so the daemon can make progress.
                _ = self.shutdown_rx.changed() => {
                    info!("WorkspaceMonitor: shutdown signal");
                    return;
                }

                // Typed commands from daemon mutation handlers.
                Some(cmd) = self.command_rx.recv() => {
                    self.handle_command(cmd).await;
                }

                // Debounced filesystem events.
                Some(batch) = self.notify_rx.recv() => {
                    self.handle_notify(batch).await;
                }

                // Fast tick — PID liveness + cheap state refresh.
                _ = fast.tick() => {
                    self.fast_tick().await;
                }

                // Reconcile tick — full walk.
                _ = reconcile.tick() => {
                    self.reconcile_tick().await;
                }
            }
        }
    }

    async fn handle_command(&mut self, cmd: MonitorCommand) {
        match cmd {
            MonitorCommand::PokeTopology { repo_alias } => {
                self.handle_topology_change(repo_alias).await;
            }
            MonitorCommand::DropRepoWorkspace {
                alias,
                workspace_name,
                ack,
            } => {
                self.handle_drop_repo_workspace(alias, workspace_name, ack)
                    .await;
            }
            MonitorCommand::DropRepo { alias, ack } => {
                self.handle_drop_repo(alias, ack).await;
            }
        }
    }

    /// Drop the repo's debouncer + RepoState entry and ack only after
    /// the `Debouncer` has been dropped on this task. Mirror of
    /// `handle_drop_repo_workspace` without the rebuild step — the
    /// entire repo is going away, so there are no remaining workspaces
    /// to watch.
    async fn handle_drop_repo(&mut self, alias: String, ack: oneshot::Sender<()>) {
        // Drop the old debouncer explicitly — releases every
        // `ReadDirectoryChangesW` handle it held so the caller's
        // `RemoveDirectoryW` can proceed.
        let prev = self.debouncers.remove(&alias);
        drop(prev);
        self.watched_paths.remove(&alias);

        // Clear the in-memory RepoState too; if AddRepo is called again
        // for the same alias it'll be rebuilt on the next topology poke.
        self.inner.repos.write().await.remove(&alias);

        let _ = ack.send(());
    }

    /// Drop the current debouncer for `alias` (releasing all its
    /// `ReadDirectoryChangesW` handles), rebuild a new debouncer over the
    /// repo's remaining workspaces (excluding `workspace_name`, which is
    /// about to be removed from disk), then send the ack.
    ///
    /// Ordering is load-bearing: the ack MUST fire only after the old
    /// `Debouncer` has been dropped. The explicit `drop(prev)` here is
    /// what makes the ordering a property of the code rather than a
    /// comment: the Debouncer's `Drop` impl closes the OS handle, so by
    /// the time `ack.send(())` runs, `RemoveDirectoryW` on the workspace
    /// path can no longer be blocked by notify's handle.
    async fn handle_drop_repo_workspace(
        &mut self,
        alias: String,
        workspace_name: String,
        ack: oneshot::Sender<()>,
    ) {
        // 1. Drop the old debouncer and its watched-paths record. The
        //    explicit `drop(prev)` is what releases the OS handles.
        let prev = self.debouncers.remove(&alias);
        drop(prev);
        self.watched_paths.remove(&alias);

        // 2. Rebuild the watcher over the remaining workspaces. The DB
        //    row for `workspace_name` is still present at this point
        //    (D5 ordering: monitor-drop ack fires BEFORE DB delete), so
        //    filter it out explicitly rather than re-listing post-delete.
        if let Ok(repo) = self.repo_store.get(&alias) {
            let workspaces = match self.ws_store.list(&alias) {
                Ok(list) => list
                    .into_iter()
                    .filter(|w| w.name != workspace_name)
                    .collect::<Vec<_>>(),
                Err(_) => Vec::new(),
            };
            if let Err(e) = self.start_watcher(&alias, &repo, &workspaces) {
                warn!(
                    "DropRepoWorkspace: watcher re-setup failed for {alias}: {e} — workspace state will still converge via 30s reconcile",
                );
            }
        }

        // 3. Ack after the handle drop AND after the replacement watcher
        //    has been installed on the remaining paths (excluding the
        //    one being deleted). The mutation handler now owns exclusive
        //    access to the soon-to-be-removed directory.
        let _ = ack.send(());
    }

    async fn handle_topology_change(&mut self, alias: Option<String>) {
        match alias {
            Some(ref alias) => match self.repo_store.get(alias) {
                Ok(repo) => {
                    debug!("topology: reloading repo {alias}");
                    self.drop_repo(alias).await;
                    self.load_repo(&repo).await;
                    let _ = self.inner.change_tx.send(ChangeKind::State(alias.clone()));
                }
                Err(_) => {
                    debug!("topology: repo {alias} removed");
                    self.drop_repo(alias).await;
                    let _ = self.inner.change_tx.send(ChangeKind::State(alias.clone()));
                }
            },
            None => {
                // Full reload.
                let current_aliases: Vec<String> =
                    self.inner.repos.read().await.keys().cloned().collect();
                for alias in &current_aliases {
                    self.drop_repo(alias).await;
                }
                match self.repo_store.list() {
                    Ok(repos) => {
                        for repo in repos {
                            self.load_repo(&repo).await;
                        }
                        self.broadcast_all().await;
                    }
                    Err(e) => warn!("topology full reload failed: {e}"),
                }
            }
        }
    }

    async fn handle_notify(&mut self, batch: NotifyBatch) {
        // A notify batch means "something changed in this repo" — walk its
        // workspaces and re-classify. Cheap: HEAD is a small file, `exists`
        // is a stat. Fast tick would also catch this within 1s, but
        // reacting immediately keeps the UI snappy (~200ms end-to-end).
        debug!(
            "notify batch for {} ({} path(s))",
            batch.repo_alias,
            batch.paths.len()
        );
        self.reconcile_repo(&batch.repo_alias).await;
    }

    /// Recompute status for every workspace in one repo. Pushes a new
    /// `RepoState` into the shared map if anything changed and emits a
    /// broadcast notification. No DB writes.
    async fn reconcile_repo(&self, alias: &str) {
        let repo = match self.repo_store.get(alias) {
            Ok(r) => r,
            Err(_) => return,
        };
        let workspaces = self.ws_store.list(alias).unwrap_or_default();

        let mut new_map: HashMap<String, WorkspaceStatus> = HashMap::new();
        for ws in &workspaces {
            new_map.insert(ws.name.clone(), classify_workspace(ws, &repo));
        }

        self.commit_workspace_map(alias, new_map).await;
    }

    /// Commit a freshly-computed workspace-status map into the shared
    /// `RepoState` for one repo. No-op when the content hasn't changed.
    /// Performs the read-vs-new comparison and the version bump under the
    /// write lock so concurrent reconciles for the same alias cannot
    /// clobber each other with stale data.
    async fn commit_workspace_map(&self, alias: &str, new_map: HashMap<String, WorkspaceStatus>) {
        let mut repos = self.inner.repos.write().await;
        let should_update = match repos.get(alias) {
            Some(existing) => existing.workspaces != new_map,
            None => true,
        };
        if !should_update {
            return;
        }
        let mut state = repos
            .get(alias)
            .map(|s| (**s).clone())
            .unwrap_or_else(|| RepoState::new(alias));
        state.workspaces = new_map;
        state.version += 1;
        repos.insert(alias.to_string(), Arc::new(state));
        drop(repos);
        let _ = self
            .inner
            .change_tx
            .send(ChangeKind::State(alias.to_string()));
    }

    /// Fast tick: PID liveness + DB exit transitions + tray update. This is
    /// the only periodic work in the daemon other than the 30s reconcile.
    async fn fast_tick(&mut self) {
        // Scan the daemon sessions map. Mirrors the old heartbeat_task — if
        // a supervisor's PID has died, mark it exited in the DB and remove
        // it from the live map.
        let (dead_ids, current_count): (Vec<(String, String)>, usize) = {
            let mut sessions = self.daemon.sessions.lock().await;
            let mut dead = Vec::new();

            for (id, s) in sessions.iter() {
                if !PlatformProcessControl::is_alive(s.supervisor_pid) {
                    dead.push((id.clone(), s.repo.clone()));
                }
            }
            for (id, _) in &dead {
                sessions.remove(id);
            }
            (dead, sessions.len())
        };

        if !dead_ids.is_empty() {
            let session_store = SessionStore::new(self.daemon.paths.clone());
            for (id, repo) in &dead_ids {
                let _ = session_store.update_status(repo, id, "exited", None);
                info!(
                    "WorkspaceMonitor: session {} supervisor died, marked exited",
                    &id[..8.min(id.len())]
                );
            }

            // Notify subscribers once per affected repo so the frontend
            // re-queries the sessions table and drops the stale "running"
            // row. Dedup so a repo with N dead sessions sends one event.
            let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for (_, repo) in &dead_ids {
                if seen.insert(repo.as_str()) {
                    let _ = self
                        .inner
                        .change_tx
                        .send(ChangeKind::Sessions(repo.clone()));
                }
            }
        }

        // Tray icon update (absorbed from heartbeat_task).
        #[cfg(windows)]
        {
            if let Some(proxy) = &self.daemon.tray_proxy {
                let _ = proxy.send_event(TrayEvent::SessionCount(current_count));
            }
        }
        #[cfg(not(windows))]
        {
            let _ = current_count;
        }
    }

    /// Slow reconcile: re-walk every repo, cross-check with
    /// `git worktree list --porcelain`, classify health, catch anything
    /// notify may have missed. Used for network drives, permission
    /// glitches, and notify overflows.
    async fn reconcile_tick(&mut self) {
        let repos = match self.repo_store.list() {
            Ok(r) => r,
            Err(_) => return,
        };
        debug!(
            "WorkspaceMonitor: reconcile tick over {} repo(s)",
            repos.len()
        );

        for repo in &repos {
            self.reconcile_repo_deep(repo).await;
        }
    }

    async fn reconcile_repo_deep(&self, repo: &Repository) {
        let alias = &repo.alias;
        let workspaces = self.ws_store.list(alias).unwrap_or_default();

        // Run porcelain once per repo for git's view of worktree health.
        // Pre-normalize the entry paths into a lookup table so the inner
        // matching loop is O(N) instead of O(N*M) with canonicalize() calls.
        let porcelain_view: Option<HashMap<String, shard_core::git::WorktreeEntry>> = {
            let source_dir = self
                .daemon
                .paths
                .repo_source_for_repo(alias, repo.local_path.as_deref());
            shard_core::git::worktree_list_porcelain(&source_dir)
                .ok()
                .map(|entries| {
                    entries
                        .into_iter()
                        .map(|e| (normalize_path(&e.path), e))
                        .collect()
                })
        };

        let mut new_map: HashMap<String, WorkspaceStatus> = HashMap::new();
        for ws in &workspaces {
            let mut status = classify_workspace(ws, repo);

            // Upgrade Healthy → Broken if porcelain doesn't know about this
            // worktree, and Healthy → Missing if porcelain reports it as
            // prunable. Base workspaces (the user's own checkout for local
            // repos) are not listed as worktrees — skip porcelain cross-check.
            if !ws.is_base {
                if let Some(ref entries) = porcelain_view {
                    let key = normalize_path(&PathBuf::from(&ws.path));
                    match entries.get(&key) {
                        None if status.health == WorkspaceHealth::Healthy => {
                            status.health = WorkspaceHealth::Broken;
                        }
                        Some(entry) if entry.prunable => {
                            status.health = WorkspaceHealth::Missing;
                        }
                        _ => {}
                    }
                }
            }

            new_map.insert(ws.name.clone(), status);
        }

        self.commit_workspace_map(alias, new_map).await;
    }
}

/// Build `WorkspaceStatus` from on-disk state alone (no porcelain cross-check).
/// Used on the hot path (notify + fast tick) because it's cheap:
///   - `exists()` stat
///   - `resolve_gitdir` — 1 or 2 file reads
///   - `read_head` — 1–2 more file reads
fn classify_workspace(ws: &Workspace, _repo: &Repository) -> WorkspaceStatus {
    let ws_path = Path::new(&ws.path);
    if !ws_path.exists() {
        return WorkspaceStatus {
            current_branch: None,
            head_sha: None,
            detached: false,
            health: WorkspaceHealth::Missing,
        };
    }

    let gitdir = match shard_core::git::resolve_gitdir(ws_path) {
        Ok(g) => g,
        Err(_) => {
            return WorkspaceStatus {
                current_branch: None,
                head_sha: None,
                detached: false,
                health: WorkspaceHealth::Broken,
            }
        }
    };

    match shard_core::git::read_head(&gitdir) {
        Ok(head) => WorkspaceStatus {
            current_branch: head.branch,
            head_sha: head.sha,
            detached: head.detached,
            health: WorkspaceHealth::Healthy,
        },
        Err(_) => WorkspaceStatus {
            current_branch: None,
            head_sha: None,
            detached: false,
            health: WorkspaceHealth::Broken,
        },
    }
}

/// Normalize a path into a key suitable for Windows-case-insensitive
/// equality. Does NOT canonicalize — the reconcile tick runs this for
/// every workspace every 30s including missing ones, and `canonicalize`
/// on a deleted path fails, so we handle path shape only (case, trailing
/// separators, `\\?\` extended-length prefix, forward vs back slashes).
///
/// This deliberately trades perfect accuracy (two symlinked paths will
/// not compare equal) for never-blocking the reconcile tick on a broken
/// filesystem entry. The only consumer is the porcelain cross-check in
/// `reconcile_repo_deep`, which is a classification hint — worst case we
/// misclassify a workspace as Broken and the frontend shows a badge until
/// the user restarts the daemon.
fn normalize_path(p: &Path) -> String {
    shard_core::git::strip_unc_prefix(p.to_path_buf())
        .to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}
