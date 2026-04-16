use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use serde::Serialize;

use crate::db;
use crate::git;
use crate::paths::ShardPaths;
use crate::{Result, ShardError};

#[derive(Debug, Clone, Serialize)]
pub struct Repository {
    pub id: String,
    pub url: String,
    pub alias: String,
    pub host: Option<String>,
    pub owner: Option<String>,
    pub name: Option<String>,
    pub local_path: Option<String>,
    pub created_at: u64,
}

pub struct RepositoryStore {
    paths: ShardPaths,
}

impl RepositoryStore {
    pub fn new(paths: ShardPaths) -> Self {
        Self { paths }
    }

    fn open_index(&self) -> Result<Connection> {
        let conn = db::open_connection(&self.paths.index_db())?;
        db::init_index_db(&conn)?;
        Ok(conn)
    }

    /// Add a new repository by URL or local path.
    ///
    /// If alias is None, one is auto-derived from the URL.
    /// Clones the repo as a bare repository and initializes its repo.db.
    pub fn add(&self, url: &str, alias: Option<&str>) -> Result<Repository> {
        let alias = match alias {
            Some(a) => a.to_string(),
            None => git::default_alias(url)
                .ok_or_else(|| ShardError::Other(
                    format!("cannot derive alias from '{url}', please provide --alias")
                ))?,
        };

        let conn = self.open_index()?;

        // Check for duplicates by url or alias
        let exists: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM repos WHERE url = ?1 OR alias = ?2",
            params![url, alias],
            |row| row.get(0),
        )?;
        if exists {
            return Err(ShardError::RepoAlreadyExists(alias));
        }

        // Parse URL components
        let (host, owner, name) = git::parse_url(url);

        // Detect local path: if url points to an existing directory, store it
        let local_path = {
            let p = std::path::Path::new(url);
            if p.is_dir() {
                p.canonicalize().ok().map(|c| {
                    let s = c.to_string_lossy().to_string();
                    // Strip Windows extended-length prefix (\\?\) — git can't handle it
                    s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
                })
            } else {
                None
            }
        };

        // Also reject duplicate canonical local paths. Two aliases pointing at
        // the same checkout would create duplicate WorkspaceMonitor watchers
        // and confuse the daemon's RepoState. The url column is a raw string
        // so plain url-matching above misses `./repo` vs `C:\abs\repo`.
        if let Some(ref canon) = local_path {
            let dup: bool = conn.query_row(
                "SELECT COUNT(*) > 0 FROM repos WHERE local_path = ?1",
                params![canon],
                |row| row.get(0),
            )?;
            if dup {
                return Err(ShardError::RepoAlreadyExists(alias));
            }
        }

        // For remote repos, create a bare clone.
        // For local repos, skip — we use the existing checkout directly.
        if local_path.is_none() {
            let source_dir = self.paths.repo_source(&alias);
            tracing::info!("cloning {} into {}", url, source_dir.display());
            git::clone_bare(url, &source_dir)?;
        }

        // Initialize repo.db for this repo
        let repo_db_path = self.paths.repo_db(&alias);
        let repo_conn = db::open_connection(&repo_db_path)?;
        db::init_repo_db(&repo_conn)?;

        // Insert into index
        let id = uuid::Uuid::now_v7().to_string();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        conn.execute(
            "INSERT INTO repos (id, url, alias, host, owner, name, local_path, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![id, url, alias, host, owner, name, local_path, now],
        )?;

        Ok(Repository {
            id,
            url: url.to_string(),
            alias,
            host,
            owner,
            name,
            local_path,
            created_at: now,
        })
    }

    /// List all registered repositories.
    pub fn list(&self) -> Result<Vec<Repository>> {
        let conn = self.open_index()?;
        let mut stmt = conn.prepare(
            "SELECT id, url, alias, host, owner, name, local_path, created_at FROM repos ORDER BY alias"
        )?;
        let repos = stmt.query_map([], |row| {
            Ok(Repository {
                id: row.get(0)?,
                url: row.get(1)?,
                alias: row.get(2)?,
                host: row.get(3)?,
                owner: row.get(4)?,
                name: row.get(5)?,
                local_path: row.get(6)?,
                created_at: row.get(7)?,
            })
        })?;

        let mut result = Vec::new();
        for repo in repos {
            result.push(repo?);
        }
        Ok(result)
    }

    /// Get a repository by alias.
    pub fn get(&self, alias: &str) -> Result<Repository> {
        let conn = self.open_index()?;
        conn.query_row(
            "SELECT id, url, alias, host, owner, name, local_path, created_at FROM repos WHERE alias = ?1",
            params![alias],
            |row| {
                Ok(Repository {
                    id: row.get(0)?,
                    url: row.get(1)?,
                    alias: row.get(2)?,
                    host: row.get(3)?,
                    owner: row.get(4)?,
                    name: row.get(5)?,
                    local_path: row.get(6)?,
                    created_at: row.get(7)?,
                })
            },
        ).map_err(|_| ShardError::RepoNotFound(alias.to_string()))
    }

    /// Fetch latest changes for a repository.
    pub fn sync(&self, alias: &str) -> Result<()> {
        let repo = self.get(alias)?;
        let source_dir = self.paths.repo_source_for_repo(alias, repo.local_path.as_deref());
        tracing::info!("syncing {}", alias);
        git::fetch(&source_dir)?;
        Ok(())
    }

    /// Remove a repository and all its data.
    ///
    /// For local repos: removes worktrees, prunes git state, cleans up `.shard/`
    /// and DB records, but NEVER deletes the original checkout.
    /// For remote repos: deletes the entire repo directory (bare clone + worktrees).
    pub fn remove(&self, alias: &str) -> Result<()> {
        let repo = self.get(alias)?;

        if repo.local_path.is_some() {
            let local_path = std::path::Path::new(repo.local_path.as_ref().unwrap());

            // Remove non-base worktrees via git worktree remove
            let ws_store = crate::workspaces::WorkspaceStore::new(ShardPaths::new()?);
            if let Ok(workspaces) = ws_store.list(alias) {
                for ws in workspaces {
                    if !ws.is_base {
                        let ws_dir = std::path::PathBuf::from(&ws.path);
                        if ws_dir.exists() {
                            let _ = git::worktree_remove(local_path, &ws_dir);
                        }
                    }
                }
            }

            // Prune stale worktree admin entries
            let _ = git::worktree_prune(local_path);

            // Remove .shard/ directory if it exists
            let shard_dir = local_path.join(".shard");
            if shard_dir.exists() {
                let _ = std::fs::remove_dir_all(&shard_dir);
            }

            // Clean up .git/info/exclude entry
            let _ = git::remove_from_exclude(local_path, ".shard/");
        }

        // Remove from index
        let conn = self.open_index()?;
        conn.execute("DELETE FROM repos WHERE alias = ?1", params![alias])?;

        // Remove repo data directory (DB, bare clone for remote, sessions)
        let repo_dir = self.paths.repo_dir(alias);
        if repo_dir.exists() {
            let _ = std::fs::remove_dir_all(&repo_dir);
        }

        Ok(())
    }
}
