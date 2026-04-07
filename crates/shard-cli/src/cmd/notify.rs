use std::io::{Read, Write};

/// Send an activity state notification to the session supervisor.
///
/// Called by harness hook scripts (e.g., Claude Code hooks) running inside
/// the PTY. Reads `SHARD_PIPE_ADDR` from the environment, connects to the
/// supervisor's named pipe, and sends a single `ActivityUpdate` frame.
///
/// Uses synchronous I/O only — no tokio runtime needed for fast startup.
pub fn run(state: String) -> shard_core::Result<()> {
    // Drain stdin — Claude Code hooks pipe JSON context via stdin and will
    // block if we don't consume it before exiting.
    let _ = std::io::stdin().lock().read_to_end(&mut Vec::new());

    let pipe_addr = match std::env::var("SHARD_PIPE_ADDR") {
        Ok(addr) if !addr.is_empty() => addr,
        _ => return Ok(()), // Not running inside a Shard session
    };

    let state_byte: u8 = match state.as_str() {
        "active" => 0x00,
        "idle" => 0x01,
        "blocked" => 0x02,
        _ => return Ok(()), // Unknown state, silently ignore
    };

    // Wire format: [u32 length=2][u8 type=0x07][u8 state]
    let frame: [u8; 6] = [0x00, 0x00, 0x00, 0x02, 0x07, state_byte];

    // Connect to the supervisor's named pipe and send the frame.
    // Silently ignore errors — hook failures must never block the agent.
    let result = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&pipe_addr)
        .and_then(|mut pipe| pipe.write_all(&frame));

    if let Err(e) = result {
        // Log at debug level — only visible if RUST_LOG is set
        tracing::debug!("notify failed for {pipe_addr}: {e}");
    }

    Ok(())
}
