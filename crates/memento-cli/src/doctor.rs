use std::fs;
use std::io::{BufRead, BufReader};
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

pub(super) struct OpenCodeOptions {
    pub(super) resume: Option<(f64, f64)>,
    pub(super) primer: bool,
    pub(super) compaction: &'static str,
    subagent_memory: &'static str,
}

impl OpenCodeOptions {
    pub(super) fn resume_summary(&self) -> String {
        match self.resume {
            Some((idle, tokens)) => format!("on(idle={idle}m,min_tokens={tokens})"),
            None => "off".into(),
        }
    }
}

pub(super) fn resolve_opencode_options(options: &Value) -> OpenCodeOptions {
    let resume = options.get("resume").and_then(|value| {
        if value == &Value::Bool(true) || value.is_object() {
            let positive = |name, default| {
                value
                    .get(name)
                    .and_then(Value::as_f64)
                    .filter(|number| number.is_finite() && *number > 0.0)
                    .unwrap_or(default)
            };
            Some((
                positive("idleMinutes", 60.0),
                positive("minContextTokens", 100000.0),
            ))
        } else {
            None
        }
    });
    let compaction = if options.get("compaction").and_then(Value::as_str) == Some("memento") {
        "memento"
    } else {
        "native"
    };
    let subagent_memory = match options.get("subagentMemory").and_then(Value::as_str) {
        Some("off") => "off",
        Some("filtered") => "filtered",
        _ => "digest",
    };
    OpenCodeOptions {
        resume,
        primer: options
            .get("projectPrimer")
            .is_some_and(|value| value == &Value::Bool(true) || value.is_object()),
        compaction,
        subagent_memory,
    }
}

pub(super) fn opencode_options_from_text(
    contents: &str,
    path: &Path,
    plugin: Option<&Path>,
) -> Result<Value, String> {
    let directory = plugin.ok_or("plugin directory unavailable")?;
    let root = CstRootNode::parse(contents, &ParseOptions::default())
        .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;
    let config = root.to_serde_value().ok_or("config has no root value")?;
    let base = path.parent().ok_or("config has no parent")?;
    let expected = normalized_path(directory, base);
    let entry = config
        .get("plugins")
        .and_then(Value::as_array)
        .and_then(|entries| {
            entries.iter().find(|entry| {
                entry
                    .as_str()
                    .or_else(|| entry.get("package").and_then(Value::as_str))
                    .is_some_and(|package| normalized_path(Path::new(package), base) == expected)
            })
        })
        .ok_or("registered plugin entry not found")?;
    match entry.get("options") {
        None | Some(Value::Null) => Ok(serde_json::json!({})),
        Some(options) if options.is_object() => Ok(options.clone()),
        Some(_) => Err("plugin options must be an object".into()),
    }
}

pub(super) fn read_opencode_options(path: &Path, plugin: Option<&Path>) -> Result<Value, String> {
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    opencode_options_from_text(&contents, path, plugin)
}

pub(super) fn opencode_settings(
    path: &Path,
    _harness: Option<crate::cross_harness::Harness>,
    plugin: Option<&Path>,
) -> Result<Vec<String>, String> {
    let options = resolve_opencode_options(&read_opencode_options(path, plugin)?);
    let mut lines = vec![format!(
        "ok: settings opencode subagent_memory={} resume={} compaction={} primer={}",
        options.subagent_memory,
        options.resume_summary(),
        options.compaction,
        if options.primer { "on" } else { "off" }
    )];
    if options.resume.is_none() {
        lines.push("warn: opencode resume off (enable: memento setup --resume on)".into());
    } else if options.compaction == "native" {
        lines.push("warn: opencode resume uses native compaction; summary request is uncached (memento setup --compaction memento)".into());
    }
    Ok(lines)
}

pub(super) fn hook_settings(
    path: &Path,
    harness: Option<crate::cross_harness::Harness>,
    _plugin: Option<&Path>,
) -> Result<Vec<String>, String> {
    let harness = harness.ok_or("hook harness unavailable")?;
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let config: Value = serde_json::from_str(&contents)
        .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;
    let is_agy = matches!(harness, crate::cross_harness::Harness::Agy);
    let hooks = config
        .get(if is_agy { "memento" } else { "hooks" })
        .and_then(Value::as_object)
        .ok_or("hook settings unavailable")?;
    let mut events = Vec::new();
    let mut subagent_memory = false;
    for event in ["SessionStart", "Stop", "SubagentStart"] {
        let Some(groups) = hooks.get(event) else {
            continue;
        };
        let groups = groups
            .as_array()
            .ok_or_else(|| format!("{event} hooks are not an array"))?;
        let mut found = false;
        for group in groups {
            let handlers: Vec<&Value> = if is_agy {
                vec![group]
            } else {
                group
                    .get("hooks")
                    .and_then(Value::as_array)
                    .map(|handlers| handlers.iter().collect())
                    .unwrap_or_default()
            };
            for handler in handlers {
                if let Some(words) = handler
                    .get("command")
                    .and_then(Value::as_str)
                    .and_then(|command| crate::cross_harness::memento_hook_words(command, harness))
                {
                    found = true;
                    if event == "SubagentStart" {
                        subagent_memory |= words.windows(2).any(|pair| {
                            pair[0] == "--subagent-memory"
                                && matches!(pair[1].as_str(), "digest" | "filtered")
                        });
                    }
                }
            }
        }
        if found {
            events.push(event);
        }
    }
    let id = match harness {
        crate::cross_harness::Harness::ClaudeCode => "claude-code",
        crate::cross_harness::Harness::Codex => "codex",
        crate::cross_harness::Harness::Agy => "agy",
    };
    let mut line = format!("ok: settings {id} hooks={}", events.join(","));
    if !is_agy {
        line.push_str(if subagent_memory {
            " subagent_memory=on"
        } else {
            " subagent_memory=off"
        });
    }
    Ok(vec![line])
}

fn settings_lines(setup: &Value, home: &Path, xdg: &Path, plugin: Option<&Path>) -> Vec<String> {
    let Some(harnesses) = setup.get("harnesses").and_then(Value::as_array) else {
        return vec!["warn: settings unavailable: saved harness list missing".into()];
    };
    harnesses
        .iter()
        .filter_map(Value::as_str)
        .flat_map(|id| {
            let result = match setup.get("scope").and_then(Value::as_str) {
                Some("user") => {
                    crate::setup::settings_for_harness(id, true, Path::new(""), home, xdg, plugin)
                }
                Some("project") => setup
                    .get("project")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "saved project directory missing".to_string())
                    .and_then(|project| {
                        crate::setup::settings_for_harness(
                            id,
                            false,
                            Path::new(project),
                            home,
                            xdg,
                            plugin,
                        )
                    }),
                _ => Err("saved scope invalid".into()),
            };
            result.unwrap_or_else(|error| vec![format!("warn: settings {id} unavailable: {error}")])
        })
        .collect()
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
            println!("warn: cannot resolve OpenCode plugin directory for registration");
            return true;
        }
        Err(error) => {
            println!(
                "warn: cannot resolve OpenCode plugin directory: {}",
                error.message
            );
            return true;
        }
    };
    let user = match setup.get("scope").and_then(Value::as_str) {
        Some("user") => true,
        Some("project") => false,
        _ => {
            println!("warn: saved OpenCode setup has invalid scope");
            return true;
        }
    };
    let project = if user {
        PathBuf::new()
    } else if let Some(project) = setup.get("project").and_then(Value::as_str) {
        PathBuf::from(project)
    } else {
        println!("warn: saved OpenCode project directory missing");
        return true;
    };
    let xdg = if user {
        match std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        {
            Some(xdg) => xdg,
            None => {
                println!("warn: cannot resolve OpenCode config: HOME is not set");
                return true;
            }
        }
    } else {
        PathBuf::new()
    };
    let config = crate::setup::opencode_config_path(user, &project, &xdg);
    match plugin_registered(&config, &directory) {
        Ok(true) => {
            println!("ok: OpenCode plugin registered in {}", config.display());
            let state_home = std::env::var_os("XDG_STATE_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
                });
            match state_home {
                Some(state_home) => {
                    for line in activity_lines(
                        &state_home.join("memento/opencode-activity.jsonl"),
                        chrono::Utc::now().timestamp_millis(),
                    ) {
                        println!("{line}");
                    }
                }
                None => println!("warn: opencode activity unavailable: HOME is not set"),
            }
            true
        }
        Err(error) => {
            println!(
                "warn: cannot read or parse OpenCode config {}: {error}",
                config.display()
            );
            true
        }
        Ok(false) => {
            println!(
                "fail: OpenCode plugin not registered in {}",
                config.display()
            );
            println!(
                "  Add this entry to the plugins array: {}",
                serde_json::json!({"package": directory})
            );
            false
        }
    }
}

fn activity_lines(path: &Path, now: i64) -> Vec<String> {
    let mut counts = [0_u64; 6];
    let mut latest = None::<i64>;
    let mut errors = Vec::<(i64, String)>::new();
    let mut found = false;
    let mut read_error = false;
    let rotated = path.with_file_name(format!(
        "{}.1",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    for candidate in [&rotated, path] {
        let file = match fs::File::open(candidate) {
            Ok(file) => {
                found = true;
                file
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                read_error = true;
                continue;
            }
        };
        for line in BufReader::new(file).lines() {
            let line = match line {
                Ok(line) => line,
                Err(_) => {
                    read_error = true;
                    break;
                }
            };
            let Ok(entry) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(ts) = entry
                .get("ts")
                .and_then(Value::as_i64)
                .filter(|ts| *ts <= now)
            else {
                continue;
            };
            if chrono::DateTime::from_timestamp_millis(ts).is_none() {
                continue;
            }
            latest = Some(latest.map_or(ts, |previous| previous.max(ts)));
            if now.saturating_sub(ts) <= 7 * 24 * 60 * 60 * 1000
                && let Some(index) = [
                    "ingest",
                    "compaction",
                    "switch",
                    "subagent_digest",
                    "resume",
                    "primer",
                ]
                .iter()
                .position(|event| entry.get("event").and_then(Value::as_str) == Some(event))
            {
                counts[index] += 1;
            }
            if now.saturating_sub(ts) <= 24 * 60 * 60 * 1000
                && matches!(
                    entry.get("level").and_then(Value::as_str),
                    Some("warn" | "error")
                )
            {
                let message = entry
                    .get("message")
                    .and_then(Value::as_str)
                    .or_else(|| entry.get("event").and_then(Value::as_str))
                    .unwrap_or("unknown")
                    .chars()
                    .map(|character| {
                        if character.is_control() {
                            ' '
                        } else {
                            character
                        }
                    })
                    .take(300)
                    .collect();
                errors.push((ts, message));
            }
        }
    }
    if !found {
        return vec![if read_error {
            "warn: opencode activity unavailable: cannot read activity file".into()
        } else {
            "warn: no opencode plugin activity recorded yet (plugin may predate activity logging or has not run)".into()
        }];
    }
    let last = latest
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| "unknown".into());
    let mut lines = vec![format!(
        "ok: opencode activity last={last} events(7d): ingest={} compaction={} switch={} subagent_digest={} resume={} primer={}",
        counts[0], counts[1], counts[2], counts[3], counts[4], counts[5]
    )];
    if !errors.is_empty() {
        errors.sort_by_key(|entry| std::cmp::Reverse(entry.0));
        let mut groups = Vec::<(&str, usize)>::new();
        let mut group_indices = std::collections::HashMap::<&str, usize>::new();
        for (_, message) in &errors {
            let message = message.as_str();
            if let Some(&index) = group_indices.get(message) {
                groups[index].1 += 1;
            } else {
                group_indices.insert(message, groups.len());
                groups.push((message, 1));
            }
        }
        let mut messages = groups
            .iter()
            .take(3)
            .map(|(message, count)| format!("{message} (x{count})"))
            .collect::<Vec<_>>()
            .join("; ");
        if groups.len() > 3 {
            messages.push_str(&format!("; +{} more", groups.len() - 3));
        }
        lines.push(format!(
            "warn: opencode activity errors(24h)={} latest: {messages}",
            errors.len()
        ));
    }
    if read_error {
        lines.push(
            "warn: opencode activity partially unavailable: cannot read activity file".into(),
        );
    }
    lines
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
    let setup = receipt.get("setup").and_then(Value::as_object);
    let mode = crate::daemon::saved_mode(setup)?;
    println!("ok: daemon mode {}", mode.name());
    if mode != crate::daemon::DaemonMode::Native {
        match crate::daemon::docker_memory_exists() {
            Ok(true) => println!("warn: Docker memory found; run memento daemon migrate"),
            Ok(false) => {}
            Err(error) => println!("warn: Docker memory detection: {}", error.message),
        }
    }
    if mode == crate::daemon::DaemonMode::Native {
        if let Ok(true) = crate::daemon::docker_memory_exists() {
            println!(
                "warn: old Docker memory volume still exists; delete it with memento daemon prune-docker"
            );
        }
        match crate::daemon::native_binary() {
            Ok(path) => println!("ok: native binary {}", path.display()),
            Err(error) => println!("warn: native binary: {}", error.message),
        }
        let state = crate::daemon::state_dir()?;
        let lock = state.join("mementod.lock");
        match crate::daemon::running_pid(&lock) {
            Ok(Some(pid)) => println!("ok: native lock {} pid={pid}", lock.display()),
            Ok(None) => println!("ok: native lock {} stopped", lock.display()),
            Err(error) => println!("warn: native lock: {}", error.message),
        }
        println!("ok: native log {}", state.join("mementod.log").display());
        println!(
            "ok: native idle exit {} seconds",
            setup.map(crate::daemon::idle_seconds).unwrap_or(1800)
        );
        let cache = std::env::var_os("HF_HOME")
            .or_else(|| std::env::var_os("FASTEMBED_CACHE_DIR"))
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("XDG_CACHE_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| {
                        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
                    })
                    .join("memento/models")
            });
        println!(
            "{}: native model cache {}",
            if cache.is_dir() { "ok" } else { "warn" },
            cache.display()
        );
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
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let plugin = plugin_directory(setup).ok().flatten();
        for line in settings_lines(setup, &home, &xdg, plugin.as_deref()) {
            println!("{line}");
        }
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
    fn activity_counts_rotated_and_current_files_and_tolerates_malformed_lines() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("activity.jsonl");
        let now = 1_700_000_000_000_i64;
        let day = 24 * 60 * 60 * 1000;
        fs::write(
            path.with_file_name("activity.jsonl.1"),
            format!(
                "{{\"ts\":{},\"level\":\"info\",\"event\":\"ingest\"}}\ninvalid\n{{\"ts\":{},\"level\":\"info\",\"event\":\"switch\"}}\n",
                now - 2 * day,
                now - 8 * day
            ),
        )
        .unwrap();
        fs::write(
            &path,
            format!(
                "{{\"ts\":{},\"level\":\"warn\",\"event\":\"compaction\",\"message\":\"first\"}}\n{{\"ts\":{},\"level\":\"error\",\"event\":\"resume\",\"message\":\"{}\"}}\n{{\"ts\":{},\"level\":\"info\",\"event\":\"subagent_digest\"}}\n{{\"ts\":{},\"level\":\"info\",\"event\":\"primer\"}}\n",
                now - 1000,
                now - 500,
                "x".repeat(400),
                now - 100,
                now - 200
            ),
        )
        .unwrap();
        let lines = activity_lines(&path, now);
        assert_eq!(
            lines[0],
            "ok: opencode activity last=2023-11-14T22:13:19Z events(7d): ingest=1 compaction=1 switch=0 subagent_digest=1 resume=1 primer=1"
        );
        assert_eq!(
            lines[1],
            format!(
                "warn: opencode activity errors(24h)=2 latest: {} (x1); first (x1)",
                "x".repeat(300)
            )
        );
    }

    #[test]
    fn activity_groups_warnings_by_message_and_orders_by_latest_occurrence() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("activity.jsonl");
        let now = 1_700_000_000_000_i64;
        fs::write(
            path.with_file_name("activity.jsonl.1"),
            format!(
                "{{\"ts\":{},\"level\":\"warn\",\"message\":\"first\"}}\n{{\"ts\":{},\"level\":\"error\",\"message\":\"second\"}}\n{{\"ts\":{},\"level\":\"warn\",\"message\":\"fourth\"}}\n",
                now - 100,
                now - 600,
                now - 400
            ),
        )
        .unwrap();
        fs::write(
            &path,
            format!(
                "{{\"ts\":{},\"level\":\"error\",\"message\":\"first\"}}\n{{\"ts\":{},\"level\":\"warn\",\"message\":\"third\"}}\n{{\"ts\":{},\"level\":\"warn\",\"message\":\"second\"}}\n{{\"ts\":{},\"level\":\"info\",\"message\":\"first\"}}\n{{\"ts\":{},\"level\":\"warn\",\"message\":\"first\"}}\n",
                now - 700,
                now - 300,
                now - 200,
                now - 50,
                now - 24 * 60 * 60 * 1000 - 1
            ),
        )
        .unwrap();
        assert_eq!(
            activity_lines(&path, now)[1],
            "warn: opencode activity errors(24h)=6 latest: first (x2); second (x2); third (x1); +1 more"
        );
    }

    #[test]
    fn activity_missing_and_unreadable_files_only_warn() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("activity.jsonl");
        assert!(activity_lines(&path, 0)[0].starts_with("warn: no opencode plugin activity"));
        fs::create_dir(&path).unwrap();
        assert!(
            activity_lines(&path, 0)
                .iter()
                .any(|line| line.starts_with("warn:"))
        );
    }

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

    #[test]
    fn opencode_options_resolve_defaults_objects_and_warnings() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("opencode.json");
        let plugin = temp.path().join("plugin");
        let write = |options: Value| {
            fs::write(
                &path,
                serde_json::json!({"plugins":[{"package":plugin,"options":options}]}).to_string(),
            )
            .unwrap();
        };
        write(serde_json::json!({}));
        let lines = opencode_settings(&path, None, Some(&plugin)).unwrap();
        assert_eq!(
            lines[0],
            "ok: settings opencode subagent_memory=digest resume=off compaction=native primer=off"
        );
        assert!(lines[1].contains("resume off"));
        write(serde_json::json!({"resume":true,"projectPrimer":true}));
        let lines = opencode_settings(&path, None, Some(&plugin)).unwrap();
        assert!(lines[0].contains("resume=on(idle=60m,min_tokens=100000)"));
        assert!(lines[0].contains("primer=on"));
        assert!(lines[1].contains("native compaction"));
        write(
            serde_json::json!({"resume":{"idleMinutes":12.5,"minContextTokens":0},"compaction":"memento","subagentMemory":"filtered","projectPrimer":{"budget":2000}}),
        );
        let lines = opencode_settings(&path, None, Some(&plugin)).unwrap();
        assert_eq!(
            lines,
            [
                "ok: settings opencode subagent_memory=filtered resume=on(idle=12.5m,min_tokens=100000) compaction=memento primer=on"
            ]
        );
        write(serde_json::json!({"resume":[],"compaction":"other","subagentMemory":"invalid"}));
        let lines = opencode_settings(&path, None, Some(&plugin)).unwrap();
        assert!(lines[0].contains("subagent_memory=digest resume=off compaction=native"));
    }

    #[test]
    fn hook_settings_and_unavailable_config_are_reported() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join(".codex");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("hooks.json"), serde_json::json!({
            "hooks": {
                "SessionStart":[{"hooks":[{"command":"/bin/memento hook codex"}]}],
                "Stop":[{"hooks":[{"command":"/bin/memento hook codex"}]}],
                "SubagentStart":[{"hooks":[{"command":"/bin/memento hook codex --subagent-memory digest"}]}]
            }
        }).to_string()).unwrap();
        let setup = serde_json::json!({"harnesses":["codex","claude-code"],"scope":"project","project":temp.path()});
        let lines = settings_lines(&setup, temp.path(), temp.path(), None);
        assert_eq!(
            lines[0],
            "ok: settings codex hooks=SessionStart,Stop,SubagentStart subagent_memory=on"
        );
        assert!(lines[1].starts_with("warn: settings claude-code unavailable: cannot read "));
    }
}
