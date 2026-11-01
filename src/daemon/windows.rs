use std::io;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    SetHandleInformation, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
};

/// Run the child without a console of its own or the launcher's stdio pipes.
pub fn spawn(command: &mut Command) -> io::Result<Child> {
    // Command::spawn inherits all inheritable handles on Windows, not just
    // the explicitly configured stdio. Keeping a captured launcher pipe open
    // in the daemon would make the caller's Command::output wait for EOF
    // until the daemon exits. This launcher runs before the Tokio runtime.
    let _stdio = StdioInheritanceGuard::new()?;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    command.spawn()
}

struct StdioInheritanceGuard(Vec<(HANDLE, u32)>);

impl StdioInheritanceGuard {
    fn new() -> io::Result<Self> {
        let mut guard = Self(Vec::new());
        for handle in [
            io::stdin().as_raw_handle(),
            io::stdout().as_raw_handle(),
            io::stderr().as_raw_handle(),
        ] {
            if handle.is_null()
                || handle == INVALID_HANDLE_VALUE
                || guard.0.iter().any(|(saved, _)| *saved == handle)
            {
                continue;
            }
            let mut flags = 0;
            // SAFETY: these are the current process's standard handles. The
            // guard restores their inheritance flags, including on errors.
            unsafe {
                if GetHandleInformation(handle, &mut flags) == 0 {
                    return Err(io::Error::last_os_error());
                }
                guard.0.push((handle, flags));
                if SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) == 0 {
                    return Err(io::Error::last_os_error());
                }
            }
        }
        Ok(guard)
    }
}

impl Drop for StdioInheritanceGuard {
    fn drop(&mut self) {
        for &(handle, flags) in &self.0 {
            // SAFETY: the handles remain owned by the process; only the
            // inheritance bit changed while spawning the detached child.
            unsafe {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, flags & HANDLE_FLAG_INHERIT);
            }
        }
    }
}

/// A zero-timeout wait distinguishes a running process from one whose handle
/// is merely still held by another process.
pub fn alive(pid: u32) -> bool {
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let event = unsafe { WaitForSingleObject(handle, 0) };
    unsafe {
        CloseHandle(handle);
    }
    event == WAIT_TIMEOUT
}

/// Windows has no cross-process SIGTERM equivalent.
pub fn terminate(pid: u32) -> io::Result<()> {
    kill(pid)
}

pub fn kill(pid: u32) -> io::Result<()> {
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let result = unsafe { TerminateProcess(handle, 1) };
    unsafe {
        CloseHandle(handle);
    }
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
