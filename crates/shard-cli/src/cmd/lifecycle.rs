//! Per-workspace lifecycle state machine (D12 of the daemon-broker plan).
//!
//! The daemon tracks an explicit `active | deleting | broken | gone` state
//! for every workspace it knows about. This is the serialization point that
//! makes `RemoveWorkspace` safe against concurrent `SpawnSession` or a
//! second `RemoveWorkspace` on the same target:
//!
//!   - `begin_delete` atomically checks the current state and transitions
//!     `Active`/`Broken` → `Deleting`. Concurrent deletes of the same
//!     target are *joined* (the second caller waits on the first's
//!     completion notifier and returns its outcome).
//!   - `check_can_mutate` is the fast-fail check used by
//!     `SpawnSession`/`CreateWorkspace` — returns an error if the target
//!     is currently being deleted.
//!   - `commit_gone` / `commit_broken` / `rollback` finalize an in-flight
//!     delete. Dropping a `DeleteGuard` without committing rolls back to
//!     `Active` (or `Broken`, if the initial state was `Broken`).
//!
//! Phase 0 lands this infrastructure only. Phase 1 wires it into the
//! `RemoveWorkspace` handler.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

/// Per-workspace lifecycle state.
#[derive(Debug, Clone)]
#[allow(dead_code)] // variants read by Phase 1
pub enum WorkspaceLifecycle {
    /// Normal state. Mutations are accepted.
    Active,
    /// A delete is in flight. Concurrent mutations are rejected; concurrent
    /// deletes wait on `completion` and inherit the outcome.
    Deleting { completion: Arc<Notify> },
    /// Partial cleanup left the workspace half-removed. A retried
    /// `RemoveWorkspace` can pick up where the previous attempt left off.
    Broken,
}

/// Outcome of a `begin_delete` call.
#[derive(Debug)]
#[allow(dead_code)] // consumed by Phase 1
pub enum BeginDelete {
    /// Caller owns the delete and must eventually commit via the guard.
    Started(DeleteGuard),
    /// Another delete is already in flight — await the returned notifier,
    /// then re-query state. After the wait, if the workspace is still
    /// `Active` the first caller rolled back; the joiner should retry.
    /// If it's `Broken`, surface the error. If it's absent, the delete
    /// succeeded and the joiner can return Ack.
    AlreadyDeleting(Arc<Notify>),
}

/// Error returned by the pre-mutation guard check.
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum LifecycleError {
    #[error("workspace '{repo}:{name}' is being deleted")]
    Deleting { repo: String, name: String },
    #[error("workspace '{repo}:{name}' is in broken state — call RemoveWorkspace to retry cleanup")]
    Broken { repo: String, name: String },
}

type StateMap = HashMap<(String, String), WorkspaceLifecycle>;

/// The daemon's lifecycle state table. Embedded in `DaemonState`.
///
/// A single `std::sync::Mutex` protects the whole table. Every critical
/// section is short (read/write a single enum; no I/O, no awaits) so the
/// std-mutex is the right choice — a tokio mutex would add await overhead
/// to hot-path checks for no benefit.
pub struct LifecycleRegistry {
    states: Mutex<StateMap>,
}

impl LifecycleRegistry {
    pub fn new() -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
        }
    }

    /// Populate the registry with a workspace in `Active` state. Idempotent.
    /// Called on startup for every known workspace, and after successful
    /// `CreateWorkspace` to register the new entry.
    #[allow(dead_code)] // consumed by Phase 1+
    pub fn register_active(&self, repo: &str, name: &str) {
        let mut map = self.states.lock().expect("lifecycle mutex poisoned");
        map.entry((repo.to_string(), name.to_string()))
            .or_insert(WorkspaceLifecycle::Active);
    }

    /// Pre-mutation guard: fail fast if the target is currently unreachable
    /// (being deleted or in broken state). Called from `SpawnSession` and
    /// `CreateWorkspace` before any work begins.
    #[allow(dead_code)]
    pub fn check_can_mutate(&self, repo: &str, name: &str) -> Result<(), LifecycleError> {
        let map = self.states.lock().expect("lifecycle mutex poisoned");
        match map.get(&(repo.to_string(), name.to_string())) {
            Some(WorkspaceLifecycle::Deleting { .. }) => Err(LifecycleError::Deleting {
                repo: repo.to_string(),
                name: name.to_string(),
            }),
            Some(WorkspaceLifecycle::Broken) => Err(LifecycleError::Broken {
                repo: repo.to_string(),
                name: name.to_string(),
            }),
            Some(WorkspaceLifecycle::Active) | None => Ok(()),
        }
    }

    /// Attempt to begin a delete. Atomic under the mutex: reads the
    /// current state, transitions `Active`/`Broken`/absent → `Deleting`,
    /// or returns `AlreadyDeleting(notifier)` if another delete is
    /// already in flight on the same target.
    ///
    /// Absent entries are treated the same as `Active` — the lifecycle
    /// map only tracks workspaces that have been observed by a daemon
    /// mutation handler or startup scan. A `RemoveWorkspace` on an
    /// unseen workspace is legal (the DB is the source of truth for
    /// whether the row exists); the gate is about serializing
    /// concurrent mutations, not about proving existence.
    #[allow(dead_code)]
    pub fn begin_delete(self: &Arc<Self>, repo: &str, name: &str) -> BeginDelete {
        let mut map = self.states.lock().expect("lifecycle mutex poisoned");
        let key = (repo.to_string(), name.to_string());
        match map.get(&key) {
            Some(WorkspaceLifecycle::Deleting { completion }) => {
                BeginDelete::AlreadyDeleting(completion.clone())
            }
            existing => {
                let prior_broken =
                    matches!(existing, Some(WorkspaceLifecycle::Broken));
                let completion = Arc::new(Notify::new());
                map.insert(
                    key.clone(),
                    WorkspaceLifecycle::Deleting {
                        completion: completion.clone(),
                    },
                );
                BeginDelete::Started(DeleteGuard {
                    registry: Arc::clone(self),
                    repo: repo.to_string(),
                    name: name.to_string(),
                    completion,
                    prior_broken,
                    finalized: false,
                })
            }
        }
    }
}

impl Default for LifecycleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard for an in-flight delete. The caller MUST call one of
/// `commit_gone`, `commit_broken`, or `rollback`. If the guard is dropped
/// without a commit (e.g., a panic), the state rolls back to `Active` (or
/// `Broken` if the delete was retrying a previously-broken entry) and the
/// completion notifier fires so any joiners unblock.
#[allow(dead_code)] // consumed by Phase 1
pub struct DeleteGuard {
    registry: Arc<LifecycleRegistry>,
    repo: String,
    name: String,
    completion: Arc<Notify>,
    prior_broken: bool,
    finalized: bool,
}

impl std::fmt::Debug for DeleteGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeleteGuard")
            .field("repo", &self.repo)
            .field("name", &self.name)
            .field("prior_broken", &self.prior_broken)
            .field("finalized", &self.finalized)
            .finish()
    }
}

#[allow(dead_code)]
impl DeleteGuard {
    /// Finalize: workspace is gone. Removes the entry from the map and
    /// fires the completion notifier.
    pub fn commit_gone(mut self) {
        {
            let mut map = self.registry.states.lock().expect("lifecycle mutex poisoned");
            map.remove(&(self.repo.clone(), self.name.clone()));
        }
        self.completion.notify_waiters();
        self.finalized = true;
    }

    /// Finalize: cleanup failed partway. Leaves a `Broken` record and
    /// fires the completion notifier so any joiners see the outcome.
    pub fn commit_broken(mut self) {
        {
            let mut map = self.registry.states.lock().expect("lifecycle mutex poisoned");
            map.insert(
                (self.repo.clone(), self.name.clone()),
                WorkspaceLifecycle::Broken,
            );
        }
        self.completion.notify_waiters();
        self.finalized = true;
    }

    /// Abort: nothing on disk was touched, revert to the prior state.
    pub fn rollback(mut self) {
        {
            let mut map = self.registry.states.lock().expect("lifecycle mutex poisoned");
            let state = if self.prior_broken {
                WorkspaceLifecycle::Broken
            } else {
                WorkspaceLifecycle::Active
            };
            map.insert((self.repo.clone(), self.name.clone()), state);
        }
        self.completion.notify_waiters();
        self.finalized = true;
    }
}

impl Drop for DeleteGuard {
    fn drop(&mut self) {
        if !self.finalized {
            // Unclean drop (panic, early return). Best-effort rollback so
            // the state doesn't stay stuck on `Deleting`.
            let mut map = self.registry.states.lock().expect("lifecycle mutex poisoned");
            let state = if self.prior_broken {
                WorkspaceLifecycle::Broken
            } else {
                WorkspaceLifecycle::Active
            };
            map.insert((self.repo.clone(), self.name.clone()), state);
            drop(map);
            self.completion.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_check_active() {
        let reg = Arc::new(LifecycleRegistry::new());
        reg.register_active("repo", "main");
        assert!(reg.check_can_mutate("repo", "main").is_ok());
    }

    #[test]
    fn unregistered_passes_check() {
        let reg = Arc::new(LifecycleRegistry::new());
        assert!(reg.check_can_mutate("repo", "never-seen").is_ok());
    }

    #[test]
    fn delete_then_gone_removes_entry() {
        let reg = Arc::new(LifecycleRegistry::new());
        reg.register_active("repo", "feature");
        match reg.begin_delete("repo", "feature") {
            BeginDelete::Started(guard) => guard.commit_gone(),
            other => panic!("unexpected: {other:?}"),
        }
        assert!(reg.check_can_mutate("repo", "feature").is_ok());
    }

    #[test]
    fn second_delete_joins_first() {
        let reg = Arc::new(LifecycleRegistry::new());
        reg.register_active("repo", "feature");
        let _first = match reg.begin_delete("repo", "feature") {
            BeginDelete::Started(g) => g,
            other => panic!("unexpected first: {other:?}"),
        };
        match reg.begin_delete("repo", "feature") {
            BeginDelete::AlreadyDeleting(_) => {}
            other => panic!("expected AlreadyDeleting, got {other:?}"),
        }
    }

    #[test]
    fn deleting_blocks_mutations() {
        let reg = Arc::new(LifecycleRegistry::new());
        reg.register_active("repo", "feature");
        let _guard = match reg.begin_delete("repo", "feature") {
            BeginDelete::Started(g) => g,
            other => panic!("unexpected: {other:?}"),
        };
        match reg.check_can_mutate("repo", "feature") {
            Err(LifecycleError::Deleting { .. }) => {}
            other => panic!("expected Deleting error, got {other:?}"),
        }
    }

    #[test]
    fn rollback_restores_active() {
        let reg = Arc::new(LifecycleRegistry::new());
        reg.register_active("repo", "feature");
        match reg.begin_delete("repo", "feature") {
            BeginDelete::Started(g) => g.rollback(),
            other => panic!("unexpected: {other:?}"),
        }
        assert!(reg.check_can_mutate("repo", "feature").is_ok());
    }

    #[test]
    fn broken_blocks_mutations_but_allows_retry_delete() {
        let reg = Arc::new(LifecycleRegistry::new());
        reg.register_active("repo", "feature");
        match reg.begin_delete("repo", "feature") {
            BeginDelete::Started(g) => g.commit_broken(),
            other => panic!("unexpected: {other:?}"),
        }
        match reg.check_can_mutate("repo", "feature") {
            Err(LifecycleError::Broken { .. }) => {}
            other => panic!("expected Broken error, got {other:?}"),
        }
        // retry delete from Broken should work
        match reg.begin_delete("repo", "feature") {
            BeginDelete::Started(g) => g.commit_gone(),
            other => panic!("expected retry to start, got {other:?}"),
        }
    }

    #[test]
    fn drop_without_commit_rolls_back() {
        let reg = Arc::new(LifecycleRegistry::new());
        reg.register_active("repo", "feature");
        {
            let _g = match reg.begin_delete("repo", "feature") {
                BeginDelete::Started(g) => g,
                other => panic!("unexpected: {other:?}"),
            };
        }
        assert!(reg.check_can_mutate("repo", "feature").is_ok());
    }
}
