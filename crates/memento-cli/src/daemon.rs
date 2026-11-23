use serde_json::{Map, Value};
use std::fs;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Args, Subcommand};
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RefreshStep {
    Pull,
    Up,
}

pub(super) fn managed_refresh_steps(setup: Option<&Map<String, Value>>) -> Vec<RefreshStep> {
    match setup
        .and_then(|setup| setup.get("managed_daemon"))
        .and_then(Value::as_bool)
    {
        Some(true) => vec![RefreshStep::Pull, RefreshStep::Up],
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
}
