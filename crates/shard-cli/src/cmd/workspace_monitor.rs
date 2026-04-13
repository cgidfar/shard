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
use tokio::sync::{broadcast, mpsc, watch, RwLock};
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

struct MonitorInner {
    repos: RwLock<HashMap<String, Arc<RepoState>>>,
    change_tx: broadcast::Sender<String>,
    /// Topology-poke channel. `Some(alias)` scopes the reload to one repo;
    /// `None` means "reload everything" (used rarely, e.g. at startup).
    topology_tx: mpsc::UnboundedSender<Option<String>>,
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

    /// Subscribe to change notifications. Returns the alias of any repo whose
    /// `RepoState` just changed. Call `snapshot`/`get` to read the fresh value.
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.inner.change_tx.subscribe()
    }

    /// Fire-and-forget topology poke. Called by daemon IPC handlers after a
    /// client has committed a repo/workspace mutation. `None` requests a
    /// full reload; `Some(alias)` scopes it to one repo.
    pub fn poke_topology(&self, repo_alias: Option<String>) {
        let _ = self.inner.topology_tx.send(repo_alias);
    }
}

/// Spawn the WorkspaceMonitor as a detached tokio task and return its handle.
///
/// The task owns all filesystem watchers and ticks; the handle provides
/// read-only access to the latest state plus the topology-poke channel.
pub fn spawn(
    daemon_state: Arc<DaemonState>,
    shutdown_rx: watch::Receiver<ShutdownMode>,
) -> (MonitorHandle, tokio::task::JoinHandle<()>) {
    let (change_tx, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
    let (topology_tx, topology_rx) = mpsc::unbounded_channel::<Option<String>>();

    let inner = Arc::new(MonitorInner {
        repos: RwLock::new(HashMap::new()),
        change_tx,
        topology_tx,
    });

    let handle = MonitorHandle {
        inner: inner.clone(),
    };

    let task_state = daemon_state.clone();
    let task_inner = inner.clone();
    let task = tokio::spawn(async move {
        let mut monitor = WorkspaceMonitor::new(task_state, task_inner, topology_rx, shutdown_rx);
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
    topology_rx: mpsc::UnboundedReceiver<Option<String>>,
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
        topology_rx: mpsc::UnboundedReceiver<Option<String>>,
        shutdown_rx: watch::Receiver<ShutdownMode>,
    ) -> Self {
        let (notify_tx, notify_rx) = mpsc::unbounded_channel();
        let repo_store = RepositoryStore::new(daemon.paths.clone());
        let ws_store = WorkspaceStore::new(daemon.paths.clone());
        Self {
            daemon,
            inner,
            topology_rx,
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

        // Seed session liveness from the daemon's current registry.
        self.seed_session_liveness().await;
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
                Ok(gitdir) => match debouncer.watch(gitdir.as_path(), RecursiveMode::NonRecursive)
                {
                    Ok(()) => watched.push(gitdir),
                    Err(e) => debug!(
                        "watch gitdir {:?} failed for {alias}/{}: {e}",
                        gitdir, ws.name
                    ),
                },
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

    async fn seed_session_liveness(&mut self) {
        let sessions = self.daemon.sessions.lock().await;
        let mut by_repo: HashMap<String, HashMap<String, bool>> = HashMap::new();
        for (id, live) in sessions.iter() {
            by_repo
                .entry(live.repo.clone())
                .or_default()
                .insert(id.clone(), PlatformProcessControl::is_alive(live.supervisor_pid));
        }
        drop(sessions);

        let mut repos = self.inner.repos.write().await;
        for (alias, alive_map) in by_repo {
            if let Some(state_arc) = repos.get_mut(&alias) {
                let mut state = (**state_arc).clone();
                state.sessions_alive = alive_map;
                state.version += 1;
                *state_arc = Arc::new(state);
            }
        }
    }

    /// Emit a change notification for every repo currently in the map.
    /// Used after the initial load so any subscriber that connected before
    /// load finished gets a refresh.
    async fn broadcast_all(&self) {
        let aliases: Vec<String> = self.inner.repos.read().await.keys().cloned().collect();
        for alias in aliases {
            let _ = self.inner.change_tx.send(alias);
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

                // Topology changes (repo/workspace add/remove).
                Some(alias) = self.topology_rx.recv() => {
                    self.handle_topology_change(alias).await;
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

    async fn handle_topology_change(&mut self, alias: Option<String>) {
        match alias {
            Some(ref alias) => {
                match self.repo_store.get(alias) {
                    Ok(repo) => {
                        debug!("topology: reloading repo {alias}");
                        self.drop_repo(alias).await;
                        self.load_repo(&repo).await;
                        let _ = self.inner.change_tx.send(alias.clone());
                    }
                    Err(_) => {
                        debug!("topology: repo {alias} removed");
                        self.drop_repo(alias).await;
                        let _ = self.inner.change_tx.send(alias.clone());
                    }
                }
            }
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
        let workspaces = match self.ws_store.list(alias) {
            Ok(w) => w,
            Err(_) => Vec::new(),
        };

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
    async fn commit_workspace_map(
        &self,
        alias: &str,
        new_map: HashMap<String, WorkspaceStatus>,
    ) {
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
        let _ = self.inner.change_tx.send(alias.to_string());
    }

    /// Fast tick: PID liveness + DB exit transitions + tray update. This is
    /// the only periodic work in the daemon other than the 30s reconcile.
    async fn fast_tick(&mut self) {
        // Scan the daemon sessions map. Mirrors the old heartbeat_task — if
        // a supervisor's PID has died, mark it exited in the DB and remove
        // it from the live map. Then fold the live set into each repo's
        // RepoState.sessions_alive.
        let (dead_ids, current_count, by_repo): (
            Vec<(String, String)>,
            usize,
            HashMap<String, HashMap<String, bool>>,
        ) = {
            let mut sessions = self.daemon.sessions.lock().await;
            let mut dead = Vec::new();
            let mut by_repo: HashMap<String, HashMap<String, bool>> = HashMap::new();

            for (id, s) in sessions.iter() {
                let alive = PlatformProcessControl::is_alive(s.supervisor_pid);
                if !alive {
                    dead.push((id.clone(), s.repo.clone()));
                }
                by_repo
                    .entry(s.repo.clone())
                    .or_default()
                    .insert(id.clone(), alive);
            }
            for (id, _) in &dead {
                sessions.remove(id);
            }
            (dead, sessions.len(), by_repo)
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
        }

        // Tray icon update (absorbed from heartbeat_task).
        #[cfg(windows)]
        {
            let _ = self
                .daemon
                .tray_proxy
                .send_event(TrayEvent::SessionCount(current_count));
        }
        #[cfg(not(windows))]
        {
            let _ = current_count;
        }

        // Fold session liveness into RepoState for every repo that has live
        // sessions. Also clear sessions_alive for repos with none left so
        // dead rows drop from the UI.
        let mut repos = self.inner.repos.write().await;
        let mut changed_aliases: Vec<String> = Vec::new();

        // First pass: apply live liveness maps.
        for (alias, alive_map) in &by_repo {
            if let Some(existing) = repos.get(alias) {
                if existing.sessions_alive != *alive_map {
                    let mut state = (**existing).clone();
                    state.sessions_alive = alive_map.clone();
                    state.version += 1;
                    repos.insert(alias.clone(), Arc::new(state));
                    changed_aliases.push(alias.clone());
                }
            }
        }

        // Second pass: clear liveness for repos the scan didn't touch.
        let all_aliases: Vec<String> = repos.keys().cloned().collect();
        for alias in all_aliases {
            if by_repo.contains_key(&alias) {
                continue;
            }
            if let Some(existing) = repos.get(&alias) {
                if !existing.sessions_alive.is_empty() {
                    let mut state = (**existing).clone();
                    state.sessions_alive.clear();
                    state.version += 1;
                    repos.insert(alias.clone(), Arc::new(state));
                    changed_aliases.push(alias);
                }
            }
        }
        drop(repos);

        for alias in changed_aliases {
            let _ = self.inner.change_tx.send(alias);
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
        debug!("WorkspaceMonitor: reconcile tick over {} repo(s)", repos.len());

        for repo in &repos {
            self.reconcile_repo_deep(repo).await;
        }
    }

    async fn reconcile_repo_deep(&self, repo: &Repository) {
        let alias = &repo.alias;
        let workspaces = match self.ws_store.list(alias) {
            Ok(w) => w,
            Err(_) => Vec::new(),
        };

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
