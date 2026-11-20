use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, ValueEnum};
use dialoguer::{Confirm, Input, MultiSelect, Select};
use serde_json::{Value, json};

use crate::cross_harness::{
    self, Harness, InstallArgs, InstallSkillsArgs, InstallSubagentMemory, SkillHarness,
};
use crate::{Cli, CliError, endpoint};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Scope {
    Project,
    User,
}

#[derive(Debug, Args)]
pub struct SetupArgs {
    #[arg(
        long,
        value_delimiter = ',',
        help = "Comma-separated harness identifiers"
    )]
    harness: Vec<String>,
    #[arg(long, value_enum, help = "Installation scope")]
    scope: Option<Scope>,
    #[arg(long, help = "Project directory")]
    project: Option<PathBuf>,
    #[arg(
        long,
        value_delimiter = ',',
        help = "Comma-separated harness=on|off settings"
    )]
    subagent_memory: Vec<String>,
    #[arg(long, conflicts_with = "no_skills", help = "Install bundled skills")]
    skills: bool,
    #[arg(long, conflicts_with = "skills", help = "Skip skills installation")]
    no_skills: bool,
    #[arg(long, help = "Bundled skills source directory")]
    skills_source: Option<PathBuf>,
    #[arg(long, help = "OpenCode plugin directory")]
    opencode_plugin: Option<PathBuf>,
    #[arg(long, help = "Apply without prompting")]
    yes: bool,
    #[arg(long, help = "Preview without writing")]
    dry_run: bool,
}

struct HarnessEntry {
    id: &'static str,
    display: &'static str,
    executable: &'static str,
    scopes: &'static [Scope],
    subagent_default: Option<bool>,
    hook: Option<Harness>,
    skills: SkillHarness,
    project_config: &'static str,
    user_config: &'static str,
    project_skills: &'static str,
    user_skills: &'static str,
    plan: fn(&HarnessEntry, &Context, bool) -> Result<Change, CliError>,
    apply: fn(&HarnessEntry, &Context, bool) -> Result<(), CliError>,
}

const SCOPES: &[Scope] = &[Scope::Project, Scope::User];
const REGISTRY: &[HarnessEntry] = &[
    HarnessEntry {
        id: "opencode",
        display: "OpenCode",
        executable: "opencode",
        scopes: SCOPES,
        subagent_default: Some(true),
        hook: None,
        skills: SkillHarness::Opencode,
        project_config: "opencode.jsonc",
        user_config: ".config/opencode/opencode.jsonc",
        project_skills: ".opencode/skills",
        user_skills: ".config/opencode/skills",
        plan: plan_opencode,
        apply: apply_opencode,
    },
    HarnessEntry {
        id: "claude-code",
        display: "Claude Code",
        executable: "claude",
        scopes: SCOPES,
        subagent_default: Some(false),
        hook: Some(Harness::ClaudeCode),
        skills: SkillHarness::ClaudeCode,
        project_config: ".claude/settings.json",
        user_config: ".claude/settings.json",
        project_skills: ".claude/skills",
        user_skills: ".claude/skills",
        plan: plan_hook,
        apply: apply_hook,
    },
    HarnessEntry {
        id: "codex",
        display: "Codex",
        executable: "codex",
        scopes: SCOPES,
        subagent_default: Some(true),
        hook: Some(Harness::Codex),
        skills: SkillHarness::Codex,
        project_config: ".codex/hooks.json",
        user_config: ".codex/hooks.json",
        project_skills: ".agents/skills",
        user_skills: ".agents/skills",
        plan: plan_hook,
        apply: apply_hook,
    },
    HarnessEntry {
        id: "agy",
        display: "agy",
        executable: "agy",
        scopes: SCOPES,
        subagent_default: None,
        hook: Some(Harness::Agy),
        skills: SkillHarness::Agy,
        project_config: ".agents/hooks.json",
        user_config: ".gemini/config/hooks.json",
        project_skills: ".agents/skills",
        user_skills: ".gemini/config/skills",
        plan: plan_hook,
        apply: apply_hook,
    },
];

struct Context {
    scope: Scope,
    project: PathBuf,
    home: PathBuf,
    xdg: PathBuf,
    executable: PathBuf,
    plugin: Option<PathBuf>,
    skills_source: Option<PathBuf>,
    url: String,
}

struct Change {
    path: PathBuf,
    description: String,
}

fn prompt_error(error: impl std::fmt::Display) -> CliError {
    CliError::other(format!("setup prompt failed: {error}"))
}

fn detected(executable: &str) -> bool {
    env::var_os("PATH").is_some_and(|path| {
        env::split_paths(&path).any(|dir| {
            let path = dir.join(executable);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                path.metadata().is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            }
            #[cfg(not(unix))]
            {
                path.is_file()
            }
        })
    })
}

fn base(context: &Context) -> &Path {
    if context.scope == Scope::User {
        &context.home
    } else {
        &context.project
    }
}

fn config_path(entry: &HarnessEntry, context: &Context) -> PathBuf {
    if entry.id == "opencode" && context.scope == Scope::User {
        return context.xdg.join("opencode/opencode.jsonc");
    }
    if entry.id == "opencode" && context.scope == Scope::Project {
        for name in [
            "opencode.json",
            "opencode.jsonc",
            ".opencode/opencode.json",
            ".opencode/opencode.jsonc",
        ] {
            let path = context.project.join(name);
            if path.is_file() {
                return path;
            }
        }
    }
    base(context).join(if context.scope == Scope::User {
        entry.user_config
    } else {
        entry.project_config
    })
}

fn skill_path(entry: &HarnessEntry, context: &Context) -> PathBuf {
    base(context).join(if context.scope == Scope::User {
        entry.user_skills
    } else {
        entry.project_skills
    })
}

fn memory_summary(entry: &HarnessEntry, enabled: bool) -> &'static str {
    if entry.subagent_default.is_none() {
        "subagent memory not supported"
    } else if entry.hook.is_none() {
        if enabled {
            "subagentMemory digest"
        } else {
            "subagentMemory off"
        }
    } else if enabled {
        "subagent memory on (SubagentStart --subagent-memory digest)"
    } else {
        "subagent memory off"
    }
}

fn hook_args(entry: &HarnessEntry, context: &Context, memory: bool, dry_run: bool) -> InstallArgs {
    InstallArgs {
        harness: entry.hook.expect("hook registry entry"),
        project: (context.scope == Scope::Project).then(|| context.project.clone()),
        user: context.scope == Scope::User,
        dry_run,
        subagent_memory: entry.subagent_default.map(|_| {
            if memory {
                InstallSubagentMemory::Digest
            } else {
                InstallSubagentMemory::Off
            }
        }),
    }
}

fn plan_hook(entry: &HarnessEntry, context: &Context, memory: bool) -> Result<Change, CliError> {
    cross_harness::install(
        &hook_args(entry, context, memory, true),
        &context.home,
        &context.executable,
    )?;
    Ok(Change {
        path: config_path(entry, context),
        description: "merge lifecycle hooks".into(),
    })
}

fn apply_hook(entry: &HarnessEntry, context: &Context, memory: bool) -> Result<(), CliError> {
    cross_harness::install(
        &hook_args(entry, context, memory, false),
        &context.home,
        &context.executable,
    )?;
    Ok(())
}

fn plugin_entry(context: &Context, memory: bool) -> Result<Value, CliError> {
    let plugin = context
        .plugin
        .as_ref()
        .ok_or_else(|| CliError::other("OpenCode plugin directory is required"))?;
    let package = plugin
        .to_str()
        .ok_or_else(|| CliError::other("OpenCode plugin directory is not UTF-8"))?;
    Ok(
        json!({"package":package,"options":{"url":context.url,"subagentMemory":if memory {"digest"} else {"off"}}}),
    )
}

fn opencode_merge(
    path: &Path,
    context: &Context,
    memory: bool,
) -> Result<Option<String>, CliError> {
    let original = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => "{}".into(),
        Err(error) => {
            return Err(CliError::other(format!(
                "cannot read {}: {error}",
                path.display()
            )));
        }
    };
    let mut value: Value = match serde_json::from_str(&original) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let root = value.as_object_mut().ok_or_else(|| {
        CliError::other(format!(
            "OpenCode config {} must be an object",
            path.display()
        ))
    })?;
    let plugins = root
        .entry("plugins")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| {
            CliError::other(format!(
                "OpenCode plugins in {} must be an array",
                path.display()
            ))
        })?;
    let new_entry = plugin_entry(context, memory)?;
    if let Some(existing) = plugins.iter_mut().find(|entry| {
        entry
            .get("package")
            .and_then(Value::as_str)
            .is_some_and(|package| {
                package
                    .trim_end_matches('/')
                    .ends_with("/adapters/opencode")
            })
            || entry
                .get("options")
                .and_then(|options| options.get("memento"))
                .and_then(Value::as_bool)
                == Some(true)
    }) {
        let object = existing
            .as_object_mut()
            .ok_or_else(|| CliError::other("invalid memento plugin entry"))?;
        object.insert("package".into(), new_entry["package"].clone());
        let options = object
            .entry("options")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| CliError::other("memento plugin options must be an object"))?;
        options.insert("url".into(), new_entry["options"]["url"].clone());
        options.insert(
            "subagentMemory".into(),
            new_entry["options"]["subagentMemory"].clone(),
        );
    } else {
        plugins.push(new_entry);
    }
    serde_json::to_string_pretty(&value)
        .map(Some)
        .map_err(|error| CliError::other(format!("cannot serialize OpenCode config: {error}")))
}

fn plan_opencode(
    entry: &HarnessEntry,
    context: &Context,
    memory: bool,
) -> Result<Change, CliError> {
    let path = config_path(entry, context);
    let description = if opencode_merge(&path, context, memory)?.is_some() {
        "merge OpenCode plugin"
    } else {
        "manual OpenCode plugin entry required (JSONC or invalid JSON)"
    };
    if description.starts_with("manual") {
        let snippet = serde_json::to_string_pretty(&plugin_entry(context, memory)?)
            .map_err(|error| CliError::other(format!("cannot serialize plugin entry: {error}")))?;
        println!(
            "Add this entry to the plugins array in {}:\n{snippet}",
            path.display()
        );
    }
    Ok(Change {
        path,
        description: description.into(),
    })
}

fn apply_opencode(entry: &HarnessEntry, context: &Context, memory: bool) -> Result<(), CliError> {
    let path = config_path(entry, context);
    match opencode_merge(&path, context, memory)? {
        Some(output) => {
            let parent = path
                .parent()
                .ok_or_else(|| CliError::other("OpenCode config has no parent"))?;
            fs::create_dir_all(parent).map_err(|error| {
                CliError::other(format!("cannot create {}: {error}", parent.display()))
            })?;
            cross_harness::save_atomic(&path, output.as_bytes())?;
        }
        None => {
            println!(
                "Add this entry to the plugins array in {}:\n{}",
                path.display(),
                serde_json::to_string_pretty(&plugin_entry(context, memory)?).map_err(|error| {
                    CliError::other(format!("cannot serialize plugin entry: {error}"))
                })?
            );
        }
    }
    Ok(())
}

fn parse_memory(values: &[String]) -> Result<HashMap<String, bool>, CliError> {
    let mut result = HashMap::new();
    for value in values {
        let (id, mode) = value
            .split_once('=')
            .ok_or_else(|| CliError::other("subagent memory must use HARNESS=on|off"))?;
        let entry = REGISTRY
            .iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| CliError::other(format!("unknown harness: {id}")))?;
        if entry.subagent_default.is_none() {
            return Err(CliError::other(format!(
                "subagent memory is not supported for {id}"
            )));
        }
        let enabled = match mode {
            "on" => true,
            "off" => false,
            _ => {
                return Err(CliError::other(format!(
                    "invalid subagent memory for {id}: {mode}"
                )));
            }
        };
        if result.insert(id.to_string(), enabled).is_some() {
            return Err(CliError::other(format!(
                "duplicate subagent memory for {id}"
            )));
        }
    }
    Ok(result)
}

fn checkout_resource(executable: &Path, relative: &str) -> Option<PathBuf> {
    executable
        .ancestors()
        .map(|ancestor| ancestor.join(relative))
        .find(|path| path.is_dir())
}

fn input_path(prompt: &str) -> Result<Option<PathBuf>, CliError> {
    let value: String = Input::new()
        .with_prompt(prompt)
        .allow_empty(true)
        .interact_text()
        .map_err(prompt_error)?;
    Ok((!value.trim().is_empty()).then(|| PathBuf::from(value.trim())))
}

fn resolve_path(
    explicit: &Option<PathBuf>,
    variable: &str,
    executable: &Path,
    relative: &str,
    interactive: bool,
    prompt: &str,
) -> Result<Option<PathBuf>, CliError> {
    let path = explicit
        .clone()
        .or_else(|| env::var_os(variable).map(PathBuf::from))
        .or_else(|| checkout_resource(executable, relative));
    let path = if path.is_none() && interactive {
        input_path(prompt)?
    } else {
        path
    };
    match path {
        Some(path) if !path.is_dir() => Err(CliError::other(format!(
            "directory does not exist: {}",
            path.display()
        ))),
        Some(path) => path.canonicalize().map(Some).map_err(|error| {
            CliError::other(format!("cannot resolve {}: {error}", path.display()))
        }),
        None => Ok(None),
    }
}

pub fn run(cli: &Cli, args: &SetupArgs) -> Result<(), CliError> {
    let terminal = io::stdin().is_terminal() && io::stderr().is_terminal();
    if !terminal && !args.yes {
        return Err(CliError::other(
            "non-interactive setup requires --yes; specify --harness, --scope, [--project], [--subagent-memory], [--skills|--no-skills], [--skills-source], [--opencode-plugin], and [--dry-run]",
        ));
    }
    let interactive = terminal && !args.yes;
    let overrides = parse_memory(&args.subagent_memory)?;
    let health = endpoint(&cli.url, &["health"])?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(cli.timeout_secs.min(3)))
        .build()
        .map_err(|error| CliError::other(format!("cannot create health client: {error}")))?;
    let mut request = client.get(health);
    if let Some(token) = &cli.token {
        request = request.bearer_auth(token);
    }
    if !request
        .send()
        .is_ok_and(|response| response.status().is_success())
    {
        eprintln!(
            "warning: daemon health check failed; start it with docker compose -f deploy/compose.yaml up -d"
        );
    }
    let scope = match args.scope {
        Some(scope) => scope,
        None if interactive => {
            if Select::new()
                .with_prompt("Installation scope")
                .items(["This project", "User-wide"])
                .default(0)
                .interact()
                .map_err(prompt_error)?
                == 0
            {
                Scope::Project
            } else {
                Scope::User
            }
        }
        None => return Err(CliError::other("--scope is required with --yes")),
    };
    if scope == Scope::User && args.project.is_some() {
        return Err(CliError::other("--project cannot be used with user scope"));
    }
    let project = args.project.clone().unwrap_or(
        env::current_dir()
            .map_err(|error| CliError::other(format!("cannot get current directory: {error}")))?,
    );
    let home = if scope == Scope::User {
        env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| CliError::other("HOME is required for user scope"))?
    } else {
        PathBuf::new()
    };
    let xdg = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let executable = env::current_exe()
        .map_err(|error| CliError::other(format!("cannot determine executable: {error}")))?;
    let selected: Vec<&HarnessEntry> = if !args.harness.is_empty() {
        let mut selected = Vec::new();
        for id in &args.harness {
            let entry = REGISTRY
                .iter()
                .find(|entry| entry.id == id)
                .ok_or_else(|| CliError::other(format!("unknown harness: {id}")))?;
            if selected
                .iter()
                .any(|selected: &&HarnessEntry| selected.id == entry.id)
            {
                return Err(CliError::other(format!("duplicate harness: {id}")));
            }
            selected.push(entry);
        }
        selected
    } else if interactive {
        let labels: Vec<String> = REGISTRY
            .iter()
            .map(|entry| {
                format!(
                    "{}{}",
                    entry.display,
                    if detected(entry.executable) {
                        " (detected)"
                    } else {
                        ""
                    }
                )
            })
            .collect();
        let defaults: Vec<bool> = REGISTRY
            .iter()
            .map(|entry| detected(entry.executable))
            .collect();
        MultiSelect::new()
            .with_prompt("Harnesses")
            .items(&labels)
            .defaults(&defaults)
            .interact()
            .map_err(prompt_error)?
            .into_iter()
            .map(|index| &REGISTRY[index])
            .collect()
    } else {
        return Err(CliError::other("--harness is required with --yes"));
    };
    if selected.is_empty() {
        return Err(CliError::other("select at least one harness"));
    }
    for id in overrides.keys() {
        if !selected.iter().any(|entry| entry.id == id) {
            return Err(CliError::other(format!(
                "subagent memory specified for unselected harness: {id}"
            )));
        }
    }
    let mut modes = HashMap::new();
    for entry in &selected {
        if !entry.scopes.contains(&scope) {
            return Err(CliError::other(format!(
                "{} does not support this scope",
                entry.id
            )));
        }
        if let Some(default) = entry.subagent_default {
            let enabled = if let Some(enabled) = overrides.get(entry.id) {
                *enabled
            } else if interactive {
                Confirm::new()
                    .with_prompt(format!(
                        "Give {} subagents parent session memory?",
                        entry.display
                    ))
                    .default(default)
                    .interact()
                    .map_err(prompt_error)?
            } else {
                default
            };
            modes.insert(entry.id, enabled);
        }
    }
    let install_skills = if args.skills {
        true
    } else if args.no_skills {
        false
    } else if interactive {
        Confirm::new()
            .with_prompt("Install memento skills?")
            .default(true)
            .interact()
            .map_err(prompt_error)?
    } else {
        true
    };
    let skills_source = if install_skills {
        resolve_path(
            &args.skills_source,
            "MEMENTO_SKILLS_DIR",
            &executable,
            "skills",
            interactive,
            "Skills source directory (leave empty to skip)",
        )?
    } else {
        None
    };
    if install_skills && skills_source.is_none() {
        eprintln!("warning: skills source unavailable; skipping skills (use --skills-source DIR)");
    }
    let plugin = if selected.iter().any(|entry| entry.id == "opencode") {
        let plugin = resolve_path(
            &args.opencode_plugin,
            "MEMENTO_OPENCODE_PLUGIN",
            &executable,
            "adapters/opencode",
            interactive,
            "OpenCode plugin directory",
        )?
        .ok_or_else(|| {
            CliError::other("OpenCode plugin directory required: use --opencode-plugin DIR")
        })?;
        if !plugin.join("node_modules").is_dir() {
            eprintln!(
                "warning: {} has no node_modules; run bun install there",
                plugin.display()
            );
        }
        Some(plugin)
    } else {
        None
    };
    let context = Context {
        scope,
        project,
        home,
        xdg,
        executable,
        plugin,
        skills_source,
        url: cli.url.clone(),
    };
    println!("Planned changes:");
    for entry in &selected {
        let memory = modes.get(entry.id).copied().unwrap_or(false);
        println!("  {}: {}", entry.display, memory_summary(entry, memory));
        let change = (entry.plan)(entry, &context, memory)?;
        println!(
            "  {}: {} - {}",
            entry.display,
            change.path.display(),
            change.description
        );
        if let Some(source) = &context.skills_source {
            let skills = InstallSkillsArgs {
                harness: entry.skills,
                project: (scope == Scope::Project).then(|| context.project.clone()),
                user: scope == Scope::User,
                source: Some(source.clone()),
                dry_run: true,
            };
            cross_harness::install_skills(&skills, &context.home)?;
            println!(
                "  {}: {} - install skills from {}",
                entry.display,
                skill_path(entry, &context).display(),
                source.display()
            );
        }
    }
    if args.dry_run {
        println!("Dry run: no changes made.");
        return Ok(());
    }
    if interactive
        && !Confirm::new()
            .with_prompt("Apply these changes?")
            .default(true)
            .interact()
            .map_err(prompt_error)?
    {
        println!("Setup cancelled.");
        return Ok(());
    }
    for entry in &selected {
        (entry.apply)(
            entry,
            &context,
            modes.get(entry.id).copied().unwrap_or(false),
        )
        .map_err(|error| {
            CliError::other(format!("{} setup failed: {}", entry.display, error.message))
        })?;
        if let Some(source) = &context.skills_source {
            let skills = InstallSkillsArgs {
                harness: entry.skills,
                project: (scope == Scope::Project).then(|| context.project.clone()),
                user: scope == Scope::User,
                source: Some(source.clone()),
                dry_run: false,
            };
            cross_harness::install_skills(&skills, &context.home).map_err(|error| {
                CliError::other(format!(
                    "{} skills failed: {}",
                    entry.display, error.message
                ))
            })?;
        }
        println!("{}: setup complete", entry.display);
    }
    println!("Next: restart your harness; Codex may ask you to trust project hooks.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn context(root: &Path) -> Context {
        Context {
            scope: Scope::Project,
            project: root.join("project"),
            home: root.join("home"),
            xdg: root.join("xdg"),
            executable: root.join("bin/memento"),
            plugin: Some(root.join("adapters/opencode")),
            skills_source: None,
            url: "http://127.0.0.1:7377".into(),
        }
    }

    #[test]
    fn registry_defaults_and_scope_plans() {
        let temp = tempfile::tempdir().unwrap();
        let mut context = context(temp.path());
        assert_eq!(
            REGISTRY
                .iter()
                .map(|entry| entry.subagent_default)
                .collect::<Vec<_>>(),
            vec![Some(true), Some(false), Some(true), None]
        );
        for scope in [Scope::Project, Scope::User] {
            context.scope = scope;
            for entry in REGISTRY {
                assert!(entry.scopes.contains(&scope));
                let change =
                    (entry.plan)(entry, &context, entry.subagent_default.unwrap_or(false)).unwrap();
                assert_eq!(change.path, config_path(entry, &context));
            }
        }
        assert!(!context.home.exists());
        assert!(!context.project.exists());
    }

    #[test]
    fn summary_describes_each_subagent_decision() {
        assert_eq!(memory_summary(&REGISTRY[0], true), "subagentMemory digest");
        assert_eq!(memory_summary(&REGISTRY[0], false), "subagentMemory off");
        assert_eq!(memory_summary(&REGISTRY[1], false), "subagent memory off");
        assert_eq!(
            memory_summary(&REGISTRY[2], true),
            "subagent memory on (SubagentStart --subagent-memory digest)"
        );
        assert_eq!(
            memory_summary(&REGISTRY[3], false),
            "subagent memory not supported"
        );
    }

    #[test]
    fn flag_parsing_and_invalid_memory() {
        let cli = Cli::try_parse_from([
            "memento",
            "setup",
            "--harness",
            "opencode,codex",
            "--scope",
            "project",
            "--subagent-memory",
            "opencode=on,codex=off",
            "--no-skills",
            "--yes",
            "--dry-run",
        ])
        .unwrap();
        let crate::Command::Setup(args) = cli.command else {
            panic!("expected setup")
        };
        assert_eq!(args.harness, ["opencode", "codex"]);
        assert!(!parse_memory(&args.subagent_memory).unwrap()["codex"]);
        assert!(parse_memory(&["agy=off".into()]).is_err());
        assert!(parse_memory(&["codex=invalid".into()]).is_err());
    }

    #[test]
    fn non_tty_without_yes_fails() {
        let cli = Cli::try_parse_from(["memento", "setup"]).unwrap();
        let crate::Command::Setup(ref args) = cli.command else {
            panic!("expected setup")
        };
        if !io::stdin().is_terminal() {
            assert!(run(&cli, args).unwrap_err().message.contains("--yes"));
        }
    }

    #[test]
    fn opencode_merge_preserves_other_settings_and_jsonc() {
        let temp = tempfile::tempdir().unwrap();
        let context = context(temp.path());
        fs::create_dir_all(&context.project).unwrap();
        let path = context.project.join("opencode.jsonc");
        let initial: Value =
            serde_json::from_str(&opencode_merge(&path, &context, true).unwrap().unwrap()).unwrap();
        assert_eq!(initial["plugins"][0]["options"]["subagentMemory"], "digest");
        fs::write(&path, r#"{"other":42,"plugins":[{"package":"foreign"}]}"#).unwrap();
        let merged: Value =
            serde_json::from_str(&opencode_merge(&path, &context, false).unwrap().unwrap())
                .unwrap();
        assert_eq!(merged["other"], 42);
        assert_eq!(merged["plugins"][0]["package"], "foreign");
        assert_eq!(merged["plugins"][1]["options"]["subagentMemory"], "off");
        fs::write(&path, r#"{"plugins":[{"package":"/old/adapters/opencode","options":{"jev":false,"url":"old"}}]}"#).unwrap();
        let updated: Value =
            serde_json::from_str(&opencode_merge(&path, &context, true).unwrap().unwrap()).unwrap();
        assert_eq!(updated["plugins"].as_array().unwrap().len(), 1);
        assert_eq!(updated["plugins"][0]["options"]["jev"], false);
        assert_eq!(updated["plugins"][0]["options"]["url"], context.url);
        let jsonc = "{\n // keep this\n \"plugins\": [],\n}";
        fs::write(&path, jsonc).unwrap();
        assert!(opencode_merge(&path, &context, true).unwrap().is_none());
        apply_opencode(&REGISTRY[0], &context, true).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), jsonc);
    }

    #[test]
    fn dry_run_does_not_write() {
        let temp = tempfile::tempdir().unwrap();
        let context = context(temp.path());
        for entry in REGISTRY {
            (entry.plan)(entry, &context, entry.subagent_default.unwrap_or(false)).unwrap();
        }
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }
}
