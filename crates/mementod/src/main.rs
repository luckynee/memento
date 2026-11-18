use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{Json, Router, extract::State, routing::get};
use clap::{Parser, Subcommand};
use memento_core::protocol::Health;
use mementod::{config::Config, embed::FastEmbedder, store::Store};
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
    let router = Router::new()
        .route("/health", get(health))
        .with_state(client);
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .context("binding HTTP listener")?;
    tracing::info!(address = %config.bind, "serving health endpoint");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown())
        .await
        .context("serving HTTP")?;
    Ok(())
}

async fn health(State(client): State<Qdrant>) -> Json<Health> {
    Json(Health {
        ok: true,
        qdrant: client.health_check().await.is_ok(),
    })
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
