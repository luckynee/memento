use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use mementod::{
    config::Config,
    embed::FastEmbedder,
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
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    let config = Config::from_env()?;
    let embedder = Arc::new(FastEmbedder::new(config.cache_dir.clone()).await?);
    if matches!(cli.command, Command::Warmup) {
        return Ok(());
    }
    let client = Qdrant::from_url(&config.qdrant_url)
        .build()
        .context("configuring Qdrant client")?;
    let store = Store::new(client.clone(), &config.collection_prefix, embedder);
    store
        .ensure_schema()
        .await
        .context("ensuring Qdrant schema")?;
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
