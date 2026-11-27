use serde_json::{Map, Value};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Args, Subcommand, ValueEnum};
use serde::Deserialize;

use crate::assets;
use crate::{Cli, CliError};

#[derive(Debug, Args)]
pub(super) struct DaemonArgs {
    #[command(subcommand)]
    command: DaemonCommand,
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    #[command(about = "Start the managed daemon and wait for health")]
    Up,
    #[command(about = "Stop the managed daemon")]
    Down,
    #[command(about = "Show managed daemon containers and health")]
    Status,
    #[command(about = "Show managed daemon logs")]
    Logs {
        #[arg(short, long, help = "Follow log output")]
        follow: bool,
    },
    #[command(about = "Pull managed daemon images")]
    Pull,
    #[command(about = "Start a native daemon if needed")]
    Ensure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(super) enum DaemonMode {
    Docker,
    Native,
    External,
}

impl DaemonMode {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Native => "native",
            Self::External => "external",
        }
    }
}

pub(super) fn saved_mode(setup: Option<&Map<String, Value>>) -> Result<DaemonMode, CliError> {
    match setup
        .and_then(|setup| setup.get("daemon_mode"))
        .and_then(Value::as_str)
    {
        Some("docker") => Ok(DaemonMode::Docker),
        Some("native") => Ok(DaemonMode::Native),
        Some("external") => Ok(DaemonMode::External),
        Some(mode) => Err(CliError::other(format!(
            "invalid saved daemon mode: {mode}"
        ))),
        None if setup.and_then(|setup| setup.get("daemon_mode")).is_some() => {
            Err(CliError::other("saved daemon mode must be a string"))
        }
        None => Ok(
            if setup
                .and_then(|setup| setup.get("managed_daemon"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                DaemonMode::Docker
            } else {
                DaemonMode::External
            },
        ),
    }
}

pub(super) fn current_setup() -> Result<Map<String, Value>, CliError> {
    let receipt = crate::receipt::read()
        .map_err(|error| CliError::other(format!("cannot read install receipt: {error}")))?;
    Ok(receipt
        .get("setup")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default())
}

pub(super) fn state_dir() -> Result<PathBuf, CliError> {
    let root = match std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(
            std::env::var_os("HOME")
                .ok_or_else(|| CliError::other("HOME is required for daemon state"))?,
        )
        .join(".local/state"),
    };
    Ok(root.join("memento"))
}

pub(super) fn native_binary() -> Result<PathBuf, CliError> {
    let current = std::env::current_exe()
        .map_err(|error| CliError::other(format!("cannot find current executable: {error}")))?;
    discover_binary(
        std::env::var_os("MEMENTO_DAEMON_BIN").map(PathBuf::from),
        &current,
        &std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>(),
    )
}

fn discover_binary(
    override_path: Option<PathBuf>,
    current: &Path,
    path: &[PathBuf],
) -> Result<PathBuf, CliError> {
    if let Some(path) = override_path {
        return executable_path(path);
    }
    let sibling = current.with_file_name("mementod");
    if sibling.is_file() {
        return executable_path(sibling);
    }
    for directory in path {
        let candidate = directory.join("mementod");
        if candidate.is_file() {
            return executable_path(candidate);
        }
    }
    Err(CliError::other(
        "cannot find mementod; install it beside memento or set MEMENTO_DAEMON_BIN",
    ))
}

fn executable_path(path: PathBuf) -> Result<PathBuf, CliError> {
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(&path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    };
    #[cfg(not(unix))]
    let executable = path.is_file();
    if executable {
        Ok(path)
    } else {
        Err(CliError::other(format!(
            "mementod binary not found: {}",
            path.display()
        )))
    }
}

pub(super) fn idle_seconds(setup: &Map<String, Value>) -> u64 {
    setup
        .get("native_idle_exit_secs")
        .and_then(Value::as_u64)
        .unwrap_or(1800)
}

fn native_bind(cli: &Cli) -> Result<String, CliError> {
    let url = reqwest::Url::parse(&cli.url)
        .map_err(|error| CliError::other(format!("invalid daemon URL: {error}")))?;
    if url.scheme() != "http"
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(CliError::other(
            "native daemon URL must be a plain HTTP origin",
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| CliError::other("native daemon URL has no host"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| CliError::other("native daemon URL has no port"))?;
    let address = format!("{host}:{port}")
        .parse::<std::net::SocketAddr>()
        .map_err(|error| {
            CliError::other(format!("native daemon URL must use an IP address: {error}"))
        })?;
    if !address.ip().is_loopback() {
        return Err(CliError::other(
            "native daemon URL must use a loopback address",
        ));
    }
    Ok(address.to_string())
}

pub(super) fn warmup() -> Result<(), CliError> {
    let binary = native_binary()?;
    eprintln!("Preparing the native embedding model (about 133 MB on first download)...");
    let status = Command::new(&binary)
        .arg("warmup")
        .env("MEMENTO_STORE", "sqlite")
        .env("MEMENTO_EMBEDDER", "candle")
        .status()
        .map_err(|error| {
            CliError::other(format!("cannot run {} warmup: {error}", binary.display()))
        })?;
    if !status.success() {
        return Err(CliError::other(format!(
            "{} warmup failed: {status}",
            binary.display()
        )));
    }
    Ok(())
}

fn native_start(
    cli: &Cli,
    setup: &Map<String, Value>,
    quiet: bool,
    timeout: Duration,
) -> Result<(), CliError> {
    let lock = state_dir()?.join("mementod.lock");
    if health(cli)?.is_some() {
        return if running_pid(&lock)?.is_some() {
            Ok(())
        } else {
            Err(CliError::other(
                "daemon URL is already serving another process; native mementod was not started",
            ))
        };
    }
    let binary = native_binary()?;
    let state = state_dir()?;
    fs::create_dir_all(&state)
        .map_err(|error| CliError::other(format!("cannot create {}: {error}", state.display())))?;
    let log_path = state.join("mementod.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|error| CliError::other(format!("cannot open {}: {error}", log_path.display())))?;
    let stderr = log
        .try_clone()
        .map_err(|error| CliError::other(format!("cannot clone log handle: {error}")))?;
    let mut command = Command::new(&binary);
    command
        .arg("serve")
        .env("MEMENTO_STORE", "sqlite")
        .env("MEMENTO_EMBEDDER", "candle")
        .env("MEMENTO_IDLE_EXIT_SECS", idle_seconds(setup).to_string())
        .env("MEMENTO_LOCK_PATH", state.join("mementod.lock"))
        .env("MEMENTO_BIND", native_bind(cli)?)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    if let Some(token) = &cli.token {
        command.env("MEMENTO_TOKEN", token);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child =
        Some(command.spawn().map_err(|error| {
            CliError::other(format!("cannot start {}: {error}", binary.display()))
        })?);
    if !quiet {
        eprintln!("Waiting for native mementod to load its model...");
    }
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if health(cli)?.is_some() && running_pid(&lock)?.is_some() {
            if !quiet {
                println!("mementod is healthy");
            }
            return Ok(());
        }
        if let Some(status) = child
            .as_mut()
            .map(|child| child.try_wait())
            .transpose()
            .map_err(|error| {
                CliError::other(format!("cannot inspect native daemon startup: {error}"))
            })?
            .flatten()
        {
            child = None;
            if !status.success() && running_pid(&lock)?.is_none() {
                return Err(CliError::other(format!(
                    "native mementod exited during startup ({status}); see {}",
                    log_path.display()
                )));
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
    Err(CliError::other(format!(
        "native mementod did not become healthy within {} seconds; see {}",
        timeout.as_secs(),
        log_path.display()
    )))
}

pub(super) fn ensure(cli: &Cli, timeout: Duration) -> Result<(), CliError> {
    let setup = current_setup()?;
    if saved_mode(Some(&setup))? == DaemonMode::Native {
        native_start(cli, &setup, true, timeout)
    } else {
        if health(cli)?.is_none() {
            eprintln!(
                "daemon unavailable; mode {} is not auto-started",
                saved_mode(Some(&setup))?.name()
            );
        }
        Ok(())
    }
}

pub(super) fn start_native(cli: &Cli, idle: u64) -> Result<(), CliError> {
    let setup = serde_json::json!({"native_idle_exit_secs": idle});
    native_start(
        cli,
        setup.as_object().unwrap(),
        false,
        Duration::from_secs(120),
    )
}

pub(super) fn restart_native(cli: &Cli, setup: &Map<String, Value>) -> Result<(), CliError> {
    native_down(cli)?;
    native_start(cli, setup, false, Duration::from_secs(120))
}

fn lock_file(path: &Path) -> Result<Option<File>, CliError> {
    match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(CliError::other(format!(
            "cannot open {}: {error}",
            path.display()
        ))),
    }
}

pub(super) fn running_pid(path: &Path) -> Result<Option<u32>, CliError> {
    let Some(file) = lock_file(path)? else {
        return Ok(None);
    };
    match file.try_lock() {
        Ok(()) => Ok(None),
        Err(std::fs::TryLockError::WouldBlock) => {
            let text = fs::read_to_string(path).map_err(|error| {
                CliError::other(format!("cannot read {}: {error}", path.display()))
            })?;
            let pid = text.trim().parse::<u32>().map_err(|error| {
                CliError::other(format!("invalid pid in {}: {error}", path.display()))
            })?;
            Ok(Some(pid))
        }
        Err(error) => Err(CliError::other(format!(
            "cannot inspect {}: {error}",
            path.display()
        ))),
    }
}

fn native_down(cli: &Cli) -> Result<(), CliError> {
    native_bind(cli)?;
    let lock = state_dir()?.join("mementod.lock");
    if running_pid(&lock)?.is_none() {
        println!("mementod is not running");
        return Ok(());
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .no_proxy()
        .build()
        .map_err(|error| CliError::other(format!("cannot create shutdown client: {error}")))?;
    let mut request = client.post(crate::endpoint(&cli.url, &["admin", "shutdown"])?);
    if let Some(token) = &cli.token {
        request = request.bearer_auth(token);
    }
    let response = match request.send() {
        Ok(response) => response,
        Err(error) if error.is_connect() => {
            println!("mementod is not running");
            return Ok(());
        }
        Err(error) => return Err(CliError::request(error, &cli.url)),
    };
    if !response.status().is_success() {
        return Err(CliError::other(format!(
            "daemon shutdown failed: HTTP {}",
            response.status()
        )));
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if running_pid(&lock)?.is_none() {
            println!("mementod stopped");
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(CliError::other("mementod did not stop within 30 seconds"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RefreshStep {
    Pull,
    Up,
}

pub(super) fn managed_refresh_steps(setup: Option<&Map<String, Value>>) -> Vec<RefreshStep> {
    match saved_mode(setup) {
        Ok(DaemonMode::Docker) => vec![RefreshStep::Pull, RefreshStep::Up],
        _ => Vec::new(),
    }
}

#[derive(Debug, Deserialize)]
struct Health {
    ok: bool,
    version: Option<String>,
}

fn compose_path() -> Result<std::path::PathBuf, CliError> {
    let path = assets::data_dir()?.join("compose.yaml");
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent)
        .map_err(|error| CliError::other(format!("cannot create {}: {error}", parent.display())))?;
    let image = std::env::var("MEMENTO_DAEMON_IMAGE").ok();
    fs::write(&path, assets::render_compose(image.as_deref()))
        .map_err(|error| CliError::other(format!("cannot write {}: {error}", path.display())))?;
    Ok(path)
}

fn docker(compose: &std::path::Path, args: &[&str], inherit: bool) -> Result<(), CliError> {
    let output = Command::new("docker")
        .arg("compose")
        .arg("-p")
        .arg("memento")
        .arg("-f")
        .arg(compose)
        .args(args)
        .stdin(Stdio::null())
        .stdout(if inherit {
            Stdio::inherit()
        } else {
            Stdio::piped()
        })
        .stderr(if inherit {
            Stdio::inherit()
        } else {
            Stdio::piped()
        })
        .output()
        .map_err(|error| {
            CliError::other(format!(
                "cannot run docker compose: {error}; install Docker with the Compose plugin"
            ))
        })?;
    if !output.status.success() {
        let details = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if details.contains("docker: 'compose' is not a docker command")
            || details.contains("unknown command: docker compose")
        {
            return Err(CliError::other(
                "Docker Compose is unavailable; install Docker Compose v2",
            ));
        }
        return Err(CliError::other(format!(
            "docker compose {} failed: {}",
            args.join(" "),
            details
        )));
    }
    if !inherit {
        print!("{}", String::from_utf8_lossy(&output.stdout));
    }
    Ok(())
}

pub(super) fn purge() -> Result<(), CliError> {
    let compose = assets::data_dir()?.join("compose.yaml");
    if !compose.is_file() {
        return Err(CliError::other(format!(
            "cannot purge managed daemon: compose file {} is missing",
            compose.display()
        )));
    }
    docker(&compose, &["down", "-v"], true)?;
    fs::remove_file(&compose).map_err(|error| {
        CliError::other(format!("cannot remove {}: {error}", compose.display()))
    })?;
    Ok(())
}

fn health(cli: &Cli) -> Result<Option<Health>, CliError> {
    let url = crate::endpoint(&cli.url, &["health"])?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .no_proxy()
        .build()
        .map_err(|error| CliError::other(format!("cannot create health client: {error}")))?;
    let mut request = client.get(url);
    if let Some(token) = &cli.token {
        request = request.bearer_auth(token);
    }
    let response = match request.send() {
        Ok(response) if response.status().is_success() => response,
        _ => return Ok(None),
    };
    let health = response
        .json::<Health>()
        .map_err(|error| CliError::other(format!("cannot parse daemon health: {error}")))?;
    Ok(health.ok.then_some(health))
}

pub(super) fn docker_available() -> bool {
    Command::new("docker")
        .args(["compose", "version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub(super) fn start(cli: &Cli) -> Result<(), CliError> {
    let compose = compose_path()?;
    docker(&compose, &["up", "-d"], true)?;
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        if let Some(status) = health(cli)? {
            println!(
                "mementod is healthy{}",
                status
                    .version
                    .map(|version| format!(" (version {version})"))
                    .unwrap_or_default()
            );
            return Ok(());
        }
        thread::sleep(Duration::from_secs(2));
    }
    Err(CliError::other(
        "mementod did not become healthy within 120 seconds",
    ))
}

pub(super) fn pull() -> Result<(), CliError> {
    let compose = compose_path()?;
    docker(&compose, &["pull"], true)
}

pub(super) fn run(cli: &Cli, args: &DaemonArgs) -> Result<(), CliError> {
    let setup = current_setup()?;
    let mode = saved_mode(Some(&setup))?;
    if let DaemonCommand::Ensure = &args.command {
        return ensure(cli, Duration::from_secs(120));
    }
    if mode == DaemonMode::Native {
        return match &args.command {
            DaemonCommand::Up => native_start(cli, &setup, false, Duration::from_secs(120)),
            DaemonCommand::Down => native_down(cli),
            DaemonCommand::Status => {
                let state = state_dir()?;
                match running_pid(&state.join("mementod.lock"))? {
                    Some(pid) => println!("native: running (pid {pid})"),
                    None => println!("native: stopped"),
                }
                println!(
                    "health: {}",
                    if health(cli)?.is_some() {
                        "ok"
                    } else {
                        "unavailable"
                    }
                );
                println!("idle exit: {} seconds", idle_seconds(&setup));
                Ok(())
            }
            DaemonCommand::Logs { follow } => {
                let path = state_dir()?.join("mementod.log");
                let mut command = Command::new("tail");
                if *follow {
                    command.arg("-f");
                }
                let status = command.arg(&path).status().map_err(|error| {
                    CliError::other(format!("cannot read {}: {error}", path.display()))
                })?;
                if status.success() {
                    Ok(())
                } else {
                    Err(CliError::other(format!("tail failed: {status}")))
                }
            }
            DaemonCommand::Pull => Err(CliError::other(
                "native daemon binaries are updated with memento update",
            )),
            DaemonCommand::Ensure => unreachable!(),
        };
    }
    if mode == DaemonMode::External {
        return match &args.command {
            DaemonCommand::Status => {
                println!(
                    "external daemon health: {}",
                    if health(cli)?.is_some() {
                        "ok"
                    } else {
                        "unavailable"
                    }
                );
                Ok(())
            }
            _ => Err(CliError::other("external daemon is not managed by memento")),
        };
    }
    let compose = compose_path()?;
    match &args.command {
        DaemonCommand::Up => start(cli),
        DaemonCommand::Down => docker(&compose, &["down"], true),
        DaemonCommand::Status => {
            docker(&compose, &["ps"], true)?;
            match health(cli)? {
                Some(status) => println!(
                    "health: ok{}",
                    status
                        .version
                        .map(|version| format!(" (version {version})"))
                        .unwrap_or_default()
                ),
                None => println!("health: unavailable"),
            }
            Ok(())
        }
        DaemonCommand::Logs { follow } => {
            if *follow {
                docker(&compose, &["logs", "-f"], true)
            } else {
                docker(&compose, &["logs"], true)
            }
        }
        DaemonCommand::Pull => pull(),
        DaemonCommand::Ensure => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_steps_depend_only_on_saved_managed_daemon_choice() {
        let setup = serde_json::json!({"managed_daemon": true});
        assert_eq!(
            managed_refresh_steps(setup.as_object()),
            vec![RefreshStep::Pull, RefreshStep::Up]
        );
        for setup in [
            serde_json::json!({"managed_daemon": false}),
            serde_json::json!({}),
        ] {
            assert!(managed_refresh_steps(setup.as_object()).is_empty());
        }
        assert!(managed_refresh_steps(None).is_empty());
    }

    #[test]
    fn legacy_setup_modes_and_explicit_mode_precedence() {
        for (value, expected) in [
            (
                serde_json::json!({"managed_daemon": true}),
                DaemonMode::Docker,
            ),
            (
                serde_json::json!({"managed_daemon": false}),
                DaemonMode::External,
            ),
            (
                serde_json::json!({"managed_daemon": true, "daemon_mode": "native"}),
                DaemonMode::Native,
            ),
        ] {
            assert_eq!(saved_mode(value.as_object()).unwrap(), expected);
        }
    }

    #[cfg(unix)]
    #[test]
    fn binary_discovery_prefers_override_then_sibling_then_path() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let sibling_dir = temp.path().join("sibling");
        let path_dir = temp.path().join("path");
        fs::create_dir_all(&sibling_dir).unwrap();
        fs::create_dir_all(&path_dir).unwrap();
        let sibling = sibling_dir.join("mementod");
        let from_path = path_dir.join("mementod");
        let override_path = temp.path().join("custom");
        for path in [&sibling, &from_path, &override_path] {
            fs::write(path, "").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let executable = sibling_dir.join("memento");
        assert_eq!(
            discover_binary(
                Some(override_path.clone()),
                &executable,
                std::slice::from_ref(&path_dir)
            )
            .unwrap(),
            override_path
        );
        assert_eq!(
            discover_binary(None, &executable, std::slice::from_ref(&path_dir)).unwrap(),
            sibling
        );
        fs::remove_file(sibling).unwrap();
        assert_eq!(
            discover_binary(None, &executable, &[path_dir]).unwrap(),
            from_path
        );
    }
}
