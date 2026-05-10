//! Library surface of the `shard-cli` crate.
//!
//! The CLI binary (`shardctl`) is the primary consumer, but the daemon
//! control loop and related helpers are also imported by the integration
//! test harness in `tests/`. Keep this surface narrow — nothing here should
//! be relied on by external crates.

pub mod attach;
pub mod cmd;
pub mod opts;

/// Advisory client_id sent in the supervisor `Hello` frame from CLI
/// processes (`shardctl attach`, the daemon's stop-and-drain probe, etc).
/// Process-stable but not cryptographically unique — the supervisor's
/// authoritative key is its server-assigned `conn_id`, so collisions
/// across simultaneously-running CLI processes are advisory only.
pub fn cli_client_id() -> u64 {
    std::process::id() as u64 ^ 0xC11A_7AC4_C11A_7AC4u64
}
