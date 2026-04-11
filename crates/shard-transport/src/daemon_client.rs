//! Wire-level client for the daemon control pipe.
//!
//! Provides connect + typed frame send/receive. Does NOT handle
//! spawn logic or exe resolution — those belong one layer up.

use tokio::io::{AsyncRead, AsyncWrite};

use crate::control_protocol::{
    read_control_frame, write_control_frame, ControlFrame, PROTOCOL_VERSION,
};

#[cfg(windows)]
use crate::control_protocol::CONTROL_PIPE_NAME;

/// A connected control-pipe client.
///
/// Wraps an async read/write stream and provides typed frame operations.
pub struct DaemonConnection<S> {
    stream: S,
}

impl<S: AsyncRead + AsyncWrite + Unpin> DaemonConnection<S> {
    /// Wrap an already-connected stream.
    pub fn new(stream: S) -> Self {
        Self { stream }
    }

    /// Perform the Hello/HelloAck handshake.
    ///
    /// Returns the daemon's reported version string on success.
    pub async fn handshake(&mut self) -> std::io::Result<String> {
        write_control_frame(
            &mut self.stream,
            &ControlFrame::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .await?;

        match read_control_frame(&mut self.stream).await? {
            Some(ControlFrame::HelloAck {
                protocol_version,
                daemon_version,
            }) => {
                if protocol_version != PROTOCOL_VERSION {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "protocol version mismatch: client={PROTOCOL_VERSION}, daemon={protocol_version}"
                        ),
                    ));
                }
                Ok(daemon_version)
            }
            Some(ControlFrame::Error { message }) => Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                format!("daemon rejected handshake: {message}"),
            )),
            Some(other) => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("expected HelloAck, got {other:?}"),
            )),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "daemon closed connection during handshake",
            )),
        }
    }

    /// Send a control frame and read the response.
    pub async fn request(&mut self, frame: &ControlFrame) -> std::io::Result<ControlFrame> {
        write_control_frame(&mut self.stream, frame).await?;
        match read_control_frame(&mut self.stream).await? {
            Some(frame) => Ok(frame),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "daemon closed connection before responding",
            )),
        }
    }

    /// Send a frame without waiting for a response.
    pub async fn send(&mut self, frame: &ControlFrame) -> std::io::Result<()> {
        write_control_frame(&mut self.stream, frame).await
    }

    /// Read the next frame from the daemon.
    pub async fn recv(&mut self) -> std::io::Result<Option<ControlFrame>> {
        read_control_frame(&mut self.stream).await
    }
}

/// Try to connect to the daemon's control pipe.
///
/// Returns `Ok(connection)` if the daemon is running and the pipe exists.
/// Returns `Err` with `NotFound` if the daemon is not running.
#[cfg(windows)]
pub async fn connect() -> std::io::Result<DaemonConnection<tokio::net::windows::named_pipe::NamedPipeClient>> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let client = ClientOptions::new().open(CONTROL_PIPE_NAME)?;
    Ok(DaemonConnection::new(client))
}

/// Try to connect to the daemon's control pipe with retries.
///
/// Retries for `timeout` duration with 100ms intervals.
/// Useful after spawning the daemon to wait for it to become ready.
#[cfg(windows)]
pub async fn connect_with_retry(
    timeout: std::time::Duration,
) -> std::io::Result<DaemonConnection<tokio::net::windows::named_pipe::NamedPipeClient>> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let start = std::time::Instant::now();
    loop {
        match ClientOptions::new().open(CONTROL_PIPE_NAME) {
            Ok(client) => return Ok(DaemonConnection::new(client)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if start.elapsed() >= timeout {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "timed out waiting for daemon control pipe",
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(e) if e.raw_os_error() == Some(231) => {
                // ERROR_PIPE_BUSY
                if start.elapsed() >= timeout {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "timed out waiting for daemon (pipe busy)",
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(e) => return Err(e),
        }
    }
}
