use std::{env, net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub store: String,
    pub sqlite_path: PathBuf,
    pub qdrant_url: String,
    pub token: Option<String>,
    pub collection_prefix: String,
    pub cache_dir: Option<PathBuf>,
    pub duplicate_cosine_threshold: f32,
    pub duplicate_jaccard_threshold: f32,
    pub stale_secs: u64,
    pub stale_sweep_secs: u64,
    pub idle_exit_secs: u64,
    pub lock_path: Option<PathBuf>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let store = match env::var("MEMENTO_STORE")
            .unwrap_or_else(|_| "qdrant".into())
            .as_str()
        {
            "qdrant" => "qdrant".to_owned(),
            "sqlite" => "sqlite".to_owned(),
            value => anyhow::bail!("invalid MEMENTO_STORE: {value}"),
        };
        let sqlite_path = if store == "sqlite" {
            sqlite_path()?
        } else {
            PathBuf::new()
        };
        Ok(Self {
            bind: env::var("MEMENTO_BIND")
                .unwrap_or_else(|_| "127.0.0.1:7377".into())
                .parse()
                .context("invalid MEMENTO_BIND")?,
            store,
            sqlite_path,
            qdrant_url: env::var("MEMENTO_QDRANT_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:6334".into()),
            token: env::var("MEMENTO_TOKEN")
                .ok()
                .filter(|value| !value.is_empty()),
            collection_prefix: env::var("MEMENTO_COLLECTION_PREFIX")
                .unwrap_or_else(|_| "memento".into()),
            cache_dir: Some(cache_dir()?),
            duplicate_cosine_threshold: env::var("MEMENTO_DUPLICATE_COSINE_THRESHOLD")
                .unwrap_or_else(|_| "0.95".into())
                .parse()
                .context("invalid MEMENTO_DUPLICATE_COSINE_THRESHOLD")?,
            duplicate_jaccard_threshold: env::var("MEMENTO_DUPLICATE_JACCARD_THRESHOLD")
                .unwrap_or_else(|_| "0.8".into())
                .parse()
                .context("invalid MEMENTO_DUPLICATE_JACCARD_THRESHOLD")?,
            stale_secs: stale_seconds(
                env::var("MEMENTO_STALE_SECS").ok().as_deref(),
                env::var("MEMENTO_STALE_DAYS").ok().as_deref(),
            )?,
            stale_sweep_secs: env::var("MEMENTO_STALE_SWEEP_SECS")
                .unwrap_or_else(|_| "3600".into())
                .parse::<u64>()
                .context("invalid MEMENTO_STALE_SWEEP_SECS")?
                .max(1),
            idle_exit_secs: env::var("MEMENTO_IDLE_EXIT_SECS")
                .unwrap_or_else(|_| "0".into())
                .parse()
                .context("invalid MEMENTO_IDLE_EXIT_SECS")?,
            lock_path: env::var_os("MEMENTO_LOCK_PATH")
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
        })
    }
}

pub fn sqlite_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("MEMENTO_SQLITE_PATH") {
        return Ok(path.into());
    }
    let data = match env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => {
            PathBuf::from(env::var_os("HOME").context("HOME is required for default SQLite path")?)
                .join(".local/share")
        }
    };
    Ok(data.join("memento/memento.db"))
}

pub fn cache_dir() -> Result<PathBuf> {
    if let Some(path) = env::var_os("FASTEMBED_CACHE_DIR") {
        return Ok(path.into());
    }
    let cache = match env::var_os("XDG_CACHE_HOME").filter(|value| !value.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => {
            PathBuf::from(env::var_os("HOME").context("HOME is required for default model cache")?)
                .join(".cache")
        }
    };
    Ok(cache.join("memento/models"))
}

fn stale_seconds(seconds: Option<&str>, days: Option<&str>) -> Result<u64> {
    if let Some(seconds) = seconds.and_then(|value| value.parse::<u64>().ok()) {
        return Ok(seconds);
    }
    let days = days
        .unwrap_or("7")
        .parse::<u64>()
        .context("invalid MEMENTO_STALE_DAYS")?;
    days.checked_mul(86_400)
        .context("MEMENTO_STALE_DAYS is too large")
}

#[cfg(test)]
mod tests {
    use super::stale_seconds;

    #[test]
    fn stale_seconds_precedence_and_parsing() {
        assert_eq!(stale_seconds(None, None).unwrap(), 604_800);
        assert_eq!(stale_seconds(Some("2"), Some("invalid")).unwrap(), 2);
        assert_eq!(stale_seconds(Some("0"), Some("7")).unwrap(), 0);
        assert_eq!(stale_seconds(Some("-1"), Some("2")).unwrap(), 172_800);
        assert_eq!(stale_seconds(Some("bad"), None).unwrap(), 604_800);
        assert!(stale_seconds(None, Some("bad")).is_err());
    }
}
