use std::{env, net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub qdrant_url: String,
    pub token: Option<String>,
    pub collection_prefix: String,
    pub cache_dir: Option<PathBuf>,
    pub duplicate_cosine_threshold: f32,
    pub duplicate_jaccard_threshold: f32,
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
            duplicate_cosine_threshold: env::var("MEMENTO_DUPLICATE_COSINE_THRESHOLD")
                .unwrap_or_else(|_| "0.95".into())
                .parse()
                .context("invalid MEMENTO_DUPLICATE_COSINE_THRESHOLD")?,
            duplicate_jaccard_threshold: env::var("MEMENTO_DUPLICATE_JACCARD_THRESHOLD")
                .unwrap_or_else(|_| "0.8".into())
                .parse()
                .context("invalid MEMENTO_DUPLICATE_JACCARD_THRESHOLD")?,
        })
    }
}
