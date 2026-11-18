use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use memento_core::lineage::SessionKey;
use memento_core::protocol::{
    DigestResponse, ErrorBody, ForgetRequest, ForgetResponse, ForgetScope, Health, IngestRequest,
    IngestResponse, Kind, Retention, Scope, SearchRequest, SearchResponse, SessionRecord,
};
use reqwest::blocking::{Client, Request};
use reqwest::{Method, Url};
use serde::Deserialize;
use serde_json::{Value, json};

mod cross_harness;

#[derive(Debug, Parser)]
#[command(name = "memento", about = "Client for the mementod memory daemon")]
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
    #[command(about = "Delete matching memories")]
    Forget(ForgetArgs),
    #[command(about = "Ingest transcript items")]
    Ingest(IngestArgs),
    #[command(about = "Print the canonical project key")]
    ProjectKey {
        #[arg(help = "Project directory")]
        dir: Option<PathBuf>,
    },
    #[command(about = "Import a harness transcript")]
    Import(cross_harness::ImportArgs),
    #[command(about = "Prepare a cross-harness continuation")]
    Continue(cross_harness::ContinueArgs),
    #[command(about = "Handle a harness lifecycle event")]
    Hook(cross_harness::HookArgs),
    #[command(about = "Install harness lifecycle hooks")]
    InstallHooks(cross_harness::InstallArgs),
}

#[derive(Debug, Args)]
struct StatsArgs {
    #[arg(long, help = "Harness identifier")]
    harness: String,
    #[arg(long, help = "Filter by project")]
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
        #[arg(long, help = "Filter by project")]
        project: Option<String>,
    },
    #[command(about = "Link a child session to a parent")]
    Link {
        #[arg(long, help = "Harness identifier")]
        harness: String,
        #[arg(long, help = "Project identifier")]
        project: String,
        #[arg(long, help = "Child session identifier")]
        child: String,
        #[arg(long, help = "Parent session identifier")]
        parent: String,
        #[arg(long, help = "Parent harness identifier")]
        parent_harness: Option<String>,
        #[arg(long, help = "Child session title")]
        title: Option<String>,
        #[arg(long, value_enum, help = "Child session retention mode")]
        retention: Option<RetentionArg>,
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
    harness: String,
    #[arg(long, help = "Project identifier")]
    project: String,
    #[arg(long, help = "Session identifier")]
    session: String,
    #[arg(long, value_enum, default_value = "lineage", help = "Search scope")]
    scope: ScopeArg,
    #[arg(long, default_value_t = 8, help = "Maximum number of results")]
    k: usize,
    #[arg(long, value_enum, help = "Filter by memory kind")]
    kind: Vec<KindArg>,
}

#[derive(Debug, Args)]
struct DigestArgs {
    #[arg(long, help = "Harness identifier")]
    harness: String,
    #[arg(long, help = "Project identifier")]
    project: String,
    #[arg(long, help = "Session identifier")]
    session: String,
    #[arg(long, help = "Digest character budget")]
    budget: Option<usize>,
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("target").required(true).args(["session", "lineage", "whole_project"])))]
struct ForgetArgs {
    #[arg(long, help = "Harness identifier")]
    harness: String,
    #[arg(long, help = "Project identifier")]
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
                    "cannot connect to {url}: {error}; start the daemon with docker compose -f deploy/compose.yaml up -d"
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
            SessionsCommand::Lineage { harness, session } => (
                Method::GET,
                vec!["v1", "sessions", harness, session, "lineage"],
            ),
        },
        Command::Search(_) => (Method::POST, vec!["v1", "search"]),
        Command::Digest(_) => (Method::GET, vec!["v1", "digest"]),
        Command::Forget(_) => (Method::POST, vec!["v1", "forget"]),
        Command::Ingest(_) => (Method::POST, vec!["v1", "ingest"]),
        Command::ProjectKey { .. }
        | Command::Import(_)
        | Command::Continue(_)
        | Command::Hook(_)
        | Command::InstallHooks(_) => return Err(CliError::other("not a legacy request")),
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
            let mut query = vec![("harness", args.harness.as_str())];
            if let Some(project) = &args.project {
                query.push(("project", project));
            }
            if let Some(session) = &args.session {
                query.push(("session", session));
            }
            request.query(&query)
        }
        Command::Sessions {
            command: SessionsCommand::List { harness, project },
        } => {
            let mut query = vec![("harness", harness.as_str())];
            if let Some(project) = project {
                query.push(("project", project));
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
                    title,
                    retention,
                },
        } => {
            let mut body =
                json!({"harness": harness, "project": project, "session": child, "parent": parent});
            if let Some(parent_harness) = parent_harness {
                body["parent_harness"] = json!(parent_harness);
            }
            if let Some(title) = title {
                body["title"] = json!(title);
            }
            if let Some(retention) = retention {
                body["retention"] = json!(Retention::from(*retention));
            }
            request.json(&body)
        }
        Command::Search(args) => request.json(&SearchRequest {
            harness: args.harness.clone(),
            project: args.project.clone(),
            session: args.session.clone(),
            query: args.query.clone(),
            scope: args.scope.into(),
            k: args.k,
            kinds: (!args.kind.is_empty())
                .then(|| args.kind.iter().copied().map(Into::into).collect()),
        }),
        Command::Digest(args) => {
            let mut query = vec![
                ("harness", args.harness.clone()),
                ("project", args.project.clone()),
                ("session", args.session.clone()),
            ];
            if let Some(budget) = args.budget {
                query.push(("budget", budget.to_string()));
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
                project: args.project.clone(),
                session,
                scope,
            })
        }
        Command::Ingest(_) => {
            request.json(ingest.ok_or_else(|| CliError::other("missing ingest input"))?)
        }
        Command::ProjectKey { .. }
        | Command::Import(_)
        | Command::Continue(_)
        | Command::Hook(_)
        | Command::InstallHooks(_) => return Err(CliError::other("not a legacy request")),
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
        Command::Search(_) => {
            let response: SearchResponse = parse_response(body)?;
            Ok(response
                .hits
                .iter()
                .map(|hit| {
                    let text: String = hit
                        .text
                        .chars()
                        .map(|ch| if ch.is_whitespace() { ' ' } else { ch })
                        .take(160)
                        .collect();
                    let kind =
                        serde_json::to_value(hit.kind).expect("kind serialization is infallible");
                    format!(
                        "{:.3}  {}  {}  {}  {}  {}",
                        hit.score,
                        kind.as_str().unwrap_or("unknown"),
                        hit.session,
                        hit.ts,
                        hit.path.as_deref().unwrap_or("-"),
                        text
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        Command::Digest(_) => {
            let response: DigestResponse = parse_response(body)?;
            Ok(response.text)
        }
        Command::Forget(_) => {
            let response: ForgetResponse = parse_response(body)?;
            Ok(format!("deleted: {}", response.deleted))
        }
        Command::Ingest(_) => {
            let response: IngestResponse = parse_response(body)?;
            Ok(format!(
                "segments: {}  skipped: {}",
                response.segments, response.skipped
            ))
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
        Command::ProjectKey { .. }
        | Command::Import(_)
        | Command::Continue(_)
        | Command::Hook(_)
        | Command::InstallHooks(_) => Err(CliError::other("not a legacy response")),
    }
}

fn run(cli: Cli) -> Result<(), CliError> {
    if matches!(
        &cli.command,
        Command::ProjectKey { .. }
            | Command::Import(_)
            | Command::Continue(_)
            | Command::Hook(_)
            | Command::InstallHooks(_)
    ) {
        return cross_harness::run(&cli);
    }
    if let Command::Forget(args) = &cli.command {
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
        assert!(matches!(args.scope, ScopeArg::Lineage));
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
            "--title",
            "title",
            "--retention",
            "keep",
        ]);
        assert_eq!(link.method(), Method::PUT);
        assert_eq!(link.url().path(), "/v1/sessions");
        assert_eq!(
            body(&link),
            json!({"harness":"h","project":"p","session":"c","parent":"s","parent_harness":"other","title":"title","retention":"keep"})
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
    }

    #[test]
    fn builds_all_forget_scopes() {
        for (selector, project, expected) in [
            (
                vec!["--session", "s"],
                None,
                json!({"harness":"h","project":null,"session":"s","scope":"session"}),
            ),
            (
                vec!["--lineage", "s"],
                Some("p"),
                json!({"harness":"h","project":"p","session":"s","scope":"lineage"}),
            ),
            (
                vec!["--whole-project"],
                Some("p"),
                json!({"harness":"h","project":"p","session":null,"scope":"project"}),
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
