use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};

/// Put the child in its own session so terminal signals and closure never
/// reach it.
pub fn spawn(command: &mut Command) -> io::Result<Child> {
    // SAFETY: setsid is async-signal-safe and the child has not run any Rust
    // code between fork and exec in this closure.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()
}

pub fn alive(pid: u32) -> bool {
    // Signal 0 probes for existence. As in Go, a permission error is reported
    // as not alive.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

pub fn terminate(pid: u32) -> io::Result<()> {
    signal(pid, libc::SIGTERM)
}

pub fn kill(pid: u32) -> io::Result<()> {
    signal(pid, libc::SIGKILL)
}

fn signal(pid: u32, signal: libc::c_int) -> io::Result<()> {
    if unsafe { libc::kill(pid as libc::pid_t, signal) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
