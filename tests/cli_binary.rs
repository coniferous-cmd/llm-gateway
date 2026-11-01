//! Binary-level CLI behavior: exit codes, output routing, and daemon lifecycle.

use std::io::{self, Read};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_llm-gateway");

fn run(args: &[&str]) -> Output {
    try_run(args).unwrap_or_else(|error| panic!("command {args:?}: {error}"))
}

fn try_run(args: &[&str]) -> io::Result<Output> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut child = Command::new(BIN)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Drain both pipes concurrently so verbose output cannot block the child.
    // Bound EOF waits too: an exited launcher can leave pipe handles inherited
    // by its daemon, which would make wait_with_output hang indefinitely.
    let stdout = read_pipe(child.stdout.take().unwrap());
    let stderr = read_pipe(child.stderr.take().unwrap());
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "process did not exit",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let collect = |pipe: mpsc::Receiver<io::Result<Vec<u8>>>| {
        pipe.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("output did not reach EOF after process exit: {error}"),
                )
            })?
    };
    Ok(Output {
        status,
        stdout: collect(stdout)?,
        stderr: collect(stderr)?,
    })
}

fn read_pipe(mut pipe: impl Read + Send + 'static) -> mpsc::Receiver<io::Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = pipe.read_to_end(&mut bytes).map(|_| bytes);
        let _ = sender.send(result);
    });
    receiver
}

fn config(path: &Path, listen: &str) {
    std::fs::write(
        path,
        format!(
            "listen: '{listen}'\nkeys:\n  - token: dev\n    target:\n      base_url: http://127.0.0.1:1\n"
        ),
    )
    .unwrap();
}

fn free_port() -> Option<u16> {
    TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|listener| listener.local_addr().ok())
        .map(|address| address.port())
}

#[test]
fn version_help_and_keygen_work_without_runtime_resources() {
    let version = run(&["version"]);
    assert!(version.status.success());
    assert!(version.stderr.is_empty());
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("llm-gateway "));

    let help = run(&["--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("Available Commands:"));

    let keygen = run(&["keygen"]);
    assert!(keygen.status.success());
    let text = String::from_utf8_lossy(&keygen.stdout);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(lines[0].starts_with("identity_key: "), "{text}");
    assert!(lines[1].starts_with("identity_pub: "), "{text}");

    let extra = run(&["version", "extra"]);
    assert_eq!(extra.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&extra.stderr).contains("unknown command"));
}

#[test]
fn check_config_is_offline_and_routes_errors_to_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    config(&path, "127.0.0.1:0");
    let path_text = path.display().to_string();

    let valid = run(&["check-config", "--config", &path_text]);
    assert!(valid.status.success());
    assert!(
        String::from_utf8_lossy(&valid.stdout)
            .contains(&format!("configuration valid: {path_text}"))
    );
    assert!(valid.stderr.is_empty());

    let missing = dir.path().join("missing.yaml");
    let missing_text = missing.display().to_string();
    let invalid = run(&["check-config", "--config", &missing_text]);
    assert_eq!(invalid.status.code(), Some(1));
    assert!(invalid.stdout.is_empty());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("configuration check failed:"));
}

#[test]
fn foreground_errors_go_to_the_log_file() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.yaml");
    let pid_path = dir.path().join("gateway.pid");
    let log_path = dir.path().join("gateway.log");
    config(&config_path, "127.0.0.1:0");
    std::fs::write(&pid_path, "invalid pid").unwrap();

    let output = run(&[
        "serve",
        "--config",
        &config_path.display().to_string(),
        "--pid-file",
        &pid_path.display().to_string(),
        "--log-file",
        &log_path.display().to_string(),
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert_eq!(log.lines().count(), 1, "{log}");
    assert!(log.contains("configuration failed"), "{log}");
}

#[test]
fn stop_without_a_pid_file_reports_json_on_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let pid_path = dir.path().join("missing.pid");
    let pid_text = pid_path.display().to_string();
    let output = run(&["stop", "--pid-file", &pid_text]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let event: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("invalid JSON: {error}: {stdout}"));
    assert_eq!(event["level"], "ERROR", "{stdout}");
    assert_eq!(event["msg"], "command failed", "{stdout}");
    let error = event["error"].as_str().expect("error must be a string");
    assert!(error.contains("stop failed"), "{stdout}");
    assert!(error.contains(&pid_text), "{stdout}");
}

struct DaemonGuard {
    pid_path: PathBuf,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = try_run(&["stop", "--pid-file", &self.pid_path.display().to_string()]);
    }
}

#[test]
fn daemon_starts_waits_ready_and_stops() {
    let Some(port) = free_port() else {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.yaml");
    let pid_path = dir.path().join("gateway.pid");
    let log_path = dir.path().join("gateway.log");
    config(&config_path, &format!("127.0.0.1:{port}"));
    let guard = DaemonGuard {
        pid_path: pid_path.clone(),
    };

    let output = run(&[
        "serve",
        "--daemon",
        "--config",
        &config_path.display().to_string(),
        "--pid-file",
        &pid_path.display().to_string(),
        "--log-file",
        &log_path.display().to_string(),
    ]);
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("started in background"));
    assert!(pid_path.exists(), "pid file should exist");
    assert!(log_path.exists(), "log file should exist");

    std::thread::sleep(Duration::from_millis(100));
    let stop = run(&["stop", "--pid-file", &pid_path.display().to_string()]);
    assert!(
        stop.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&stop.stdout),
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(!pid_path.exists(), "pid file should be removed");
    drop(guard);
}
