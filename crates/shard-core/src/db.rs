use std::path::Path;

use rusqlite::Connection;

use crate::Result;

/// Open a SQLite connection with WAL mode and busy timeout.
///
/// Every connection in the project goes through this function to ensure
/// consistent concurrency settings across all processes.
pub fn open_connection(path: &Path) -> Result<Connection> {
    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

/// Initialize the global index.db schema
pub fn init_index_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS repos (
            id TEXT PRIMARY KEY,
            url TEXT NOT NULL UNIQUE,
            alias TEXT NOT NULL UNIQUE,
            host TEXT,
            owner TEXT,
            name TEXT,
            local_path TEXT,
            collapsed INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL
        );"
    )?;
    // Migration: add local_path column if missing (existing databases)
    let _ = conn.execute_batch("ALTER TABLE repos ADD COLUMN local_path TEXT;");
    Ok(())
}

/// Initialize a per-repo repo.db schema
pub fn init_repo_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS workspaces (
            name TEXT PRIMARY KEY,
            branch TEXT NOT NULL,
            path TEXT NOT NULL UNIQUE,
            is_base INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            workspace_name TEXT NOT NULL,
            command_json TEXT NOT NULL,
            transport_addr TEXT NOT NULL UNIQUE,
            log_path TEXT NOT NULL UNIQUE,
            supervisor_pid INTEGER,
            child_pid INTEGER,
            status TEXT NOT NULL,
            exit_code INTEGER,
            created_at INTEGER NOT NULL,
            stopped_at INTEGER,
            FOREIGN KEY (workspace_name) REFERENCES workspaces(name)
        );"
    )?;
    // Migration: add is_base column if missing (existing databases)
    let _ = conn.execute_batch("ALTER TABLE workspaces ADD COLUMN is_base INTEGER NOT NULL DEFAULT 0;");
    // Migration: add label column if missing (existing databases)
    let _ = conn.execute_batch("ALTER TABLE sessions ADD COLUMN label TEXT;");
    Ok(())
}
