//! Command-line parsing and command dispatch.
//!
//! The parser is hand-written to preserve the reference CLI's exact exit
//! codes, output routing, legacy flag normalization, and flag-scoping rules.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::json;

use crate::config;
use crate::conversationlog::Store;
use crate::daemon;
use crate::e2ee::{Dialer, Identity};
use crate::keymap::Map;
use crate::logging::{JsonLogger, fields};
use crate::proxy::forward::{Forwarder, PeerDialer};
use crate::server::{self, AppState};

const STOP_GRACE: Duration = Duration::from_secs(12);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    Root,
    Serve,
    Stop,
    CheckConfig,
    Keygen,
    Version,
    Completion,
}

impl Command {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "serve" => Self::Serve,
            "stop" => Self::Stop,
            "check-config" => Self::CheckConfig,
            "keygen" => Self::Keygen,
            "version" => Self::Version,
            "completion" => Self::Completion,
            _ => return None,
        })
    }
}

#[derive(Default)]
struct Parsed {
    command: Option<Command>,
    config: Option<String>,
    daemon: bool,
    log_file: Option<String>,
    pid_file: Option<String>,
    legacy_stop: bool,
    legacy_version: bool,
    legacy_keygen: bool,
    legacy_check: bool,
    shell: Option<String>,
    help: bool,
}

impl Parsed {
    fn command(&self) -> Command {
        self.command.unwrap_or(Command::Root)
    }
}

struct UsageError(String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Run the CLI and return its process exit code.
pub fn run(args: Vec<String>, stdout: Box<dyn Write + Send>, stderr: Box<dyn Write + Send>) -> i32 {
    let out = SharedWriter::new(stdout);
    let err = SharedWriter::new(stderr);
    let logger = JsonLogger::new(Box::new(out.clone()));

    let args = normalize_legacy(args);
    let parsed = match parse(&args) {
        Ok(parsed) => parsed,
        Err(error) => {
            let mut stderr = err.clone();
            let _ = writeln!(stderr, "{error}");
            return 2;
        }
    };
    if parsed.help {
        let mut stdout = out.clone();
        print_help(parsed.command(), &mut stdout);
        return 0;
    }

    match parsed.command() {
        Command::Root => run_root(&parsed, &out, &err, &logger),
        Command::Serve => run_serve(&parsed, &logger),
        Command::Stop => run_stop(&parsed, &logger),
        Command::CheckConfig => run_check_config(&parsed, &out, &err),
        Command::Keygen => run_keygen(&out, &err),
        Command::Version => run_version(&out),
        Command::Completion => run_completion(&parsed, &out, &err),
    }
}

fn run_root(
    parsed: &Parsed,
    out: &SharedWriter,
    err: &SharedWriter,
    logger: &Arc<JsonLogger>,
) -> i32 {
    if parsed.legacy_check {
        if let Some(operation) = check_conflict(parsed) {
            let mut stderr = err.clone();
            let _ = writeln!(
                stderr,
                "configuration check failed: --check-config cannot be combined with {operation}"
            );
            return 1;
        }
        return run_check_config(parsed, out, err);
    }
    if parsed.legacy_keygen {
        return run_keygen(out, err);
    }
    if parsed.legacy_version {
        return run_version(out);
    }
    if parsed.legacy_stop && parsed.daemon {
        return gateway_error(logger, "--daemon and --stop cannot be combined");
    }
    if parsed.legacy_stop {
        return run_stop(parsed, logger);
    }
    run_serve(parsed, logger)
}

fn check_conflict(parsed: &Parsed) -> Option<&'static str> {
    [
        ("--daemon", parsed.daemon),
        ("--stop", parsed.legacy_stop),
        ("--keygen", parsed.legacy_keygen),
        ("--version", parsed.legacy_version),
    ]
    .into_iter()
    .find_map(|(name, enabled)| enabled.then_some(name))
}

fn gateway_error(logger: &Arc<JsonLogger>, message: impl Into<String>) -> i32 {
    let mut event = fields();
    event.insert("error".into(), json!(message.into()));
    logger.error("command failed", event);
    1
}

fn run_check_config(parsed: &Parsed, out: &SharedWriter, err: &SharedWriter) -> i32 {
    let path = match resolve_config_path(parsed.config.as_deref()) {
        Ok(path) => path,
        Err(error) => {
            let mut stderr = err.clone();
            let _ = writeln!(stderr, "configuration check failed: {error}");
            return 1;
        }
    };
    match config::load(&path) {
        Ok(_) => {
            let mut stdout = out.clone();
            let _ = writeln!(stdout, "configuration valid: {}", path.display());
            0
        }
        Err(error) => {
            let mut stderr = err.clone();
            let _ = writeln!(stderr, "configuration check failed: {error}");
            1
        }
    }
}

fn run_keygen(out: &SharedWriter, err: &SharedWriter) -> i32 {
    match generate_keys(out) {
        Ok(()) => 0,
        Err(error) => {
            let mut stderr = err.clone();
            let _ = writeln!(stderr, "keygen failed: {error}");
            1
        }
    }
}

fn generate_keys(out: &SharedWriter) -> Result<(), String> {
    let params: snow::params::NoiseParams = "Noise_IK_25519_ChaChaPoly_BLAKE2s"
        .parse()
        .map_err(|error: snow::Error| error.to_string())?;
    let keypair = snow::Builder::new(params)
        .generate_keypair()
        .map_err(|error| error.to_string())?;
    let private = base64::engine::general_purpose::STANDARD.encode(&keypair.private);
    let public = base64::engine::general_purpose::STANDARD.encode(&keypair.public);
    let mut stdout = out.clone();
    writeln!(stdout, "identity_key: {private}").map_err(|e| e.to_string())?;
    writeln!(stdout, "identity_pub: {public}").map_err(|e| e.to_string())?;
    Ok(())
}

fn run_version(out: &SharedWriter) -> i32 {
    let mut stdout = out.clone();
    let _ = writeln!(stdout, "llm-gateway {}", env!("LLM_GATEWAY_VERSION"));
    0
}

fn run_completion(parsed: &Parsed, out: &SharedWriter, err: &SharedWriter) -> i32 {
    let Some(shell) = parsed.shell.as_deref() else {
        let mut stderr = err.clone();
        let _ = writeln!(
            stderr,
            "unknown command \"completion\" needs a shell argument"
        );
        return 2;
    };
    let script = match shell {
        "bash" => {
            "# bash completion for llm-gateway\ncomplete -W 'serve stop check-config keygen version completion' llm-gateway\n"
        }
        "zsh" => {
            "#compdef llm-gateway\n_arguments '1:command:(serve stop check-config keygen version completion)'\n"
        }
        "fish" => {
            "complete -c llm-gateway -f -a 'serve stop check-config keygen version completion'\n"
        }
        "powershell" => {
            "# powershell completion for llm-gateway\nRegister-ArgumentCompleter -CommandName llm-gateway -ScriptBlock { param($wordToComplete) 'serve','stop','check-config','keygen','version','completion' }\n"
        }
        _ => {
            let mut stderr = err.clone();
            let _ = writeln!(stderr, "unknown command \"completion {shell}\"");
            return 2;
        }
    };
    let mut stdout = out.clone();
    let _ = stdout.write_all(script.as_bytes());
    0
}

fn run_stop(parsed: &Parsed, logger: &Arc<JsonLogger>) -> i32 {
    let path = match resolve_pid_path(parsed.pid_file.as_deref()) {
        Ok(path) => path,
        Err(error) => return gateway_error(logger, format!("configuration failed: {error}")),
    };
    match daemon::stop(&path, STOP_GRACE) {
        Ok(pid) => {
            let mut event = fields();
            event.insert("pid".into(), json!(pid));
            event.insert("pid_file".into(), json!(path.display().to_string()));
            logger.info("stopped", event);
            0
        }
        Err(error) => gateway_error(
            logger,
            format!("stop failed (pid file {}): {error}", path.display()),
        ),
    }
}

fn run_serve(parsed: &Parsed, logger: &Arc<JsonLogger>) -> i32 {
    let config_path = match resolve_config_path(parsed.config.as_deref()) {
        Ok(path) => path,
        Err(error) => return gateway_error(logger, format!("configuration failed: {error}")),
    };
    let config = match config::load(&config_path) {
        Ok(config) => config,
        Err(error) => {
            let message = if parsed.daemon {
                format!("background start failed: {error}")
            } else {
                format!("configuration failed: {error}")
            };
            return gateway_error(logger, message);
        }
    };
    if parsed.daemon {
        return match run_background(&config, &config_path, parsed, logger) {
            Ok(()) => 0,
            Err(error) => gateway_error(logger, format!("background start failed: {error}")),
        };
    }

    let logger = match parsed.log_file.as_deref().filter(|path| !path.is_empty()) {
        Some(path) => match JsonLogger::file(Path::new(path)) {
            Ok(logger) => logger,
            Err(error) => {
                return gateway_error(
                    logger,
                    format!("configuration failed: open log file {path}: {error}"),
                );
            }
        },
        None => logger.clone(),
    };
    match serve_foreground(config, parsed, logger.clone()) {
        Ok(()) => 0,
        Err(error) => gateway_error(&logger, error),
    }
}

fn run_background(
    config: &config::Config,
    config_path: &Path,
    parsed: &Parsed,
    logger: &Arc<JsonLogger>,
) -> Result<(), String> {
    let pid_path = resolve_pid_path(parsed.pid_file.as_deref())?;
    if let Some(pid) = daemon::running_pid(&pid_path).map_err(|e| e.to_string())? {
        return Err(format!(
            "gateway is already running with pid {pid} (pid file {})",
            pid_path.display()
        ));
    }
    let log_path = resolve_log_path(parsed.log_file.as_deref())?;
    let exe = std::env::current_exe().map_err(|error| format!("resolve executable: {error}"))?;
    let args = vec![
        "serve".to_string(),
        "--config".to_string(),
        config_path.display().to_string(),
        "--log-file".to_string(),
        log_path.display().to_string(),
        "--pid-file".to_string(),
        pid_path.display().to_string(),
    ];
    let mut child = daemon::start(&exe, &args, &log_path).map_err(|e| e.to_string())?;
    wait_ready(
        &mut child,
        &config.listen,
        &pid_path,
        Duration::from_secs(5),
    )
    .map_err(|error| format!("{error} (logs: {})", log_path.display()))?;
    let mut event = fields();
    event.insert("pid".into(), json!(child.id()));
    event.insert("listen".into(), json!(config.listen));
    event.insert("log".into(), json!(log_path.display().to_string()));
    event.insert("pid_file".into(), json!(pid_path.display().to_string()));
    logger.info("started in background", event);
    Ok(())
}

fn wait_ready(
    child: &mut Child,
    listen: &str,
    pid_path: &Path,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("process exited during startup: {status}"));
        }
        if daemon::read_pid(pid_path).ok() == Some(child.id()) && probe(listen) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "gateway did not start listening on {listen} within {timeout:?}"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn probe(listen: &str) -> bool {
    let address = daemon::probe_address(listen);
    match address.to_socket_addrs() {
        Ok(addresses) => addresses.into_iter().any(|address| {
            std::net::TcpStream::connect_timeout(&address, Duration::from_millis(250)).is_ok()
        }),
        Err(_) => false,
    }
}

fn serve_foreground(
    config: config::Config,
    parsed: &Parsed,
    logger: Arc<JsonLogger>,
) -> Result<(), String> {
    let pid_path = parsed
        .pid_file
        .as_deref()
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    if let Some(path) = &pid_path {
        if let Some(pid) =
            daemon::running_pid(path).map_err(|e| format!("configuration failed: {e}"))?
        {
            return Err(format!(
                "configuration failed: gateway is already running with pid {pid} (pid file {})",
                path.display()
            ));
        }
        daemon::write_pid(path, std::process::id())
            .map_err(|e| format!("configuration failed: {e}"))?;
    }
    let _pid_guard = PidGuard(pid_path.map(|path| (path, std::process::id())));

    let mut peer_dialers: HashMap<String, Arc<dyn PeerDialer>> = HashMap::new();
    let mut allow: HashSet<[u8; 32]> = HashSet::new();
    let mut identity = None;
    let mut e2ee_listen = None;
    if let Some(e2ee) = &config.e2ee {
        let local = Identity::parse_private(&e2ee.identity_key)
            .map_err(|e| format!("configuration failed: e2ee.identity_key: {e}"))?;
        for peer in &e2ee.peers {
            let pinned = Identity::parse_public(&peer.identity_pub).map_err(|e| {
                format!(
                    "configuration failed: e2ee peer {:?} identity_pub: {e}",
                    peer.name
                )
            })?;
            let address = config::origin::authority_key(&peer.base_url).ok_or_else(|| {
                format!("configuration failed: e2ee peer {:?} base_url", peer.name)
            })?;
            allow.insert(pinned);
            let dialer: Arc<dyn PeerDialer> = Arc::new(Dialer::new(
                local.clone(),
                HashMap::from([(address.clone(), pinned)]),
            ));
            peer_dialers.insert(address, dialer);
        }
        identity = Some(local);
        e2ee_listen = Some(e2ee.listen.clone());
    }

    let recorder = config.conversation_log.enabled.then(|| {
        Store::new(
            config.conversation_log.directory.clone(),
            config.conversation_log.max_body_bytes,
            logger.clone(),
        )
    });
    let keys = Arc::new(Map::new(config.keys.clone()));
    let forwarder =
        Arc::new(Forwarder::new(peer_dialers).map_err(|e| format!("configuration failed: {e}"))?);
    let state = Arc::new(AppState {
        keys,
        forwarder,
        recorder,
        logger: logger.clone(),
    });

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("server failed: {e}"))?;
    runtime.block_on(async move {
        let listen = config.listen.clone();
        let listener = tokio::net::TcpListener::bind(&listen)
            .await
            .map_err(|e| format!("server failed: {e}"))?;
        let e2ee_listener = match &e2ee_listen {
            Some(address) => Some(
                tokio::net::TcpListener::bind(address)
                    .await
                    .map_err(|e| format!("e2ee listen failed ({address}): {e}"))?,
            ),
            None => None,
        };
        let mut event = fields();
        event.insert("listen".into(), json!(listen));
        event.insert("version".into(), json!(env!("LLM_GATEWAY_VERSION")));
        logger.info("starting", event);
        if let Some(address) = &e2ee_listen {
            let mut event = fields();
            event.insert("listen".into(), json!(address));
            event.insert("peers".into(), json!(allow.len()));
            logger.info("e2ee listening", event);
        }

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            shutdown_signal().await;
            let _ = shutdown_tx.send(true);
        });
        let plain = server::serve(listener, state.clone(), wait_shutdown(shutdown_rx.clone()));
        match (e2ee_listener, identity) {
            (Some(e2ee_listener), Some(identity)) => {
                let relayed = crate::e2ee::listener::serve(
                    e2ee_listener,
                    identity,
                    allow,
                    state,
                    wait_shutdown(shutdown_rx),
                );
                tokio::join!(plain, relayed);
            }
            _ => plain.await,
        }
        Ok(())
    })
}

struct PidGuard(Option<(PathBuf, u32)>);

impl Drop for PidGuard {
    fn drop(&mut self) {
        if let Some((path, pid)) = &self.0 {
            daemon::remove_pid(path, *pid);
        }
    }
}

async fn wait_shutdown(mut receiver: tokio::sync::watch::Receiver<bool>) {
    while !*receiver.borrow() {
        if receiver.changed().await.is_err() {
            break;
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn resolve_config_path(path: Option<&str>) -> Result<PathBuf, String> {
    match path.filter(|path| !path.is_empty()) {
        Some(path) => Ok(PathBuf::from(path)),
        None => config::default_path().map_err(|error| error.to_string()),
    }
}

fn resolve_pid_path(path: Option<&str>) -> Result<PathBuf, String> {
    match path.filter(|path| !path.is_empty()) {
        Some(path) => Ok(PathBuf::from(path)),
        None => Ok(config::default_dir()
            .map_err(|error| error.to_string())?
            .join("llm-gateway.pid")),
    }
}

fn resolve_log_path(path: Option<&str>) -> Result<PathBuf, String> {
    match path.filter(|path| !path.is_empty()) {
        Some(path) => Ok(PathBuf::from(path)),
        None => Ok(config::default_dir()
            .map_err(|error| error.to_string())?
            .join("llm-gateway.log")),
    }
}

fn normalize_legacy(args: Vec<String>) -> Vec<String> {
    let mut args = args;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].clone();
        if arg == "--" {
            break;
        }
        if !arg.starts_with('-') {
            index += 1;
            continue;
        }
        let stripped = arg
            .strip_prefix("--")
            .or_else(|| arg.strip_prefix('-'))
            .unwrap_or(&arg);
        let (name, has_value) = match stripped.split_once('=') {
            Some((name, _)) => (name, true),
            None => (stripped, false),
        };
        if matches!(
            name,
            "config"
                | "log-file"
                | "pid-file"
                | "daemon"
                | "stop"
                | "version"
                | "keygen"
                | "check-config"
        ) {
            if !arg.starts_with("--") {
                args[index] = format!("-{arg}");
            }
            if !has_value && matches!(name, "config" | "log-file" | "pid-file") {
                index += 2;
                continue;
            }
        }
        index += 1;
    }
    args
}

fn parse(args: &[String]) -> Result<Parsed, UsageError> {
    let mut parsed = Parsed::default();
    let mut command_index = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            break;
        }
        if arg.starts_with('-') && arg != "-" {
            let name = flag_name(arg);
            if matches!(name, "config" | "log-file" | "pid-file") && !arg.contains('=') {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        let Some(command) = Command::from_name(arg) else {
            return Err(unknown_command(arg));
        };
        parsed.command = Some(command);
        command_index = Some(index);
        break;
    }

    let command = parsed.command();
    let mut positional = Vec::new();
    let mut index = 0;
    let mut after_separator = false;
    while index < args.len() {
        if command_index == Some(index) {
            index += 1;
            continue;
        }
        let arg = &args[index];
        if after_separator {
            positional.push(arg.clone());
            index += 1;
            continue;
        }
        if arg == "--" {
            after_separator = true;
            index += 1;
            continue;
        }
        if !arg.starts_with('-') || arg == "-" {
            positional.push(arg.clone());
            index += 1;
            continue;
        }
        let (name, inline) = split_flag(arg);
        if name == "h" || name == "help" {
            parsed.help = true;
            index += 1;
            continue;
        }
        if !allowed(command, name) {
            return Err(unknown_flag(arg));
        }
        if is_bool_flag(name) {
            let value = match inline {
                Some(value) => parse_bool(value).ok_or_else(|| {
                    UsageError(format!("invalid argument {value:?} for \"{name}\" flag"))
                })?,
                None => true,
            };
            set_bool(&mut parsed, name, value);
            index += 1;
        } else {
            let value = match inline {
                Some(value) => value.to_string(),
                None => {
                    index += 1;
                    args.get(index)
                        .cloned()
                        .ok_or_else(|| UsageError(format!("flag needs an argument: {arg}")))?
                }
            };
            set_string(&mut parsed, name, value);
            index += 1;
        }
    }

    match command {
        Command::Completion => {
            if positional.is_empty() {
                return Err(UsageError(
                    "unknown command \"completion\" needs a shell argument".into(),
                ));
            }
            if positional.len() > 1 {
                return Err(unknown_command(&positional[1]));
            }
            parsed.shell = Some(positional.remove(0));
        }
        _ => {
            if let Some(extra) = positional.first() {
                return Err(unknown_command(extra));
            }
        }
    }
    Ok(parsed)
}

fn flag_name(arg: &str) -> &str {
    let stripped = arg
        .strip_prefix("--")
        .or_else(|| arg.strip_prefix('-'))
        .unwrap_or(arg);
    stripped.split_once('=').map_or(stripped, |(name, _)| name)
}

fn split_flag(arg: &str) -> (&str, Option<&str>) {
    let stripped = arg
        .strip_prefix("--")
        .or_else(|| arg.strip_prefix('-'))
        .unwrap_or(arg);
    match stripped.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (stripped, None),
    }
}

fn is_bool_flag(name: &str) -> bool {
    matches!(
        name,
        "daemon" | "stop" | "version" | "keygen" | "check-config"
    )
}

fn allowed(command: Command, name: &str) -> bool {
    match command {
        Command::Root => matches!(
            name,
            "config"
                | "daemon"
                | "log-file"
                | "pid-file"
                | "stop"
                | "version"
                | "keygen"
                | "check-config"
        ),
        Command::Serve => matches!(name, "config" | "daemon" | "log-file" | "pid-file"),
        Command::Stop => name == "pid-file",
        Command::CheckConfig => name == "config",
        Command::Keygen | Command::Version | Command::Completion => false,
    }
}

fn set_bool(parsed: &mut Parsed, name: &str, value: bool) {
    match name {
        "daemon" => parsed.daemon = value,
        "stop" => parsed.legacy_stop = value,
        "version" => parsed.legacy_version = value,
        "keygen" => parsed.legacy_keygen = value,
        "check-config" => parsed.legacy_check = value,
        _ => {}
    }
}

fn set_string(parsed: &mut Parsed, name: &str, value: String) {
    match name {
        "config" => parsed.config = Some(value),
        "log-file" => parsed.log_file = Some(value),
        "pid-file" => parsed.pid_file = Some(value),
        _ => {}
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Some(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Some(false),
        _ => None,
    }
}

fn unknown_flag(arg: &str) -> UsageError {
    UsageError(format!("unknown flag: {arg}"))
}

fn unknown_command(name: &str) -> UsageError {
    UsageError(format!("unknown command {name:?} for \"llm-gateway\""))
}

fn print_help(command: Command, out: &mut impl Write) {
    let text = match command {
        Command::Root => {
            "An LLM API gateway; runs in the foreground by default\n\n\
             Usage:\n  llm-gateway [flags]\n  llm-gateway [command]\n\n\
             Available Commands:\n\
               serve         Run the gateway\n\
               stop          Stop the gateway recorded in the pid file\n\
               check-config  Validate configuration offline without startup side effects\n\
               keygen        Generate an e2ee identity keypair\n\
               version       Print version\n\
               completion    Generate shell completion scripts\n\n\
             Flags:\n\
                   --config string    path to YAML configuration (default ~/.config/llm-gateway/config.yaml)\n\
                   --daemon           run detached from the terminal\n\
                   --log-file string  append logs to this file\n\
                   --pid-file string  path to the pid file\n\
                   --stop             stop the gateway (prefer the stop command)\n\
                   --version          print version (prefer the version command)\n\
                   --keygen           generate an e2ee keypair (prefer the keygen command)\n\
                   --check-config     validate configuration offline (prefer the check-config command)\n\
               -h, --help             help for llm-gateway\n"
        }
        Command::Serve => {
            "Run the gateway\n\n\
             Usage:\n  llm-gateway serve [flags]\n\n\
             Flags:\n\
                   --config string    path to YAML configuration\n\
                   --daemon           run detached from the terminal\n\
                   --log-file string  append logs to this file\n\
                   --pid-file string  path to the pid file\n\
               -h, --help             help for serve\n"
        }
        Command::Stop => {
            "Stop the gateway recorded in the pid file\n\n\
             Usage:\n  llm-gateway stop [flags]\n\n\
             Flags:\n\
                   --pid-file string  path to the pid file\n\
               -h, --help             help for stop\n"
        }
        Command::CheckConfig => {
            "Validate configuration offline without startup side effects\n\n\
             Usage:\n  llm-gateway check-config [flags]\n\n\
             Flags:\n\
                   --config string  path to YAML configuration\n\
               -h, --help           help for check-config\n"
        }
        Command::Keygen => "Generate an e2ee identity keypair\n\nUsage:\n  llm-gateway keygen\n",
        Command::Version => "Print version\n\nUsage:\n  llm-gateway version\n",
        Command::Completion => {
            "Generate shell completion scripts\n\nUsage:\n  llm-gateway completion [bash|zsh|fish|powershell]\n"
        }
    };
    let _ = out.write_all(text.as_bytes());
}

#[derive(Clone)]
struct SharedWriter(Arc<Mutex<Box<dyn Write + Send>>>);

impl SharedWriter {
    fn new(writer: Box<dyn Write + Send>) -> Self {
        Self(Arc::new(Mutex::new(writer)))
    }
}

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .flush()
    }
}

/// A writable buffer shared with a test after [`run`] returns.
#[derive(Clone, Default)]
pub struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl SharedBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contents(&self) -> Vec<u8> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.contents()).into_owned()
    }
}

impl Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invoke(args: &[&str]) -> (i32, String, String) {
        let stdout = SharedBuffer::new();
        let stderr = SharedBuffer::new();
        let code = run(
            args.iter().map(|arg| (*arg).to_string()).collect(),
            Box::new(stdout.clone()),
            Box::new(stderr.clone()),
        );
        (code, stdout.text(), stderr.text())
    }

    fn contains_all(haystack: &str, needles: &[&str]) -> bool {
        needles.iter().all(|needle| haystack.contains(needle))
    }

    #[test]
    fn flag_matrix_matches_reference_cli() {
        type Case<'a> = (&'a str, &'a [&'a str], i32, &'a [&'a str], &'a [&'a str]);
        let cases: &[Case<'_>] = &[
            ("help", &["--help"], 0, &["Available Commands:"], &[]),
            ("serve help", &["serve", "--help"], 0, &["--daemon"], &[]),
            ("unknown flag", &["--unknown"], 2, &[], &["unknown flag"]),
            (
                "unknown command",
                &["unknown"],
                2,
                &[],
                &["unknown command"],
            ),
            (
                "missing value",
                &["--config"],
                2,
                &[],
                &["flag needs an argument"],
            ),
            (
                "invalid bool",
                &["--daemon=invalid"],
                2,
                &[],
                &["invalid argument"],
            ),
            ("version", &["version"], 0, &["llm-gateway"], &[]),
            (
                "single dash version",
                &["-version"],
                0,
                &["llm-gateway"],
                &[],
            ),
            (
                "single dash config value",
                &["-config", "-version", "-check-config"],
                1,
                &[],
                &["read config"],
            ),
            (
                "single dash config equals",
                &["-config=missing", "-check-config"],
                1,
                &[],
                &["read config"],
            ),
            (
                "version bypasses startup",
                &["--version", "--daemon", "--stop", "--config", "missing"],
                0,
                &["llm-gateway"],
                &[],
            ),
            (
                "keygen precedes version",
                &["--keygen", "--version", "--daemon", "--stop"],
                0,
                &["identity_key:"],
                &[],
            ),
            ("keygen", &["keygen"], 0, &["identity_key:"], &[]),
            (
                "daemon stop conflict",
                &["--daemon", "--stop"],
                1,
                &["--daemon and --stop cannot be combined"],
                &[],
            ),
            (
                "check conflict precedes config",
                &["--check-config", "--version", "--config", "missing"],
                1,
                &[],
                &["--check-config cannot be combined with --version"],
            ),
            (
                "check rejects daemon",
                &["check-config", "--daemon"],
                2,
                &[],
                &["unknown flag"],
            ),
            (
                "check rejects pid file",
                &["check-config", "--pid-file", "unused"],
                2,
                &[],
                &["unknown flag"],
            ),
            (
                "check rejects log file",
                &["check-config", "--log-file", "unused"],
                2,
                &[],
                &["unknown flag"],
            ),
            (
                "serve rejects legacy stop",
                &["serve", "--stop"],
                2,
                &[],
                &["unknown flag"],
            ),
            (
                "stop rejects config",
                &["stop", "--config", "unused"],
                2,
                &[],
                &["unknown flag"],
            ),
            (
                "keygen rejects config",
                &["keygen", "--config", "unused"],
                2,
                &[],
                &["unknown flag"],
            ),
            (
                "version rejects extra args",
                &["version", "extra"],
                2,
                &[],
                &["unknown command"],
            ),
            (
                "default foreground",
                &["--config", "missing"],
                1,
                &["read config"],
                &[],
            ),
            (
                "serve foreground",
                &["serve", "--config", "missing"],
                1,
                &["read config"],
                &[],
            ),
            (
                "serve daemon",
                &["serve", "--daemon", "--config", "missing"],
                1,
                &["background start failed"],
                &[],
            ),
            (
                "completion",
                &["completion", "bash"],
                0,
                &["bash completion"],
                &[],
            ),
        ];
        for (name, args, code, stdout, stderr) in cases {
            let (actual_code, actual_stdout, actual_stderr) = invoke(args);
            assert_eq!(
                actual_code, *code,
                "{name}: stdout={actual_stdout} stderr={actual_stderr}"
            );
            assert!(
                contains_all(&actual_stdout, stdout),
                "{name}: stdout={actual_stdout}"
            );
            assert!(
                contains_all(&actual_stderr, stderr),
                "{name}: stderr={actual_stderr}"
            );
            if stderr.is_empty() {
                assert!(actual_stderr.is_empty(), "{name}: {actual_stderr}");
            }
            if stdout.is_empty() {
                assert!(actual_stdout.is_empty(), "{name}: {actual_stdout}");
            }
        }
    }

    #[test]
    fn check_config_reports_success_and_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(
            &path,
            "keys: [{token: secret, target: {base_url: 'http://127.0.0.1:1'}}]\n",
        )
        .unwrap();
        let path_text = path.display().to_string();
        for command in ["check-config", "--check-config"] {
            let (code, stdout, stderr) = invoke(&[command, "--config", &path_text]);
            assert_eq!(code, 0, "{command}: {stderr}");
            assert!(
                stdout.contains("configuration valid: "),
                "{command}: {stdout}"
            );
            assert!(stderr.is_empty());
            let missing = dir.path().join("missing").display().to_string();
            let (code, stdout, stderr) = invoke(&[command, "--config", &missing]);
            assert_eq!(code, 1);
            assert!(stdout.is_empty(), "{stdout}");
            assert!(stderr.contains("configuration check failed:"), "{stderr}");
        }
    }

    #[test]
    fn stop_reports_missing_pid_file_as_json_on_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.pid");
        let path_text = path.display().to_string();
        for command in ["stop", "--stop"] {
            let (code, stdout, stderr) = invoke(&[command, "--pid-file", &path_text]);
            assert_eq!(code, 1);
            assert!(stderr.is_empty(), "{stderr}");
            let event: serde_json::Value = serde_json::from_str(&stdout)
                .unwrap_or_else(|error| panic!("invalid JSON: {error}: {stdout}"));
            assert_eq!(event["level"], "ERROR", "{stdout}");
            assert_eq!(event["msg"], "command failed", "{stdout}");
            let error = event["error"].as_str().expect("error must be a string");
            assert!(error.contains("stop failed"), "{stdout}");
            assert!(error.contains(&path_text), "{stdout}");
        }
    }

    #[test]
    fn parse_rejects_missing_values_and_extra_commands() {
        assert!(parse(&["--config".into()]).is_err());
        assert!(parse(&["version".into(), "extra".into()]).is_err());
        assert!(parse(&["serve".into(), "--stop".into()]).is_err());
    }
}
