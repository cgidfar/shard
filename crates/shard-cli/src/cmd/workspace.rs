use shard_core::workspaces::{WorkspaceMode, WorkspaceStore};
use shard_core::ShardPaths;

use crate::opts::{parse_target, WorkspaceCommands};

pub fn run(command: WorkspaceCommands) -> shard_core::Result<()> {
    let paths = ShardPaths::new()?;
    let store = WorkspaceStore::new(paths);

    match command {
        WorkspaceCommands::Create { repo, name, branch } => {
            let ws = store.create(
                &repo,
                name.as_deref(),
                WorkspaceMode::NewBranch,
                branch.as_deref(),
                false,
            )?;
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
            remove_via_daemon(repo, ws_name)?;
            println!("Removed workspace '{}:{}'", repo, ws_name);
        }
    }
    Ok(())
}

/// Route `workspace remove` through the daemon so the lifecycle gate,
/// session stop, and watcher drop land in the correct order (fixes SHA-55).
/// Falls back to an error if the daemon isn't running — CLI removes of live
/// workspaces have always required a daemon to stop any bound sessions.
fn remove_via_daemon(repo: &str, ws_name: &str) -> shard_core::Result<()> {
    use shard_transport::control_protocol::ControlFrame;
    use shard_transport::daemon_client;

    let rt = tokio::runtime::Runtime::new()
        .map_err(|e| shard_core::ShardError::Other(format!("tokio: {e}")))?;

    rt.block_on(async {
        let mut conn = daemon_client::connect()
            .await
            .map_err(|e| shard_core::ShardError::Other(format!("daemon not running: {e}")))?;
        conn.handshake()
            .await
            .map_err(|e| shard_core::ShardError::Other(format!("daemon handshake: {e}")))?;
        conn.request_typed(
            &ControlFrame::RemoveWorkspace {
                repo: repo.to_string(),
                name: ws_name.to_string(),
            },
            |f| match f {
                ControlFrame::RemoveWorkspaceAck => Ok(()),
                other => Err(other),
            },
        )
        .await
        .map_err(|e| shard_core::ShardError::Other(e.to_string()))
    })
}
