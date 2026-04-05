use shard_core::repos::RepositoryStore;
use shard_core::sessions::SessionStore;
use shard_core::ShardPaths;
use shard_supervisor::process::{PlatformProcessControl, ProcessControl};

use crate::opts::PruneCommands;

pub fn run(command: PruneCommands) -> shard_core::Result<()> {
    match command {
        PruneCommands::Sessions => prune_sessions(),
    }
}

fn prune_sessions() -> shard_core::Result<()> {
    let paths = ShardPaths::new()?;
    let repo_store = RepositoryStore::new(ShardPaths::new()?);
    let session_store = SessionStore::new(ShardPaths::new()?);

    let repos = repo_store.list()?;
    let mut pruned = 0;

    for repo in &repos {
        let repo_db = paths.repo_db(&repo.alias);
        if !repo_db.exists() {
            continue;
        }

        let sessions = session_store.list(&repo.alias, None)?;
        for session in &sessions {
            // Only check sessions that should be alive
            if session.status != "running" && session.status != "starting" {
                continue;
            }

            // Check if the supervisor is actually still running
            let alive = session
                .supervisor_pid
                .map(|pid| PlatformProcessControl::is_alive(pid))
                .unwrap_or(false);

            if !alive {
                // Supervisor is dead — mark session as failed
                session_store.update_status(
                    &repo.alias,
                    &session.id,
                    "failed",
                    None,
                )?;
                println!(
                    "  Pruned {} [{}:{}] — supervisor (pid {:?}) no longer running",
                    &session.id[..8],
                    repo.alias,
                    session.workspace_name,
                    session.supervisor_pid,
                );
                pruned += 1;
            }
        }
    }

    if pruned == 0 {
        println!("No stale sessions found.");
    } else {
        println!("Pruned {pruned} stale session(s).");
    }

    Ok(())
}
