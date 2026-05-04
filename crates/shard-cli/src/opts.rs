use clap::{Parser, Subcommand};
use shard_core::Harness;

#[derive(Parser)]
#[command(name = "shardctl", about = "Workspace manager for parallel coding agents")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Manage repositories
    Repo {
        #[command(subcommand)]
        command: RepoCommands,
    },

    /// Manage workspaces
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommands,
    },

    /// Clean up stale resources
    Prune {
        #[command(subcommand)]
        command: PruneCommands,
    },

    /// Manage sessions
    Session {
        #[command(subcommand)]
        command: SessionCommands,
    },

    /// [hidden] Send activity state to the session supervisor via hook
    #[command(hide = true)]
    Notify {
        /// Activity state: active, idle, blocked
        state: String,
    },

    /// Manage the Shard daemon
    Daemon {
        #[command(subcommand)]
        command: DaemonCommands,
    },
}

#[derive(Subcommand)]
pub enum DaemonCommands {
    /// [hidden] Start the daemon process (auto-started by clients)
    #[command(hide = true)]
    Start,

    /// Stop the running daemon (gracefully stops all sessions)
    Stop,

    /// Show daemon status
    Status,
}

#[derive(Subcommand)]
pub enum RepoCommands {
    /// Register a new repository
    Add {
        /// Git URL, SSH URL, or local path
        url: String,

        /// Short alias for the repo (auto-derived from URL if omitted)
        #[arg(long)]
        alias: Option<String>,
    },

    /// Fetch latest changes for a repository
    Sync {
        /// Repository alias
        alias: String,
    },

    /// Remove a repository and all its data
    Remove {
        /// Repository alias
        alias: String,
    },

    /// List all registered repositories
    List {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum WorkspaceCommands {
    /// Create a new workspace (git worktree)
    Create {
        /// Repository alias
        repo: String,

        /// Workspace name (defaults to branch name)
        name: Option<String>,

        /// Branch to check out (defaults to repo's default branch)
        #[arg(long)]
        branch: Option<String>,
    },

    /// List workspaces for a repository
    List {
        /// Repository alias
        repo: String,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Show workspace info
    Info {
        /// Target as repo:workspace
        target: String,
    },

    /// Remove a workspace
    Remove {
        /// Target as repo:workspace
        target: String,
    },

    /// Adopt a pre-existing external git worktree as a Shard workspace.
    ///
    /// The path must already be a registered, non-prunable worktree of
    /// `repo`. Shard records the row and skips filesystem teardown on
    /// remove (untrack only). Local repos only.
    Adopt {
        /// Repository alias
        repo: String,

        /// Absolute path to the existing worktree directory
        path: String,

        /// Workspace name (defaults to safe form of the worktree's HEAD branch)
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum SessionCommands {
    /// Create a new session in a workspace
    Create {
        /// Target as repo:workspace
        target: String,

        /// Harness type (claude-code, codex)
        #[arg(long)]
        harness: Option<Harness>,

        /// Command to run (defaults to system shell)
        #[arg(last = true)]
        command: Vec<String>,
    },

    /// List sessions
    List {
        /// Optional filter: repo or repo:workspace
        target: Option<String>,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Attach to a running session
    Attach {
        /// Session ID (UUID)
        id: String,
    },

    /// Stop a running session
    Stop {
        /// Session ID (UUID)
        id: String,

        /// Force-kill immediately
        #[arg(long)]
        force: bool,
    },

    /// Remove a stopped session
    Remove {
        /// Session ID (UUID)
        id: String,
    },

    /// [hidden] Run as a session supervisor
    #[command(hide = true)]
    Serve {
        /// Repository alias
        #[arg(long)]
        repo: String,

        /// Workspace name
        #[arg(long)]
        workspace: String,

        /// Session ID
        #[arg(long)]
        session_id: String,

        /// Transport address
        #[arg(long)]
        transport_addr: String,

        /// Command to run
        #[arg(last = true)]
        command: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum PruneCommands {
    /// Clean up dead sessions (running/starting status but supervisor is gone)
    Sessions,
}

/// Parse a "repo:workspace" target string.
/// Returns (repo_alias, workspace_name).
pub fn parse_target(target: &str) -> Result<(&str, &str), String> {
    match target.split_once(':') {
        Some((repo, ws)) if !repo.is_empty() && !ws.is_empty() => Ok((repo, ws)),
        _ => Err(format!(
            "invalid target '{target}': expected format repo:workspace (e.g., shard:main)"
        )),
    }
}
