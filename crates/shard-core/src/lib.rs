pub mod db;
pub mod error;
pub mod git;
pub mod harness;
pub mod hooks;
pub mod paths;
pub mod repos;
pub mod sessions;
pub mod shell;
pub mod workspaces;

pub use error::{ShardError, Result};
pub use harness::Harness;
pub use paths::ShardPaths;
pub use shell::default_command;

/// Application name used for window titles, tray tooltips, etc.
pub const APP_NAME: &str = "Shard";

/// GUI executable name (platform-specific).
#[cfg(windows)]
pub const APP_EXE: &str = "shard-app.exe";
#[cfg(not(windows))]
pub const APP_EXE: &str = "shard-app";
