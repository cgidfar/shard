use clap::Parser;
use shard_cli::cmd;
use shard_cli::opts::{Cli, Commands};

fn main() {
    // Note: the `serve` and `daemon start` subcommands set up their own logging.
    // For all other commands, log to stderr with WARN level by default.
    let is_serve = std::env::args().any(|a| a == "serve");
    let is_notify = std::env::args().any(|a| a == "notify");
    let is_daemon_start = {
        let args: Vec<_> = std::env::args().collect();
        args.iter().any(|a| a == "daemon")
            && args.iter().any(|a| a == "start")
    };
    if !is_serve && !is_notify && !is_daemon_start {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive(tracing::Level::WARN.into()),
            )
            .init();
    }

    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Repo { command } => cmd::repo::run(command),
        Commands::Workspace { command } => cmd::workspace::run(command),
        Commands::Session { command } => cmd::session::run(command),
        Commands::Prune { command } => cmd::prune::run(command),
        Commands::Notify { state } => cmd::notify::run(state),
        Commands::Daemon { command } => cmd::daemon::run(command),
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
