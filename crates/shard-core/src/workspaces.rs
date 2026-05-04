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
    fn worktree_list_porcelain(&self, repo_dir: &Path) -> Result<Vec<git::WorktreeEntry>>;
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

    fn worktree_list_porcelain(&self, repo_dir: &Path) -> Result<Vec<git::WorktreeEntry>> {
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
    /// True when Shard did not create this worktree — the path was registered
    /// via `WorkspaceStore::adopt`. Skip filesystem teardown on remove (mirrors
    /// `is_base` semantics) so the user's externally-managed directory stays
    /// intact.
    pub is_external: bool,
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
///
/// `external_path` carries the porcelain-reported path when the branch is
/// checked out in an externally-managed worktree (one absent from the DB).
/// The frontend uses it as the source of truth for adopt-mode dispatch —
/// `checked_out_by` remains a display-only label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BranchInfo {
    pub name: String,
    pub is_head: bool,
    pub checked_out_by: Option<String>,
    pub external_path: Option<String>,
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

    /// Side-effect-free name resolution for an adopt request — the daemon
    /// runs this before the lifecycle gate so explicit and implicit names
    /// are both gated uniformly. Mirrors `resolve_workspace_name` for the
    /// create path.
    ///
    /// Reads HEAD at `external_path` via `git symbolic-ref` (no fetch, no
    /// commit). Detached HEAD is rejected here so callers don't have to
    /// invent a branch name.
    pub fn resolve_adopt_name(
        &self,
        repo_alias: &str,
        external_path: &Path,
        name: Option<&str>,
    ) -> Result<String> {
        validate_repo_alias(repo_alias)?;
        let resolved = match name {
            Some(n) => n.to_string(),
            None => {
                if !external_path.exists() || !external_path.is_dir() {
                    return Err(ShardError::Other(format!(
                        "path does not exist or is not a directory: {}",
                        external_path.display()
                    )));
                }
                let branch = git::default_branch(external_path).map_err(|e| {
                    ShardError::Other(format!(
                        "cannot read HEAD at {}: {e}",
                        external_path.display()
                    ))
                })?;
                safe_workspace_name(&branch)
            }
        };
        validate_workspace_name(&resolved)?;
        Ok(resolved)
    }

    /// Adopt an externally-managed worktree into Shard tracking.
    ///
    /// The directory must already be a registered, non-prunable worktree of
    /// this repo. No `git worktree add` is run; the only mutation is the DB
    /// INSERT. Sets `is_external=1` so [`Self::remove`] (and the daemon's
    /// remove handler) skip filesystem teardown — adoption is "untrack only"
    /// on the way back out.
    ///
    /// `name` defaults to `safe_workspace_name(branch)` where `branch` is the
    /// worktree's current HEAD. Detached HEAD is rejected.
    ///
    /// Local repos only in v1; remote (bare-cloned) repos return an error.
    pub fn adopt(
        &self,
        repo_alias: &str,
        external_path: &Path,
        name: Option<&str>,
    ) -> Result<Workspace> {
        validate_repo_alias(repo_alias)?;
        let repo_store = RepositoryStore::new(self.paths.clone());
        let repo = repo_store.get(repo_alias)?;

        let local_path = repo.local_path.as_deref().ok_or_else(|| {
            ShardError::Other("adopt is only supported for local repos".into())
        })?;

        if !external_path.exists() || !external_path.is_dir() {
            return Err(ShardError::Other(format!(
                "path does not exist or is not a directory: {}",
                external_path.display()
            )));
        }

        // Reject the repo's own base checkout — that's a different repair
        // path (an existing or missing is_base row), not adoption.
        let local_key = normalize_worktree_path(Path::new(local_path));
        let adopt_key = normalize_worktree_path(external_path);
        if local_key == adopt_key {
            return Err(ShardError::Other(format!(
                "cannot adopt the repo's base checkout: {}",
                external_path.display()
            )));
        }

        // Reject paths inside the repo's `.shard/` directory. That dir
        // is reserved for Shard-managed worktrees and gets `remove_dir_all`'d
        // on `RemoveRepo`, which would silently delete an adopted external
        // worktree's contents — bypassing the is_external untrack-only
        // contract. Adopting into the managed root is also semantically
        // wrong: managed worktrees are created via `WorkspaceStore::create`,
        // not adoption.
        let managed_key = normalize_worktree_path(&Path::new(local_path).join(".shard"));
        let managed_prefix = format!("{managed_key}\\");
        if adopt_key.starts_with(&managed_prefix) {
            return Err(ShardError::Other(format!(
                "cannot adopt a path under the repo's .shard directory: {}",
                external_path.display()
            )));
        }

        // Confirm the path is registered as a live worktree of this repo.
        // Persist the porcelain entry's path (not the user-provided one) so
        // reconcile lookups match what git reports.
        let source_dir = self
            .paths
            .repo_source_for_repo(repo_alias, repo.local_path.as_deref());
        let entries = git::worktree_list_porcelain(&source_dir)?;
        let entry = entries
            .iter()
            .find(|e| !e.prunable && normalize_worktree_path(&e.path) == adopt_key)
            .ok_or_else(|| {
                ShardError::Other(format!(
                    "path is not a registered worktree of repo '{}': {}",
                    repo_alias,
                    external_path.display()
                ))
            })?;

        let branch = entry.branch.clone().ok_or_else(|| {
            ShardError::Other(format!(
                "cannot adopt worktree at {}: detached HEAD is not supported",
                external_path.display()
            ))
        })?;

        let ws_name = match name {
            Some(n) => {
                validate_workspace_name(n)?;
                n.to_string()
            }
            None => safe_workspace_name(&branch),
        };

        let path_str = entry.path.to_string_lossy().to_string();

        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_repo_db(&repo_db_path)?;

        let name_exists: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM workspaces WHERE name = ?1",
            params![ws_name],
            |row| row.get(0),
        )?;
        if name_exists {
            return Err(ShardError::WorkspaceAlreadyExists(ws_name));
        }

        // Path uniqueness must be normalized — SQLite UNIQUE on the raw path
        // string would miss casing/separator/UNC variants.
        let existing = self.list(repo_alias).unwrap_or_default();
        if existing
            .iter()
            .any(|ws| normalize_worktree_path(Path::new(&ws.path)) == adopt_key)
        {
            return Err(ShardError::Other(format!(
                "a workspace already tracks this path: {}",
                external_path.display()
            )));
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        conn.execute(
            "INSERT INTO workspaces (name, branch, path, is_base, is_external, created_at) \
             VALUES (?1, ?2, ?3, 0, 1, ?4)",
            params![ws_name, branch, path_str, now],
        )?;

        Ok(Workspace {
            name: ws_name,
            branch,
            path: path_str,
            is_base: false,
            is_external: true,
            created_at: now,
        })
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

        let source_dir = self
            .paths
            .repo_source_for_repo(repo_alias, repo.local_path.as_deref());

        let default_branch = || git::default_branch(&source_dir);

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
                        ShardError::Other("existing_branch mode requires a branch name".into())
                    })?;
                    (target.to_string(), target.to_string(), None)
                }
            }
        };

        let ws_name = resolve_workspace_name_from_branch(name, mode, &branch_for_db);
        validate_workspace_name(&ws_name)?;

        // Check for duplicates in DB
        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_repo_db(&repo_db_path)?;

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
            if let Some(owner) =
                self.worktree_owning_branch(repo_alias, &source_dir, &branch_for_db)?
            {
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
            let dir =
                self.paths
                    .workspace_dir_for_repo(repo_alias, &ws_name, repo.local_path.as_deref());
            tracing::info!("creating worktree at {}", dir.display());
            git::worktree_add(&source_dir, &dir, &base_branch, new_branch.as_deref())?;

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
            "INSERT INTO workspaces (name, branch, path, is_base, is_external, created_at) VALUES (?1, ?2, ?3, ?4, 0, ?5)",
            params![ws_name, branch_for_db, path_str, is_base as i32, now],
        )?;

        Ok(Workspace {
            name: ws_name,
            branch: branch_for_db,
            path: path_str,
            is_base,
            is_external: false,
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
        let source_dir = self
            .paths
            .repo_source_for_repo(repo_alias, repo.local_path.as_deref());

        let branches = git::list_branches(&source_dir).unwrap_or_default();
        let head = git::default_branch(&source_dir).ok();

        let workspaces = self.list(repo_alias).unwrap_or_default();
        let path_to_workspace: std::collections::HashMap<String, String> = workspaces
            .iter()
            .map(|ws| {
                (
                    normalize_worktree_path(std::path::Path::new(&ws.path)),
                    ws.name.clone(),
                )
            })
            .collect();

        let mut branch_to_workspace: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        // For branches checked out in unmanaged worktrees, carry the porcelain
        // path so the frontend can pass it to `adopt` without having to derive
        // it from the (display-only) `(external: <name>)` label.
        let mut branch_to_external_path: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        if let Ok(entries) = git::worktree_list_porcelain(&source_dir) {
            for entry in entries {
                if entry.prunable || entry.detached {
                    continue;
                }
                let (Some(branch), key) =
                    (entry.branch.clone(), normalize_worktree_path(&entry.path))
                else {
                    continue;
                };
                let managed = path_to_workspace.get(&key).cloned();
                let label = match &managed {
                    Some(name) => name.clone(),
                    None => {
                        // Carry the porcelain-reported path verbatim — that's
                        // the shape `WorkspaceStore::adopt` matches against
                        // and the shape it persists.
                        branch_to_external_path
                            .insert(branch.clone(), entry.path.to_string_lossy().to_string());
                        external_worktree_label(&entry.path)
                    }
                };
                branch_to_workspace.insert(branch, label);
            }
        }

        Ok(branches
            .into_iter()
            .map(|name| {
                let is_head = head.as_deref() == Some(name.as_str());
                let checked_out_by = branch_to_workspace.get(&name).cloned();
                let external_path = branch_to_external_path.get(&name).cloned();
                BranchInfo {
                    name,
                    is_head,
                    checked_out_by,
                    external_path,
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
        Ok(Some(
            managed.unwrap_or_else(|| external_worktree_label(&entry.path)),
        ))
    }

    /// List all workspaces for a repo.
    pub fn list(&self, repo_alias: &str) -> Result<Vec<Workspace>> {
        validate_repo_alias(repo_alias)?;
        // Verify the repo exists
        let repo_store = RepositoryStore::new(self.paths.clone());
        let _repo = repo_store.get(repo_alias)?;

        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_repo_db(&repo_db_path)?;

        let mut stmt = conn.prepare(
            "SELECT name, branch, path, is_base, is_external, created_at FROM workspaces ORDER BY name",
        )?;
        let workspaces = stmt.query_map([], |row| {
            let is_base_int: i32 = row.get(3)?;
            let is_external_int: i32 = row.get(4)?;
            Ok(Workspace {
                name: row.get(0)?,
                branch: row.get(1)?,
                path: row.get(2)?,
                is_base: is_base_int != 0,
                is_external: is_external_int != 0,
                created_at: row.get(5)?,
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
        let conn = db::open_repo_db(&repo_db_path)?;

        conn.query_row(
            "SELECT name, branch, path, is_base, is_external, created_at FROM workspaces WHERE name = ?1",
            params![ws_name],
            |row| {
                let is_base_int: i32 = row.get(3)?;
                let is_external_int: i32 = row.get(4)?;
                Ok(Workspace {
                    name: row.get(0)?,
                    branch: row.get(1)?,
                    path: row.get(2)?,
                    is_base: is_base_int != 0,
                    is_external: is_external_int != 0,
                    created_at: row.get(5)?,
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

        if !ws.is_base && !ws.is_external {
            let repo_store = RepositoryStore::new(self.paths.clone());
            let repo = repo_store.get(repo_alias)?;
            let source_dir = self
                .paths
                .repo_source_for_repo(repo_alias, repo.local_path.as_deref());
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
        let conn = db::open_repo_db(&repo_db_path)?;
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
        let conn = db::open_repo_db(&repo_db_path)?;
        conn.execute("DELETE FROM workspaces WHERE name = ?1", params![ws_name])?;
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
