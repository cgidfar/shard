use shard_transport::control_protocol::ControlFrame;
use shard_transport::daemon_client;

/// Route a single CLI request through the daemon without spawning it.
pub(crate) fn run<T>(
    frame: ControlFrame,
    extract: impl FnOnce(ControlFrame) -> Result<T, ControlFrame>,
) -> shard_core::Result<T> {
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
