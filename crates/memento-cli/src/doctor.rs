use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use clap::Args;
use jsonc_parser::ParseOptions;
use jsonc_parser::cst::CstRootNode;
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

fn normalized_path(path: &Path, base: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    if let Ok(canonical) = absolute.canonicalize() {
        return canonical;
    }
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn plugin_registered(config: &Path, directory: &Path) -> Result<bool, String> {
    let contents = fs::read_to_string(config).map_err(|error| error.to_string())?;
    let root = CstRootNode::parse(&contents, &ParseOptions::default())
        .map_err(|error| error.to_string())?;
    let value = root.to_serde_value().ok_or("config has no root value")?;
    let base = config.parent().ok_or("config has no parent")?;
    let expected = normalized_path(directory, base);
    Ok(value
        .get("plugins")
        .and_then(Value::as_array)
        .is_some_and(|plugins| {
            plugins.iter().any(|entry| {
                entry
                    .as_str()
                    .or_else(|| entry.get("package").and_then(Value::as_str))
                    .is_some_and(|package| normalized_path(Path::new(package), base) == expected)
            })
        }))
}

fn check_registration(setup: &Value) -> bool {
    if !setup
        .get("harnesses")
        .and_then(Value::as_array)
        .is_some_and(|harnesses| {
            harnesses
                .iter()
                .any(|entry| entry.as_str() == Some("opencode"))
        })
    {
        return true;
    }
    let directory = match plugin_directory(setup) {
        Ok(Some(directory)) => directory,
        Ok(None) => {
            println!("fail: cannot resolve OpenCode plugin directory for registration");
            return false;
        }
        Err(error) => {
            println!(
                "fail: cannot resolve OpenCode plugin directory: {}",
                error.message
            );
            return false;
        }
    };
    let user = match setup.get("scope").and_then(Value::as_str) {
        Some("user") => true,
        Some("project") => false,
        _ => {
            println!("fail: saved OpenCode setup has invalid scope");
            return false;
        }
    };
    let project = if user {
        PathBuf::new()
    } else if let Some(project) = setup.get("project").and_then(Value::as_str) {
        PathBuf::from(project)
    } else {
        println!("fail: saved OpenCode project directory missing");
        return false;
    };
    let xdg = if user {
        match std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        {
            Some(xdg) => xdg,
            None => {
                println!("fail: cannot resolve OpenCode config: HOME is not set");
                return false;
            }
        }
    } else {
        PathBuf::new()
    };
    let config = crate::setup::opencode_config_path(user, &project, &xdg);
    match plugin_registered(&config, &directory) {
        Ok(true) => {
            println!("ok: OpenCode plugin registered in {}", config.display());
            true
        }
        result => {
            println!(
                "fail: OpenCode plugin not registered in {}",
                config.display()
            );
            if let Err(error) = result {
                println!("  cannot read or parse config: {error}");
            }
            println!(
                "  Add this entry to the plugins array: {}",
                serde_json::json!({"package": directory})
            );
            false
        }
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
        failed |= !check_registration(setup);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_checks_jsonc_objects_strings_and_missing_entries() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("opencode.jsonc");
        let plugin = temp.path().join("memento/opencode-plugin");
        fs::write(
            &config,
            format!(
                "{{\n  // keep\n  \"plugins\": [{{\"package\": \"{}/\"}},],\n}}",
                plugin.display()
            ),
        )
        .unwrap();
        assert!(plugin_registered(&config, &plugin).unwrap());
        fs::write(&config, r#"{"plugins":["memento/./opencode-plugin/"]}"#).unwrap();
        assert!(plugin_registered(&config, &plugin).unwrap());
        fs::write(&config, r#"{"plugins":[{"package":"/other"}]}"#).unwrap();
        assert!(!plugin_registered(&config, &plugin).unwrap());
        fs::write(&config, r#"{"plugins":[invalid]}"#).unwrap();
        assert!(plugin_registered(&config, &plugin).is_err());
    }

    #[test]
    fn registration_respects_saved_project_and_reports_failure() {
        let temp = tempfile::tempdir().unwrap();
        let plugin = temp.path().join("plugin");
        let config = temp.path().join("opencode.json");
        let setup = serde_json::json!({
            "harnesses": ["opencode"],
            "scope": "project",
            "project": temp.path(),
            "opencode_plugin": plugin
        });
        fs::write(
            &config,
            serde_json::json!({"plugins": [plugin]}).to_string(),
        )
        .unwrap();
        assert!(check_registration(&setup));
        fs::write(&config, r#"{"plugins":[]}"#).unwrap();
        assert!(!check_registration(&setup));
    }
}
