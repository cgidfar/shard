//! Runtime state types shared between the daemon monitor and its subscribers.
//!
//! These are derived-state primitives — not persisted to SQLite. The daemon
//! computes them from git + filesystem + live process state and fans them out
//! to subscribers (shard-app today, shardctl observers later) as versioned
//! per-repo snapshots.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Health classification for a workspace's on-disk + git state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceHealth {
    /// Path exists, git knows about it (for non-base worktrees), HEAD readable.
    Healthy,
    /// Worktree directory is gone from disk.
    Missing,
    /// Dir exists but git registration is inconsistent (e.g. `git worktree
    /// list` doesn't know about it), or HEAD cannot be read.
    Broken,
}

/// Live, derived status for a single workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceStatus {
    /// Short branch name (without `refs/heads/`). `None` when detached or missing.
    pub current_branch: Option<String>,
    /// Commit SHA currently pointed to by HEAD. `None` when missing / unreadable.
    pub head_sha: Option<String>,
    /// True when HEAD is a bare SHA (rebase / bisect / explicit detach).
    pub detached: bool,
    pub health: WorkspaceHealth,
}

/// Per-repo snapshot the daemon broadcasts to subscribers.
///
/// Everything is keyed by the user-visible alias/name rather than internal IDs
/// so the frontend can match against the existing sidebar tree without an
/// extra lookup layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoState {
    pub repo_alias: String,
    /// Monotonic version number, bumped on every observed change. Subscribers
    /// ignore snapshots with version <= their last seen value, which makes
    /// out-of-order delivery across reconnects harmless.
    pub version: u64,
    pub workspaces: HashMap<String, WorkspaceStatus>,
}

impl RepoState {
    pub fn new(repo_alias: impl Into<String>) -> Self {
        Self {
            repo_alias: repo_alias.into(),
            version: 0,
            workspaces: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_repo_state_is_empty_with_zero_version() {
        let state = RepoState::new("my-repo");
        assert_eq!(state.repo_alias, "my-repo");
        assert_eq!(state.version, 0);
        assert!(state.workspaces.is_empty());
    }

    #[test]
    fn workspace_status_equality_ignores_hashmap_order() {
        let a = WorkspaceStatus {
            current_branch: Some("main".to_string()),
            head_sha: Some("abc123".to_string()),
            detached: false,
            health: WorkspaceHealth::Healthy,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn workspace_health_serializes_lowercase() {
        // The wire encoding expects the string form to be lowercase so the
        // frontend's TS union type ("healthy" | "missing" | "broken") aligns.
        let json = serde_json::to_string(&WorkspaceHealth::Healthy).unwrap();
        assert_eq!(json, "\"healthy\"");
        let json = serde_json::to_string(&WorkspaceHealth::Missing).unwrap();
        assert_eq!(json, "\"missing\"");
        let json = serde_json::to_string(&WorkspaceHealth::Broken).unwrap();
        assert_eq!(json, "\"broken\"");
    }
}
