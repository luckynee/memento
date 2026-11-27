use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, ensure};
use qdrant_client::{
    Payload, Qdrant,
    qdrant::{PointId, ScrollPointsBuilder, point_id::PointIdOptions},
};
use rusqlite::{Connection, OpenFlags};

use crate::{embed::Embedder, sqlite_store::SqliteStore, store::segment_from_payload};

const COLLECTIONS: [&str; 3] = ["segments", "sessions", "continuations"];
const SEGMENT_FIELDS: [&str; 11] = [
    "harness",
    "project",
    "session",
    "kind",
    "text",
    "source_id",
    "chunk_index",
    "ts",
    "tool",
    "path",
    "paths",
];

#[derive(Debug)]
pub struct MigrationReport {
    pub counts: [u64; 3],
    pub ignored_segment_fields: BTreeMap<String, u64>,
    pub preserved_segment_fields: BTreeMap<String, u64>,
    pub elapsed: Duration,
}

struct TemporaryDatabase(PathBuf);

impl Drop for TemporaryDatabase {
    fn drop(&mut self) {
        for path in [
            &self.0,
            &sidecar(&self.0, "-wal"),
            &sidecar(&self.0, "-shm"),
        ] {
            let _ = fs::remove_file(path);
        }
    }
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    name.into()
}

fn ensure_no_sidecars(path: &Path) -> Result<()> {
    for suffix in ["-wal", "-shm"] {
        ensure!(
            !sidecar(path, suffix).exists(),
            "destination has SQLite {suffix} sidecar; stop the daemon first"
        );
    }
    Ok(())
}

fn lock_destination(path: &Path, force: bool) -> Result<Option<Connection>> {
    ensure_no_sidecars(path)?;
    let exists = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                !metadata.file_type().is_symlink(),
                "destination cannot be a symbolic link"
            );
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error).context("checking migration destination"),
    };
    if !exists {
        return Ok(None);
    }
    ensure!(
        force,
        "destination already exists; use --force after stopping the daemon"
    );
    ensure!(path.is_file(), "destination is not a regular file");
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .with_context(|| {
            format!(
                "opening destination {}; stop the daemon first",
                path.display()
            )
        })?;
    connection.busy_timeout(Duration::from_millis(250))?;
    connection
        .execute_batch("BEGIN EXCLUSIVE")
        .context("locking destination; stop the daemon first")?;
    ensure_no_sidecars(path)?;
    Ok(Some(connection))
}

fn point_id(id: Option<PointId>) -> Result<String> {
    match id.and_then(|id| id.point_id_options) {
        Some(PointIdOptions::Uuid(id)) => Ok(id),
        Some(PointIdOptions::Num(id)) => Ok(id.to_string()),
        None => Err(anyhow!("missing Qdrant point id")),
    }
}

pub async fn migrate(
    qdrant_url: &str,
    prefix: &str,
    destination: &Path,
    force: bool,
    embedder: Arc<dyn Embedder>,
) -> Result<MigrationReport> {
    let started = Instant::now();
    ensure!(
        destination != Path::new(":memory:"),
        "migration requires a file destination"
    );
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let destination_lock = lock_destination(destination, force)?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let temporary_path = parent.join(format!(
        ".memento-migrate-{}-{nonce}.db",
        std::process::id()
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(&temporary_path)
        .context("creating temporary migration database")?;
    let temporary = TemporaryDatabase(temporary_path);
    let store = SqliteStore::open(temporary.0.clone(), embedder).await?;
    store.ensure_schema().await?;
    let client = Qdrant::from_url(qdrant_url)
        .skip_compatibility_check()
        .build()
        .context("configuring migration Qdrant client")?;
    let mut counts = [0; 3];
    let mut ignored_segment_fields = BTreeMap::new();
    let mut preserved_segment_fields = BTreeMap::new();
    for (index, collection) in COLLECTIONS.iter().enumerate() {
        let name = format!("{prefix}_{collection}");
        let mut offset = None;
        loop {
            let mut request = ScrollPointsBuilder::new(&name)
                .limit(64u32)
                .with_payload(true)
                .with_vectors(false);
            if let Some(id) = offset {
                request = request.offset(id);
            }
            let page = client
                .scroll(request)
                .await
                .with_context(|| format!("scrolling {name}"))?;
            let mut segments = Vec::new();
            for point in page.result {
                counts[index] += 1;
                if *collection == "segments" {
                    for key in point
                        .payload
                        .keys()
                        .filter(|key| !SEGMENT_FIELDS.contains(&key.as_str()))
                    {
                        let target = if matches!(key.as_str(), "source_role" | "provenance") {
                            &mut preserved_segment_fields
                        } else {
                            &mut ignored_segment_fields
                        };
                        *target.entry(key.clone()).or_insert(0) += 1;
                    }
                    let json = serde_json::Value::from(Payload::from(point.payload.clone()));
                    let optional_string = |key: &str| -> Result<Option<String>> {
                        match json.get(key) {
                            None | Some(serde_json::Value::Null) => Ok(None),
                            Some(serde_json::Value::String(value)) => Ok(Some(value.clone())),
                            Some(_) => Err(anyhow!("segment {key} must be a string or null")),
                        }
                    };
                    let role = optional_string("source_role")?;
                    let provenance = optional_string("provenance")?;
                    let segment = segment_from_payload(point.id, point.payload)
                        .context("decoding migrated segment")?;
                    segments.push((segment, role, provenance));
                } else {
                    let id = point_id(point.id)?;
                    let payload = serde_json::Value::from(Payload::from(point.payload));
                    store
                        .migrate_payload(collection, id, payload)
                        .await
                        .with_context(|| format!("writing {name} point"))?;
                }
            }
            if !segments.is_empty() {
                store
                    .migrate_segments(segments)
                    .await
                    .context("writing migrated segments")?;
            }
            offset = page.next_page_offset;
            if offset.is_none() {
                break;
            }
        }
    }
    let written = store.migration_counts().await?;
    ensure!(
        counts == written,
        "migration count mismatch: source {counts:?}, destination {written:?}"
    );
    store.finish_migration().await?;
    drop(store);
    ensure_no_sidecars(destination)?;
    if destination_lock.is_none() {
        ensure!(
            !destination.exists(),
            "destination appeared during migration; stop the daemon first"
        );
        fs::hard_link(&temporary.0, destination)
            .with_context(|| format!("installing {}", destination.display()))?;
    } else {
        fs::rename(&temporary.0, destination)
            .with_context(|| format!("replacing {}", destination.display()))?;
    }
    drop(destination_lock);
    Ok(MigrationReport {
        counts,
        ignored_segment_fields,
        preserved_segment_fields,
        elapsed: started.elapsed(),
    })
}
