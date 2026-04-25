use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::process::ProcessControl;

const STILL_ACTIVE: u32 = 259;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
const DETACHED_PROCESS: u32 = 0x00000008;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x01000000;

pub struct WindowsProcessControl;

impl ProcessControl for WindowsProcessControl {
    fn is_alive(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess};

        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle == 0 {
                return false;
            }
            let mut exit_code: u32 = 0;
            let result = GetExitCodeProcess(handle, &mut exit_code);
            CloseHandle(handle);
            result != 0 && exit_code == STILL_ACTIVE
        }
    }

    fn terminate(pid: u32) -> std::io::Result<()> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess};

        const PROCESS_TERMINATE: u32 = 0x0001;

        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if handle == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let result = TerminateProcess(handle, 1);
            CloseHandle(handle);
            if result == 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn spawn_detached(exe: &Path, args: &[String]) -> std::io::Result<u32> {
        use std::os::windows::process::CommandExt;

        // Redirect stderr to a crash log for debugging
        let crash_log = std::env::temp_dir().join("shard-supervisor-stderr.log");
        let stderr_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&crash_log)?;

        // Try with CREATE_BREAKAWAY_FROM_JOB first (survives parent Job Object),
        // fall back without it if the parent job doesn't allow breakaway.
        let child = Command::new(exe)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_file.try_clone()?))
            .creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB)
            .spawn()
            .or_else(|_| {
                Command::new(exe)
                    .args(args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::from(stderr_file))
                    .creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS)
                    .spawn()
            })?;

        Ok(child.id())
    }
}

/// Spawn a detached process and return both its PID and a process HANDLE.
///
/// The handle is opened with sufficient rights for `DaemonJobGuard::assign_process()`.
/// The caller MUST close the handle via `CloseHandle` after use.
///
/// This is used by the daemon to spawn supervisors and immediately assign them
/// to its Job Object before the handle is dropped.
pub fn spawn_detached_with_handle(
    exe: &Path,
    args: &[String],
) -> std::io::Result<(u32, windows_sys::Win32::Foundation::HANDLE)> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::OpenProcess;

    let crash_log = std::env::temp_dir().join("shard-supervisor-stderr.log");
    let stderr_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&crash_log)?;

    // Spawn without BREAKAWAY — we WANT the child to stay in the daemon's job
    // if the daemon itself is in one, or to be assignable to our job.
    let child = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS)
        .spawn()?;

    let pid = child.id();

    // Open a handle with the rights needed for job assignment.
    // Keep `child` alive until after OpenProcess to prevent PID reuse race.
    let handle = unsafe { OpenProcess(JOB_ASSIGN_ACCESS, 0, pid) };
    drop(child);
    if handle == 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok((pid, handle))
}

/// Access rights needed for AssignProcessToJobObject + liveness checks.
const JOB_ASSIGN_ACCESS: u32 = 0x0100  // PROCESS_SET_QUOTA
    | 0x0001                            // PROCESS_TERMINATE
    | 0x1000                            // PROCESS_QUERY_LIMITED_INFORMATION
    | 0x00100000;                       // SYNCHRONIZE

/// Open a handle to an already-running process for job re-adoption.
///
/// Returns the handle on success. The caller MUST close it via `CloseHandle`.
/// Returns an error if the process doesn't exist or access is denied.
pub fn open_process_for_job(pid: u32) -> std::io::Result<windows_sys::Win32::Foundation::HANDLE> {
    use windows_sys::Win32::System::Threading::OpenProcess;

    let handle = unsafe { OpenProcess(JOB_ASSIGN_ACCESS, 0, pid) };
    if handle == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(handle)
}

/// Force-terminate a specific supervisor PID, guarding against PID reuse.
///
/// Used by the daemon's `stop_session_and_wait` fallback path when a
/// supervisor fails to exit within the graceful-stop window. The
/// `expected_creation_time` must be the creation time captured by the
/// daemon when it originally spawned (or adopted) the supervisor — it's
/// compared against the live process's creation time before terminating
/// so we never kill a reused PID that now belongs to an unrelated
/// process. On mismatch (or if the handle can't be opened) the function
/// returns an error and does nothing.
pub fn force_kill_pid_checked(pid: u32, expected_creation_time: u64) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess};

    const PROCESS_TERMINATE: u32 = 0x0001;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const SYNCHRONIZE: u32 = 0x00100000;

    unsafe {
        let handle = OpenProcess(
            PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
            0,
            pid,
        );
        if handle == 0 {
            return Err(std::io::Error::last_os_error());
        }

        // Verify creation time matches. 0 means "unknown" (adoption fallback)
        // — in that case we trust the PID rather than refuse to kill, since
        // the alternative is leaving a stuck supervisor alive.
        if expected_creation_time != 0 {
            let actual = match get_process_creation_time(handle) {
                Ok(t) => t,
                Err(e) => {
                    CloseHandle(handle);
                    return Err(e);
                }
            };
            if actual != expected_creation_time {
                CloseHandle(handle);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!(
                        "pid {pid} creation time mismatch (expected {expected_creation_time}, got {actual}) — refusing to kill reused PID"
                    ),
                ));
            }
        }

        let result = TerminateProcess(handle, 1);
        if result == 0 {
            CloseHandle(handle);
            return Err(std::io::Error::last_os_error());
        }

        let wait_result = wait_for_process_handle(handle, Duration::from_secs(5));
        CloseHandle(handle);
        if !wait_result? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("pid {pid} did not exit after TerminateProcess"),
            ));
        }
    }
    Ok(())
}

/// Get the creation time of a process (for PID reuse detection).
///
/// Returns the creation time as a Windows FILETIME value (100ns intervals since 1601-01-01).
pub fn get_process_creation_time(
    process_handle: windows_sys::Win32::Foundation::HANDLE,
) -> std::io::Result<u64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;

    unsafe {
        let mut creation: FILETIME = std::mem::zeroed();
        let mut exit: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();

        let result = GetProcessTimes(
            process_handle,
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        );
        if result == 0 {
            return Err(std::io::Error::last_os_error());
        }

        let time = (creation.dwHighDateTime as u64) << 32 | creation.dwLowDateTime as u64;
        Ok(time)
    }
}

/// Wait for a PID to exit, guarding against PID reuse.
///
/// Returns `Ok(true)` if the process is gone/signaled, `Ok(false)` if the
/// timeout elapsed, and `Err` if the live PID fails the creation-time guard or
/// cannot be queried.
pub fn wait_for_pid_exit_checked(
    pid: u32,
    expected_creation_time: u64,
    timeout: Duration,
) -> std::io::Result<bool> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::OpenProcess;

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const SYNCHRONIZE: u32 = 0x00100000;

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid);
        if handle == 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(87) {
                return Ok(true);
            }
            return Err(err);
        }

        if expected_creation_time != 0 {
            let actual = match get_process_creation_time(handle) {
                Ok(t) => t,
                Err(e) => {
                    CloseHandle(handle);
                    return Err(e);
                }
            };
            if actual != expected_creation_time {
                CloseHandle(handle);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!(
                        "pid {pid} creation time mismatch (expected {expected_creation_time}, got {actual})"
                    ),
                ));
            }
        }

        let result = wait_for_process_handle(handle, timeout);
        CloseHandle(handle);
        result
    }
}

unsafe fn wait_for_process_handle(
    handle: windows_sys::Win32::Foundation::HANDLE,
    timeout: Duration,
) -> std::io::Result<bool> {
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    const WAIT_OBJECT_0: u32 = 0x00000000;
    const WAIT_TIMEOUT: u32 = 0x00000102;
    const WAIT_FAILED: u32 = 0xFFFFFFFF;

    let millis = timeout.as_millis().min(u32::MAX as u128) as u32;
    match WaitForSingleObject(handle, millis) {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        WAIT_FAILED => Err(std::io::Error::last_os_error()),
        other => Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("WaitForSingleObject returned unexpected code {other}"),
        )),
    }
}
