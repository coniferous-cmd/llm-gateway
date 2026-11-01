use std::io;
use std::os::windows::process::CommandExt;
use std::process::Command;

use windows_sys::Win32::Foundation::{CloseHandle, SYNCHRONIZE, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
};

/// Run the child without a console of its own.
pub fn detach(command: &mut Command) {
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
}

/// A zero-timeout wait distinguishes a running process from one whose handle
/// is merely still held by another process.
pub fn alive(pid: u32) -> bool {
    let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
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
