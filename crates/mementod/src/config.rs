use std::{env, net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub qdrant_url: String,
    pub token: Option<String>,
    pub collection_prefix: String,
    pub cache_dir: Option<PathBuf>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            bind: env::var("MEMENTO_BIND")
                .unwrap_or_else(|_| "127.0.0.1:7377".into())
                .parse()
                .context("invalid MEMENTO_BIND")?,
            qdrant_url: env::var("MEMENTO_QDRANT_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:6334".into()),
            token: env::var("MEMENTO_TOKEN")
                .ok()
                .filter(|value| !value.is_empty()),
            collection_prefix: env::var("MEMENTO_COLLECTION_PREFIX")
                .unwrap_or_else(|_| "memento".into()),
            cache_dir: env::var_os("FASTEMBED_CACHE_DIR").map(PathBuf::from),
        })
    }
}
