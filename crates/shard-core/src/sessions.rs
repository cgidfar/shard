use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::params;
use serde::Serialize;

use crate::db;
use crate::harness::Harness;
use crate::identifiers::{validate_repo_alias, validate_session_id, validate_workspace_name};
use crate::paths::ShardPaths;
use crate::{Result, ShardError};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
    pub label: Option<String>,
    pub harness: Option<Harness>,
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
        harness: Option<Harness>,
    ) -> Result<Session> {
        validate_repo_alias(repo_alias)?;
        validate_workspace_name(workspace_name)?;
        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_repo_db(&repo_db_path)?;

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

        let harness_str = harness.map(|h| h.to_string());
        conn.execute(
            "INSERT INTO sessions (id, workspace_name, command_json, transport_addr, log_path, status, created_at, harness)
             VALUES (?1, ?2, ?3, ?4, ?5, 'starting', ?6, ?7)",
            params![id, workspace_name, command_json, transport_addr, log_path_str, now, harness_str],
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
            label: None,
            harness,
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
        validate_repo_alias(repo_alias)?;
        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_repo_db(&repo_db_path)?;

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
        validate_repo_alias(repo_alias)?;
        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_repo_db(&repo_db_path)?;
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
        validate_repo_alias(repo_alias)?;
        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_repo_db(&repo_db_path)?;
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
        validate_repo_alias(repo_alias)?;
        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_repo_db(&repo_db_path)?;
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
        validate_repo_alias(repo_alias)?;
        if let Some(ws) = workspace_name {
            validate_workspace_name(ws)?;
        }
        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_repo_db(&repo_db_path)?;

        let mut sessions = Vec::new();

        if let Some(ws) = workspace_name {
            let mut stmt = conn.prepare(
                "SELECT id, workspace_name, command_json, transport_addr, log_path,
                        supervisor_pid, child_pid, status, exit_code, created_at, stopped_at, label, harness
                 FROM sessions WHERE workspace_name = ?1 ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map(params![ws], row_to_session)?;
            for row in rows {
                sessions.push(row?);
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, workspace_name, command_json, transport_addr, log_path,
                        supervisor_pid, child_pid, status, exit_code, created_at, stopped_at, label, harness
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
        validate_repo_alias(repo_alias)?;
        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_repo_db(&repo_db_path)?;
        conn.query_row(
            "SELECT id, workspace_name, command_json, transport_addr, log_path,
                    supervisor_pid, child_pid, status, exit_code, created_at, stopped_at, label, harness
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
            validate_repo_alias(&alias)?;
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
            let conn = db::open_repo_db(&repo_db_path)?;
            let like_pattern = format!("{session_id}%");
            let mut pstmt = conn.prepare(
                "SELECT id, workspace_name, command_json, transport_addr, log_path,
                        supervisor_pid, child_pid, status, exit_code, created_at, stopped_at, label, harness
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
    ///
    /// Order is filesystem-first, DB-second: a leaked file handle (e.g. an
    /// open log writer the supervisor didn't release) makes `remove_dir_all`
    /// fail, and we propagate the error rather than ack a half-complete
    /// removal. Keeping the DB row preserves the retry handle — the next
    /// `RemoveSession` call sees the same row and can try again.
    pub fn remove(&self, repo_alias: &str, session_id: &str) -> Result<()> {
        validate_repo_alias(repo_alias)?;
        validate_session_id(session_id)?;
        let session = self.get(repo_alias, session_id)?;
        if session.status == "running" || session.status == "starting" {
            return Err(ShardError::Other(format!(
                "cannot remove session '{}' with status '{}' — stop it first",
                session_id, session.status
            )));
        }

        let session_dir = self.paths.session_dir(repo_alias, session_id);
        if session_dir.exists() {
            std::fs::remove_dir_all(&session_dir).map_err(|e| {
                ShardError::Other(format!(
                    "failed to clean session directory {}: {e}",
                    session_dir.display()
                ))
            })?;
        }

        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_repo_db(&repo_db_path)?;
        conn.execute("DELETE FROM sessions WHERE id = ?1", params![session_id])?;

        Ok(())
    }

    /// Rename a session (set or clear its label).
    ///
    /// Returns `SessionNotFound` if no row matched — bare `UPDATE` would
    /// silently no-op, which breaks the RPC contract symmetry with
    /// `remove`/`get` (callers expect a typed error rather than an Ack
    /// for a non-existent row).
    pub fn rename(
        &self,
        repo_alias: &str,
        session_id: &str,
        label: Option<&str>,
    ) -> Result<()> {
        validate_repo_alias(repo_alias)?;
        let repo_db_path = self.paths.repo_db(repo_alias);
        let conn = db::open_repo_db(&repo_db_path)?;
        let rows = conn.execute(
            "UPDATE sessions SET label = ?1 WHERE id = ?2",
            params![label, session_id],
        )?;
        if rows == 0 {
            return Err(ShardError::SessionNotFound(session_id.to_string()));
        }
        Ok(())
    }
}

fn row_to_session(row: &rusqlite::Row) -> rusqlite::Result<Session> {
    let harness_str: Option<String> = row.get(12)?;
    let harness = harness_str.and_then(|s| s.parse::<Harness>().ok());

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
        label: row.get(11)?,
        harness,
    })
}
