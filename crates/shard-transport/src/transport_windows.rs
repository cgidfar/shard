use async_trait::async_trait;
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions};

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

    async fn accept(server: &Self::Server) -> std::io::Result<Self::Client> {
        // Wait for a client to connect to the existing server instance
        server.connect().await?;

        // The current server instance is now connected.
        // We need to return a client-like handle. But named pipes on Windows
        // work differently — the server IS the connected stream.
        // We'll handle this by having the caller use the server directly
        // after connect(). For multi-client support, we create new instances.
        //
        // This trait method doesn't map perfectly to Windows named pipes.
        // The actual multi-client pattern is handled in the event loop.
        // For now, connect as a client to the same pipe.
        let client = ClientOptions::new().open(server_name_placeholder())?;
        Ok(client)
    }

    async fn connect(address: &str) -> std::io::Result<Self::Client> {
        // Retry loop — the server may not be listening yet
        for _ in 0..50 {
            match ClientOptions::new().open(address) {
                Ok(client) => return Ok(client),
                Err(e) if e.raw_os_error() == Some(231) => {
                    // ERROR_PIPE_BUSY — server exists but all instances are busy
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

fn server_name_placeholder() -> &'static str {
    // This function exists because the accept() trait method doesn't
    // map well to Windows named pipes. The real multi-client pattern
    // is implemented directly in the event loop.
    unreachable!("use event loop's direct pipe handling instead")
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
