use std::path::Path;

/// Platform-abstracted process lifecycle management.
pub trait ProcessControl {
    /// Check if a process is still running.
    fn is_alive(pid: u32) -> bool;

    /// Terminate a process (hard kill).
    fn terminate(pid: u32) -> std::io::Result<()>;

    /// Spawn a detached supervisor process that outlives the parent.
    /// Returns the supervisor's PID.
    fn spawn_detached(exe: &Path, args: &[String]) -> std::io::Result<u32>;
}

#[cfg(windows)]
pub use crate::process_windows::WindowsProcessControl as PlatformProcessControl;
