use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use mementod::{
    config::Config,
    embed::FastEmbedder,
    routes::{AppState, backfill_activity, router},
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
    let router = router(AppState {
        store: Arc::new(store),
        config: config.clone(),
        policy: Default::default(),
        rerank: Default::default(),
    });
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .context("binding HTTP listener")?;
    tracing::info!(address = %config.bind, "serving HTTP");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown())
        .await
        .context("serving HTTP")?;
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
