use shard_core::repos::Repository;
use shard_transport::control_protocol::ControlFrame;
use shard_transport::daemon_client;

use crate::opts::RepoCommands;

pub fn run(command: RepoCommands) -> shard_core::Result<()> {
    match command {
        RepoCommands::Add { url, alias } => {
            let repo = add_via_daemon(&url, alias)?;
            println!("Added repository '{}' ({})", repo.alias, repo.url);
            // The daemon auto-creates the default-branch workspace as
            // part of AddRepo; emit a line so `shardctl repo add` output
            // matches the previous direct-path behaviour.
            println!("Created default workspace on branch '{}'", repo.alias);
        }

        RepoCommands::Sync { alias } => {
            sync_via_daemon(&alias)?;
            println!("Synced '{alias}'");
        }

        RepoCommands::Remove { alias } => {
            remove_via_daemon(&alias)?;
            println!("Removed '{alias}'");
        }

        RepoCommands::List { json } => {
            let repos = list_via_daemon()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&repos).unwrap());
            } else if repos.is_empty() {
                println!("No repositories registered.");
            } else {
                for repo in &repos {
                    let display = if let (Some(host), Some(owner), Some(name)) =
                        (&repo.host, &repo.owner, &repo.name)
                    {
                        format!("{host}/{owner}/{name}")
                    } else {
                        repo.url.clone()
                    };
                    println!("  {} — {}", repo.alias, display);
                }
            }
        }
    }
    Ok(())
}

/// Route one request through the daemon. Same shape as
/// `cmd/workspace.rs::run_daemon_rpc`. Daemon startup is on the caller;
/// CLI repo subcommands assume a daemon is already running.
fn run_daemon_rpc<T>(
    frame: ControlFrame,
    extract: impl FnOnce(ControlFrame) -> Result<T, ControlFrame>,
) -> shard_core::Result<T> {
    let rt = tokio::runtime::Runtime::new()
        .map_err(|e| shard_core::ShardError::Other(format!("runtime: {e}")))?;
    rt.block_on(async move {
        let mut conn = daemon_client::connect()
            .await
            .map_err(|e| shard_core::ShardError::Other(format!("daemon connect: {e}")))?;
        conn.handshake()
            .await
            .map_err(|e| shard_core::ShardError::Other(format!("daemon handshake: {e}")))?;
        conn.request_typed(&frame, extract)
            .await
            .map_err(|e| shard_core::ShardError::Other(e.to_string()))
    })
}

fn add_via_daemon(url: &str, alias: Option<String>) -> shard_core::Result<Repository> {
    run_daemon_rpc(
        ControlFrame::AddRepo {
            url: url.to_string(),
            alias,
        },
        |f| match f {
            ControlFrame::AddRepoAck { repo } => Ok(repo),
            other => Err(other),
        },
    )
}

fn remove_via_daemon(alias: &str) -> shard_core::Result<()> {
    run_daemon_rpc(
        ControlFrame::RemoveRepo {
            alias: alias.to_string(),
        },
        |f| match f {
            ControlFrame::RemoveRepoAck => Ok(()),
            other => Err(other),
        },
    )
}

fn sync_via_daemon(alias: &str) -> shard_core::Result<()> {
    run_daemon_rpc(
        ControlFrame::SyncRepo {
            alias: alias.to_string(),
        },
        |f| match f {
            ControlFrame::SyncRepoAck => Ok(()),
            other => Err(other),
        },
    )
}

fn list_via_daemon() -> shard_core::Result<Vec<Repository>> {
    run_daemon_rpc(ControlFrame::ListRepos, |f| match f {
        ControlFrame::RepoList { repos } => Ok(repos),
        other => Err(other),
    })
}
