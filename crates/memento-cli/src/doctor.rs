use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use clap::Args;
use serde_json::Value;

use crate::{Cli, CliError};

#[derive(Debug, Args)]
pub(super) struct DoctorArgs {
    #[arg(long, help = "Skip network checks")]
    offline: bool,
}

#[derive(Debug, Args)]
pub(super) struct WriteReceiptArgs {
    #[arg(long, required = true, help = "Installer method")]
    method: String,
    #[arg(long, required = true, help = "Installation binary directory")]
    bin_dir: PathBuf,
}

pub(super) fn write_receipt(args: &WriteReceiptArgs) -> Result<(), CliError> {
    crate::receipt::merge_fields(serde_json::json!({
        "method": args.method,
        "bin_dir": args.bin_dir,
        "version": env!("CARGO_PKG_VERSION")
    }))
    .map_err(|error| CliError::other(format!("cannot write install receipt: {error}")))
}

fn docker_check(args: &[&str]) -> bool {
    Command::new("docker")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn daemon_health(cli: &Cli) -> Result<Option<Value>, CliError> {
    let url = crate::endpoint(&cli.url, &["health"])?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|error| CliError::other(format!("cannot create health client: {error}")))?;
    let mut request = client.get(url);
    if let Some(token) = &cli.token {
        request = request.bearer_auth(token);
    }
    match request.send() {
        Ok(response) if response.status().is_success() => response
            .json()
            .map(Some)
            .map_err(|error| CliError::other(format!("cannot parse daemon health: {error}"))),
        _ => Ok(None),
    }
}

fn plugin_directory(setup: &Value) -> Result<Option<PathBuf>, CliError> {
    if let Some(path) = setup.get("opencode_plugin").and_then(Value::as_str) {
        return Ok(Some(PathBuf::from(path)));
    }
    if setup
        .get("harnesses")
        .and_then(Value::as_array)
        .is_some_and(|harnesses| {
            harnesses
                .iter()
                .any(|harness| harness.as_str() == Some("opencode"))
        })
        && crate::assets::OPENCODE_BUNDLE_AVAILABLE
    {
        return Ok(Some(crate::assets::data_dir()?.join("opencode-plugin")));
    }
    Ok(None)
}

fn check_plugin(setup: &Value) {
    match plugin_directory(setup) {
        Ok(Some(directory)) => {
            if !directory.is_dir() {
                println!(
                    "warn: OpenCode plugin directory missing: {}",
                    directory.display()
                );
                return;
            }
            let package_path = directory.join("package.json");
            match fs::read_to_string(&package_path) {
                Ok(contents) => match serde_json::from_str::<Value>(&contents) {
                    Ok(package) => {
                        let version = package
                            .get("version")
                            .and_then(Value::as_str)
                            .unwrap_or("-");
                        let marker = if version == env!("CARGO_PKG_VERSION") {
                            "ok"
                        } else {
                            "warn"
                        };
                        println!(
                            "{marker}: OpenCode plugin {} version={version}",
                            directory.display()
                        );
                    }
                    Err(error) => println!(
                        "warn: cannot parse OpenCode plugin package {}: {error}",
                        package_path.display()
                    ),
                },
                Err(error) => println!(
                    "warn: cannot read OpenCode plugin package {}: {error}",
                    package_path.display()
                ),
            }
        }
        Ok(None) => println!("warn: no saved OpenCode plugin directory"),
        Err(error) => println!(
            "warn: cannot resolve OpenCode plugin directory: {}",
            error.message
        ),
    }
}

pub(super) fn run(cli: &Cli, args: &DoctorArgs) -> Result<(), CliError> {
    let mut failed = false;
    println!(
        "ok: memento {} target={}",
        env!("CARGO_PKG_VERSION"),
        env!("MEMENTO_TARGET")
    );
    match std::env::current_exe() {
        Ok(path) => println!("ok: executable {}", path.display()),
        Err(error) => {
            failed = true;
            println!("fail: executable path: {error}");
        }
    }
    let receipt = match crate::receipt::read() {
        Ok(receipt) => {
            println!(
                "ok: receipt method={} bin_dir={} version={}",
                receipt.get("method").and_then(Value::as_str).unwrap_or("-"),
                receipt
                    .get("bin_dir")
                    .and_then(Value::as_str)
                    .unwrap_or("-"),
                receipt
                    .get("version")
                    .and_then(Value::as_str)
                    .unwrap_or("-")
            );
            receipt
        }
        Err(error) => {
            failed = true;
            println!("fail: receipt: {error}");
            Default::default()
        }
    };
    if args.offline {
        println!("warn: latest release skipped (--offline)");
    } else {
        match crate::update::release_version() {
            Ok(version) => println!("ok: latest release {version}"),
            Err(error) => println!("warn: latest release unavailable: {}", error.message),
        }
    }
    let docker = docker_check(&["--version"]);
    println!(
        "{}: docker {}",
        if docker { "ok" } else { "warn" },
        if docker { "present" } else { "not found" }
    );
    let compose = docker_check(&["compose", "version"]);
    println!(
        "{}: docker compose {}",
        if compose { "ok" } else { "warn" },
        if compose { "present" } else { "not found" }
    );
    match daemon_health(cli) {
        Ok(Some(health)) if health.get("ok").and_then(Value::as_bool) == Some(true) => {
            let daemon_version = health.get("version").and_then(Value::as_str);
            println!(
                "ok: daemon health version={}",
                daemon_version.unwrap_or("-")
            );
            if daemon_version.is_some_and(|version| version != env!("CARGO_PKG_VERSION")) {
                println!(
                    "warn: daemon version {} differs from CLI {}",
                    daemon_version.unwrap_or("-"),
                    env!("CARGO_PKG_VERSION")
                );
            }
        }
        Ok(_) => println!("warn: daemon unavailable or unhealthy"),
        Err(error) => println!("warn: daemon health check failed: {}", error.message),
    }
    if let Some(setup) = receipt.get("setup") {
        check_plugin(setup);
        println!("ok: saved setup {setup}");
    } else {
        println!("warn: no saved setup");
    }
    if failed {
        Err(CliError::other("doctor found failed checks"))
    } else {
        Ok(())
    }
}
