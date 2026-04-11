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
