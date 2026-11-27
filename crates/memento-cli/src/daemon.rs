use serde_json::{Map, Value};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::IsTerminal;
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
    #[command(about = "Migrate Docker memories to native storage")]
    Migrate {
        #[arg(long, help = "Migrate without prompting")]
        yes: bool,
        #[arg(long, help = "Keep Qdrant running after migration")]
        keep_docker_running: bool,
        #[arg(long, help = "Back up and replace existing native data")]
        force: bool,
    },
    #[command(about = "Delete the old Docker stack and volume after migration")]
    PruneDocker {
        #[arg(long, help = "Delete without prompting")]
        yes: bool,
        #[arg(long, help = "Allow pruning when the saved daemon mode is external")]
        force: bool,
    },
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
            return Err(CliError::other(
                "native daemon lock is held but the shutdown endpoint is unavailable",
            ));
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
        .arg(compose_project()?)
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

pub(super) fn purge(cli: &Cli) -> Result<(), CliError> {
    native_down(cli)?;
    if running_pid(&state_dir()?.join("mementod.lock"))?.is_some() {
        return Err(CliError::other("native daemon is still running"));
    }
    let compose = assets::data_dir()?.join("compose.yaml");
    if compose.is_file() {
        docker(&compose, &["down", "-v"], true)?;
        fs::remove_file(&compose).map_err(|error| {
            CliError::other(format!("cannot remove {}: {error}", compose.display()))
        })?;
    } else if docker_memory_exists()? {
        return Err(CliError::other(
            "Docker volume exists but compose file is missing",
        ));
    }
    for path in native_purge_paths()? {
        let result = if path.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        match result {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(CliError::other(format!(
                    "cannot remove {}: {error}",
                    path.display()
                )));
            }
        }
    }
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
    if let DaemonCommand::Migrate {
        yes,
        keep_docker_running,
        force,
    } = &args.command
    {
        return migrate(cli, *yes, *keep_docker_running, *force);
    }
    if let DaemonCommand::PruneDocker { yes, force } = &args.command {
        return prune_docker(*yes, *force);
    }
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
            DaemonCommand::Migrate { .. } | DaemonCommand::PruneDocker { .. } => unreachable!(),
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
        DaemonCommand::Migrate { .. } | DaemonCommand::PruneDocker { .. } => unreachable!(),
    }
}

pub(super) fn compose_project() -> Result<String, CliError> {
    validated_project(std::env::var("MEMENTO_COMPOSE_PROJECT").unwrap_or_else(|_| "memento".into()))
}

fn validated_project(project: String) -> Result<String, CliError> {
    if project.is_empty()
        || !project
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte))
        || !project.as_bytes()[0].is_ascii_alphanumeric()
    {
        return Err(CliError::other("invalid MEMENTO_COMPOSE_PROJECT"));
    }
    Ok(project)
}

pub(super) fn docker_memory_exists() -> Result<bool, CliError> {
    let project = compose_project()?;
    let output = match Command::new("docker")
        .args(["volume", "ls", "--filter"])
        .arg(format!("label=com.docker.compose.project={project}"))
        .args([
            "--filter",
            "label=com.docker.compose.volume=qdrant_storage",
            "--format",
            "{{.Name}}",
        ])
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(CliError::other(format!(
                "cannot inspect Docker volumes: {error}"
            )));
        }
    };
    if !output.status.success() {
        return Err(CliError::other(format!(
            "cannot inspect Docker volumes: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(!output.stdout.iter().all(u8::is_ascii_whitespace))
}

fn model_cache_dir() -> Result<PathBuf, CliError> {
    let root = std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or_else(|| CliError::other("HOME is required for model cache"))?;
    Ok(root.join("memento/models"))
}

fn database_paths(database: &Path) -> Vec<PathBuf> {
    ["", "-wal", "-shm"]
        .iter()
        .map(|suffix| {
            let mut path = database.as_os_str().to_os_string();
            path.push(suffix);
            PathBuf::from(path)
        })
        .collect()
}

pub(super) fn native_purge_paths() -> Result<Vec<PathBuf>, CliError> {
    Ok(purge_paths_in(
        &assets::data_dir()?,
        &state_dir()?,
        &model_cache_dir()?,
    ))
}

fn purge_paths_in(data: &Path, state: &Path, model_cache: &Path) -> Vec<PathBuf> {
    let mut paths = database_paths(&data.join("memento.db"));
    if let Ok(entries) = fs::read_dir(data) {
        let mut backups = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("memento.db.bak-"))
            })
            .collect::<Vec<_>>();
        backups.sort();
        paths.extend(backups);
    }
    paths.extend([
        state.join("mementod.lock"),
        state.join("mementod.log"),
        model_cache.to_path_buf(),
    ]);
    paths
}

fn migration_command(project: &str, user: &str, cache: &Path, data: &Path, image: &str) -> Command {
    let mut command = Command::new("docker");
    command
        .args(["run", "--rm", "--network"])
        .arg(format!("{project}_default"))
        .args([
            "--user",
            user,
            "-e",
            "HOME=/tmp",
            "-e",
            "MEMENTO_EMBEDDER=candle",
            "-e",
            "FASTEMBED_CACHE_DIR=/models",
            "-v",
        ])
        .arg(format!("{}:/models", cache.display()))
        .arg("-v")
        .arg(format!("{}:/out", data.display()))
        .arg(image)
        .args([
            "migrate",
            "--qdrant-url",
            "http://qdrant:6334",
            "--to",
            "/out/memento.db",
        ]);
    command
}

#[derive(Debug, Default, PartialEq, Eq)]
struct DockerStack {
    containers: Vec<String>,
    networks: Vec<String>,
    volumes: Vec<String>,
    compose_file: bool,
}

impl DockerStack {
    fn is_empty(&self) -> bool {
        self.containers.is_empty()
            && self.networks.is_empty()
            && self.volumes.is_empty()
            && !self.compose_file
    }
}

fn prune_refusal(mode: DaemonMode, stack: &DockerStack, force: bool) -> Option<String> {
    match mode {
        DaemonMode::Docker => Some(
            "the saved daemon mode is docker, so the Docker volume is your live memory; run memento daemon migrate first".into(),
        ),
        DaemonMode::External if !stack.volumes.is_empty() && !force => Some(
            "the saved daemon mode is external and the Docker memory volume may be live; use --force only if it is not".into(),
        ),
        _ => None,
    }
}

fn prune_commands(stack: &DockerStack) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    for (prefix, names) in [
        (vec!["rm", "-f"], &stack.containers),
        (vec!["network", "rm"], &stack.networks),
        (vec!["volume", "rm"], &stack.volumes),
    ] {
        if !names.is_empty() {
            let mut command: Vec<String> = prefix.into_iter().map(String::from).collect();
            command.extend(names.iter().cloned());
            commands.push(command);
        }
    }
    commands
}

fn docker_labeled(args: &[&str], project: &str, format: &str) -> Result<Vec<String>, CliError> {
    let output = Command::new("docker")
        .args(args)
        .arg("--filter")
        .arg(format!("label=com.docker.compose.project={project}"))
        .args(["--format", format])
        .output()
        .map_err(|error| CliError::other(format!("cannot run docker: {error}")))?;
    if !output.status.success() {
        return Err(CliError::other(format!(
            "docker {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(String::from)
        .collect())
}

fn docker_stack(project: &str, compose: &Path) -> Result<DockerStack, CliError> {
    Ok(DockerStack {
        containers: docker_labeled(&["ps", "-a"], project, "{{.Names}}")?,
        networks: docker_labeled(&["network", "ls"], project, "{{.Name}}")?,
        volumes: docker_labeled(&["volume", "ls"], project, "{{.Name}}")?,
        compose_file: compose.is_file(),
    })
}

fn volume_size(volume: &str) -> Option<String> {
    let output = Command::new("docker")
        .args(["system", "df", "-v"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .find(|fields| fields.len() >= 3 && fields[0] == volume)
        .map(|fields| fields[fields.len() - 1].to_string())
}

fn prune_preview(stack: &DockerStack, compose: &Path) -> Vec<String> {
    let mut lines = Vec::new();
    for container in &stack.containers {
        lines.push(format!("container {container}"));
    }
    for network in &stack.networks {
        lines.push(format!("network {network}"));
    }
    for volume in &stack.volumes {
        let size = volume_size(volume).unwrap_or_else(|| "unknown size".into());
        lines.push(format!("volume {volume} ({size})"));
    }
    if stack.compose_file {
        lines.push(format!("file {}", compose.display()));
    }
    lines
}

fn prune_docker(yes: bool, force: bool) -> Result<(), CliError> {
    let setup = current_setup()?;
    let mode = saved_mode(Some(&setup))?;
    let project = compose_project()?;
    let compose = assets::data_dir()?.join("compose.yaml");
    let stack = docker_stack(&project, &compose)?;
    if let Some(reason) = prune_refusal(mode, &stack, force) {
        return Err(CliError::other(format!("refusing to prune: {reason}")));
    }
    if stack.is_empty() {
        println!("Nothing to prune for Docker project {project}.");
        return Ok(());
    }
    println!(
        "Docker project {project}: the following will be deleted (native data is not touched):"
    );
    for line in prune_preview(&stack, &compose) {
        println!("  {line}");
    }
    if !yes {
        if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
            return Err(CliError::other(
                "prune-docker requires confirmation or --yes",
            ));
        }
        if !dialoguer::Confirm::new()
            .with_prompt("Delete the Docker backup?")
            .default(false)
            .interact()
            .map_err(|error| CliError::other(format!("prune prompt failed: {error}")))?
        {
            return Err(CliError::other("prune cancelled"));
        }
    }
    for arguments in prune_commands(&stack) {
        let output = Command::new("docker")
            .args(&arguments)
            .output()
            .map_err(|error| CliError::other(format!("cannot run docker: {error}")))?;
        if !output.status.success() {
            return Err(CliError::other(format!(
                "docker {} failed: {}",
                arguments.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
    }
    if stack.compose_file {
        fs::remove_file(&compose).map_err(|error| {
            CliError::other(format!("cannot remove {}: {error}", compose.display()))
        })?;
    }
    println!("Docker backup deleted.");
    Ok(())
}

pub(super) fn native_database_path() -> Result<PathBuf, CliError> {
    Ok(assets::data_dir()?.join("memento.db"))
}

pub(super) fn native_memory_count(cli: &Cli, mode: DaemonMode) -> Option<u64> {
    if mode != DaemonMode::Native || !native_database_path().ok()?.is_file() {
        return None;
    }
    ensure(cli, Duration::from_secs(120)).ok()?;
    stats(cli).ok()
}

fn migration_prune_hint() -> &'static str {
    "Delete the Docker backup later with: memento daemon prune-docker"
}

fn identity() -> Result<String, CliError> {
    let get = |flag| -> Result<String, CliError> {
        let output = Command::new("id")
            .arg(flag)
            .output()
            .map_err(|error| CliError::other(format!("cannot determine user identity: {error}")))?;
        if !output.status.success() {
            return Err(CliError::other("cannot determine user identity"));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    Ok(format!("{}:{}", get("-u")?, get("-g")?))
}

fn stats(cli: &Cli) -> Result<u64, CliError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .no_proxy()
        .build()
        .map_err(|error| CliError::other(format!("cannot create stats client: {error}")))?;
    let mut request = client.get(crate::endpoint(&cli.url, &["v1", "stats"])?);
    if let Some(token) = &cli.token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|error| CliError::request(error, &cli.url))?;
    let value: Value = response
        .json()
        .map_err(|error| CliError::other(format!("cannot parse daemon stats: {error}")))?;
    value
        .get("total")
        .and_then(Value::as_u64)
        .ok_or_else(|| CliError::other("daemon stats has no total"))
}

fn backup_database(database: &Path, force: bool) -> Result<Vec<(PathBuf, PathBuf)>, CliError> {
    let paths = database_paths(database);
    if paths.iter().any(|path| path.exists()) && !force {
        return Err(CliError::other(
            "native database or sidecars already exist; use --force to back them up",
        ));
    }
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.fZ");
    let mut backup = Vec::new();
    for (index, original) in paths.iter().enumerate() {
        if !original.exists() {
            continue;
        }
        let mut name = OsString::from(database.as_os_str());
        name.push(format!(".bak-{stamp}"));
        name.push(["", "-wal", "-shm"][index]);
        let destination = PathBuf::from(name);
        if destination.exists() {
            restore_database(&backup)?;
            return Err(CliError::other(format!(
                "backup already exists: {}",
                destination.display()
            )));
        }
        if let Err(error) = fs::rename(original, &destination) {
            restore_database(&backup)?;
            return Err(CliError::other(format!(
                "cannot back up {}: {error}",
                original.display()
            )));
        }
        backup.push((original.clone(), destination));
    }
    Ok(backup)
}

fn restore_database(backup: &[(PathBuf, PathBuf)]) -> Result<(), CliError> {
    for (original, saved) in backup.iter().rev() {
        fs::rename(saved, original).map_err(|error| {
            CliError::other(format!(
                "cannot restore {} from {}: {error}",
                original.display(),
                saved.display()
            ))
        })?;
    }
    Ok(())
}

fn remove_database(database: &Path) -> Result<(), CliError> {
    for path in database_paths(database) {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(CliError::other(format!(
                    "cannot remove {}: {error}",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

pub(super) fn offer_migration(cli: &Cli) -> Result<bool, CliError> {
    let setup = current_setup()?;
    if saved_mode(Some(&setup))? == DaemonMode::Native || !docker_memory_exists()? {
        return Ok(false);
    }
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        eprintln!("Docker memory found; run memento daemon migrate");
        return Ok(false);
    }
    let count = stats(cli)
        .ok()
        .map(|count| format!("{count} "))
        .unwrap_or_default();
    if dialoguer::Confirm::new()
        .with_prompt(format!(
            "Move your {count}memories from Docker to the native daemon?"
        ))
        .default(true)
        .interact()
        .map_err(|error| CliError::other(format!("migration prompt failed: {error}")))?
    {
        migrate(cli, true, false, false)?;
        return Ok(true);
    }
    Ok(false)
}

fn migrate(cli: &Cli, yes: bool, keep_docker_running: bool, force: bool) -> Result<(), CliError> {
    if !docker_memory_exists()? {
        return Err(CliError::other("no Docker Qdrant volume found"));
    }
    let project = compose_project()?;
    let setup = current_setup()?;
    if setup.is_empty() {
        return Err(CliError::other("migration requires a saved setup"));
    }
    let compose = assets::data_dir()?.join("compose.yaml");
    if !compose.is_file() {
        return Err(CliError::other(format!(
            "compose file missing: {}",
            compose.display()
        )));
    }
    let database = assets::data_dir()?.join("memento.db");
    if std::env::var_os("MEMENTO_SQLITE_PATH").is_some() {
        return Err(CliError::other(
            "unset MEMENTO_SQLITE_PATH before migration; custom native paths are not supported",
        ));
    }
    if database_paths(&database).iter().any(|path| path.exists()) && !force {
        return Err(CliError::other(
            "native database or sidecars already exist; use --force to back them up",
        ));
    }
    println!("Qdrant volume found in Docker project {project}");
    println!(
        "Migration re-embeds all memories and may take several minutes; the first model download is about 133 MB."
    );
    if !yes {
        if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
            return Err(CliError::other("migration requires confirmation or --yes"));
        }
        if !dialoguer::Confirm::new()
            .with_prompt("Stop the daemons and migrate?")
            .default(true)
            .interact()
            .map_err(|error| CliError::other(format!("migration prompt failed: {error}")))?
        {
            return Err(CliError::other("migration cancelled"));
        }
    }
    let cache = model_cache_dir()?;
    fs::create_dir_all(&cache)
        .map_err(|error| CliError::other(format!("cannot create {}: {error}", cache.display())))?;
    fs::create_dir_all(database.parent().unwrap())
        .map_err(|error| CliError::other(format!("cannot create data directory: {error}")))?;
    let old_receipt = crate::receipt::read().map_err(CliError::other)?;
    let running_native = running_pid(&state_dir()?.join("mementod.lock"))?.is_some();
    if running_native {
        native_down(cli)?;
    }
    let source_count = if running_native {
        None
    } else {
        stats(cli).ok()
    };
    docker(&compose, &["up", "-d", "qdrant"], true)?;
    docker(&compose, &["stop", "mementod"], true)?;
    let backup = match backup_database(&database, force) {
        Ok(backup) => backup,
        Err(error) => {
            let _ = docker(&compose, &["up", "-d", "mementod"], true);
            return Err(error);
        }
    };
    let result = (|| {
        let image_override = std::env::var("MEMENTO_DAEMON_IMAGE").ok();
        let mut command = migration_command(
            &project,
            &identity()?,
            &cache,
            database.parent().unwrap(),
            assets::daemon_image(image_override.as_deref()),
        );
        command.stdout(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| CliError::other(format!("cannot run migration container: {error}")))?;
        let mut report = String::new();
        use std::io::Read;
        child
            .stdout
            .take()
            .ok_or_else(|| CliError::other("migration output unavailable"))?
            .read_to_string(&mut report)
            .map_err(|error| CliError::other(format!("cannot read migration report: {error}")))?;
        print!("{report}");
        let status = child
            .wait()
            .map_err(|error| CliError::other(format!("cannot wait for migration: {error}")))?;
        if !status.success() {
            return Err(CliError::other(format!(
                "migration container failed: {status}"
            )));
        }
        let reported_count = migration_report_count(&report)?;
        let mut native_setup = setup.clone();
        native_setup.insert("daemon_mode".into(), Value::String("native".into()));
        native_setup.insert("managed_daemon".into(), Value::Bool(false));
        native_start(cli, &native_setup, false, Duration::from_secs(120))?;
        let migrated_count = stats(cli)?;
        if reported_count != migrated_count {
            return Err(CliError::other(format!(
                "segment count mismatch: migrated {reported_count} native {migrated_count}"
            )));
        }
        if source_count.is_some_and(|count| count != migrated_count) {
            return Err(CliError::other(format!(
                "segment count mismatch: source {} native {migrated_count}",
                source_count.unwrap()
            )));
        }
        if !keep_docker_running {
            docker(&compose, &["stop"], true)?;
        }
        crate::receipt::merge_setup(Value::Object(native_setup.clone()))
            .map_err(CliError::other)?;
        crate::setup::reapply_native_opencode(cli, &native_setup)?;
        println!(
            "Migrated {migrated_count} memories. Qdrant volume {project}_qdrant_storage remains as a backup."
        );
        println!("{}", migration_prune_hint());
        if let Some((_, path)) = backup.first() {
            println!("Previous native database backup: {}", path.display());
        }
        Ok(())
    })();
    if let Err(error) = result {
        let mut rollback_errors = Vec::new();
        if let Err(failure) = native_down(cli) {
            rollback_errors.push(failure.message);
        }
        if running_pid(&state_dir()?.join("mementod.lock"))?.is_none() {
            if let Err(failure) = remove_database(&database) {
                rollback_errors.push(failure.message);
            }
            if let Err(failure) = restore_database(&backup) {
                rollback_errors.push(failure.message);
            }
        } else {
            rollback_errors.push("native lock still held; database backup was not restored".into());
        }
        if let Err(failure) = crate::receipt::write(old_receipt) {
            rollback_errors.push(failure);
        }
        if let Err(failure) = docker(&compose, &["up", "-d", "mementod"], true) {
            rollback_errors.push(failure.message);
        }
        return Err(CliError::other(format!(
            "migration failed: {}; rollback: {}",
            error.message,
            if rollback_errors.is_empty() {
                "Docker restarted and native data restored".into()
            } else {
                rollback_errors.join("; ")
            }
        )));
    }
    Ok(())
}

fn migration_report_count(report: &str) -> Result<u64, CliError> {
    report
        .lines()
        .find_map(|line| line.trim().strip_prefix("segments:")?.trim().parse().ok())
        .ok_or_else(|| CliError::other("migration report does not contain a segment count"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_container_arguments_are_exact() {
        let command = migration_command(
            "memento-migtest",
            "123:456",
            Path::new("/cache"),
            Path::new("/data"),
            "image:test",
        );
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            [
                "run",
                "--rm",
                "--network",
                "memento-migtest_default",
                "--user",
                "123:456",
                "-e",
                "HOME=/tmp",
                "-e",
                "MEMENTO_EMBEDDER=candle",
                "-e",
                "FASTEMBED_CACHE_DIR=/models",
                "-v",
                "/cache:/models",
                "-v",
                "/data:/out",
                "image:test",
                "migrate",
                "--qdrant-url",
                "http://qdrant:6334",
                "--to",
                "/out/memento.db"
            ]
        );
    }

    #[test]
    fn forced_backup_and_failed_migration_restore_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("memento.db");
        let paths = database_paths(&database);
        for (index, path) in paths.iter().enumerate() {
            fs::write(path, index.to_string()).unwrap();
        }
        assert!(backup_database(&database, false).is_err());
        let backup = backup_database(&database, true).unwrap();
        assert_eq!(backup.len(), 3);
        fs::write(&database, "partial").unwrap();
        remove_database(&database).unwrap();
        restore_database(&backup).unwrap();
        for (index, path) in paths.iter().enumerate() {
            assert_eq!(fs::read_to_string(path).unwrap(), index.to_string());
        }
    }

    #[test]
    fn compose_project_validation_accepts_override_and_rejects_bad_names() {
        assert_eq!(
            validated_project("memento-migtest".into()).unwrap(),
            "memento-migtest"
        );
        assert!(validated_project("".into()).is_err());
        assert!(validated_project("Bad Name".into()).is_err());
        assert!(validated_project("-lead".into()).is_err());
    }

    #[test]
    fn purge_paths_include_database_sidecars_backups_state_and_model_cache() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("memento.db.bak-20260101T000000Z"), "x").unwrap();
        fs::write(data.join("unrelated.txt"), "x").unwrap();
        let paths = purge_paths_in(&data, &dir.path().join("state"), &dir.path().join("models"));
        let names = paths
            .iter()
            .map(|path| path.strip_prefix(dir.path()).unwrap().display().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "data/memento.db",
                "data/memento.db-wal",
                "data/memento.db-shm",
                "data/memento.db.bak-20260101T000000Z",
                "state/mementod.lock",
                "state/mementod.log",
                "models"
            ]
        );
    }

    #[test]
    fn prune_refuses_docker_mode_and_unforced_external_with_volume() {
        let stack = DockerStack {
            volumes: vec!["memento_qdrant_storage".into()],
            ..DockerStack::default()
        };
        assert!(prune_refusal(DaemonMode::Docker, &DockerStack::default(), true).is_some());
        assert!(prune_refusal(DaemonMode::Docker, &stack, true).is_some());
        assert!(prune_refusal(DaemonMode::External, &stack, false).is_some());
        assert!(prune_refusal(DaemonMode::External, &stack, true).is_none());
        assert!(prune_refusal(DaemonMode::External, &DockerStack::default(), false).is_none());
        assert!(prune_refusal(DaemonMode::Native, &stack, false).is_none());
    }

    #[test]
    fn empty_stack_has_nothing_to_prune() {
        let stack = DockerStack::default();
        assert!(stack.is_empty());
        assert!(prune_commands(&stack).is_empty());
        assert!(
            !DockerStack {
                compose_file: true,
                ..DockerStack::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn prune_plan_has_exact_docker_arguments() {
        let stack = DockerStack {
            containers: vec!["memento-mementod-1".into(), "memento-qdrant-1".into()],
            networks: vec!["memento_default".into()],
            volumes: vec!["memento_qdrant_storage".into()],
            compose_file: true,
        };
        assert_eq!(
            prune_commands(&stack),
            vec![
                vec!["rm", "-f", "memento-mementod-1", "memento-qdrant-1"],
                vec!["network", "rm", "memento_default"],
                vec!["volume", "rm", "memento_qdrant_storage"],
            ]
        );
    }

    #[test]
    fn migration_hint_points_at_prune_docker_not_purge() {
        let hint = migration_prune_hint();
        assert!(hint.ends_with("memento daemon prune-docker"));
        assert!(!hint.contains("purge"));
    }

    #[test]
    fn migration_report_requires_count() {
        assert_eq!(
            migration_report_count("segments: 20\nsessions: 1\n").unwrap(),
            20
        );
        assert!(migration_report_count("sessions: 1").is_err());
    }

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
