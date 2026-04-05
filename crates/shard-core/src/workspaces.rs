use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::params;
use serde::Serialize;

use crate::db;
use crate::git;
use crate::paths::ShardPaths;
use crate::repos::RepositoryStore;
use crate::{Result, ShardError};

#[derive(Debug, Clone, Serialize)]
pub struct Workspace {
    pub name: String,
    pub branch: String,
    pub path: String,
    pub created_at: u64,
}

pub struct WorkspaceStore {
    paths: ShardPaths,
}

impl WorkspaceStore {
    pub fn new(paths: ShardPaths) -> Self {
        Self { paths }
    }

    /// Create a new workspace (git worktree) for a repo.
    ///
    /// If a custom name is given AND it differs from the branch, a new git branch
    /// is created with that name (based on the source branch). This avoids the git
    /// limitation of one worktree per branch.
    ///
    /// If no branch is given, the repo's default branch is used.
    pub fn create(
        &self,
        repo_alias: &str,
        name: Option<&str>,
        branch: Option<&str>,
    ) -> Result<Workspace> {
        // Verify the repo exists and get its local_path
        let repo_store = RepositoryStore::new(ShardPaths::new()?);
        let repo = repo_store.get(repo_alias)?;

        let source_dir = self.paths.repo_source(repo_alias);

        // Determine base branch
        let base_branch = match branch {
            Some(b) => b.to_string(),
            None => git::default_branch(&source_dir)?,
        };

        // Determine workspace name
        let ws_name = match name {
            Some(n) => n.to_string(),
            None => base_branch.clone(),
        };

        // If the workspace name differs from the branch, create a new branch.
        // This handles the case where the base branch already has a worktree.
        let (branch_for_db, new_branch) = if ws_name != base_branch {
            (ws_name.clone(), Some(ws_name.clone()))
        } else {
            (base_branch.clone(), None)
        };

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

        // Resolve workspace directory:
        // Local repos: .shard/{name}/ next to the original repo
        // Remote repos: AppData fallback
        let ws_dir = self.paths.workspace_dir_for_repo(
            repo_alias,
            &ws_name,
            repo.local_path.as_deref(),
        );
        tracing::info!("creating worktree at {}", ws_dir.display());
        git::worktree_add(
            &source_dir,
            &ws_dir,
            &base_branch,
            new_branch.as_deref(),
        )?;

        // Record in DB
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let path_str = ws_dir.to_string_lossy().to_string();

        conn.execute(
            "INSERT INTO workspaces (name, branch, path, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![ws_name, branch_for_db, path_str, now],
        )?;

        Ok(Workspace {
            name: ws_name,
            branch: branch_for_db,
            path: path_str,
            created_at: now,
        })
    }

    /// List all workspaces for a repo.
    pub fn list(&self, repo_alias: &str) -> Result<Vec<Workspace>> {
        // Verify the repo exists
        let repo_store = RepositoryStore::new(ShardPaths::new()?);
        let _repo = repo_store.get(repo_alias)?;

        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_connection(&repo_db_path)?;

        let mut stmt = conn.prepare(
            "SELECT name, branch, path, created_at FROM workspaces ORDER BY name",
        )?;
        let workspaces = stmt.query_map([], |row| {
            Ok(Workspace {
                name: row.get(0)?,
                branch: row.get(1)?,
                path: row.get(2)?,
                created_at: row.get(3)?,
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
        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_connection(&repo_db_path)?;

        conn.query_row(
            "SELECT name, branch, path, created_at FROM workspaces WHERE name = ?1",
            params![ws_name],
            |row| {
                Ok(Workspace {
                    name: row.get(0)?,
                    branch: row.get(1)?,
                    path: row.get(2)?,
                    created_at: row.get(3)?,
                })
            },
        )
        .map_err(|_| ShardError::WorkspaceNotFound(format!("{repo_alias}:{ws_name}")))
    }

    /// Remove a workspace (git worktree + DB record).
    pub fn remove(&self, repo_alias: &str, ws_name: &str) -> Result<()> {
        let ws = self.get(repo_alias, ws_name)?;
        let source_dir = self.paths.repo_source(repo_alias);
        let ws_dir = std::path::PathBuf::from(&ws.path);

        // Remove the git worktree
        if ws_dir.exists() {
            git::worktree_remove(&source_dir, &ws_dir)?;
        }

        // Remove from DB
        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_connection(&repo_db_path)?;
        conn.execute("DELETE FROM workspaces WHERE name = ?1", params![ws_name])?;

        Ok(())
    }
}
