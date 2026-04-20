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
            let (repo, ws_name) =
                parse_target(&target).map_err(|e| shard_core::ShardError::Other(e))?;
            let ws = store.get(repo, ws_name)?;
            println!("Workspace: {}:{}", repo, ws.name);
            println!("  Branch: {}", ws.branch);
            println!("  Path:   {}", ws.path);
        }

        WorkspaceCommands::Remove { target } => {
            let (repo, ws_name) =
                parse_target(&target).map_err(|e| shard_core::ShardError::Other(e))?;
            remove_via_daemon(repo, ws_name)?;
            println!("Removed workspace '{}:{}'", repo, ws_name);
        }
    }
    Ok(())
}

/// Template for every CLI → daemon RPC. Spins up a single-thread tokio
/// runtime per command (matches the `daemon stop` pattern), connects,
/// handshakes, sends `frame`, and passes the response through `extract`.
fn run_daemon_rpc<T>(
    frame: shard_transport::control_protocol::ControlFrame,
    extract: impl FnOnce(
        shard_transport::control_protocol::ControlFrame,
    ) -> Result<T, shard_transport::control_protocol::ControlFrame>,
) -> shard_core::Result<T> {
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
        conn.request_typed(&frame, extract)
            .await
            .map_err(|e| shard_core::ShardError::Other(e.to_string()))
    })
}

fn create_via_daemon(
    repo: &str,
    name: Option<String>,
    branch: Option<String>,
) -> shard_core::Result<Workspace> {
    use shard_transport::control_protocol::ControlFrame;
    run_daemon_rpc(
        ControlFrame::CreateWorkspace {
            repo: repo.to_string(),
            name,
            mode: WorkspaceMode::NewBranch,
            branch,
        },
        |f| match f {
            ControlFrame::CreateWorkspaceAck { workspace } => Ok(workspace),
            other => Err(other),
        },
    )
}

fn list_via_daemon(repo: &str) -> shard_core::Result<Vec<WorkspaceWithStatus>> {
    use shard_transport::control_protocol::ControlFrame;
    run_daemon_rpc(
        ControlFrame::ListWorkspaces {
            repo: repo.to_string(),
        },
        |f| match f {
            ControlFrame::WorkspaceList { items } => Ok(items),
            other => Err(other),
        },
    )
}

/// Route `workspace remove` through the daemon so the lifecycle gate,
/// session stop, and watcher drop land in the correct order (fixes SHA-55).
fn remove_via_daemon(repo: &str, ws_name: &str) -> shard_core::Result<()> {
    use shard_transport::control_protocol::ControlFrame;
    run_daemon_rpc(
        ControlFrame::RemoveWorkspace {
            repo: repo.to_string(),
            name: ws_name.to_string(),
        },
        |f| match f {
            ControlFrame::RemoveWorkspaceAck => Ok(()),
            other => Err(other),
        },
    )
}
