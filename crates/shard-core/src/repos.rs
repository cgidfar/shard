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

        // Check for duplicates
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
                // Canonicalize to get the absolute path
                p.canonicalize()
                    .ok()
                    .map(|c| c.to_string_lossy().to_string())
            } else {
                None
            }
        };

        // Clone bare repo
        let source_dir = self.paths.repo_source(&alias);
        tracing::info!("cloning {} into {}", url, source_dir.display());
        git::clone_bare(url, &source_dir)?;

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
        let _repo = self.get(alias)?;
        let source_dir = self.paths.repo_source(alias);
        tracing::info!("syncing {}", alias);
        git::fetch(&source_dir)?;
        Ok(())
    }

    /// Remove a repository and all its data.
    pub fn remove(&self, alias: &str) -> Result<()> {
        let _repo = self.get(alias)?;

        // Remove from index
        let conn = self.open_index()?;
        conn.execute("DELETE FROM repos WHERE alias = ?1", params![alias])?;

        // Remove directory
        let repo_dir = self.paths.repo_dir(alias);
        if repo_dir.exists() {
            std::fs::remove_dir_all(&repo_dir)?;
        }

        Ok(())
    }
}
