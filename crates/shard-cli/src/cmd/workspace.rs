use shard_core::workspaces::{Workspace, WorkspaceMode, WorkspaceStore, WorkspaceWithStatus};
use shard_core::ShardPaths;

use crate::opts::{parse_target, WorkspaceCommands};

pub fn run(command: WorkspaceCommands) -> shard_core::Result<()> {
    match command {
        WorkspaceCommands::Create { repo, name, branch } => {
            let ws = create_via_daemon(&repo, name, branch)?;
            println!(
                "Created workspace '{}:{}' on branch '{}'",
                repo, ws.name, ws.branch
            );
            println!("  Path: {}", ws.path);
        }

        WorkspaceCommands::List { repo, json } => {
            let items = list_via_daemon(&repo)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&items).unwrap());
            } else if items.is_empty() {
                println!("No workspaces for '{repo}'.");
            } else {
                for item in &items {
                    let ws = &item.workspace;
                    println!(
                        "  {}:{} — branch '{}' at {}",
                        repo, ws.name, ws.branch, ws.path
                    );
                }
            }
        }

        WorkspaceCommands::Info { target } => {
            // Info is a point lookup (immutable fields) — stays direct per D4.
            let paths = ShardPaths::new()?;
            let store = WorkspaceStore::new(paths);
            let (repo, ws_name) = parse_target(&target).map_err(shard_core::ShardError::Other)?;
            let ws = store.get(repo, ws_name)?;
            println!("Workspace: {}:{}", repo, ws.name);
            println!("  Branch: {}", ws.branch);
            println!("  Path:   {}", ws.path);
        }

        WorkspaceCommands::Remove { target } => {
            let (repo, ws_name) = parse_target(&target).map_err(shard_core::ShardError::Other)?;
            remove_via_daemon(repo, ws_name)?;
            println!("Removed workspace '{}:{}'", repo, ws_name);
        }

        WorkspaceCommands::Adopt { repo, path, name } => {
            let ws = adopt_via_daemon(&repo, &path, name)?;
            println!(
                "Adopted workspace '{}:{}' on branch '{}'",
                repo, ws.name, ws.branch
            );
            println!("  Path: {}", ws.path);
        }
    }
    Ok(())
}

fn create_via_daemon(
    repo: &str,
    name: Option<String>,
    branch: Option<String>,
) -> shard_core::Result<Workspace> {
    use shard_transport::control_protocol::ControlFrame;
    crate::cmd::daemon_rpc::run(
        ControlFrame::CreateWorkspace {
            repo: repo.to_string(),
            name,
            mode: WorkspaceMode::NewBranch,
            branch,
        },
        |f| match f {
            ControlFrame::CreateWorkspaceAck { workspace } => Some(workspace),
            _ => None,
        },
    )
}

fn list_via_daemon(repo: &str) -> shard_core::Result<Vec<WorkspaceWithStatus>> {
    use shard_transport::control_protocol::ControlFrame;
    crate::cmd::daemon_rpc::run(
        ControlFrame::ListWorkspaces {
            repo: repo.to_string(),
        },
        |f| match f {
            ControlFrame::WorkspaceList { items } => Some(items),
            _ => None,
        },
    )
}

fn adopt_via_daemon(
    repo: &str,
    path: &str,
    name: Option<String>,
) -> shard_core::Result<Workspace> {
    use shard_transport::control_protocol::ControlFrame;
    crate::cmd::daemon_rpc::run(
        ControlFrame::AdoptWorkspace {
            repo: repo.to_string(),
            path: path.to_string(),
            name,
        },
        |f| match f {
            ControlFrame::AdoptWorkspaceAck { workspace } => Some(workspace),
            _ => None,
        },
    )
}

/// Route `workspace remove` through the daemon so the lifecycle gate,
/// session stop, and watcher drop land in the correct order (fixes SHA-55).
fn remove_via_daemon(repo: &str, ws_name: &str) -> shard_core::Result<()> {
    use shard_transport::control_protocol::ControlFrame;
    crate::cmd::daemon_rpc::run(
        ControlFrame::RemoveWorkspace {
            repo: repo.to_string(),
            name: ws_name.to_string(),
        },
        |f| match f {
            ControlFrame::RemoveWorkspaceAck => Some(()),
            _ => None,
        },
    )
}
