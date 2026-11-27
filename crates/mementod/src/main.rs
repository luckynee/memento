use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use mementod::{
    config::{Config, cache_dir, sqlite_path},
    embed::{create_embedder, default_embedder},
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
    let cache = cache_dir()?;
    unsafe { std::env::set_var("FASTEMBED_CACHE_DIR", &cache) };
    tokio::runtime::Runtime::new()
        .context("creating async runtime")?
        .block_on(run())
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    let config = Config::from_env()?;
    let selection = std::env::var("MEMENTO_EMBEDDER").unwrap_or_else(|_| default_embedder().into());
    let embedder = create_embedder(&selection, config.cache_dir.clone()).await?;
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
    let router = router_with_state(state);
    tracing::info!(address = %config.bind, "serving HTTP");
    let server_result = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown().await;
            drop(sweep_shutdown);
        })
        .await;
    if let Some(handle) = sweep_handle {
        handle.await.context("joining background stale sweep")?;
    }
    server_result.context("serving HTTP")?;
    Ok(())
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
