use std::path::Path;
use std::process::{Command, Stdio};

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
