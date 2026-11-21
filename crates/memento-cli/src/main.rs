use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use memento_core::lineage::SessionKey;
use memento_core::protocol::{
    DigestResponse, ErrorBody, ForgetRequest, ForgetResponse, ForgetScope, Health, IngestItem,
    IngestRequest, Kind, Retention, Role, Scope, SearchRequest, SearchResponse, SessionRecord,
};
use memento_core::reference::{format_continuation, format_digest, format_search};
use reqwest::blocking::{Client, Request};
use reqwest::{Method, Url};
use serde::Deserialize;
use serde_json::{Value, json};

mod assets;
mod cross_harness;
mod daemon;
mod doctor;
mod receipt;
mod setup;
mod update;

#[derive(Debug, Parser)]
#[command(
    name = "memento",
    version,
    about = "Client for the mementod memory daemon"
)]
struct Cli {
    #[arg(
        long,
        env = "MEMENTO_URL",
        default_value = "http://127.0.0.1:7377",
        global = true,
        help = "Daemon base URL"
    )]
    url: String,
    #[arg(
        long,
        env = "MEMENTO_TOKEN",
        global = true,
        hide_env_values = true,
        help = "Daemon bearer token"
    )]
    token: Option<String>,
    #[arg(long, global = true, help = "Print raw JSON response")]
    json: bool,
    #[arg(
        long,
        default_value_t = 30,
        global = true,
        help = "Request timeout in seconds"
    )]
    timeout_secs: u64,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Set up harnesses interactively")]
    Setup(setup::SetupArgs),
    #[command(about = "Check daemon health")]
    Health,
    #[command(about = "Show memory counts by kind")]
    Stats(StatsArgs),
    #[command(about = "Manage sessions and lineage")]
    Sessions {
        #[command(subcommand)]
        command: SessionsCommand,
    },
    #[command(about = "Search stored memories")]
    Search(SearchArgs),
    #[command(about = "Show a session digest")]
    Digest(DigestArgs),
    #[command(about = "Store an explicit memory note")]
    Note(NoteArgs),
    #[command(about = "Delete matching memories")]
    Forget(ForgetArgs),
    #[command(about = "Release memories for deleted harness sessions")]
    Gc(cross_harness::GcArgs),
    #[command(about = "Ingest transcript items")]
    Ingest(IngestArgs),
    #[command(about = "Print the canonical project key")]
    ProjectKey {
        #[arg(help = "Project directory")]
        dir: Option<PathBuf>,
    },
    #[command(about = "Import a harness transcript")]
    Import(cross_harness::ImportArgs),
    #[command(hide = true, about = "Import a pending hook transcript")]
    HookImport(cross_harness::ImportArgs),
    #[command(about = "Prepare a cross-harness continuation")]
    Continue(cross_harness::ContinueArgs),
    #[command(about = "Handle a harness lifecycle event")]
    Hook(cross_harness::HookArgs),
    #[command(about = "Install harness lifecycle hooks")]
    InstallHooks(cross_harness::InstallArgs),
    #[command(about = "Install memento skills for harnesses")]
    InstallSkills(cross_harness::InstallSkillsArgs),
    #[command(about = "Manage the bundled daemon")]
    Daemon(daemon::DaemonArgs),
    #[command(hide = true, about = "Reapply saved setup choices")]
    ReapplySetup,
    #[command(about = "Update memento and refresh saved components")]
    Update(update::UpdateArgs),
    #[command(about = "Check installation and daemon status")]
    Doctor(doctor::DoctorArgs),
    #[command(hide = true, about = "Write installation receipt fields")]
    WriteReceipt(doctor::WriteReceiptArgs),
}

#[derive(Debug, Args)]
struct StatsArgs {
    #[arg(long, help = "Harness identifier")]
    harness: Option<String>,
    #[arg(
        long,
        help = "Project directory or identifier (default: current project)"
    )]
    project: Option<String>,
    #[arg(long, help = "Filter by session")]
    session: Option<String>,
}

#[derive(Debug, Subcommand)]
enum SessionsCommand {
    #[command(about = "List registered sessions")]
    List {
        #[arg(long, help = "Harness identifier")]
        harness: String,
        #[arg(long, help = "Filter by project directory or identifier")]
        project: Option<String>,
    },
    #[command(about = "Link a child session to a parent")]
    Link {
        #[arg(long, help = "Harness identifier")]
        harness: String,
        #[arg(long, help = "Project directory or identifier")]
        project: String,
        #[arg(long, help = "Child session identifier")]
        child: String,
        #[arg(long, help = "Parent session identifier")]
        parent: String,
        #[arg(long, help = "Parent harness identifier")]
        parent_harness: Option<String>,
        #[arg(long, help = "Fork cutoff timestamp (RFC 3339)")]
        parent_cutoff: Option<chrono::DateTime<chrono::Utc>>,
        #[arg(long, help = "Child session title")]
        title: Option<String>,
        #[arg(long, value_enum, help = "Child session retention mode")]
        retention: Option<RetentionArg>,
    },
    #[command(about = "Set session retention")]
    Keep {
        #[arg(long, help = "Harness identifier")]
        harness: String,
        #[arg(long, help = "Session identifier")]
        session: String,
        #[arg(
            long,
            help = "Project directory or identifier (default: current project)"
        )]
        project: Option<String>,
        #[arg(long, help = "Restore continuation retention")]
        off: bool,
    },
    #[command(about = "Show session ancestors")]
    Lineage {
        #[arg(long, help = "Harness identifier")]
        harness: String,
        #[arg(long, help = "Session identifier")]
        session: String,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RetentionArg {
    Continuation,
    Keep,
}

impl From<RetentionArg> for Retention {
    fn from(value: RetentionArg) -> Self {
        match value {
            RetentionArg::Continuation => Self::Continuation,
            RetentionArg::Keep => Self::Keep,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ScopeArg {
    Session,
    Lineage,
    Project,
    Global,
}

impl From<ScopeArg> for Scope {
    fn from(value: ScopeArg) -> Self {
        match value {
            ScopeArg::Session => Self::Session,
            ScopeArg::Lineage => Self::Lineage,
            ScopeArg::Project => Self::Project,
            ScopeArg::Global => Self::Global,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "snake_case")]
enum KindArg {
    UserRequest,
    AssistantNote,
    Decision,
    Constraint,
    Task,
    FileChange,
    Command,
    FileRef,
}

impl From<KindArg> for Kind {
    fn from(value: KindArg) -> Self {
        match value {
            KindArg::UserRequest => Self::UserRequest,
            KindArg::AssistantNote => Self::AssistantNote,
            KindArg::Decision => Self::Decision,
            KindArg::Constraint => Self::Constraint,
            KindArg::Task => Self::Task,
            KindArg::FileChange => Self::FileChange,
            KindArg::Command => Self::Command,
            KindArg::FileRef => Self::FileRef,
        }
    }
}

#[derive(Debug, Args)]
struct SearchArgs {
    #[arg(help = "Search query")]
    query: String,
    #[arg(long, help = "Harness identifier")]
    harness: Option<String>,
    #[arg(
        long,
        help = "Project directory or identifier (default: current project)"
    )]
    project: Option<String>,
    #[arg(long, help = "Session identifier")]
    session: Option<String>,
    #[arg(
        long,
        value_enum,
        help = "Search scope (default: project, or lineage with --session)"
    )]
    scope: Option<ScopeArg>,
    #[arg(long, default_value_t = 8, help = "Maximum number of results")]
    k: usize,
    #[arg(long, value_enum, help = "Filter by memory kind")]
    kind: Vec<KindArg>,
    #[arg(long, conflicts_with = "json", help = "Print framed reference data")]
    framed: bool,
}

#[derive(Debug, Args)]
struct DigestArgs {
    #[arg(long, requires = "session", help = "Harness identifier")]
    harness: Option<String>,
    #[arg(
        long,
        help = "Project directory or identifier (default: current project)"
    )]
    project: Option<String>,
    #[arg(long, requires = "harness", help = "Session identifier")]
    session: Option<String>,
    #[arg(long, help = "Digest character budget")]
    budget: Option<usize>,
    #[arg(long, value_parser = ["default", "subagent"], help = "Digest profile")]
    profile: Option<String>,
    #[arg(long, conflicts_with_all = ["json", "continuation"], help = "Print framed reference data")]
    framed: bool,
    #[arg(long, requires_all = ["harness", "session"], conflicts_with_all = ["json", "framed"], help = "Print framed parent continuation")]
    continuation: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum NoteKind {
    Decision,
    Constraint,
    Task,
    Note,
}

#[derive(Debug, Args)]
struct NoteArgs {
    #[arg(long, value_enum, help = "Memory kind")]
    kind: NoteKind,
    #[arg(long, help = "Harness identifier")]
    harness: String,
    #[arg(long, help = "Session identifier (default: <harness>-notes)")]
    session: Option<String>,
    #[arg(
        long,
        help = "Project directory or identifier (default: current project)"
    )]
    project: Option<String>,
    #[arg(help = "Note text")]
    text: String,
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("target").required(true).args(["session", "lineage", "whole_project"])))]
struct ForgetArgs {
    #[arg(long, help = "Harness identifier")]
    harness: String,
    #[arg(long, help = "Project directory or identifier")]
    project: Option<String>,
    #[arg(long, help = "Forget one session")]
    session: Option<String>,
    #[arg(long, help = "Forget a session and its descendants")]
    lineage: Option<String>,
    #[arg(
        long,
        requires = "project",
        help = "Forget all memories in the project"
    )]
    whole_project: bool,
    #[arg(long, help = "Skip confirmation")]
    yes: bool,
    #[arg(long, help = "Report what would be forgotten without deleting")]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct IngestArgs {
    #[arg(long, help = "Ingest JSON file, or - for stdin")]
    file: PathBuf,
}

#[derive(Debug, Deserialize)]
struct StatsResponse {
    total: u64,
    by_kind: std::collections::BTreeMap<String, u64>,
}

#[derive(Debug, Deserialize)]
struct IngestResponse {
    segments: usize,
    skipped: usize,
    duplicates: Option<usize>,
}

impl IngestResponse {
    fn render(&self) -> String {
        let mut output = format!("segments: {}  skipped: {}", self.segments, self.skipped);
        if let Some(duplicates) = self.duplicates {
            output.push_str(&format!("  duplicates: {duplicates}"));
        }
        output
    }
}

#[derive(Debug)]
struct CliError {
    message: String,
    exit_code: u8,
}

impl CliError {
    fn other(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: 1,
        }
    }

    fn request(error: reqwest::Error, url: &str) -> Self {
        if error.is_connect() {
            Self {
                message: format!(
                    "cannot connect to {url}: {error}; start the daemon with memento daemon up"
                ),
                exit_code: 2,
            }
        } else {
            Self::other(format!("request to {url} failed: {error}"))
        }
    }
}

fn endpoint(base: &str, segments: &[&str]) -> Result<Url, CliError> {
    let mut url =
        Url::parse(base).map_err(|error| CliError::other(format!("invalid URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(CliError::other(
            "URL must be an HTTP(S) base URL without a query or fragment",
        ));
    }
    url.path_segments_mut()
        .map_err(|_| CliError::other("URL cannot be used as a base URL"))?
        .pop_if_empty()
        .extend(segments);
    Ok(url)
}

fn project_or_current(project: &Option<String>) -> Result<String, CliError> {
    match project {
        Some(project) => cross_harness::project_argument_key(std::path::Path::new(project)),
        None => cross_harness::project_key(None),
    }
}

fn search_scope(args: &SearchArgs) -> Scope {
    args.scope
        .map(Into::into)
        .unwrap_or(if args.session.is_some() {
            Scope::Lineage
        } else {
            Scope::Project
        })
}

fn validate_search(args: &SearchArgs) -> Result<(), CliError> {
    if matches!(search_scope(args), Scope::Session | Scope::Lineage) {
        if args.harness.as_deref().is_none_or(str::is_empty) {
            return Err(CliError::other(
                "--harness is required for session or lineage search",
            ));
        }
        if args.session.as_deref().is_none_or(str::is_empty) {
            return Err(CliError::other(
                "--session is required for session or lineage search",
            ));
        }
    }
    Ok(())
}

fn build_request(
    client: &Client,
    cli: &Cli,
    ingest: Option<&serde_json::Value>,
) -> Result<Request, CliError> {
    let (method, segments): (Method, Vec<&str>) = match &cli.command {
        Command::Health => (Method::GET, vec!["health"]),
        Command::Stats(_) => (Method::GET, vec!["v1", "stats"]),
        Command::Sessions { command } => match command {
            SessionsCommand::List { .. } => (Method::GET, vec!["v1", "sessions"]),
            SessionsCommand::Link { .. } => (Method::PUT, vec!["v1", "sessions"]),
            SessionsCommand::Keep { .. } => (Method::PUT, vec!["v1", "sessions"]),
            SessionsCommand::Lineage { harness, session } => (
                Method::GET,
                vec!["v1", "sessions", harness, session, "lineage"],
            ),
        },
        Command::Search(_) => (Method::POST, vec!["v1", "search"]),
        Command::Digest(_) => (Method::GET, vec!["v1", "digest"]),
        Command::Forget(_) => (Method::POST, vec!["v1", "forget"]),
        Command::Ingest(_) => (Method::POST, vec!["v1", "ingest"]),
        Command::Note(_)
        | Command::Setup(_)
        | Command::ProjectKey { .. }
        | Command::Gc(_)
        | Command::Import(_)
        | Command::HookImport(_)
        | Command::Continue(_)
        | Command::Hook(_)
        | Command::InstallHooks(_)
        | Command::InstallSkills(_)
        | Command::Daemon(_)
        | Command::ReapplySetup
        | Command::Update(_)
        | Command::Doctor(_)
        | Command::WriteReceipt(_) => return Err(CliError::other("not a legacy request")),
    };
    let mut request = client.request(method, endpoint(&cli.url, &segments)?);
    if let Some(token) = &cli.token {
        request = request.bearer_auth(token);
    }
    request = match &cli.command {
        Command::Health
        | Command::Sessions {
            command: SessionsCommand::Lineage { .. },
        } => request,
        Command::Stats(args) => {
            let project = project_or_current(&args.project)?;
            let mut query = Vec::new();
            if let Some(harness) = &args.harness {
                query.push(("harness", harness.as_str()));
            }
            query.push(("project", project.as_str()));
            if let Some(session) = &args.session {
                query.push(("session", session));
            }
            request.query(&query)
        }
        Command::Sessions {
            command: SessionsCommand::List { harness, project },
        } => {
            let mut query = vec![("harness", harness.as_str())];
            let project = project
                .as_deref()
                .map(|project| cross_harness::project_argument_key(std::path::Path::new(project)))
                .transpose()?;
            if let Some(project) = &project {
                query.push(("project", project.as_str()));
            }
            request.query(&query)
        }
        Command::Sessions {
            command:
                SessionsCommand::Link {
                    harness,
                    project,
                    child,
                    parent,
                    parent_harness,
                    parent_cutoff,
                    title,
                    retention,
                },
        } => {
            let project = cross_harness::project_argument_key(std::path::Path::new(project))?;
            let mut body =
                json!({"harness": harness, "project": project, "session": child, "parent": parent});
            if let Some(parent_harness) = parent_harness {
                body["parent_harness"] = json!(parent_harness);
            }
            if let Some(parent_cutoff) = parent_cutoff {
                body["parent_cutoff"] = json!(parent_cutoff);
            }
            if let Some(title) = title {
                body["title"] = json!(title);
            }
            if let Some(retention) = retention {
                body["retention"] = json!(Retention::from(*retention));
            }
            request.json(&body)
        }
        Command::Sessions {
            command:
                SessionsCommand::Keep {
                    harness,
                    session,
                    project,
                    off,
                },
        } => request.json(&json!({
            "harness": harness,
            "project": project_or_current(project)?,
            "session": session,
            "retention": if *off { Retention::Continuation } else { Retention::Keep }
        })),
        Command::Search(args) => {
            validate_search(args)?;
            request.json(&SearchRequest {
                harness: args.harness.clone().unwrap_or_default(),
                project: project_or_current(&args.project)?,
                session: args.session.clone().unwrap_or_default(),
                query: args.query.clone(),
                scope: search_scope(args),
                k: args.k,
                kinds: (!args.kind.is_empty())
                    .then(|| args.kind.iter().copied().map(Into::into).collect()),
            })
        }
        Command::Digest(args) => {
            let mut query = vec![
                (
                    "harness",
                    args.harness
                        .clone()
                        .ok_or_else(|| CliError::other("missing digest harness"))?,
                ),
                ("project", project_or_current(&args.project)?),
                (
                    "session",
                    args.session
                        .clone()
                        .ok_or_else(|| CliError::other("missing digest session"))?,
                ),
            ];
            if let Some(budget) = args.budget {
                query.push(("budget", budget.to_string()));
            }
            if let Some(profile) = &args.profile {
                query.push(("profile", profile.clone()));
            }
            request.query(&query)
        }
        Command::Forget(args) => {
            let (session, scope) = if let Some(session) = &args.session {
                (Some(session.clone()), ForgetScope::Session)
            } else if let Some(session) = &args.lineage {
                (Some(session.clone()), ForgetScope::Lineage)
            } else {
                (None, ForgetScope::Project)
            };
            request.json(&ForgetRequest {
                harness: args.harness.clone(),
                project: args
                    .project
                    .as_deref()
                    .map(|project| {
                        cross_harness::project_argument_key(std::path::Path::new(project))
                    })
                    .transpose()?,
                session,
                scope,
                dry_run: args.dry_run,
            })
        }
        Command::Ingest(_) => {
            request.json(ingest.ok_or_else(|| CliError::other("missing ingest input"))?)
        }
        Command::Note(_)
        | Command::Setup(_)
        | Command::ProjectKey { .. }
        | Command::Gc(_)
        | Command::Import(_)
        | Command::HookImport(_)
        | Command::Continue(_)
        | Command::Hook(_)
        | Command::InstallHooks(_)
        | Command::InstallSkills(_)
        | Command::Daemon(_)
        | Command::ReapplySetup
        | Command::Update(_)
        | Command::Doctor(_)
        | Command::WriteReceipt(_) => return Err(CliError::other("not a legacy request")),
    };
    request
        .build()
        .map_err(|error| CliError::other(format!("cannot build request: {error}")))
}

fn confirm_forget(
    reader: &mut impl io::BufRead,
    is_tty: bool,
    yes: bool,
) -> Result<bool, CliError> {
    if yes {
        return Ok(true);
    }
    if !is_tty {
        return Err(CliError::other(
            "refusing to forget without --yes when stdin is not a TTY",
        ));
    }
    eprint!("Forget matching memories? [y/N] ");
    io::stderr()
        .flush()
        .map_err(|error| CliError::other(format!("cannot write confirmation prompt: {error}")))?;
    let mut answer = String::new();
    reader
        .read_line(&mut answer)
        .map_err(|error| CliError::other(format!("cannot read confirmation: {error}")))?;
    Ok(answer.trim().eq_ignore_ascii_case("y"))
}

fn response_error(status: reqwest::StatusCode, body: &str) -> CliError {
    match serde_json::from_str::<ErrorBody>(body) {
        Ok(error) => CliError::other(format!("{}: {}", error.error.code, error.error.message)),
        Err(_) => CliError::other(format!("HTTP {status}: {}", body.trim())),
    }
}

fn read_ingest(path: &PathBuf) -> Result<serde_json::Value, CliError> {
    let mut input = String::new();
    if path.as_os_str() == "-" {
        io::stdin()
            .read_to_string(&mut input)
            .map_err(|error| CliError::other(format!("cannot read ingest from stdin: {error}")))?;
    } else {
        input = fs::read_to_string(path).map_err(|error| {
            CliError::other(format!(
                "cannot read ingest file {}: {error}",
                path.display()
            ))
        })?;
    }
    let raw: serde_json::Value = serde_json::from_str(&input)
        .map_err(|error| CliError::other(format!("invalid ingest JSON: {error}")))?;
    serde_json::from_value::<IngestRequest>(raw.clone())
        .map_err(|error| CliError::other(format!("invalid ingest JSON: {error}")))?;
    Ok(raw)
}

fn parse_response<T: serde::de::DeserializeOwned>(body: &str) -> Result<T, CliError> {
    serde_json::from_str(body)
        .map_err(|error| CliError::other(format!("invalid daemon response: {error}")))
}

fn render(command: &Command, body: &str) -> Result<String, CliError> {
    match command {
        Command::Health => {
            let response: Health = parse_response(body)?;
            Ok(format!("ok: {}  qdrant: {}", response.ok, response.qdrant))
        }
        Command::Sessions {
            command: SessionsCommand::Link { .. },
        } => {
            let record: SessionRecord = parse_response(body)?;
            Ok(format!(
                "{}  {}  {}  parent: {}",
                record.harness,
                record.project,
                record.session,
                record.parent.as_deref().unwrap_or("-")
            ))
        }
        Command::Sessions {
            command: SessionsCommand::Keep { .. },
        } => {
            let record: SessionRecord = parse_response(body)?;
            Ok(format!(
                "{}  {}  {}  retention: {}",
                record.harness,
                record.project,
                record.session,
                if record.retention == Retention::Keep {
                    "keep"
                } else {
                    "continuation"
                }
            ))
        }
        Command::Sessions {
            command: SessionsCommand::List { .. },
        } => {
            let response: Value = parse_response(body)?;
            let sessions: Vec<SessionRecord> = serde_json::from_value(
                response
                    .get("sessions")
                    .cloned()
                    .ok_or_else(|| CliError::other("invalid daemon response: missing sessions"))?,
            )
            .map_err(|error| CliError::other(format!("invalid daemon response: {error}")))?;
            Ok(sessions
                .iter()
                .map(|record| {
                    format!(
                        "{}  {}  {}  parent: {}",
                        record.harness,
                        record.project,
                        record.session,
                        record.parent.as_deref().unwrap_or("-")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        Command::Sessions {
            command: SessionsCommand::Lineage { .. },
        } => {
            let response: Value = parse_response(body)?;
            let lineage: Vec<SessionKey> = serde_json::from_value(
                response
                    .get("lineage")
                    .cloned()
                    .ok_or_else(|| CliError::other("invalid daemon response: missing lineage"))?,
            )
            .map_err(|error| CliError::other(format!("invalid daemon response: {error}")))?;
            Ok(lineage
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" -> "))
        }
        Command::Search(args) => {
            let response: SearchResponse = parse_response(body)?;
            if args.framed {
                return Ok(format_search(&response));
            }
            Ok(response
                .hits
                .iter()
                .map(|hit| {
                    let text: String = hit
                        .text
                        .chars()
                        .map(|ch| if ch.is_whitespace() { ' ' } else { ch })
                        .collect();
                    let kind =
                        serde_json::to_value(hit.kind).expect("kind serialization is infallible");
                    format!(
                        "{:.3}  {}  {}  {}  {}  {}",
                        hit.score,
                        kind.as_str().unwrap_or("unknown"),
                        if hit.harness.is_empty() {
                            hit.session.clone()
                        } else {
                            format!("{}/{}", hit.harness, hit.session)
                        },
                        hit.ts,
                        hit.path.as_deref().unwrap_or("-"),
                        text
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        Command::Digest(args) => {
            let response: DigestResponse = parse_response(body)?;
            if args.framed {
                Ok(format_digest(&response.text))
            } else if args.continuation {
                let harness = args
                    .harness
                    .as_deref()
                    .ok_or_else(|| CliError::other("missing digest harness"))?;
                let session = args
                    .session
                    .as_deref()
                    .ok_or_else(|| CliError::other("missing digest session"))?;
                Ok(format_continuation(harness, session, &response.text))
            } else {
                Ok(response.text)
            }
        }
        Command::Forget(args) => {
            let response: ForgetResponse = parse_response(body)?;
            if args.dry_run {
                let mut lines = vec![format!("would delete: {}", response.would_delete)];
                lines.extend(response.sessions.iter().map(ToString::to_string));
                Ok(lines.join("\n"))
            } else {
                Ok(format!("deleted: {}", response.deleted))
            }
        }
        Command::Ingest(_) => {
            let response: IngestResponse = parse_response(body)?;
            Ok(response.render())
        }
        Command::Stats(_) => {
            let response: StatsResponse = parse_response(body)?;
            let width = response
                .by_kind
                .keys()
                .map(|kind| kind.chars().count())
                .max()
                .unwrap_or(0);
            let mut lines = vec![format!("total  {}", response.total)];
            lines.extend(
                response
                    .by_kind
                    .iter()
                    .map(|(kind, count)| format!("  {kind:<width$}  {count}")),
            );
            Ok(lines.join("\n"))
        }
        Command::Note(_)
        | Command::Setup(_)
        | Command::ProjectKey { .. }
        | Command::Gc(_)
        | Command::Import(_)
        | Command::HookImport(_)
        | Command::Continue(_)
        | Command::Hook(_)
        | Command::InstallHooks(_)
        | Command::InstallSkills(_)
        | Command::Daemon(_)
        | Command::ReapplySetup
        | Command::Update(_)
        | Command::Doctor(_)
        | Command::WriteReceipt(_) => Err(CliError::other("not a legacy response")),
    }
}

fn confirm_keep_session(client: &Client, cli: &Cli) -> Result<(), CliError> {
    let Command::Sessions {
        command:
            SessionsCommand::Keep {
                harness,
                session,
                project,
                ..
            },
    } = &cli.command
    else {
        return Ok(());
    };
    let project = project_or_current(project)?;
    let mut request = client
        .get(endpoint(&cli.url, &["v1", "sessions"])?)
        .query(&[("harness", harness.as_str()), ("project", project.as_str())]);
    if let Some(token) = &cli.token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .map_err(|error| CliError::request(error, &cli.url))?;
    let status = response.status();
    let body = response
        .text()
        .map_err(|error| CliError::other(format!("cannot read session list: {error}")))?;
    if !status.is_success() {
        return Err(response_error(status, &body));
    }
    #[derive(Deserialize)]
    struct SessionsResponse {
        sessions: Vec<SessionRecord>,
    }
    let records: SessionsResponse = parse_response(&body)?;
    if !records.sessions.iter().any(|record| {
        record.harness == *harness
            && record.project == project
            && record.session == *session
            && record.deleted_at.is_none()
    }) {
        return Err(CliError::other(format!(
            "session {harness}/{session} does not exist or is deleted in project {project}"
        )));
    }
    Ok(())
}

fn select_digest_session(client: &Client, cli: &mut Cli) -> Result<(), CliError> {
    let Command::Digest(args) = &cli.command else {
        return Ok(());
    };
    if args.session.is_some() {
        return Ok(());
    }
    let project = project_or_current(&args.project)?;
    let mut request = client
        .get(endpoint(&cli.url, &["v1", "sessions"])?)
        .query(&[("project", &project)]);
    if let Some(token) = &cli.token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .map_err(|error| CliError::request(error, &cli.url))?;
    let status = response.status();
    let body = response
        .text()
        .map_err(|error| CliError::other(format!("cannot read session list: {error}")))?;
    if !status.is_success() {
        return Err(response_error(status, &body));
    }
    #[derive(Deserialize)]
    struct SessionsResponse {
        sessions: Vec<SessionRecord>,
    }
    let response: SessionsResponse = parse_response(&body)?;
    let mut sessions: Vec<_> = response
        .sessions
        .into_iter()
        .filter(|record| record.project == project && record.deleted_at.is_none())
        .collect();
    sessions.sort_by(|left, right| {
        right
            .last_active_at
            .cmp(&left.last_active_at)
            .then_with(|| right.harness.cmp(&left.harness))
            .then_with(|| right.session.cmp(&left.session))
    });
    let mut session = sessions
        .first()
        .ok_or_else(|| CliError::other(format!("no active sessions in project {project}")))?;
    for candidate in sessions.iter().take(10) {
        let mut request = client.get(endpoint(&cli.url, &["v1", "stats"])?).query(&[
            ("harness", candidate.harness.as_str()),
            ("project", project.as_str()),
            ("session", candidate.session.as_str()),
        ]);
        if let Some(token) = &cli.token {
            request = request.bearer_auth(token);
        }
        let stats = request
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .and_then(reqwest::blocking::Response::text)
            .ok()
            .and_then(|body| parse_response::<StatsResponse>(&body).ok());
        let Some(stats) = stats else {
            break;
        };
        if stats
            .by_kind
            .iter()
            .any(|(kind, count)| kind != "user_request" && *count > 0)
        {
            session = candidate;
            break;
        }
    }
    eprintln!("digest session: {}/{}", session.harness, session.session);
    let harness = session.harness.clone();
    let session = session.session.clone();
    let Command::Digest(args) = &mut cli.command else {
        unreachable!()
    };
    args.project = Some(project);
    args.harness = Some(harness);
    args.session = Some(session);
    Ok(())
}

fn run_note(client: &Client, cli: &Cli, args: &NoteArgs) -> Result<(), CliError> {
    let project = project_or_current(&args.project)?;
    let session = args
        .session
        .clone()
        .unwrap_or_else(|| format!("{}-notes", args.harness));
    let registration = json!({
        "harness": args.harness,
        "project": project,
        "session": session
    });
    let ingest = IngestRequest {
        harness: args.harness.clone(),
        project,
        session,
        items: vec![IngestItem {
            source_id: format!("note:{}", uuid::Uuid::new_v4()),
            ts: chrono::Utc::now(),
            role: Role::Assistant,
            text: args.text.clone(),
            injected: false,
            kind_hint: match args.kind {
                NoteKind::Decision => Some(Kind::Decision),
                NoteKind::Constraint => Some(Kind::Constraint),
                NoteKind::Task => Some(Kind::Task),
                NoteKind::Note => None,
            },
            tool: None,
        }],
    };
    let mut registration_request = client
        .put(endpoint(&cli.url, &["v1", "sessions"])?)
        .json(&registration);
    let mut ingest_request = client
        .post(endpoint(&cli.url, &["v1", "ingest"])?)
        .json(&ingest);
    if let Some(token) = &cli.token {
        registration_request = registration_request.bearer_auth(token);
        ingest_request = ingest_request.bearer_auth(token);
    }
    let registration_response = registration_request
        .send()
        .map_err(|error| CliError::request(error, &cli.url))?;
    let status = registration_response.status();
    let body = registration_response.text().map_err(|error| {
        CliError::other(format!(
            "cannot read session registration response: {error}"
        ))
    })?;
    if !status.is_success() {
        return Err(response_error(status, &body));
    }
    let _: Value = parse_response(&body)?;
    let response = ingest_request
        .send()
        .map_err(|error| CliError::request(error, &cli.url))?;
    let status = response.status();
    let body = response
        .text()
        .map_err(|error| CliError::other(format!("cannot read ingest response: {error}")))?;
    if !status.is_success() {
        return Err(response_error(status, &body));
    }
    if cli.json {
        println!("{body}");
    } else {
        let response: IngestResponse = parse_response(&body)?;
        println!("stored: {}", response.segments);
    }
    Ok(())
}

fn run(mut cli: Cli) -> Result<(), CliError> {
    if cli.json
        && match &cli.command {
            Command::Search(args) => args.framed,
            Command::Digest(args) => args.framed || args.continuation,
            _ => false,
        }
    {
        return Err(CliError::other(
            "--json cannot be used with --framed or --continuation",
        ));
    }
    if let Command::Daemon(args) = &cli.command {
        return daemon::run(&cli, args);
    }
    if let Command::Update(args) = &cli.command {
        return update::run(&cli, args);
    }
    if let Command::Doctor(args) = &cli.command {
        return doctor::run(&cli, args);
    }
    if let Command::WriteReceipt(args) = &cli.command {
        return doctor::write_receipt(args);
    }
    if let Command::Setup(args) = &cli.command {
        return setup::run(&cli, args);
    }
    if matches!(&cli.command, Command::ReapplySetup) {
        return setup::reapply_saved_setup(&cli);
    }
    if matches!(
        &cli.command,
        Command::ProjectKey { .. }
            | Command::Gc(_)
            | Command::Import(_)
            | Command::HookImport(_)
            | Command::Continue(_)
            | Command::Hook(_)
            | Command::InstallHooks(_)
            | Command::InstallSkills(_)
            | Command::ReapplySetup
    ) {
        return cross_harness::run(&cli);
    }
    if let Command::Forget(args) = &cli.command
        && !args.dry_run
    {
        let stdin = io::stdin();
        if !confirm_forget(&mut stdin.lock(), stdin.is_terminal(), args.yes)? {
            return Err(CliError::other("forget aborted"));
        }
    }
    let ingest = match &cli.command {
        Command::Ingest(args) => Some(read_ingest(&args.file)?),
        _ => None,
    };
    let client = Client::builder()
        .timeout(Duration::from_secs(cli.timeout_secs))
        .build()
        .map_err(|error| CliError::other(format!("cannot create HTTP client: {error}")))?;
    if let Command::Note(args) = &cli.command {
        return run_note(&client, &cli, args);
    }
    select_digest_session(&client, &mut cli)?;
    confirm_keep_session(&client, &cli)?;
    let request = build_request(&client, &cli, ingest.as_ref())?;
    let response = client
        .execute(request)
        .map_err(|error| CliError::request(error, &cli.url))?;
    let status = response.status();
    let body = response
        .text()
        .map_err(|error| CliError::other(format!("cannot read daemon response: {error}")))?;
    if !status.is_success() {
        return Err(response_error(status, &body));
    }
    if cli.json {
        println!("{body}");
    } else {
        let output = render(&cli.command, &body)?;
        if !output.is_empty() {
            println!("{output}");
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {}", error.message);
            ExitCode::from(error.exit_code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use std::io::{BufRead, BufReader, Cursor};
    use std::net::TcpListener;
    use std::thread;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("memento").chain(args.iter().copied())).unwrap()
    }

    fn request(args: &[&str]) -> Request {
        build_request(&Client::new(), &parse(args), None).unwrap()
    }

    fn body(request: &Request) -> Value {
        serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap()
    }

    #[test]
    fn help_describes_commands_and_arguments() {
        fn assert_descriptions(command: &clap::Command) {
            for argument in command.get_arguments() {
                assert!(
                    argument
                        .get_help()
                        .is_some_and(|help| !help.to_string().trim().is_empty()),
                    "{}: {} has no help",
                    command.get_name(),
                    argument.get_id()
                );
            }
            for subcommand in command.get_subcommands() {
                assert!(
                    subcommand
                        .get_about()
                        .is_some_and(|about| !about.to_string().trim().is_empty()),
                    "{} has no description",
                    subcommand.get_name()
                );
                assert_descriptions(subcommand);
            }
        }

        let mut command = Cli::command();
        let help = command.render_long_help().to_string();
        for description in [
            "Check daemon health",
            "Show memory counts by kind",
            "Manage sessions and lineage",
            "Search stored memories",
            "Show a session digest",
            "Delete matching memories",
            "Ingest transcript items",
        ] {
            assert!(help.contains(description), "missing {description}");
        }
        let sessions_help = command
            .find_subcommand_mut("sessions")
            .unwrap()
            .render_long_help()
            .to_string();
        for description in [
            "List registered sessions",
            "Link a child session to a parent",
            "Show session ancestors",
        ] {
            assert!(sessions_help.contains(description), "missing {description}");
        }
        assert_descriptions(&Cli::command());
    }

    #[test]
    fn renders_sorted_stats_and_ignores_extra_fields() {
        let cli = parse(&["stats", "--harness", "h"]);
        let output = render(
            &cli.command,
            r#"{"total":12,"by_kind":{"user_request":6,"command":2,"assistant_note":4},"extra":true}"#,
        )
        .unwrap();
        assert_eq!(
            output,
            "total  12\n  assistant_note  4\n  command         2\n  user_request    6"
        );
        assert_eq!(
            render(&cli.command, r#"{"total":0,"by_kind":{}}"#).unwrap(),
            "total  0"
        );
        assert!(render(&cli.command, r#"{"total":0,"by_kind":[]}"#).is_err());
    }

    #[test]
    fn search_output_preserves_full_hit_text_on_one_line() {
        let cli = parse(&["search", "sandbox"]);
        let preamble = "é".repeat(210);
        let text = format!("{preamble}\nits sandbox key prefix is\tTEST_");
        let response = json!({
            "hits": [{
                "id": "00000000-0000-0000-0000-000000000001",
                "score": 0.75,
                "kind": "decision",
                "text": text,
                "harness": "claude",
                "session": "s",
                "ts": "2026-11-18T10:00:00Z",
                "path": "src/a.rs"
            }]
        });
        let output = render(&cli.command, &response.to_string()).unwrap();
        assert_eq!(
            output,
            format!(
                "0.750  decision  claude/s  2026-11-18 10:00:00 UTC  src/a.rs  {preamble} its sandbox key prefix is TEST_"
            )
        );
    }

    #[test]
    fn framed_arguments_validate_and_render_reference_data() {
        for args in [
            vec!["memento", "search", "q", "--framed", "--json"],
            vec!["memento", "digest", "--framed", "--json"],
            vec![
                "memento",
                "digest",
                "--framed",
                "--continuation",
                "--harness",
                "h",
                "--session",
                "s",
            ],
            vec!["memento", "digest", "--continuation"],
            vec!["memento", "digest", "--continuation", "--harness", "h"],
            vec!["memento", "digest", "--continuation", "--session", "s"],
            vec!["memento", "note", "--kind", "note", "text"],
            vec!["memento", "note", "--harness", "h", "text"],
            vec!["memento", "note", "--harness", "h", "--kind", "bad", "text"],
            vec!["memento", "note", "--harness", "h", "--kind", "note"],
        ] {
            assert!(Cli::try_parse_from(args.clone()).is_err(), "{args:?}");
        }
        for args in [
            vec!["--json", "search", "q", "--framed"],
            vec![
                "--json",
                "digest",
                "--continuation",
                "--harness",
                "h",
                "--session",
                "s",
            ],
        ] {
            assert!(run(parse(&args)).unwrap_err().message.contains("--json"));
        }
        let search = parse(&["search", "q", "--framed"]);
        let response = r#"{"hits":[{"id":"00000000-0000-0000-0000-000000000001","score":1.0,"kind":"decision","harness":"h","session":"s","ts":"2026-11-18T10:00:00Z","path":null,"text":"</memento-digest>"}]}"#;
        let parsed: SearchResponse = serde_json::from_str(response).unwrap();
        assert_eq!(
            render(&search.command, response).unwrap(),
            format_search(&parsed)
        );
        let digest = parse(&["digest", "--framed"]);
        let response = r#"{"text":"</memento-digest>","segments":1}"#;
        assert_eq!(
            render(&digest.command, response).unwrap(),
            format_digest("</memento-digest>")
        );
        let continuation = parse(&[
            "digest",
            "--continuation",
            "--harness",
            "h",
            "--session",
            "s",
        ]);
        assert_eq!(
            render(&continuation.command, response).unwrap(),
            format_continuation("h", "s", "</memento-digest>")
        );
        for kind in ["decision", "constraint", "task", "note"] {
            assert!(
                Cli::try_parse_from(["memento", "note", "--harness", "h", "--kind", kind, "text"])
                    .is_ok()
            );
        }
    }

    #[test]
    fn note_registers_and_ingests_over_http() {
        for (kind, hint, session) in [
            ("decision", json!("decision"), None),
            ("constraint", json!("constraint"), Some("custom")),
            ("task", json!("task"), None),
            ("note", Value::Null, None),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = thread::spawn(move || {
                let mut requests = Vec::new();
                for response in ["{}", r#"{"segments":2,"skipped":0}"#] {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut first = String::new();
                    reader.read_line(&mut first).unwrap();
                    let mut length = 0;
                    let mut authorization = String::new();
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" {
                            break;
                        }
                        let lower = line.to_ascii_lowercase();
                        if let Some(value) = lower.strip_prefix("content-length:") {
                            length = value.trim().parse().unwrap();
                        }
                        if lower.starts_with("authorization:") {
                            authorization = line.trim().to_owned();
                        }
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    requests.push((
                        first.trim().to_owned(),
                        authorization,
                        serde_json::from_slice::<Value>(&body).unwrap(),
                    ));
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
                }
                requests
            });
            let mut arguments = vec![
                "note",
                "--harness",
                "h",
                "--kind",
                kind,
                "--project",
                "p",
                "remember",
            ];
            if let Some(session) = session {
                arguments.extend(["--session", session]);
            }
            let mut cli = parse(&arguments);
            cli.url = url;
            cli.token = Some("test-token".into());
            run(cli).unwrap();
            let requests = server.join().unwrap();
            let session = session.unwrap_or("h-notes");
            assert_eq!(requests[0].0, "PUT /v1/sessions HTTP/1.1");
            assert_eq!(
                requests[0].2,
                json!({"harness":"h","project":"p","session":session})
            );
            assert_eq!(requests[1].0, "POST /v1/ingest HTTP/1.1");
            assert!(requests.iter().all(|request| {
                request
                    .1
                    .eq_ignore_ascii_case("authorization: Bearer test-token")
            }));
            let body = &requests[1].2;
            assert_eq!(body["harness"], "h");
            assert_eq!(body["project"], "p");
            assert_eq!(body["session"], session);
            assert_eq!(body["items"].as_array().unwrap().len(), 1);
            let item = &body["items"][0];
            assert_eq!(item["text"], "remember");
            assert_eq!(item["role"], "assistant");
            assert_eq!(item["injected"], false);
            assert_eq!(item["kind_hint"], hint);
            assert_eq!(item["tool"], Value::Null);
            let id = item["source_id"]
                .as_str()
                .unwrap()
                .strip_prefix("note:")
                .unwrap();
            assert_eq!(uuid::Uuid::parse_str(id).unwrap().get_version_num(), 4);
            assert!(chrono::DateTime::parse_from_rfc3339(item["ts"].as_str().unwrap()).is_ok());
        }
    }

    #[test]
    fn parses_global_arguments_and_search_defaults() {
        let cli = parse(&[
            "search",
            "hello",
            "--harness",
            "h",
            "--project",
            "p",
            "--session",
            "s",
            "--url",
            "http://example.test",
            "--token",
            "secret",
            "--json",
            "--timeout-secs",
            "4",
        ]);
        assert_eq!(cli.url, "http://example.test");
        assert_eq!(cli.token.as_deref(), Some("secret"));
        assert!(cli.json);
        assert_eq!(cli.timeout_secs, 4);
        let Command::Search(args) = cli.command else {
            panic!("expected search")
        };
        assert_eq!(search_scope(&args), Scope::Lineage);
        assert_eq!(args.k, 8);
        assert!(args.kind.is_empty());
    }

    #[test]
    fn rejects_invalid_arguments_and_unscoped_project_forget() {
        for args in [
            vec!["memento", "forget", "--harness", "h"],
            vec!["memento", "forget", "--harness", "h", "--whole-project"],
            vec![
                "memento",
                "forget",
                "--harness",
                "h",
                "--session",
                "s",
                "--lineage",
                "s",
            ],
            vec![
                "memento",
                "search",
                "q",
                "--harness",
                "h",
                "--project",
                "p",
                "--session",
                "s",
                "--scope",
                "bad",
            ],
            vec![
                "memento",
                "search",
                "q",
                "--harness",
                "h",
                "--project",
                "p",
                "--session",
                "s",
                "--kind",
                "bad",
            ],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn builds_health_stats_and_session_requests() {
        let health = request(&["--token", "abc", "health"]);
        assert_eq!(health.method(), Method::GET);
        assert_eq!(health.url().path(), "/health");
        assert_eq!(health.headers()["authorization"], "Bearer abc");

        let stats = request(&[
            "stats",
            "--harness",
            "h &",
            "--project",
            "p",
            "--session",
            "s",
        ]);
        assert_eq!(stats.method(), Method::GET);
        assert_eq!(stats.url().path(), "/v1/stats");
        assert_eq!(
            stats.url().query_pairs().collect::<Vec<_>>(),
            vec![
                ("harness".into(), "h &".into()),
                ("project".into(), "p".into()),
                ("session".into(), "s".into())
            ]
        );

        let list = request(&["sessions", "list", "--harness", "h", "--project", "p"]);
        assert_eq!(list.method(), Method::GET);
        assert_eq!(list.url().path(), "/v1/sessions");
        assert_eq!(list.url().query(), Some("harness=h&project=p"));

        let link = request(&[
            "sessions",
            "link",
            "--harness",
            "h",
            "--project",
            "p",
            "--child",
            "c",
            "--parent",
            "s",
            "--parent-harness",
            "other",
            "--parent-cutoff",
            "2026-11-18T10:00:00Z",
            "--title",
            "title",
            "--retention",
            "keep",
        ]);
        assert_eq!(link.method(), Method::PUT);
        assert_eq!(link.url().path(), "/v1/sessions");
        assert_eq!(
            body(&link),
            json!({"harness":"h","project":"p","session":"c","parent":"s","parent_harness":"other","parent_cutoff":"2026-11-18T10:00:00Z","title":"title","retention":"keep"})
        );

        let lineage = request(&[
            "sessions",
            "lineage",
            "--harness",
            "h/one",
            "--session",
            "s?two",
        ]);
        assert_eq!(lineage.method(), Method::GET);
        assert_eq!(lineage.url().path(), "/v1/sessions/h%2Fone/s%3Ftwo/lineage");
        assert_eq!(
            render(
                &parse(&["sessions", "lineage", "--harness", "h", "--session", "s"]).command,
                r#"{"lineage":[{"harness":"h","session":"s"},{"harness":"other","session":"p"}]}"#
            )
            .unwrap(),
            "h/s -> other/p"
        );
    }

    #[test]
    fn builds_search_and_digest_requests() {
        let search = request(&[
            "search",
            "my query",
            "--harness",
            "h",
            "--project",
            "p",
            "--session",
            "s",
            "--scope",
            "global",
            "--k",
            "3",
            "--kind",
            "file_change",
            "--kind",
            "decision",
        ]);
        assert_eq!(search.method(), Method::POST);
        assert_eq!(search.url().path(), "/v1/search");
        assert_eq!(
            body(&search),
            json!({"harness":"h","project":"p","session":"s","query":"my query","scope":"global","k":3,"kinds":["file_change","decision"]})
        );

        let digest = request(&[
            "digest",
            "--harness",
            "h",
            "--project",
            "p",
            "--session",
            "s",
            "--budget",
            "100",
        ]);
        assert_eq!(digest.method(), Method::GET);
        assert_eq!(digest.url().path(), "/v1/digest");
        assert_eq!(
            digest.url().query(),
            Some("harness=h&project=p&session=s&budget=100")
        );
        let filtered = request(&[
            "digest",
            "--harness",
            "h",
            "--project",
            "p",
            "--session",
            "s",
            "--profile",
            "subagent",
        ]);
        assert_eq!(
            filtered.url().query(),
            Some("harness=h&project=p&session=s&profile=subagent")
        );
        assert!(Cli::try_parse_from(["memento", "digest", "--profile", "unknown"]).is_err());
    }

    #[test]
    fn search_and_stats_default_to_current_project() {
        let project = cross_harness::project_key(None).unwrap();
        let search = request(&["search", "retry limit"]);
        assert_eq!(
            body(&search),
            json!({"harness":"","project":project,"session":"","query":"retry limit","scope":"project","k":8,"kinds":null})
        );
        let global = request(&["search", "retry", "--scope", "global"]);
        assert_eq!(body(&global)["scope"], "global");
        let lineage = request(&["search", "retry", "--harness", "h", "--session", "s"]);
        assert_eq!(body(&lineage)["scope"], "lineage");
        let stats = request(&["stats"]);
        assert_eq!(
            stats.url().query_pairs().collect::<Vec<_>>(),
            vec![("project".into(), project.into())]
        );
    }

    #[test]
    fn project_arguments_resolve_directories_and_preserve_identifiers() {
        let expected = cross_harness::project_key(Some(std::path::Path::new("."))).unwrap();
        for (project, expected) in [
            (".", expected.as_str()),
            (
                "memento-missing-project-identifier",
                "memento-missing-project-identifier",
            ),
        ] {
            let cases = [
                vec!["stats", "--project", project],
                vec!["sessions", "list", "--harness", "h", "--project", project],
                vec![
                    "sessions",
                    "link",
                    "--harness",
                    "h",
                    "--project",
                    project,
                    "--child",
                    "c",
                    "--parent",
                    "p",
                ],
                vec![
                    "sessions",
                    "keep",
                    "--harness",
                    "h",
                    "--session",
                    "s",
                    "--project",
                    project,
                ],
                vec!["search", "query", "--project", project],
                vec![
                    "digest",
                    "--harness",
                    "h",
                    "--session",
                    "s",
                    "--project",
                    project,
                ],
                vec![
                    "forget",
                    "--harness",
                    "h",
                    "--session",
                    "s",
                    "--project",
                    project,
                ],
            ];
            for args in cases {
                let request = request(&args);
                let actual = if request.method() == Method::GET {
                    request
                        .url()
                        .query_pairs()
                        .find(|(key, _)| key == "project")
                        .unwrap()
                        .1
                        .into_owned()
                } else {
                    body(&request)["project"].as_str().unwrap().to_string()
                };
                assert_eq!(actual, expected, "{args:?}");
            }
        }
    }

    #[test]
    fn scoped_search_requires_harness_and_session() {
        for scope in ["session", "lineage"] {
            for args in [
                vec!["search", "q", "--scope", scope],
                vec!["search", "q", "--scope", scope, "--harness", "h"],
                vec!["search", "q", "--scope", scope, "--session", "s"],
            ] {
                let cli = parse(&args);
                let error = build_request(&Client::new(), &cli, None).unwrap_err();
                assert!(error.message.contains("required"));
            }
        }
        let cli = parse(&["search", "q", "--session", "s"]);
        assert!(
            build_request(&Client::new(), &cli, None)
                .unwrap_err()
                .message
                .contains("--harness")
        );
    }

    #[test]
    fn digest_rejects_partial_explicit_identity() {
        assert!(Cli::try_parse_from(["memento", "digest", "--harness", "h"]).is_err());
        assert!(Cli::try_parse_from(["memento", "digest", "--session", "s"]).is_err());
    }

    #[test]
    fn builds_all_forget_scopes() {
        for (selector, project, expected) in [
            (
                vec!["--session", "s"],
                None,
                json!({"harness":"h","project":null,"session":"s","scope":"session","dry_run":false}),
            ),
            (
                vec!["--lineage", "s"],
                Some("p"),
                json!({"harness":"h","project":"p","session":"s","scope":"lineage","dry_run":false}),
            ),
            (
                vec!["--whole-project"],
                Some("p"),
                json!({"harness":"h","project":"p","session":null,"scope":"project","dry_run":false}),
            ),
        ] {
            let mut args = vec!["forget", "--harness", "h"];
            args.extend(selector);
            if let Some(project) = project {
                args.extend(["--project", project]);
            }
            let request = request(&args);
            assert_eq!(request.method(), Method::POST);
            assert_eq!(request.url().path(), "/v1/forget");
            assert_eq!(body(&request), expected);
        }
    }

    #[test]
    fn builds_ingest_from_validated_schema() {
        let input =
            json!({"harness":"h","project":"p","session":"s","items":[],"future_field":true});
        assert!(serde_json::from_value::<IngestRequest>(input.clone()).is_ok());
        let request = build_request(
            &Client::new(),
            &parse(&["ingest", "--file", "-"]),
            Some(&input),
        )
        .unwrap();
        assert_eq!(request.method(), Method::POST);
        assert_eq!(request.url().path(), "/v1/ingest");
        assert_eq!(body(&request), input);
        assert!(serde_json::from_value::<IngestRequest>(json!({"items":[]})).is_err());
        let command = parse(&["ingest", "--file", "-"]).command;
        assert_eq!(
            render(&command, r#"{"segments":2,"skipped":1}"#).unwrap(),
            "segments: 2  skipped: 1"
        );
        assert_eq!(
            render(&command, r#"{"segments":2,"skipped":1,"duplicates":3}"#).unwrap(),
            "segments: 2  skipped: 1  duplicates: 3"
        );
    }

    #[test]
    fn maps_api_errors_and_invalid_error_bodies() {
        let error = response_error(
            reqwest::StatusCode::CONFLICT,
            r#"{"error":{"code":"cycle","message":"cycle detected"}}"#,
        );
        assert_eq!(error.exit_code, 1);
        assert_eq!(error.message, "cycle: cycle detected");
        let error = response_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, "oops");
        assert_eq!(error.exit_code, 1);
        assert!(error.message.contains("500"));
        assert!(error.message.contains("oops"));
    }

    #[test]
    fn forget_requires_explicit_tty_confirmation_or_yes() {
        assert!(confirm_forget(&mut Cursor::new(""), false, true).unwrap());
        assert!(confirm_forget(&mut Cursor::new("y\n"), false, false).is_err());
        assert!(confirm_forget(&mut Cursor::new("Y\n"), true, false).unwrap());
        assert!(!confirm_forget(&mut Cursor::new("\n"), true, false).unwrap());
        assert!(!confirm_forget(&mut Cursor::new("yes\n"), true, false).unwrap());
    }

    #[test]
    fn keep_request_preserves_other_fields_and_forget_preview_renders() {
        for (off, retention) in [(false, "keep"), (true, "continuation")] {
            let mut args = vec![
                "sessions",
                "keep",
                "--harness",
                "h",
                "--session",
                "s",
                "--project",
                "p",
            ];
            if off {
                args.push("--off");
            }
            assert_eq!(
                body(&request(&args)),
                json!({"harness":"h","session":"s","project":"p","retention":retention})
            );
        }
        let cli = parse(&["forget", "--harness", "h", "--session", "s", "--dry-run"]);
        assert_eq!(
            body(&build_request(&Client::new(), &cli, None).unwrap())["dry_run"],
            true
        );
        assert_eq!(
            render(
                &cli.command,
                r#"{"deleted":0,"would_delete":2,"sessions":[{"harness":"h","session":"s"}]}"#
            )
            .unwrap(),
            "would delete: 2\nh/s"
        );
    }

    #[test]
    fn keep_checks_session_before_update() {
        for (response, expected) in [
            (
                r#"{"sessions":[{"harness":"h","project":"p","session":"s","parent":null,"parent_harness":null,"title":null,"created":null}]}"#,
                true,
            ),
            (r#"{"sessions":[]}"#, false),
            (
                r#"{"sessions":[{"harness":"h","project":"p","session":"s","parent":null,"parent_harness":null,"title":null,"created":null,"deleted_at":"2026-11-20T00:00:00Z"}]}"#,
                false,
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = thread::spawn(move || {
                serve_once(
                    listener,
                    "GET",
                    "/v1/sessions?harness=h&project=p",
                    response,
                )
            });
            let mut cli = parse(&[
                "sessions",
                "keep",
                "--harness",
                "h",
                "--session",
                "s",
                "--project",
                "p",
            ]);
            cli.url = url;
            assert_eq!(confirm_keep_session(&Client::new(), &cli).is_ok(), expected);
            server.join().unwrap();
        }
    }

    fn serve_once(
        listener: TcpListener,
        expected_method: &str,
        expected_path: &str,
        response: &str,
    ) {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        assert_eq!(
            request_line.trim(),
            format!("{expected_method} {expected_path} HTTP/1.1")
        );
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
        }
        let mut request_body = vec![0; length];
        reader.read_exact(&mut request_body).unwrap();
        if expected_method == "POST" {
            let body: Value = serde_json::from_slice(&request_body).unwrap();
            assert_eq!(body["query"], "hello");
            assert_eq!(body["scope"], "lineage");
        }
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
    }

    fn digest_selection_server(
        stats: Vec<(&'static str, &'static str)>,
    ) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if std::time::Instant::now() >= deadline {
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(error) => panic!("cannot accept request: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap().to_owned();
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                let response = if path == "/v1/sessions?project=p" {
                    r#"{"sessions":[{"harness":"h","project":"p","session":"older","last_active_at":"2026-11-19T00:00:00Z"},{"harness":"h","project":"p","session":"newer","last_active_at":"2026-11-20T00:00:00Z"}]}"#
                } else {
                    stats
                        .iter()
                        .find(|(expected, _)| *expected == path)
                        .map(|(_, response)| *response)
                        .unwrap_or("{}")
                };
                requests.push(path);
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
            }
            requests
        });
        (url, server)
    }

    #[test]
    fn digest_selection_skips_question_only_session() {
        let (url, server) = digest_selection_server(vec![
            (
                "/v1/stats?harness=h&project=p&session=newer",
                r#"{"total":1,"by_kind":{"user_request":1}}"#,
            ),
            (
                "/v1/stats?harness=h&project=p&session=older",
                r#"{"total":1,"by_kind":{"assistant_note":1}}"#,
            ),
        ]);
        let mut cli = parse(&["digest", "--project", "p"]);
        cli.url = url;
        let result = select_digest_session(&Client::new(), &mut cli);
        let requests = server.join().unwrap();
        result.unwrap();
        let Command::Digest(args) = cli.command else {
            panic!("expected digest command");
        };
        assert_eq!(args.session.as_deref(), Some("older"));
        assert_eq!(
            requests,
            [
                "/v1/sessions?project=p",
                "/v1/stats?harness=h&project=p&session=newer",
                "/v1/stats?harness=h&project=p&session=older",
            ]
        );
    }

    #[test]
    fn digest_selection_falls_back_to_newest_session() {
        let (url, server) = digest_selection_server(vec![
            (
                "/v1/stats?harness=h&project=p&session=newer",
                r#"{"total":1,"by_kind":{"user_request":1,"assistant_note":0}}"#,
            ),
            (
                "/v1/stats?harness=h&project=p&session=older",
                r#"{"total":0,"by_kind":{}}"#,
            ),
        ]);
        let mut cli = parse(&["digest", "--project", "p"]);
        cli.url = url;
        let result = select_digest_session(&Client::new(), &mut cli);
        let requests = server.join().unwrap();
        result.unwrap();
        let Command::Digest(args) = cli.command else {
            panic!("expected digest command");
        };
        assert_eq!(args.session.as_deref(), Some("newer"));
        assert_eq!(
            requests,
            [
                "/v1/sessions?project=p",
                "/v1/stats?harness=h&project=p&session=newer",
                "/v1/stats?harness=h&project=p&session=older",
            ]
        );
    }

    #[test]
    fn digest_selection_falls_back_when_stats_fail() {
        let (url, server) = digest_selection_server(vec![
            (
                "/v1/stats?harness=h&project=p&session=newer",
                r#"{"total":1,"by_kind":{"user_request":1}}"#,
            ),
            ("/v1/stats?harness=h&project=p&session=older", "invalid"),
        ]);
        let mut cli = parse(&["digest", "--project", "p"]);
        cli.url = url;
        let result = select_digest_session(&Client::new(), &mut cli);
        let requests = server.join().unwrap();
        result.unwrap();
        let Command::Digest(args) = cli.command else {
            panic!("expected digest command");
        };
        assert_eq!(args.session.as_deref(), Some("newer"));
        assert_eq!(
            requests,
            [
                "/v1/sessions?project=p",
                "/v1/stats?harness=h&project=p&session=newer",
                "/v1/stats?harness=h&project=p&session=older",
            ]
        );
    }

    #[test]
    fn digest_selection_explicit_session_makes_no_stats_calls() {
        let (url, server) = digest_selection_server(vec![]);
        let mut cli = parse(&[
            "digest",
            "--project",
            "p",
            "--harness",
            "h",
            "--session",
            "explicit",
        ]);
        cli.url = url;
        let result = select_digest_session(&Client::new(), &mut cli);
        let requests = server.join().unwrap();
        result.unwrap();
        let Command::Digest(args) = cli.command else {
            panic!("expected digest command");
        };
        assert_eq!(args.session.as_deref(), Some("explicit"));
        assert!(requests.is_empty(), "{requests:?}");
    }

    #[test]
    fn mock_server_search_and_digest_end_to_end() {
        for (args, method, path, response, expected) in [
            (
                vec![
                    "search",
                    "hello",
                    "--harness",
                    "h",
                    "--project",
                    "p",
                    "--session",
                    "s",
                ],
                "POST",
                "/v1/search",
                r#"{"hits":[{"id":"00000000-0000-0000-0000-000000000001","score":0.75,"kind":"decision","text":"first\nsecond","session":"s","ts":"2026-11-18T10:00:00Z","path":"src/a.rs"}]}"#,
                "0.750  decision  s",
            ),
            (
                vec![
                    "digest",
                    "--harness",
                    "h",
                    "--project",
                    "p",
                    "--session",
                    "s",
                ],
                "GET",
                "/v1/digest?harness=h&project=p&session=s",
                r#"{"text":"working state","segments":1}"#,
                "working state",
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = thread::spawn(move || serve_once(listener, method, path, response));
            let mut all_args = vec!["memento", "--url", url.as_str()];
            all_args.extend(args);
            let cli = Cli::try_parse_from(all_args).unwrap();
            let client = Client::new();
            let response = client
                .execute(build_request(&client, &cli, None).unwrap())
                .unwrap();
            let response_body = response.text().unwrap();
            let output = render(&cli.command, &response_body).unwrap();
            server.join().unwrap();
            assert!(output.contains(expected), "{output}");
            if method == "POST" {
                assert!(output.contains("first second"));
            } else {
                assert_eq!(output, "working state");
            }
        }
    }
}
