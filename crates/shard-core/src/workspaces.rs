use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::db;
use crate::git;
use crate::identifiers::{safe_workspace_name, validate_repo_alias, validate_workspace_name};
use crate::paths::ShardPaths;
use crate::repos::RepositoryStore;
use crate::{Result, ShardError};

/// Narrow abstraction over the git operations the daemon's workspace-remove
/// workflow needs. Lets integration tests inject failures at specific steps
/// (e.g., force `worktree_remove` to fail so the `broken` state transition
/// is exercised).
///
/// Production uses [`RealGitOps`]; tests plug in a stub.
pub trait WorkspaceGitOps: Send + Sync {
    fn worktree_remove(&self, repo_dir: &Path, worktree_path: &Path) -> Result<()>;
    fn worktree_prune(&self, repo_dir: &Path) -> Result<()>;
    fn worktree_list_porcelain(
        &self,
        repo_dir: &Path,
    ) -> Result<Vec<git::WorktreeEntry>>;
}

/// Production git-ops implementation — delegates straight to
/// [`crate::git`].
pub struct RealGitOps;

impl WorkspaceGitOps for RealGitOps {
    fn worktree_remove(&self, repo_dir: &Path, worktree_path: &Path) -> Result<()> {
        git::worktree_remove(repo_dir, worktree_path)
    }

    fn worktree_prune(&self, repo_dir: &Path) -> Result<()> {
        git::worktree_prune(repo_dir)
    }

    fn worktree_list_porcelain(
        &self,
        repo_dir: &Path,
    ) -> Result<Vec<git::WorktreeEntry>> {
        git::worktree_list_porcelain(repo_dir)
    }
}

/// Convenience constructor for the default git-ops impl.
pub fn default_git_ops() -> Arc<dyn WorkspaceGitOps> {
    Arc::new(RealGitOps)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Workspace {
    pub name: String,
    pub branch: String,
    pub path: String,
    pub is_base: bool,
    pub created_at: u64,
}

/// How a workspace obtains its branch.
///
/// `NewBranch` creates a fresh branch based on `branch` (the base) and checks
/// it out in a new worktree. `ExistingBranch` checks out an existing branch
/// in a new worktree; it will fail with `ShardError::BranchAlreadyCheckedOut`
/// if the branch is already HEAD of another live worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode {
    NewBranch,
    ExistingBranch,
}

/// A branch in the repo source, enriched with worktree occupancy info.
///
/// Surfaces to the frontend so the new-workspace wizard can pick a base
/// branch (mode = NewBranch) or pick an existing branch to check out
/// (mode = ExistingBranch) and warn when that branch is already claimed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BranchInfo {
    pub name: String,
    pub is_head: bool,
    pub checked_out_by: Option<String>,
}

/// A workspace persisted row enriched with live derived status from the
/// daemon's monitor. Returned by `ControlFrame::ListWorkspaces` so callers
/// get both halves in one round trip instead of joining them client-side.
///
/// Kept in `shard-core` (not `shard-app`) so both the Tauri backend and
/// `shardctl` can render the same shape.
///
/// `status` is `None` when the daemon has no snapshot for this repo yet
/// (e.g. right after `AddRepo`, before the monitor's first tick). The
/// frontend must handle this as "unknown", not "unhealthy".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceWithStatus {
    #[serde(flatten)]
    pub workspace: Workspace,
    pub status: Option<crate::state::WorkspaceStatus>,
}

pub struct WorkspaceStore {
    paths: ShardPaths,
}

impl WorkspaceStore {
    pub fn new(paths: ShardPaths) -> Self {
        Self { paths }
    }

    /// Resolve the effective workspace name from `(name, mode, branch)`
    /// WITHOUT touching disk or the DB. Side-effect-free except for the
    /// git `default_branch` lookup (a plumbing `git symbolic-ref` — no
    /// fetch, no commit).
    ///
    /// Separated from `create` so the daemon's mutation handler can
    /// gate-check the resolved name against the lifecycle registry
    /// before any work is committed. Mirrors the exact resolution logic
    /// in `create` — if it drifts, the gate check stops matching what
    /// `create` writes to the DB.
    ///
    /// Only models the non-base path. `is_base=true` workspaces are
    /// only created during `AddRepo`, which has its own handler in
    /// Phase 3.
    pub fn resolve_workspace_name(
        &self,
        repo_alias: &str,
        name: Option<&str>,
        mode: WorkspaceMode,
        branch: Option<&str>,
    ) -> Result<String> {
        validate_repo_alias(repo_alias)?;
        let repo_store = RepositoryStore::new(self.paths.clone());
        let repo = repo_store.get(repo_alias)?;
        let source_dir = self
            .paths
            .repo_source_for_repo(repo_alias, repo.local_path.as_deref());

        let branch_for_db = match mode {
            WorkspaceMode::NewBranch => match branch {
                Some(b) => b.to_string(),
                None => git::default_branch(&source_dir)?,
            },
            WorkspaceMode::ExistingBranch => branch
                .ok_or_else(|| {
                    ShardError::Other("existing_branch mode requires a branch name".into())
                })?
                .to_string(),
        };

        let resolved = resolve_workspace_name_from_branch(name, mode, &branch_for_db);
        validate_workspace_name(&resolved)?;
        Ok(resolved)
    }

    /// Create a new workspace for a repo.
    ///
    /// `is_base` means the workspace points to the original checkout
    /// (no git worktree created, `branch` is ignored). Otherwise a git
    /// worktree is created.
    ///
    /// For non-base workspaces, `mode` picks the branch semantics:
    /// - `NewBranch`: `branch` is the base to fork from (defaults to HEAD).
    ///   The workspace name becomes the new branch name.
    /// - `ExistingBranch`: `branch` is the existing branch to check out.
    ///   Fails with `BranchAlreadyCheckedOut` if that branch is already
    ///   HEAD of another live worktree.
    pub fn create(
        &self,
        repo_alias: &str,
        name: Option<&str>,
        mode: WorkspaceMode,
        branch: Option<&str>,
        is_base: bool,
    ) -> Result<Workspace> {
        validate_repo_alias(repo_alias)?;
        let repo_store = RepositoryStore::new(self.paths.clone());
        let repo = repo_store.get(repo_alias)?;

        let source_dir = self.paths.repo_source_for_repo(
            repo_alias,
            repo.local_path.as_deref(),
        );

        let default_branch =
            || git::default_branch(&source_dir);

        let (branch_for_db, base_branch, new_branch) = if is_base {
            // is_base ignores mode; branch_for_db = current HEAD.
            let head = default_branch()?;
            (head.clone(), head, None)
        } else {
            match mode {
                WorkspaceMode::NewBranch => {
                    let base = match branch {
                        Some(b) => b.to_string(),
                        None => default_branch()?,
                    };
                    let ws_name = match name {
                        Some(n) => n.to_string(),
                        None => base.clone(),
                    };
                    // If the requested name matches the base, reuse it
                    // (no new branch) — preserves prior behavior where
                    // "new workspace on <branch>" meant "check out <branch>".
                    if ws_name == base {
                        (base.clone(), base, None)
                    } else {
                        (ws_name.clone(), base, Some(ws_name))
                    }
                }
                WorkspaceMode::ExistingBranch => {
                    let target = branch.ok_or_else(|| {
                        ShardError::Other(
                            "existing_branch mode requires a branch name".into(),
                        )
                    })?;
                    (target.to_string(), target.to_string(), None)
                }
            }
        };

        let ws_name = resolve_workspace_name_from_branch(name, mode, &branch_for_db);
        validate_workspace_name(&ws_name)?;

        // Check for duplicates in DB
        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_connection(&repo_db_path)?;

        let exists: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM workspaces WHERE name = ?1",
            params![ws_name],
            |row| row.get(0),
        )?;
        if exists {
            return Err(ShardError::WorkspaceAlreadyExists(ws_name));
        }

        // Pre-flight: any path that checks out an existing branch (either
        // ExistingBranch mode, or NewBranch where name matches an existing
        // branch) must not collide with another live worktree. Git would
        // reject the `worktree add` with a generic error; we front-run it
        // to surface the owning workspace name.
        if !is_base && new_branch.is_none() {
            if let Some(owner) = self.worktree_owning_branch(
                repo_alias,
                &source_dir,
                &branch_for_db,
            )? {
                return Err(ShardError::BranchAlreadyCheckedOut {
                    branch: branch_for_db,
                    workspace: owner,
                });
            }
        }

        let ws_dir = if is_base {
            // Base workspace = original checkout, no worktree needed
            let lp = repo.local_path.as_deref().ok_or_else(|| {
                ShardError::Other("is_base=true but repo has no local_path".into())
            })?;
            std::path::PathBuf::from(lp)
        } else {
            // Create a git worktree
            let dir = self.paths.workspace_dir_for_repo(
                repo_alias,
                &ws_name,
                repo.local_path.as_deref(),
            );
            tracing::info!("creating worktree at {}", dir.display());
            git::worktree_add(
                &source_dir,
                &dir,
                &base_branch,
                new_branch.as_deref(),
            )?;

            // For local repos, hide .shard/ from git status on first worktree
            if repo.local_path.is_some() {
                let _ = git::add_to_exclude(&source_dir, ".shard/");
            }

            dir
        };

        // Record in DB
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let path_str = ws_dir.to_string_lossy().to_string();

        conn.execute(
            "INSERT INTO workspaces (name, branch, path, is_base, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![ws_name, branch_for_db, path_str, is_base as i32, now],
        )?;

        Ok(Workspace {
            name: ws_name,
            branch: branch_for_db,
            path: path_str,
            is_base,
            created_at: now,
        })
    }

    /// List branches in the repo source, marking which is HEAD and which
    /// are already checked out in a live worktree.
    ///
    /// Cross-references `git worktree list --porcelain` with the DB's
    /// workspace rows so each occupied branch is labeled with the
    /// workspace name rather than a raw path.
    pub fn list_branch_info(&self, repo_alias: &str) -> Result<Vec<BranchInfo>> {
        validate_repo_alias(repo_alias)?;
        let repo_store = RepositoryStore::new(self.paths.clone());
        let repo = repo_store.get(repo_alias)?;
        let source_dir = self.paths.repo_source_for_repo(
            repo_alias,
            repo.local_path.as_deref(),
        );

        let branches = match git::list_branches(&source_dir) {
            Ok(b) => b,
            Err(_) => Vec::new(),
        };
        let head = git::default_branch(&source_dir).ok();

        let workspaces = self.list(repo_alias).unwrap_or_default();
        let path_to_workspace: std::collections::HashMap<String, String> = workspaces
            .iter()
            .map(|ws| (normalize_worktree_path(std::path::Path::new(&ws.path)), ws.name.clone()))
            .collect();

        let mut branch_to_workspace: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        if let Ok(entries) = git::worktree_list_porcelain(&source_dir) {
            for entry in entries {
                if entry.prunable || entry.detached {
                    continue;
                }
                let (Some(branch), key) = (entry.branch.clone(), normalize_worktree_path(&entry.path))
                else {
                    continue;
                };
                let label = path_to_workspace
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| external_worktree_label(&entry.path));
                branch_to_workspace.insert(branch, label);
            }
        }

        Ok(branches
            .into_iter()
            .map(|name| {
                let is_head = head.as_deref() == Some(name.as_str());
                let checked_out_by = branch_to_workspace.get(&name).cloned();
                BranchInfo {
                    name,
                    is_head,
                    checked_out_by,
                }
            })
            .collect())
    }

    /// If `branch` is currently HEAD of a live (non-detached, non-prunable)
    /// worktree, return a label identifying it — the Shard workspace name
    /// if the worktree is managed, otherwise a descriptive marker so the
    /// caller can still refuse to duplicate the checkout.
    fn worktree_owning_branch(
        &self,
        repo_alias: &str,
        source_dir: &std::path::Path,
        branch: &str,
    ) -> Result<Option<String>> {
        validate_repo_alias(repo_alias)?;
        let entries = git::worktree_list_porcelain(source_dir)?;
        let match_entry = entries
            .iter()
            .find(|e| !e.prunable && !e.detached && e.branch.as_deref() == Some(branch));
        let Some(entry) = match_entry else {
            return Ok(None);
        };
        let key = normalize_worktree_path(&entry.path);
        let workspaces = self.list(repo_alias).unwrap_or_default();
        let managed = workspaces
            .into_iter()
            .find(|ws| normalize_worktree_path(std::path::Path::new(&ws.path)) == key)
            .map(|ws| ws.name);
        Ok(Some(managed.unwrap_or_else(|| external_worktree_label(&entry.path))))
    }

    /// List all workspaces for a repo.
    pub fn list(&self, repo_alias: &str) -> Result<Vec<Workspace>> {
        validate_repo_alias(repo_alias)?;
        // Verify the repo exists
        let repo_store = RepositoryStore::new(self.paths.clone());
        let _repo = repo_store.get(repo_alias)?;

        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_connection(&repo_db_path)?;

        let mut stmt = conn.prepare(
            "SELECT name, branch, path, is_base, created_at FROM workspaces ORDER BY name",
        )?;
        let workspaces = stmt.query_map([], |row| {
            let is_base_int: i32 = row.get(3)?;
            Ok(Workspace {
                name: row.get(0)?,
                branch: row.get(1)?,
                path: row.get(2)?,
                is_base: is_base_int != 0,
                created_at: row.get(4)?,
            })
        })?;

        let mut result = Vec::new();
        for ws in workspaces {
            result.push(ws?);
        }
        Ok(result)
    }

    /// Get a specific workspace by repo alias and workspace name.
    pub fn get(&self, repo_alias: &str, ws_name: &str) -> Result<Workspace> {
        validate_repo_alias(repo_alias)?;
        validate_workspace_name(ws_name)?;
        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_connection(&repo_db_path)?;

        conn.query_row(
            "SELECT name, branch, path, is_base, created_at FROM workspaces WHERE name = ?1",
            params![ws_name],
            |row| {
                let is_base_int: i32 = row.get(3)?;
                Ok(Workspace {
                    name: row.get(0)?,
                    branch: row.get(1)?,
                    path: row.get(2)?,
                    is_base: is_base_int != 0,
                    created_at: row.get(4)?,
                })
            },
        )
        .map_err(|_| ShardError::WorkspaceNotFound(format!("{repo_alias}:{ws_name}")))
    }

    /// Remove a workspace (git worktree + DB record).
    ///
    /// If the workspace is a base checkout (`is_base=true`), only the DB record
    /// is removed — the original directory is never touched.
    ///
    /// Handles three filesystem states for non-base workspaces:
    ///
    /// - **Healthy**: `git worktree remove --force` succeeds and removes the
    ///   directory + admin entry in one call.
    /// - **Missing** (directory gone): skips git remove and runs `worktree
    ///   prune` to drop any stale `.git/worktrees/<name>/` admin entry that
    ///   survived the directory deletion.
    /// - **Broken** (directory exists but git doesn't recognize it as a
    ///   worktree — happens after manual deletion + recreation, after a
    ///   prior prune, or when a directory was created out-of-band): falls
    ///   back to `prune` + `std::fs::remove_dir_all`. Without this fallback,
    ///   the user's only path to clean up a broken row is to manually
    ///   delete files then re-run remove, which the UI doesn't expose.
    pub fn remove(&self, repo_alias: &str, ws_name: &str) -> Result<()> {
        validate_repo_alias(repo_alias)?;
        validate_workspace_name(ws_name)?;
        let ws = self.get(repo_alias, ws_name)?;

        if !ws.is_base {
            let repo_store = RepositoryStore::new(self.paths.clone());
            let repo = repo_store.get(repo_alias)?;
            let source_dir = self.paths.repo_source_for_repo(
                repo_alias,
                repo.local_path.as_deref(),
            );
            let ws_dir = std::path::PathBuf::from(&ws.path);

            if ws_dir.exists() {
                // Try the clean path first. Only fall back to manual deletion
                // when git no longer recognizes this path as a registered
                // worktree; any other failure must preserve the directory so
                // we don't discard local state on transient git errors.
                if let Err(remove_err) = git::worktree_remove(&source_dir, &ws_dir) {
                    match is_registered_worktree(&source_dir, &ws_dir) {
                        Ok(false) => {
                            git::worktree_prune(&source_dir)?;
                            std::fs::remove_dir_all(&ws_dir).map_err(|e| {
                                ShardError::Other(format!(
                                    "failed to remove worktree directory {}: {}",
                                    ws_dir.display(),
                                    e
                                ))
                            })?;
                        }
                        Ok(true) | Err(_) => return Err(remove_err),
                    }
                }
            } else {
                // Missing state: dir already gone. Prune in case git still
                // has an admin entry pointing at the vanished path.
                let _ = git::worktree_prune(&source_dir);
            }
        }

        // Remove from DB
        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_connection(&repo_db_path)?;
        conn.execute("DELETE FROM workspaces WHERE name = ?1", params![ws_name])?;

        Ok(())
    }

    /// Delete the DB row for a workspace. Does NOT touch the filesystem or
    /// run any git commands. Used by the daemon's `RemoveWorkspace`
    /// workflow after the filesystem side (via [`remove_worktree_fs`]) has
    /// already succeeded.
    pub fn delete_row(&self, repo_alias: &str, ws_name: &str) -> Result<()> {
        validate_repo_alias(repo_alias)?;
        validate_workspace_name(ws_name)?;
        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_connection(&repo_db_path)?;
        conn.execute(
            "DELETE FROM workspaces WHERE name = ?1",
            params![ws_name],
        )?;
        Ok(())
    }
}

/// Run the filesystem side of a workspace remove through the given
/// [`WorkspaceGitOps`]: `git worktree remove --force` with a prune + manual
/// `remove_dir_all` fallback for broken rows. Mirrors the logic embedded
/// in [`WorkspaceStore::remove`] but decoupled from the DB step so the
/// daemon can interleave state-machine transitions between them.
///
/// Returns `Ok(())` if the workspace directory and git admin entry are
/// gone after this call. Errors are recoverable — the caller should mark
/// the workspace `broken` and preserve the DB row.
pub fn remove_worktree_fs(
    git_ops: &dyn WorkspaceGitOps,
    source_dir: &Path,
    ws_dir: &Path,
) -> Result<()> {
    if ws_dir.exists() {
        if let Err(remove_err) = git_ops.worktree_remove(source_dir, ws_dir) {
            // Only fall back to manual deletion when git no longer
            // recognizes this path as a registered worktree. Any other
            // failure must preserve the directory — a transient git
            // error shouldn't blow away local state.
            let registered = git_ops
                .worktree_list_porcelain(source_dir)
                .map(|entries| {
                    let key = normalize_worktree_path(ws_dir);
                    entries
                        .iter()
                        .any(|e| normalize_worktree_path(&e.path) == key)
                })
                .unwrap_or(true);

            if registered {
                return Err(remove_err);
            }

            git_ops.worktree_prune(source_dir)?;
            std::fs::remove_dir_all(ws_dir).map_err(|e| {
                ShardError::Other(format!(
                    "failed to remove worktree directory {}: {}",
                    ws_dir.display(),
                    e
                ))
            })?;
        }
    } else {
        // Directory already gone — prune any stale admin entry.
        let _ = git_ops.worktree_prune(source_dir);
    }
    Ok(())
}

fn is_registered_worktree(repo_dir: &std::path::Path, ws_dir: &std::path::Path) -> Result<bool> {
    let ws_key = normalize_worktree_path(ws_dir);
    let entries = git::worktree_list_porcelain(repo_dir)?;
    Ok(entries
        .iter()
        .any(|entry| normalize_worktree_path(&entry.path) == ws_key))
}

fn external_worktree_label(path: &std::path::Path) -> String {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    if name.is_empty() {
        "(external worktree)".into()
    } else {
        format!("(external: {name})")
    }
}

fn resolve_workspace_name_from_branch(
    name: Option<&str>,
    mode: WorkspaceMode,
    branch_for_db: &str,
) -> String {
    match (mode, name) {
        (WorkspaceMode::ExistingBranch, None) => safe_workspace_name(branch_for_db),
        (WorkspaceMode::ExistingBranch, Some(n)) if n == branch_for_db => {
            safe_workspace_name(branch_for_db)
        }
        (WorkspaceMode::NewBranch, None) => safe_workspace_name(branch_for_db),
        (WorkspaceMode::NewBranch, Some(n)) if n == branch_for_db => {
            safe_workspace_name(branch_for_db)
        }
        (_, Some(n)) => n.to_string(),
    }
}

fn normalize_worktree_path(path: &std::path::Path) -> String {
    std::fs::canonicalize(path)
        .map(git::strip_unc_prefix)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_worktree_path_ignores_case_and_separator_style() {
        let a =
            normalize_worktree_path(std::path::Path::new("C:\\Repos\\Shard\\.shard\\Feature\\"));
        let b = normalize_worktree_path(std::path::Path::new("c:/repos/shard/.shard/feature"));
        assert_eq!(a, b);
    }
}
