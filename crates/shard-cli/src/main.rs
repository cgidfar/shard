pub mod attach;
mod cmd;
mod opts;

use clap::Parser;
use opts::{Cli, Commands};

fn main() {
    // Note: the `serve` subcommand sets up its own logging to a file.
    // For all other commands, log to stderr with WARN level by default.
    // Check if we're running as a supervisor before initializing the
    // default subscriber (to avoid double-init).
    let is_serve = std::env::args().any(|a| a == "serve");
    if !is_serve {
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
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
