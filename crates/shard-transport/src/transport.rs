use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};

/// Platform-abstracted IPC transport for session communication.
///
/// Windows: named pipes (`\\.\pipe\shard-session-{id}`)
/// Mac: Unix domain sockets (`/tmp/shard/session-{id}.sock`)
#[async_trait]
pub trait SessionTransport: Send + Sync + 'static {
    type Server: Send;
    type Client: AsyncRead + AsyncWrite + Send + Unpin;

    /// Create a server that listens for client connections.
    async fn bind(address: &str) -> std::io::Result<Self::Server>;

    /// Connect as a client.
    async fn connect(address: &str) -> std::io::Result<Self::Client>;

    /// Generate a platform-appropriate address for a session.
    fn session_address(session_id: &str) -> String;
}
