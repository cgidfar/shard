use shard_core::repos::RepositoryStore;
use shard_core::workspaces::WorkspaceStore;
use shard_core::ShardPaths;

use crate::opts::RepoCommands;

pub fn run(command: RepoCommands) -> shard_core::Result<()> {
    let paths = ShardPaths::new()?;
    paths.ensure_dirs()?;
    let store = RepositoryStore::new(ShardPaths::new()?);

    match command {
        RepoCommands::Add { url, alias } => {
            let repo = store.add(&url, alias.as_deref())?;
            println!("Added repository '{}' ({})", repo.alias, repo.url);

            // Auto-create a workspace for the default branch
            let ws_store = WorkspaceStore::new(ShardPaths::new()?);
            let is_local = repo.local_path.is_some();
            let source_dir = paths.repo_source_for_repo(&repo.alias, repo.local_path.as_deref());
            match shard_core::git::default_branch(&source_dir) {
                Ok(branch) => {
                    match ws_store.create(&repo.alias, Some(&branch), Some(&branch), is_local) {
                        Ok(ws) => println!("Created default workspace '{}'", ws.name),
                        Err(e) => eprintln!("Warning: could not auto-create workspace: {e}"),
                    }
                }
                Err(e) => eprintln!("Warning: could not detect default branch: {e}"),
            }
        }

        RepoCommands::Sync { alias } => {
            store.sync(&alias)?;
            println!("Synced '{alias}'");
        }

        RepoCommands::Remove { alias } => {
            store.remove(&alias)?;
            println!("Removed '{alias}'");
        }

        RepoCommands::List { json } => {
            let repos = store.list()?;
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
