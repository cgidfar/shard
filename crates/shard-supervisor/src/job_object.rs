/// Windows Job Object for orphan prevention (per-supervisor).
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

/// Windows Job Object for daemon-level lifecycle management.
///
/// Unlike `JobGuard`, this does NOT assign the current process to the job.
/// The daemon holds this handle and assigns supervisor child processes to it.
/// On daemon crash, `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` terminates all
/// supervised processes. On intentional restart, `remove_kill_on_close()`
/// is called first so supervisors survive.
#[cfg(windows)]
pub struct DaemonJobGuard {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl DaemonJobGuard {
    /// Create a new Job Object for daemon use.
    ///
    /// The daemon process itself is NOT assigned to this job.
    /// Use `assign_process()` to add supervisor child processes.
    pub fn new() -> std::io::Result<Self> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::*;

        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job == 0 {
                return Err(std::io::Error::last_os_error());
            }

            // Configure: kill all processes when the job handle is closed.
            // Also allow breakaway so supervisors can create their own child jobs.
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags =
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;

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

            Ok(DaemonJobGuard { handle: job })
        }
    }

    /// Assign an external process to this job by its raw HANDLE.
    ///
    /// The handle must have been opened with at least
    /// `PROCESS_SET_QUOTA | PROCESS_TERMINATE`.
    /// The caller is responsible for closing the handle afterwards.
    pub fn assign_process(&self, process_handle: windows_sys::Win32::Foundation::HANDLE) -> std::io::Result<()> {
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        unsafe {
            let result = AssignProcessToJobObject(self.handle, process_handle);
            if result == 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Remove KILL_ON_JOB_CLOSE before intentional daemon exit.
    ///
    /// After this call, closing the job handle will NOT terminate
    /// the assigned processes. They survive for re-adoption by a
    /// new daemon instance.
    pub fn remove_kill_on_close(&self) -> std::io::Result<()> {
        use windows_sys::Win32::System::JobObjects::*;

        unsafe {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            // Clear all limit flags (specifically removes KILL_ON_JOB_CLOSE)
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK;

            let result = SetInformationJobObject(
                self.handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if result == 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for DaemonJobGuard {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

// Safety: The job handle can be sent between threads.
#[cfg(windows)]
unsafe impl Send for DaemonJobGuard {}
#[cfg(windows)]
unsafe impl Sync for DaemonJobGuard {}
