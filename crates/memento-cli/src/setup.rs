use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, ValueEnum};
use dialoguer::{Confirm, Input, MultiSelect, Select};
use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstRootNode};
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
    #[arg(long, help = "Start the managed daemon if it is unavailable")]
    start_daemon: bool,
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
    install_skills: bool,
    embedded_plugin: bool,
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
    if entry.id == "opencode" {
        return opencode_config_path(context.scope == Scope::User, &context.project, &context.xdg);
    }
    base(context).join(if context.scope == Scope::User {
        entry.user_config
    } else {
        entry.project_config
    })
}

pub(super) fn opencode_config_path(user: bool, project: &Path, xdg: &Path) -> PathBuf {
    if user {
        let directory = xdg.join("opencode");
        let jsonc = directory.join("opencode.jsonc");
        let json = directory.join("opencode.json");
        if !jsonc.is_file() && json.is_file() {
            json
        } else {
            jsonc
        }
    } else {
        for name in [
            "opencode.json",
            "opencode.jsonc",
            ".opencode/opencode.json",
            ".opencode/opencode.jsonc",
        ] {
            let path = project.join(name);
            if path.is_file() {
                return path;
            }
        }
        project.join("opencode.jsonc")
    }
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

fn opencode_merge(path: &Path, context: &Context, memory: bool) -> Result<String, CliError> {
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
    let root = CstRootNode::parse(&original, &ParseOptions::default()).map_err(|error| {
        let snippet = plugin_entry(context, memory)
            .and_then(|entry| {
                serde_json::to_string_pretty(&entry).map_err(|error| {
                    CliError::other(format!("cannot serialize plugin entry: {error}"))
                })
            })
            .unwrap_or_else(|error| error.message);
        CliError::other(format!(
            "cannot parse OpenCode config {}: {error}\nAdd this entry to the plugins array after fixing the config:\n{snippet}",
            path.display()
        ))
    })?;
    let object = root.object_value().ok_or_else(|| {
        CliError::other(format!(
            "OpenCode config {} must be an object",
            path.display()
        ))
    })?;
    let plugins = object.array_value_or_create("plugins").ok_or_else(|| {
        CliError::other(format!(
            "OpenCode plugins in {} must be an array",
            path.display()
        ))
    })?;
    let new_entry = plugin_entry(context, memory)?;
    let planned_package = new_entry["package"].as_str().expect("plugin package");
    if let Some(existing) = plugins.elements().into_iter().find_map(|entry| {
        let object = entry.as_object()?;
        let value = object.to_serde_value()?;
        let matched = value
            .get("package")
            .and_then(Value::as_str)
            .is_some_and(|package| {
                let package = package.trim_end_matches('/');
                package.ends_with("/adapters/opencode")
                    || package == planned_package.trim_end_matches('/')
                    || package.ends_with("/memento/opencode-plugin")
            })
            || value
                .get("options")
                .and_then(|options| options.get("memento"))
                .and_then(Value::as_bool)
                == Some(true);
        matched.then_some(object)
    }) {
        let options = existing
            .object_value_or_create("options")
            .ok_or_else(|| CliError::other("memento plugin options must be an object"))?;
        for (object, name, value) in [
            (&existing, "package", planned_package),
            (&options, "url", context.url.as_str()),
            (
                &options,
                "subagentMemory",
                if memory { "digest" } else { "off" },
            ),
        ] {
            if let Some(property) = object.get(name) {
                property.set_value(value.into());
            } else {
                object.append(name, value.into());
            }
        }
    } else {
        let subagent_memory = if memory { "digest" } else { "off" };
        let url = context.url.as_str();
        plugins.append(CstInputValue::Object(vec![
            ("package".into(), planned_package.into()),
            (
                "options".into(),
                CstInputValue::Object(vec![
                    ("url".into(), url.into()),
                    ("subagentMemory".into(), subagent_memory.into()),
                ]),
            ),
        ]));
    }
    Ok(root.to_string())
}

fn plan_opencode(
    entry: &HarnessEntry,
    context: &Context,
    memory: bool,
) -> Result<Change, CliError> {
    let path = config_path(entry, context);
    opencode_merge(&path, context, memory)?;
    Ok(Change {
        path,
        description: "merge OpenCode plugin".into(),
    })
}

fn apply_opencode(entry: &HarnessEntry, context: &Context, memory: bool) -> Result<(), CliError> {
    let path = config_path(entry, context);
    let output = opencode_merge(&path, context, memory)?;
    let parent = path
        .parent()
        .ok_or_else(|| CliError::other("OpenCode config has no parent"))?;
    fs::create_dir_all(parent)
        .map_err(|error| CliError::other(format!("cannot create {}: {error}", parent.display())))?;
    cross_harness::save_atomic(&path, output.as_bytes())?;
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
    let daemon_healthy = request
        .send()
        .is_ok_and(|response| response.status().is_success());
    let mut managed_daemon = false;
    if !daemon_healthy {
        let should_start = args.start_daemon
            || (interactive
                && crate::daemon::docker_available()
                && Confirm::new()
                    .with_prompt("Daemon is unavailable. Start the managed daemon?")
                    .default(true)
                    .interact()
                    .map_err(prompt_error)?);
        if should_start {
            crate::daemon::start(cli)?;
            managed_daemon = true;
        } else {
            eprintln!("warning: daemon health check failed; start it with memento daemon up");
        }
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
    let skill_override = args
        .skills_source
        .clone()
        .or_else(|| env::var_os("MEMENTO_SKILLS_DIR").map(PathBuf::from));
    let skills_source = skill_override
        .map(|path| {
            if !path.is_dir() {
                return Err(CliError::other(format!(
                    "skills source directory does not exist: {}",
                    path.display()
                )));
            }
            path.canonicalize().map_err(|error| {
                CliError::other(format!(
                    "cannot resolve skills source {}: {error}",
                    path.display()
                ))
            })
        })
        .transpose()?;
    let mut embedded_plugin = false;
    let plugin = if selected.iter().any(|entry| entry.id == "opencode") {
        let override_plugin = args
            .opencode_plugin
            .clone()
            .or_else(|| env::var_os("MEMENTO_OPENCODE_PLUGIN").map(PathBuf::from));
        let plugin = if let Some(plugin) = override_plugin {
            resolve_path(
                &Some(plugin),
                "MEMENTO_OPENCODE_PLUGIN",
                &executable,
                "adapters/opencode",
                false,
                "OpenCode plugin directory",
            )?
        } else if crate::assets::OPENCODE_BUNDLE_AVAILABLE {
            embedded_plugin = true;
            Some(crate::assets::data_dir()?.join("opencode-plugin"))
        } else {
            resolve_path(
                &None,
                "MEMENTO_OPENCODE_PLUGIN",
                &executable,
                "adapters/opencode",
                interactive,
                "OpenCode plugin directory",
            )?
        }
        .ok_or_else(|| {
            CliError::other("OpenCode plugin directory required: use --opencode-plugin DIR")
        })?;
        let embedded_default = crate::assets::OPENCODE_BUNDLE_AVAILABLE
            && args.opencode_plugin.is_none()
            && env::var_os("MEMENTO_OPENCODE_PLUGIN").is_none();
        if !embedded_default
            && plugin.join("src/index.ts").is_file()
            && !plugin.join("node_modules").is_dir()
        {
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
        install_skills,
        embedded_plugin,
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
        if context.install_skills {
            let skills = InstallSkillsArgs {
                harness: entry.skills,
                project: (scope == Scope::Project).then(|| context.project.clone()),
                user: scope == Scope::User,
                source: context.skills_source.clone(),
                dry_run: true,
            };
            cross_harness::install_skills(&skills, &context.home)?;
            println!(
                "  {}: {} - install {} skills",
                entry.display,
                skill_path(entry, &context).display(),
                if context.skills_source.is_some() {
                    "override"
                } else {
                    "embedded"
                }
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
    if context.embedded_plugin {
        crate::assets::write_opencode_plugin(&crate::assets::data_dir()?)?;
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
        if context.install_skills {
            let skills = InstallSkillsArgs {
                harness: entry.skills,
                project: (scope == Scope::Project).then(|| context.project.clone()),
                user: scope == Scope::User,
                source: context.skills_source.clone(),
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
    let setup_receipt = json!({
        "harnesses": selected.iter().map(|entry| entry.id).collect::<Vec<_>>(),
        "scope": if scope == Scope::Project { "project" } else { "user" },
        "project": if scope == Scope::Project {
            Some(context.project.canonicalize().map_err(|error| CliError::other(format!("cannot canonicalize project {}: {error}", context.project.display())))?.display().to_string())
        } else {
            None
        },
        "subagent_memory": modes,
        "skills": install_skills,
        "managed_daemon": managed_daemon,
        "opencode_plugin": if context.embedded_plugin {
            None
        } else {
            context.plugin.as_ref().map(|path| path.display().to_string())
        }
    });
    crate::receipt::merge_setup(setup_receipt)
        .map_err(|error| CliError::other(format!("cannot update install receipt: {error}")))?;
    println!("Next: restart your harness; Codex may ask you to trust project hooks.");
    Ok(())
}

pub(super) fn reapply_saved_setup(cli: &Cli) -> Result<(), CliError> {
    let receipt = crate::receipt::read()
        .map_err(|error| CliError::other(format!("cannot read install receipt: {error}")))?;
    let setup = receipt
        .get("setup")
        .and_then(Value::as_object)
        .ok_or_else(|| CliError::other("install receipt has no saved setup"))?;
    let harnesses = setup
        .get("harnesses")
        .and_then(Value::as_array)
        .ok_or_else(|| CliError::other("saved setup has no harness list"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| CliError::other("saved harness id is not a string"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let scope = match setup.get("scope").and_then(Value::as_str) {
        Some("project") => Scope::Project,
        Some("user") => Scope::User,
        _ => return Err(CliError::other("saved setup has invalid scope")),
    };
    let subagent_memory = setup
        .get("subagent_memory")
        .and_then(Value::as_object)
        .map(|modes| {
            modes
                .iter()
                .filter_map(|(id, enabled)| {
                    enabled
                        .as_bool()
                        .map(|enabled| format!("{id}={}", if enabled { "on" } else { "off" }))
                })
                .collect()
        })
        .unwrap_or_default();
    let skills = setup.get("skills").and_then(Value::as_bool).unwrap_or(true);
    let plugin = setup
        .get("opencode_plugin")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let args = SetupArgs {
        harness: harnesses,
        scope: Some(scope),
        project: setup
            .get("project")
            .and_then(Value::as_str)
            .map(PathBuf::from),
        subagent_memory,
        skills,
        no_skills: !skills,
        skills_source: None,
        opencode_plugin: plugin,
        yes: true,
        start_daemon: false,
        dry_run: false,
    };
    run(cli, &args)
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
            install_skills: true,
            embedded_plugin: false,
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
            serde_json::from_str(&opencode_merge(&path, &context, true).unwrap()).unwrap();
        assert_eq!(initial["plugins"][0]["options"]["subagentMemory"], "digest");
        fs::write(&path, r#"{"other":42,"plugins":[{"package":"foreign"}]}"#).unwrap();
        let merged: Value =
            serde_json::from_str(&opencode_merge(&path, &context, false).unwrap()).unwrap();
        assert_eq!(merged["other"], 42);
        assert_eq!(merged["plugins"][0]["package"], "foreign");
        assert_eq!(merged["plugins"][1]["options"]["subagentMemory"], "off");
        fs::write(&path, r#"{"plugins":[{"package":"/old/adapters/opencode","options":{"jev":false,"url":"old"}}]}"#).unwrap();
        let updated: Value =
            serde_json::from_str(&opencode_merge(&path, &context, true).unwrap()).unwrap();
        assert_eq!(updated["plugins"].as_array().unwrap().len(), 1);
        assert_eq!(updated["plugins"][0]["options"]["jev"], false);
        assert_eq!(updated["plugins"][0]["options"]["url"], context.url);
        let jsonc = "{\n // keep this\n \"plugins\": [],\n}";
        fs::write(&path, jsonc).unwrap();
        apply_opencode(&REGISTRY[0], &context, true).unwrap();
        let merged = fs::read_to_string(&path).unwrap();
        assert!(merged.contains("// keep this"));
        let root = CstRootNode::parse(&merged, &ParseOptions::default()).unwrap();
        assert_eq!(
            root.to_serde_value().unwrap()["plugins"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn opencode_jsonc_update_is_idempotent_and_keeps_comments() {
        let temp = tempfile::tempdir().unwrap();
        let mut context = context(temp.path());
        context.plugin = Some(temp.path().join("memento/opencode-plugin"));
        fs::create_dir_all(&context.project).unwrap();
        let path = context.project.join("opencode.jsonc");
        fs::write(
            &path,
            format!(
                "{{\n  // root comment\n  \"plugins\": [{{\n    \"package\": \"{}/\",\n    \"options\": {{\n      // option comment\n      \"jev\": false,\n      \"url\": \"old\",\n    }},\n  }},],\n}}",
                context.plugin.as_ref().unwrap().display()
            ),
        )
        .unwrap();
        apply_opencode(&REGISTRY[0], &context, true).unwrap();
        apply_opencode(&REGISTRY[0], &context, false).unwrap();
        let merged = fs::read_to_string(&path).unwrap();
        assert!(merged.contains("// root comment"));
        assert!(merged.contains("// option comment"));
        let root = CstRootNode::parse(&merged, &ParseOptions::default()).unwrap();
        let value = root.to_serde_value().unwrap();
        assert_eq!(value["plugins"].as_array().unwrap().len(), 1);
        assert_eq!(value["plugins"][0]["options"]["jev"], false);
        assert_eq!(value["plugins"][0]["options"]["subagentMemory"], "off");
    }

    #[test]
    fn invalid_opencode_config_fails_plan_and_apply_with_snippet() {
        let temp = tempfile::tempdir().unwrap();
        let context = context(temp.path());
        fs::create_dir_all(&context.project).unwrap();
        let path = context.project.join("opencode.jsonc");
        fs::write(&path, "{\"plugins\": [invalid]}").unwrap();
        for error in [
            plan_opencode(&REGISTRY[0], &context, true).err().unwrap(),
            apply_opencode(&REGISTRY[0], &context, true).unwrap_err(),
        ] {
            assert!(error.message.contains(path.to_str().unwrap()));
            assert!(error.message.contains("Add this entry"));
            assert!(error.message.contains("\"package\""));
        }
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "{\"plugins\": [invalid]}"
        );
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
