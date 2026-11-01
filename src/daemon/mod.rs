//! Detached process startup and pid-file lifecycle.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;

#[derive(Debug)]
pub enum StopError {
    NotRunning(String),
    Io(io::Error),
}

impl std::fmt::Display for StopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRunning(detail) => write!(f, "gateway is not running ({detail})"),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for StopError {}

impl From<io::Error> for StopError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Launch `exe` detached, with stdout and stderr appended to `log_path`.
pub fn start(exe: &Path, args: &[String], log_path: &Path) -> io::Result<Child> {
    let log = open_log(log_path)?;
    let mut command = Command::new(exe);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    platform::spawn(&mut command)
}

pub fn write_pid(path: &Path, pid: u32) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    writeln!(file, "{pid}")
}

pub fn read_pid(path: &Path) -> io::Result<u32> {
    let text = std::fs::read_to_string(path)?;
    let text = text.trim();
    let pid: u32 = text.parse().map_err(|_| invalid_pid(path, text))?;
    if pid == 0 {
        return Err(invalid_pid(path, text));
    }
    Ok(pid)
}

fn invalid_pid(path: &Path, text: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("pid file {}: invalid contents {text:?}", path.display()),
    )
}

/// Delete `path` only while it still records `pid`.
pub fn remove_pid(path: &Path, pid: u32) {
    if read_pid(path).is_ok_and(|current| current == pid) {
        let _ = std::fs::remove_file(path);
    }
}

/// The live gateway recorded in the pid file: `None` when none is running or
/// the file is absent; `Err` when it cannot be read. A stale entry is removed.
pub fn running_pid(path: &Path) -> io::Result<Option<u32>> {
    let pid = match read_pid(path) {
        Ok(pid) => pid,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if alive(pid) {
        return Ok(Some(pid));
    }
    remove_pid(path, pid);
    Ok(None)
}

/// Stop the gateway recorded in `path` and remove the pid file.
pub fn stop(path: &Path, grace: Duration) -> Result<u32, StopError> {
    let pid = match read_pid(path) {
        Ok(pid) => pid,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(StopError::NotRunning(format!(
                "no pid file at {}",
                path.display()
            )));
        }
        Err(error) => return Err(StopError::Io(error)),
    };
    if !alive(pid) {
        remove_pid(path, pid);
        return Err(StopError::NotRunning(format!(
            "stale pid file for {pid} removed"
        )));
    }
    platform::terminate(pid)?;
    if !wait_exit(pid, grace) {
        platform::kill(pid)?;
        if !wait_exit(pid, Duration::from_secs(2)) {
            return Err(StopError::Io(io::Error::other(format!(
                "pid {pid} did not exit after kill"
            ))));
        }
    }
    remove_pid(path, pid);
    Ok(pid)
}

pub fn alive(pid: u32) -> bool {
    pid != 0 && platform::alive(pid)
}

fn wait_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

fn open_log(path: &Path) -> io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Turn a listen address into a dialable probe address. Wildcard hosts map to
/// loopback; other hosts are left alone.
pub fn probe_address(listen: &str) -> String {
    let (host, port) = match listen.rsplit_once(':') {
        Some((host, port)) if !port.contains(']') => (host, port),
        _ => return listen.to_string(),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() || host == "0.0.0.0" || host == "::" {
        format!("127.0.0.1:{port}")
    } else if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_file_roundtrip_and_guarded_removal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gateway.pid");
        write_pid(&path, 1234).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1234\n");
        assert_eq!(read_pid(&path).unwrap(), 1234);
        remove_pid(&path, 9999);
        assert!(path.exists());
        remove_pid(&path, 1234);
        assert!(!path.exists());
    }

    #[test]
    fn invalid_pid_contents_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gateway.pid");
        std::fs::write(&path, "invalid pid").unwrap();
        assert!(read_pid(&path).is_err());
        std::fs::write(&path, "0").unwrap();
        assert!(read_pid(&path).is_err());
    }

    #[test]
    fn missing_pid_file_reports_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.pid");
        assert_eq!(running_pid(&path).unwrap(), None);
        let error = stop(&path, Duration::from_millis(10)).unwrap_err();
        assert!(error.to_string().contains("no pid file"));
    }

    #[test]
    fn probe_address_maps_wildcards() {
        assert_eq!(probe_address(":8080"), "127.0.0.1:8080");
        assert_eq!(probe_address("0.0.0.0:8080"), "127.0.0.1:8080");
        assert_eq!(probe_address("[::]:8080"), "127.0.0.1:8080");
        assert_eq!(probe_address("localhost:8080"), "localhost:8080");
        assert_eq!(probe_address("127.0.0.1:8080"), "127.0.0.1:8080");
    }
}
