use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::params;
use serde::Serialize;

use crate::db;
use crate::paths::ShardPaths;
use crate::{Result, ShardError};

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub id: String,
    pub workspace_name: String,
    pub command_json: String,
    pub transport_addr: String,
    pub log_path: String,
    pub supervisor_pid: Option<u32>,
    pub child_pid: Option<u32>,
    pub status: String,
    pub exit_code: Option<i32>,
    pub created_at: u64,
    pub stopped_at: Option<u64>,
}

pub struct SessionStore {
    paths: ShardPaths,
}

impl SessionStore {
    pub fn new(paths: ShardPaths) -> Self {
        Self { paths }
    }

    /// Create a new session record in the DB.
    /// Returns the session with status "starting".
    pub fn create(
        &self,
        repo_alias: &str,
        workspace_name: &str,
        command: &[String],
        transport_addr: &str,
    ) -> Result<Session> {
        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_connection(&repo_db_path)?;

        let id = uuid::Uuid::now_v7().to_string();
        let command_json = serde_json::to_string(command)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Session directory and log path
        let session_dir = self.paths.session_dir(repo_alias, &id);
        std::fs::create_dir_all(&session_dir)?;
        let log_path = session_dir.join("session.log");
        let log_path_str = log_path.to_string_lossy().to_string();

        conn.execute(
            "INSERT INTO sessions (id, workspace_name, command_json, transport_addr, log_path, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'starting', ?6)",
            params![id, workspace_name, command_json, transport_addr, log_path_str, now],
        )?;

        Ok(Session {
            id,
            workspace_name: workspace_name.to_string(),
            command_json,
            transport_addr: transport_addr.to_string(),
            log_path: log_path_str,
            supervisor_pid: None,
            child_pid: None,
            status: "starting".to_string(),
            exit_code: None,
            created_at: now,
            stopped_at: None,
        })
    }

    /// Update a session's status.
    pub fn update_status(
        &self,
        repo_alias: &str,
        session_id: &str,
        status: &str,
        exit_code: Option<i32>,
    ) -> Result<()> {
        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_connection(&repo_db_path)?;

        let now = if status == "exited" || status == "stopped" || status == "failed" {
            Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            )
        } else {
            None
        };

        conn.execute(
            "UPDATE sessions SET status = ?1, exit_code = ?2, stopped_at = ?3 WHERE id = ?4",
            params![status, exit_code, now, session_id],
        )?;
        Ok(())
    }

    /// Update the transport address for a session.
    pub fn update_transport_addr(
        &self,
        repo_alias: &str,
        session_id: &str,
        addr: &str,
    ) -> Result<()> {
        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_connection(&repo_db_path)?;
        conn.execute(
            "UPDATE sessions SET transport_addr = ?1 WHERE id = ?2",
            params![addr, session_id],
        )?;
        Ok(())
    }

    /// Update the supervisor PID for a session.
    pub fn set_supervisor_pid(
        &self,
        repo_alias: &str,
        session_id: &str,
        pid: u32,
    ) -> Result<()> {
        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_connection(&repo_db_path)?;
        conn.execute(
            "UPDATE sessions SET supervisor_pid = ?1 WHERE id = ?2",
            params![pid, session_id],
        )?;
        Ok(())
    }

    /// Update the child PID for a session.
    pub fn set_child_pid(
        &self,
        repo_alias: &str,
        session_id: &str,
        pid: u32,
    ) -> Result<()> {
        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_connection(&repo_db_path)?;
        conn.execute(
            "UPDATE sessions SET child_pid = ?1 WHERE id = ?2",
            params![pid, session_id],
        )?;
        Ok(())
    }

    /// List sessions, optionally filtered by workspace.
    pub fn list(
        &self,
        repo_alias: &str,
        workspace_name: Option<&str>,
    ) -> Result<Vec<Session>> {
        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_connection(&repo_db_path)?;

        let mut sessions = Vec::new();

        if let Some(ws) = workspace_name {
            let mut stmt = conn.prepare(
                "SELECT id, workspace_name, command_json, transport_addr, log_path,
                        supervisor_pid, child_pid, status, exit_code, created_at, stopped_at
                 FROM sessions WHERE workspace_name = ?1 ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map(params![ws], row_to_session)?;
            for row in rows {
                sessions.push(row?);
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, workspace_name, command_json, transport_addr, log_path,
                        supervisor_pid, child_pid, status, exit_code, created_at, stopped_at
                 FROM sessions ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map([], row_to_session)?;
            for row in rows {
                sessions.push(row?);
            }
        }

        Ok(sessions)
    }

    /// Get a session by ID.
    pub fn get(&self, repo_alias: &str, session_id: &str) -> Result<Session> {
        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_connection(&repo_db_path)?;
        conn.query_row(
            "SELECT id, workspace_name, command_json, transport_addr, log_path,
                    supervisor_pid, child_pid, status, exit_code, created_at, stopped_at
             FROM sessions WHERE id = ?1",
            params![session_id],
            row_to_session,
        )
        .map_err(|_| ShardError::SessionNotFound(session_id.to_string()))
    }

    /// Find a session by ID (or ID prefix) across all repos.
    ///
    /// Supports prefix matching — e.g., "019d5a15" matches the full UUID.
    /// Returns an error if the prefix matches multiple sessions.
    pub fn find_by_id(&self, session_id: &str) -> Result<(String, Session)> {
        let index_conn = db::open_connection(&self.paths.index_db())?;
        db::init_index_db(&index_conn)?;

        let mut stmt = index_conn.prepare("SELECT alias FROM repos ORDER BY alias")?;
        let aliases: Vec<String> = stmt
            .query_map([], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();

        let mut matches: Vec<(String, Session)> = Vec::new();

        for alias in aliases {
            let repo_db_path = self.paths.repo_db(&alias);
            if !repo_db_path.exists() {
                continue;
            }

            // Try exact match first
            match self.get(&alias, session_id) {
                Ok(session) => return Ok((alias, session)),
                Err(ShardError::SessionNotFound(_)) => {}
                Err(e) => return Err(e),
            }

            // Try prefix match
            let conn = db::open_connection(&repo_db_path)?;
            let like_pattern = format!("{session_id}%");
            let mut pstmt = conn.prepare(
                "SELECT id, workspace_name, command_json, transport_addr, log_path,
                        supervisor_pid, child_pid, status, exit_code, created_at, stopped_at
                 FROM sessions WHERE id LIKE ?1",
            )?;
            let rows = pstmt.query_map(params![like_pattern], row_to_session)?;
            for row in rows {
                matches.push((alias.clone(), row?));
            }
        }

        match matches.len() {
            0 => Err(ShardError::SessionNotFound(session_id.to_string())),
            1 => Ok(matches.into_iter().next().unwrap()),
            n => Err(ShardError::Other(format!(
                "ambiguous session ID prefix '{session_id}' matches {n} sessions"
            ))),
        }
    }

    /// Remove a session record from the DB and clean up its directory.
    pub fn remove(&self, repo_alias: &str, session_id: &str) -> Result<()> {
        let session = self.get(repo_alias, session_id)?;
        if session.status == "running" || session.status == "starting" {
            return Err(ShardError::Other(format!(
                "cannot remove session '{}' with status '{}' — stop it first",
                session_id, session.status
            )));
        }

        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_connection(&repo_db_path)?;
        conn.execute("DELETE FROM sessions WHERE id = ?1", params![session_id])?;

        // Clean up session directory
        let session_dir = self.paths.session_dir(repo_alias, session_id);
        if session_dir.exists() {
            let _ = std::fs::remove_dir_all(&session_dir);
        }

        Ok(())
    }
}

fn row_to_session(row: &rusqlite::Row) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        workspace_name: row.get(1)?,
        command_json: row.get(2)?,
        transport_addr: row.get(3)?,
        log_path: row.get(4)?,
        supervisor_pid: row.get(5)?,
        child_pid: row.get(6)?,
        status: row.get(7)?,
        exit_code: row.get(8)?,
        created_at: row.get(9)?,
        stopped_at: row.get(10)?,
    })
}
