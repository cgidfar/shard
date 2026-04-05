use thiserror::Error;

#[derive(Error, Debug)]
pub enum ShardError {
    #[error("repository not found: {0}")]
    RepoNotFound(String),

    #[error("repository already exists: {0}")]
    RepoAlreadyExists(String),

    #[error("workspace not found: {0}")]
    WorkspaceNotFound(String),

    #[error("workspace already exists: {0}")]
    WorkspaceAlreadyExists(String),

    #[error("session not found: {0}")]
    SessionNotFound(String),

    #[error("git error: {0}")]
    Git(String),

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, ShardError>;
