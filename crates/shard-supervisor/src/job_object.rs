/// Windows Job Object for orphan prevention.
///
/// Creates a Job Object with JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE.
/// When the supervisor process dies (for any reason), the OS
/// automatically terminates all processes in the job — including
/// the PTY child.
///
/// The job handle is held for the lifetime of this struct.
/// When dropped, the handle is closed, which triggers the kill.
#[cfg(windows)]
pub struct JobGuard {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl JobGuard {
    /// Create a new Job Object and assign the current process to it.
    ///
    /// All child processes spawned after this call will inherit the job.
    pub fn new() -> std::io::Result<Self> {
        use std::ptr::null;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::*;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;

        unsafe {
            let job = CreateJobObjectW(null(), std::ptr::null());
            if job == 0 {
                return Err(std::io::Error::last_os_error());
            }

            // Configure: kill all processes when the job handle is closed
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            let result = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if result == 0 {
                let err = std::io::Error::last_os_error();
                CloseHandle(job);
                return Err(err);
            }

            // Assign current process to the job
            let result = AssignProcessToJobObject(job, GetCurrentProcess());
            if result == 0 {
                let err = std::io::Error::last_os_error();
                CloseHandle(job);
                return Err(err);
            }

            Ok(JobGuard { handle: job })
        }
    }
}

#[cfg(windows)]
impl Drop for JobGuard {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}
