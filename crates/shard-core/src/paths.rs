use std::path::PathBuf;

use directories::ProjectDirs;

use crate::{Result, ShardError};

/// Resolves all standard paths for Shard data storage.
///
/// Respects `SHARD_DATA_DIR` env var override.
/// Default: `%LOCALAPPDATA%\shard\data\` on Windows,
///          `~/Library/Application Support/shard/` on Mac.
#[derive(Clone)]
pub struct ShardPaths {
    data_dir: PathBuf,
}

impl ShardPaths {
    pub fn new() -> Result<Self> {
        let data_dir = if let Ok(override_dir) = std::env::var("SHARD_DATA_DIR") {
            PathBuf::from(override_dir)
        } else {
            let dirs = ProjectDirs::from("", "", "shard")
                .ok_or_else(|| ShardError::Other("cannot determine data directory".into()))?;
            dirs.data_local_dir().to_path_buf()
        };
        Ok(Self { data_dir })
    }

    /// Construct paths rooted at an explicit data directory, bypassing the
    /// `SHARD_DATA_DIR` env var and `ProjectDirs`. Used by the integration
    /// test harness so parallel tests never share state through the process
    /// environment.
    pub fn from_data_dir(data_dir: PathBuf) -> Self {
        Self { data_dir }
    }

    /// Root data directory (e.g., `%LOCALAPPDATA%\shard\data\`)
    pub fn data_dir(&self) -> &PathBuf {
        &self.data_dir
    }

    /// Path to the global index database
    pub fn index_db(&self) -> PathBuf {
        self.data_dir.join("index.db")
    }

    /// Root directory for all repository data
    pub fn repos_dir(&self) -> PathBuf {
        self.data_dir.join("repos")
    }

    /// Directory for a specific repo (by alias)
    pub fn repo_dir(&self, alias: &str) -> PathBuf {
        self.repos_dir().join(alias)
    }

    /// Path to a repo's database
    pub fn repo_db(&self, alias: &str) -> PathBuf {
        self.repo_dir(alias).join("repo.db")
    }

    /// Path to a repo's bare git clone (remote repos only)
    pub fn repo_source(&self, alias: &str) -> PathBuf {
        self.repo_dir(alias).join("source.git")
    }

    /// Resolve the git source directory for a repo.
    ///
    /// - Local repo:  the original checkout (`local_path`)
    /// - Remote repo: the bare clone (`source.git`)
    pub fn repo_source_for_repo(&self, alias: &str, local_path: Option<&str>) -> PathBuf {
        if let Some(lp) = local_path {
            PathBuf::from(lp)
        } else {
            self.repo_source(alias)
        }
    }

    /// Root directory for a repo's workspaces (fallback location in AppData).
    pub fn workspaces_dir(&self, alias: &str) -> PathBuf {
        self.repo_dir(alias).join("workspaces")
    }

    /// Directory for a specific workspace (fallback location in AppData).
    pub fn workspace_dir(&self, alias: &str, workspace: &str) -> PathBuf {
        self.workspaces_dir(alias).join(workspace)
    }

    /// Resolve the workspace directory, preferring `.shard/` next to the
    /// original repo when a local_path is known.
    ///
    /// - Local repo:  `{local_path}/.shard/{workspace}/`
    /// - Remote repo: `{appdata}/repos/{alias}/workspaces/{workspace}/`
    pub fn workspace_dir_for_repo(
        &self,
        alias: &str,
        workspace: &str,
        local_path: Option<&str>,
    ) -> PathBuf {
        if let Some(lp) = local_path {
            PathBuf::from(lp).join(".shard").join(workspace)
        } else {
            self.workspace_dir(alias, workspace)
        }
    }

    /// Root directory for a repo's sessions
    pub fn sessions_dir(&self, alias: &str) -> PathBuf {
        self.repo_dir(alias).join("sessions")
    }

    /// Directory for a specific session
    pub fn session_dir(&self, alias: &str, session_id: &str) -> PathBuf {
        self.sessions_dir(alias).join(session_id)
    }

    /// Ensure all required directories exist
    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(self.repos_dir())?;
        Ok(())
    }
}
