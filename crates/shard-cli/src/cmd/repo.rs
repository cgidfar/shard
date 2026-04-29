use shard_core::repos::Repository;
use shard_transport::control_protocol::ControlFrame;

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

fn add_via_daemon(url: &str, alias: Option<String>) -> shard_core::Result<Repository> {
    crate::cmd::daemon_rpc::run(
        ControlFrame::AddRepo {
            url: url.to_string(),
            alias,
        },
        |f| match f {
            ControlFrame::AddRepoAck { repo } => Some(repo),
            _ => None,
        },
    )
}

fn remove_via_daemon(alias: &str) -> shard_core::Result<()> {
    crate::cmd::daemon_rpc::run(
        ControlFrame::RemoveRepo {
            alias: alias.to_string(),
        },
        |f| match f {
            ControlFrame::RemoveRepoAck => Some(()),
            _ => None,
        },
    )
}

fn sync_via_daemon(alias: &str) -> shard_core::Result<()> {
    crate::cmd::daemon_rpc::run(
        ControlFrame::SyncRepo {
            alias: alias.to_string(),
        },
        |f| match f {
            ControlFrame::SyncRepoAck => Some(()),
            _ => None,
        },
    )
}

fn list_via_daemon() -> shard_core::Result<Vec<Repository>> {
    crate::cmd::daemon_rpc::run(ControlFrame::ListRepos, |f| match f {
        ControlFrame::RepoList { repos } => Some(repos),
        _ => None,
    })
}
