//! Wire-level client for the daemon control pipe.
//!
//! Provides connect + typed frame send/receive. Does NOT handle
//! spawn logic or exe resolution — those belong one layer up.

use std::fmt;

use tokio::io::{AsyncRead, AsyncWrite};

/// Error from a typed request to the daemon.
///
/// Separates transport failures from daemon-reported errors so callers can
/// surface different messages (e.g., "daemon not running" vs. "workspace is
/// being deleted"). Transport errors are usually worth retrying;
/// `DaemonReported` errors are not.
#[derive(Debug)]
pub enum DaemonError {
    /// The daemon responded with an `Error { message }` frame.
    Reported(String),
    /// Local transport error (pipe closed, decode failure, unexpected frame).
    Transport(std::io::Error),
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DaemonError::Reported(msg) => write!(f, "{msg}"),
            DaemonError::Transport(e) => write!(f, "transport error: {e}"),
        }
    }
}

impl std::error::Error for DaemonError {}

impl From<std::io::Error> for DaemonError {
    fn from(e: std::io::Error) -> Self {
        DaemonError::Transport(e)
    }
}

/// Windows error code for ERROR_PIPE_BUSY (231).
#[cfg(windows)]
pub const ERROR_PIPE_BUSY: i32 = 231;

use crate::control_protocol::{
    read_control_frame, write_control_frame, ControlFrame, PROTOCOL_VERSION,
};

#[cfg(windows)]
use crate::control_protocol::CONTROL_PIPE_NAME;

#[cfg(windows)]
pub type NamedPipeDaemonConnection =
    DaemonConnection<tokio::net::windows::named_pipe::NamedPipeClient>;

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

    /// Send a control frame and extract a typed response.
    ///
    /// Folds the `Error { message }` response frame into `DaemonError::Reported`
    /// so callers don't need to pattern-match on it at every call site. The
    /// `extract` closure runs only for non-error responses; if it returns
    /// `None`, the frame is treated as a protocol violation and surfaced as
    /// a transport error. Typical usage:
    ///
    /// ```ignore
    /// conn.request_typed(&ControlFrame::ListSessions, |f| match f {
    ///     ControlFrame::SessionList { sessions } => Some(sessions),
    ///     _ => None,
    /// }).await?
    /// ```
    pub async fn request_typed<T>(
        &mut self,
        frame: &ControlFrame,
        extract: impl FnOnce(ControlFrame) -> Result<T, ControlFrame>,
    ) -> Result<T, DaemonError> {
        let response = self.request(frame).await?;
        match response {
            ControlFrame::Error { message } => Err(DaemonError::Reported(message)),
            other => extract(other).map_err(|bad| {
                DaemonError::Transport(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unexpected response frame: {bad:?}"),
                ))
            }),
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

/// Try to connect to the daemon's default control pipe.
///
/// Returns `Ok(connection)` if the daemon is running and the pipe exists.
/// Returns `Err` with `NotFound` if the daemon is not running.
#[cfg(windows)]
pub async fn connect() -> std::io::Result<NamedPipeDaemonConnection> {
    connect_to(CONTROL_PIPE_NAME).await
}

/// Connect to the daemon, running `spawn` only when the control pipe is absent.
///
/// Callers own executable resolution and process creation; this helper owns the
/// transport decision tree so CLI and app callers handle `NotFound`, busy
/// pipes, and post-spawn readiness the same way.
#[cfg(windows)]
pub async fn connect_or_spawn(
    spawn: impl FnOnce() -> std::io::Result<()>,
    startup_timeout: std::time::Duration,
) -> std::io::Result<NamedPipeDaemonConnection> {
    match connect().await {
        Ok(conn) => return Ok(conn),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
            return connect_with_retry(startup_timeout).await;
        }
        Err(e) => return Err(e),
    }

    spawn()?;
    connect_with_retry(startup_timeout).await
}

/// Try to connect to a specific control pipe by name. Production callers
/// should use [`connect`]; the integration test harness uses this to reach
/// a headless daemon running on a unique per-test pipe.
#[cfg(windows)]
pub async fn connect_to(pipe_name: &str) -> std::io::Result<NamedPipeDaemonConnection> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let client = ClientOptions::new().open(pipe_name)?;
    Ok(DaemonConnection::new(client))
}

/// Try to connect to the daemon's default control pipe with retries.
///
/// Retries for `timeout` duration with 100ms intervals.
/// Useful after spawning the daemon to wait for it to become ready.
#[cfg(windows)]
pub async fn connect_with_retry(
    timeout: std::time::Duration,
) -> std::io::Result<NamedPipeDaemonConnection> {
    connect_to_with_retry(CONTROL_PIPE_NAME, timeout).await
}

/// Like [`connect_with_retry`], but against an explicit pipe name.
#[cfg(windows)]
pub async fn connect_to_with_retry(
    pipe_name: &str,
    timeout: std::time::Duration,
) -> std::io::Result<NamedPipeDaemonConnection> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let start = std::time::Instant::now();
    loop {
        match ClientOptions::new().open(pipe_name) {
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
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
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
