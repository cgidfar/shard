use async_trait::async_trait;
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};

use crate::transport::SessionTransport;

pub struct NamedPipeTransport;

#[async_trait]
impl SessionTransport for NamedPipeTransport {
    type Server = NamedPipeServer;
    type Client = NamedPipeClient;

    async fn bind(address: &str) -> std::io::Result<Self::Server> {
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(address)?;
        Ok(server)
    }

    async fn connect(address: &str) -> std::io::Result<Self::Client> {
        // Retry loop — the server may not be listening yet
        for _ in 0..50 {
            match ClientOptions::new().open(address) {
                Ok(client) => return Ok(client),
                Err(e) if e.raw_os_error() == Some(crate::daemon_client::ERROR_PIPE_BUSY) => {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Pipe doesn't exist yet
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timed out connecting to named pipe",
        ))
    }

    fn session_address(session_id: &str) -> String {
        format!(r"\\.\pipe\shard-session-{session_id}")
    }
}

/// Create a new named pipe server instance for multi-client support.
///
/// After one client connects to the first instance, create a new instance
/// with the same name to accept the next client.
pub fn create_pipe_instance(address: &str, first: bool) -> std::io::Result<NamedPipeServer> {
    ServerOptions::new()
        .first_pipe_instance(first)
        .create(address)
}

/// Generate the named pipe address for a session.
///
/// Equivalent to `NamedPipeTransport::session_address()` but callable
/// without going through the trait (for use in daemon/client code).
pub fn session_pipe_name(session_id: &str) -> String {
    format!(r"\\.\pipe\shard-session-{session_id}")
}
