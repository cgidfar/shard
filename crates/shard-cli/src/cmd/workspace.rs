use shard_core::workspaces::WorkspaceStore;
use shard_core::ShardPaths;

use crate::opts::{parse_target, WorkspaceCommands};

pub fn run(command: WorkspaceCommands) -> shard_core::Result<()> {
    let paths = ShardPaths::new()?;
    let store = WorkspaceStore::new(paths);

    match command {
        WorkspaceCommands::Create { repo, name, branch } => {
            let ws = store.create(&repo, name.as_deref(), branch.as_deref(), false)?;
            println!("Created workspace '{}:{}' on branch '{}'", repo, ws.name, ws.branch);
            println!("  Path: {}", ws.path);
        }

        WorkspaceCommands::List { repo, json } => {
            let workspaces = store.list(&repo)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&workspaces).unwrap());
            } else if workspaces.is_empty() {
                println!("No workspaces for '{repo}'.");
            } else {
                for ws in &workspaces {
                    println!("  {}:{} — branch '{}' at {}", repo, ws.name, ws.branch, ws.path);
                }
            }
        }

        WorkspaceCommands::Info { target } => {
            let (repo, ws_name) = parse_target(&target)
                .map_err(|e| shard_core::ShardError::Other(e))?;
            let ws = store.get(repo, ws_name)?;
            println!("Workspace: {}:{}", repo, ws.name);
            println!("  Branch: {}", ws.branch);
            println!("  Path:   {}", ws.path);
        }

        WorkspaceCommands::Remove { target } => {
            let (repo, ws_name) = parse_target(&target)
                .map_err(|e| shard_core::ShardError::Other(e))?;
            store.remove(repo, ws_name)?;
            println!("Removed workspace '{}:{}'", repo, ws_name);
        }
    }
    Ok(())
}
