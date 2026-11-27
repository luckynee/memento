use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use axum::{
    extract::{Request, State},
    middleware::{self, Next},
    response::Response,
};
use clap::{Parser, Subcommand};
use mementod::{
    config::{Config, cache_dir, sqlite_path},
    embed::CandleEmbedder,
    migrate::migrate,
    routes::{AppState, RouterState, backfill_activity, router_with_state, run_stale_sweep},
    store::Store,
};
use qdrant_client::Qdrant;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Serve,
    Warmup,
    Migrate {
        #[arg(long)]
        to: Option<PathBuf>,
        #[arg(long)]
        qdrant_url: Option<String>,
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long)]
        force: bool,
    },
}

fn main() -> Result<()> {
    validate_embedder_selection(std::env::var("MEMENTO_EMBEDDER"))?;
    let cache = cache_dir()?;
    unsafe { std::env::set_var("FASTEMBED_CACHE_DIR", &cache) };
    let threads = match std::env::var("MEMENTO_EMBED_THREADS") {
        Ok(value) => {
            let threads = value
                .parse::<usize>()
                .context("invalid MEMENTO_EMBED_THREADS")?;
            anyhow::ensure!(threads > 0, "MEMENTO_EMBED_THREADS must be positive");
            threads
        }
        Err(std::env::VarError::NotPresent) => std::thread::available_parallelism()
            .map(|threads| threads.get().min(4))
            .unwrap_or(1),
        Err(error) => return Err(error).context("reading MEMENTO_EMBED_THREADS"),
    };
    unsafe {
        std::env::set_var("RAYON_NUM_THREADS", threads.to_string());
        std::env::set_var("CANDLE_NUM_THREADS", threads.to_string());
    }
    tokio::runtime::Runtime::new()
        .context("creating async runtime")?
        .block_on(run())
}

fn validate_embedder_selection(selection: Result<String, std::env::VarError>) -> Result<()> {
    match selection {
        Ok(value) if value == "candle" => Ok(()),
        Ok(value) if value == "onnx" => {
            anyhow::bail!("ONNX embedder was removed; unset MEMENTO_EMBEDDER or set it to candle")
        }
        Ok(value) => anyhow::bail!("invalid MEMENTO_EMBEDDER: {value}"),
        Err(std::env::VarError::NotPresent) => Ok(()),
        Err(error) => Err(error).context("reading MEMENTO_EMBEDDER"),
    }
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    let config = Config::from_env()?;
    let _lock = if matches!(cli.command, Command::Serve) {
        match config.lock_path.as_deref().map(acquire_lock).transpose()? {
            Some(Some(lock)) => Some(lock),
            Some(None) => {
                tracing::info!("already running");
                return Ok(());
            }
            None => None,
        }
    } else {
        None
    };
    let embedder = Arc::new(CandleEmbedder::new().await?);
    match cli.command {
        Command::Warmup => return Ok(()),
        Command::Migrate {
            to,
            qdrant_url,
            prefix,
            force,
        } => {
            let destination = match to {
                Some(path) => path,
                None => sqlite_path()?,
            };
            let report = migrate(
                qdrant_url.as_deref().unwrap_or(&config.qdrant_url),
                prefix.as_deref().unwrap_or(&config.collection_prefix),
                &destination,
                force,
                embedder,
            )
            .await?;
            println!("segments: {}", report.counts[0]);
            println!("sessions: {}", report.counts[1]);
            println!("continuations: {}", report.counts[2]);
            println!("elapsed: {:.3}s", report.elapsed.as_secs_f64());
            for (field, count) in report.ignored_segment_fields {
                eprintln!("warning: segment parser ignores {field}: {count} points");
            }
            for (field, count) in report.preserved_segment_fields {
                eprintln!(
                    "warning: segment parser ignores {field}: {count} points; preserved for deduplication"
                );
            }
            return Ok(());
        }
        Command::Serve => {}
    }
    let store = if config.store == "sqlite" {
        Store::sqlite(config.sqlite_path.clone(), embedder).await?
    } else {
        let client = Qdrant::from_url(&config.qdrant_url)
            .build()
            .context("configuring Qdrant client")?;
        Store::new(client, &config.collection_prefix, embedder)
    };
    store
        .ensure_schema()
        .await
        .context("ensuring store schema")?;
    backfill_activity(&store, chrono::Utc::now())
        .await
        .context("backfilling session activity")?;
    let state = RouterState::new(AppState {
        store: Arc::new(store),
        config: config.clone(),
        policy: Default::default(),
        rerank: Default::default(),
    });
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .context("binding HTTP listener")?;
    let (sweep_shutdown, receiver) = tokio::sync::oneshot::channel();
    let sweep_handle = if config.stale_secs > 0 {
        let handle = tokio::spawn(run_stale_sweep(state.clone(), receiver));
        Some(handle)
    } else {
        None
    };
    let activity = Arc::new(Mutex::new(Activity {
        last_finished: Instant::now(),
        in_flight: 0,
    }));
    let router = router_with_state(state.clone()).layer(middleware::from_fn_with_state(
        activity.clone(),
        track_activity,
    ));
    tracing::info!(address = %config.bind, "serving HTTP");
    let server_result = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        tokio::select! {
            _ = shutdown() => {},
            _ = idle_shutdown(activity, config.idle_exit_secs) => {},
            _ = state.shutdown_requested() => {},
        }
        drop(sweep_shutdown);
    })
    .await;
    if let Some(handle) = sweep_handle {
        handle.await.context("joining background stale sweep")?;
    }
    server_result.context("serving HTTP")?;
    Ok(())
}

fn acquire_lock(path: &Path) -> Result<Option<File>> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating lock directory {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening lock {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("locking {}", path.display())),
    }
    file.set_len(0)
        .with_context(|| format!("truncating lock {}", path.display()))?;
    writeln!(file, "{}", std::process::id())
        .with_context(|| format!("writing lock {}", path.display()))?;
    Ok(Some(file))
}

struct Activity {
    last_finished: Instant,
    in_flight: usize,
}

struct ActiveRequest(Arc<Mutex<Activity>>);

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        let mut activity = self.0.lock().unwrap_or_else(|error| error.into_inner());
        activity.in_flight -= 1;
        activity.last_finished = Instant::now();
    }
}

async fn track_activity(
    State(activity): State<Arc<Mutex<Activity>>>,
    request: Request,
    next: Next,
) -> Response {
    {
        let mut state = activity.lock().unwrap_or_else(|error| error.into_inner());
        state.in_flight += 1;
    }
    let _request = ActiveRequest(activity);
    next.run(request).await
}

async fn idle_shutdown(activity: Arc<Mutex<Activity>>, seconds: u64) {
    if seconds == 0 {
        std::future::pending::<()>().await;
    }
    let timeout = Duration::from_secs(seconds);
    loop {
        tokio::time::sleep(timeout.min(Duration::from_secs(1))).await;
        let idle = {
            let state = activity.lock().unwrap_or_else(|error| error.into_inner());
            state.in_flight == 0 && state.last_finished.elapsed() >= timeout
        };
        if idle {
            tracing::info!("exiting after idle timeout");
            return;
        }
    }
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => tracing::error!(%error, "installing SIGTERM handler"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedder_selection_accepts_candle_and_rejects_onnx() {
        assert!(validate_embedder_selection(Err(std::env::VarError::NotPresent)).is_ok());
        assert!(validate_embedder_selection(Ok("candle".into())).is_ok());
        let error = validate_embedder_selection(Ok("onnx".into())).unwrap_err();
        assert!(error.to_string().contains("ONNX embedder was removed"));
        assert!(validate_embedder_selection(Ok("other".into())).is_err());
    }

    #[test]
    fn lock_contention_does_not_overwrite_owner() {
        let path = std::env::temp_dir().join(format!("mementod-lock-test-{}", std::process::id()));
        let lock = acquire_lock(&path).unwrap().unwrap();
        assert!(acquire_lock(&path).unwrap().is_none());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            std::process::id().to_string()
        );
        drop(lock);
        assert!(acquire_lock(&path).unwrap().is_some());
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn idle_waits_for_inflight_request_and_then_exits() {
        let activity = Arc::new(Mutex::new(Activity {
            last_finished: Instant::now() - Duration::from_secs(2),
            in_flight: 1,
        }));
        let task = tokio::spawn(idle_shutdown(activity.clone(), 1));
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(!task.is_finished());
        {
            let mut state = activity.lock().unwrap();
            state.in_flight = 0;
            state.last_finished = Instant::now() - Duration::from_secs(2);
        }
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
    }
}
