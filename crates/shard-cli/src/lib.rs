//! Library surface of the `shard-cli` crate.
//!
//! The CLI binary (`shardctl`) is the primary consumer, but the daemon
//! control loop and related helpers are also imported by the integration
//! test harness in `tests/`. Keep this surface narrow — nothing here should
//! be relied on by external crates.

pub mod attach;
pub mod cmd;
pub mod opts;
