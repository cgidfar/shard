pub mod control_protocol;
pub mod daemon_client;
pub mod protocol;
pub mod transport;

#[cfg(windows)]
pub mod transport_windows;

// #[cfg(unix)]
// pub mod transport_unix;

pub use transport::SessionTransport;

/// Return the platform-native transport type.
#[cfg(windows)]
pub type PlatformTransport = transport_windows::NamedPipeTransport;

/// Platform-native server type. Resolved through `PlatformTransport`'s
/// `SessionTransport::Server` associated type so call sites never name the
/// concrete Windows/Unix type directly.
#[cfg(windows)]
pub type PlatformServer = <PlatformTransport as SessionTransport>::Server;

/// Platform-native client type. Same rationale as `PlatformServer`.
#[cfg(windows)]
pub type PlatformClient = <PlatformTransport as SessionTransport>::Client;
