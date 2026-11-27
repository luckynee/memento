use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, anyhow, ensure};
use memento_core::{
    lineage::{Ancestor, SessionKey},
    protocol::{ContinuationRecord, Role, SessionRecord},
    segment::Segment,
};
use qdrant_client::qdrant::{PointId, point_id::PointIdOptions};
use rusqlite::{Connection, params, params_from_iter, types::Value};
use uuid::Uuid;

use crate::{
    embed::Embedder,
    store::{
        CONTINUATION_NAMESPACE, SESSION_NAMESPACE, ScopeFilter, kind_name, prose_kind, role_name,
        shingle_jaccard,
    },
};

pub struct SqliteStore {
    connection: Arc<Mutex<Connection>>,
    embedder: Arc<dyn Embedder>,
}

impl SqliteStore {
    pub async fn open(path: PathBuf, embedder: Arc<dyn Embedder>) -> Result<Self> {
        ensure!(
            embedder.dimensions() == 384,
            "embedding dimensions must be 384"
        );
        let connection = tokio::task::spawn_blocking(move || -> Result<Connection> {
            if path == std::path::Path::new(":memory:") {
                return Connection::open_in_memory().context("opening in-memory SQLite database");
            }
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating SQLite directory {}", parent.display()))?;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)
                    .map(|_| ())
                    .or_else(|error| {
                        if error.kind() == std::io::ErrorKind::AlreadyExists {
                            Ok(())
                        } else {
                            Err(error)
                        }
                    })
                    .with_context(|| format!("creating SQLite database {}", path.display()))?;
            }
            let connection = Connection::open(&path)
                .with_context(|| format!("opening SQLite database {}", path.display()))?;
            connection.busy_timeout(std::time::Duration::from_secs(5))?;
            connection.pragma_update(None, "journal_mode", "WAL")?;
            Ok(connection)
        })
        .await
        .context("joining SQLite initialization")??;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            embedder,
        })
    }

    async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let connection = Arc::clone(&self.connection);
        tokio::task::spawn_blocking(move || {
            let mut connection = connection
                .lock()
                .map_err(|_| anyhow!("SQLite connection lock poisoned"))?;
            operation(&mut connection)
        })
        .await
        .context("joining SQLite operation")?
    }

    pub async fn health(&self) -> Result<()> {
        self.run(|connection| {
            connection
                .query_row("SELECT 1", [], |_| Ok(()))
                .context("checking SQLite health")
        })
        .await
    }

    pub async fn ensure_schema(&self) -> Result<()> {
        self.run(|connection| {
            let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
            ensure!(version <= 1, "unsupported SQLite schema version {version}");
            connection.execute_batch(
                "BEGIN;
                CREATE TABLE IF NOT EXISTS segments (
                    id TEXT PRIMARY KEY, harness TEXT NOT NULL, project TEXT NOT NULL, session TEXT NOT NULL,
                    kind TEXT NOT NULL, text TEXT NOT NULL, source_id TEXT NOT NULL, chunk_index INTEGER NOT NULL,
                    ts TEXT NOT NULL, ts_seconds INTEGER NOT NULL, ts_nanos INTEGER NOT NULL,
                    tool TEXT, path TEXT, paths TEXT NOT NULL, source_role TEXT, provenance TEXT, vector BLOB NOT NULL
                );
                CREATE INDEX IF NOT EXISTS segments_project ON segments(project);
                CREATE INDEX IF NOT EXISTS segments_session ON segments(harness, session);
                CREATE INDEX IF NOT EXISTS segments_kind ON segments(kind);
                CREATE INDEX IF NOT EXISTS segments_source ON segments(source_id);
                CREATE INDEX IF NOT EXISTS segments_ts ON segments(ts_seconds, ts_nanos);
                CREATE VIRTUAL TABLE IF NOT EXISTS segments_fts USING fts5(text, content='segments', content_rowid='rowid', tokenize='porter unicode61');
                CREATE TRIGGER IF NOT EXISTS segments_insert AFTER INSERT ON segments BEGIN
                    INSERT INTO segments_fts(rowid, text) VALUES (new.rowid, new.text);
                END;
                CREATE TRIGGER IF NOT EXISTS segments_delete AFTER DELETE ON segments BEGIN
                    INSERT INTO segments_fts(segments_fts, rowid, text) VALUES ('delete', old.rowid, old.text);
                END;
                CREATE TRIGGER IF NOT EXISTS segments_update AFTER UPDATE ON segments BEGIN
                    INSERT INTO segments_fts(segments_fts, rowid, text) VALUES ('delete', old.rowid, old.text);
                    INSERT INTO segments_fts(rowid, text) VALUES (new.rowid, new.text);
                END;
                CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, harness TEXT NOT NULL, project TEXT NOT NULL, session TEXT NOT NULL, payload TEXT NOT NULL);
                CREATE INDEX IF NOT EXISTS sessions_project ON sessions(project);
                CREATE INDEX IF NOT EXISTS sessions_key ON sessions(harness, session);
                CREATE TABLE IF NOT EXISTS continuations (id TEXT PRIMARY KEY, project TEXT NOT NULL, payload TEXT NOT NULL);
                CREATE INDEX IF NOT EXISTS continuations_project ON continuations(project);
                PRAGMA user_version = 1;
                COMMIT;"
            ).context("creating SQLite schema")
        }).await
    }

    pub async fn upsert_segments(&self, segments: &[Segment]) -> Result<()> {
        for batch in segments.chunks(64) {
            let vectors = self
                .embedder
                .embed_documents(batch.iter().map(|segment| segment.text.clone()).collect())
                .await
                .context("embedding segment batch")?;
            ensure!(
                vectors.len() == batch.len(),
                "embedder returned incorrect vector count"
            );
            let batch = batch.to_vec();
            self.run(move |connection| {
                let transaction = connection.transaction()?;
                for (segment, vector) in batch.iter().zip(vectors) {
                    write_segment(&transaction, segment, &vector, None)?;
                }
                transaction
                    .commit()
                    .context("committing SQLite segment batch")
            })
            .await?;
        }
        Ok(())
    }

    pub async fn ingest_segments(
        &self,
        segments: &[Segment],
        roles: &HashMap<String, Role>,
        lineage: &[Ancestor],
        cosine_threshold: f32,
        jaccard_threshold: f32,
    ) -> Result<(usize, usize)> {
        let filter = ScopeFilter {
            lineage: Some(lineage.to_vec()),
            ..Default::default()
        };
        validate_filter(&filter)?;
        let mut counts = (0, 0);
        for batch in segments.chunks(64) {
            let vectors = self
                .embedder
                .embed_documents(batch.iter().map(|segment| segment.text.clone()).collect())
                .await
                .context("embedding segment batch")?;
            ensure!(
                vectors.len() == batch.len(),
                "embedder returned incorrect vector count"
            );
            let batch = batch.to_vec();
            let roles = roles.clone();
            let filter = filter.clone();
            let result = self
                .run(move |connection| {
                    let mut counts = (0, 0);
                    for (segment, vector) in batch.iter().zip(vectors) {
                        ensure!(
                            vector.len() == 384,
                            "embedder returned incorrect vector dimensions"
                        );
                        let role = roles.get(&segment.source_id).copied();
                        let duplicate = if prose_kind(segment.kind) {
                            let mut candidates = load_segments(connection, &filter)?;
                            candidates.retain(|candidate| {
                                candidate.segment.project == segment.project
                                    && candidate.segment.kind == segment.kind
                                    && candidate.source_role.as_deref() == Some(role_name(role))
                                    && candidate.provenance.as_deref()
                                        == Some(segment.tool.as_deref().unwrap_or_default())
                                    && candidate.segment.source_id != segment.source_id
                            });
                            candidates.sort_by(|a, b| {
                                cosine(&vector, &b.vector)
                                    .total_cmp(&cosine(&vector, &a.vector))
                                    .then(a.segment.id.cmp(&b.segment.id))
                            });
                            candidates.iter().take(3).any(|candidate| {
                                cosine(&vector, &candidate.vector) >= cosine_threshold
                                    && shingle_jaccard(&segment.text, &candidate.segment.text)
                                        >= jaccard_threshold
                            })
                        } else {
                            false
                        };
                        if duplicate {
                            connection.execute(
                                "DELETE FROM segments WHERE id = ?1",
                                [segment.id.to_string()],
                            )?;
                            counts.1 += 1;
                        } else {
                            write_segment(connection, segment, &vector, Some(role_name(role)))?;
                            counts.0 += 1;
                        }
                    }
                    Ok(counts)
                })
                .await?;
            counts.0 += result.0;
            counts.1 += result.1;
        }
        Ok(counts)
    }

    pub async fn delete_stale_chunks(
        &self,
        harness: &str,
        session: &str,
        source_id: &str,
        count: u32,
    ) -> Result<()> {
        let values = (harness.to_owned(), session.to_owned(), source_id.to_owned());
        self.run(move |connection| {
            connection.execute("DELETE FROM segments WHERE harness=?1 AND session=?2 AND source_id=?3 AND chunk_index>=?4", params![values.0, values.1, values.2, count])?;
            Ok(())
        }).await
    }

    pub async fn hybrid_candidates(
        &self,
        query: &str,
        filter: &ScopeFilter,
        limit: u64,
    ) -> Result<Vec<(Segment, f32)>> {
        validate_filter(filter)?;
        let vector = self
            .embedder
            .embed_query(query.to_owned())
            .await
            .context("embedding query")?;
        ensure!(
            vector.len() == 384,
            "embedder returned incorrect vector dimensions"
        );
        let filter = filter.clone();
        let lexical_query = fts_query(query);
        self.run(move |connection| {
            let candidates = load_segments(connection, &filter)?;
            let mut dense: Vec<_> = candidates.iter().collect();
            dense.sort_by(|a, b| cosine(&vector, &b.vector).total_cmp(&cosine(&vector, &a.vector)).then(a.segment.id.cmp(&b.segment.id)));
            let mut scores = HashMap::<Uuid, (Segment, f32)>::new();
            for (rank, candidate) in dense.into_iter().take(50).enumerate() {
                scores.insert(candidate.segment.id, (candidate.segment.clone(), 1.0 / (rank + 2) as f32));
            }
            if !lexical_query.is_empty() {
                let (condition, mut values) = sql_filter(&filter)?;
                values.insert(0, Value::Text(lexical_query));
                let sql = format!("SELECT segments.id FROM segments_fts JOIN segments ON segments.rowid=segments_fts.rowid WHERE segments_fts MATCH ? AND {condition} ORDER BY bm25(segments_fts), segments.id LIMIT 50");
                let mut statement = connection.prepare(&sql)?;
                let ids = statement.query_map(params_from_iter(values), |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
                for (rank, id) in ids.into_iter().enumerate() {
                    let id = Uuid::parse_str(&id)?;
                    if let Some((_, score)) = scores.get_mut(&id) {
                        *score += 1.0 / (rank + 2) as f32;
                    } else {
                        let segment = candidates.iter().find(|segment| segment.segment.id == id).context("missing lexical segment")?.segment.clone();
                        scores.insert(id, (segment, 1.0 / (rank + 2) as f32));
                    }
                }
            }
            let mut result: Vec<_> = scores.into_values().collect();
            result.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.id.cmp(&b.0.id)));
            result.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
            Ok(result)
        }).await
    }

    pub async fn scroll_segments(&self, filter: &ScopeFilter) -> Result<Vec<Segment>> {
        let filter = filter.clone();
        self.run(move |connection| {
            Ok(load_segments(connection, &filter)?
                .into_iter()
                .map(|item| item.segment)
                .collect())
        })
        .await
    }

    pub async fn count_segments(&self, filter: &ScopeFilter) -> Result<u64> {
        let filter = filter.clone();
        self.run(move |connection| {
            let (condition, values) = sql_filter(&filter)?;
            let count: i64 = connection
                .query_row(
                    &format!("SELECT count(*) FROM segments WHERE {condition}"),
                    params_from_iter(values),
                    |row| row.get(0),
                )
                .context("counting SQLite segments")?;
            u64::try_from(count).context("invalid SQLite segment count")
        })
        .await
    }

    pub async fn delete_segments(&self, filter: &ScopeFilter) -> Result<u64> {
        let filter = filter.clone();
        self.run(move |connection| {
            let (condition, values) = sql_filter(&filter)?;
            Ok(connection.execute(
                &format!("DELETE FROM segments WHERE {condition}"),
                params_from_iter(values),
            )? as u64)
        })
        .await
    }

    pub async fn delete_sessions(&self, filter: &ScopeFilter) -> Result<()> {
        let filter = filter.clone();
        self.run(move |connection| {
            let (condition, values) = sql_filter(&filter)?;
            connection.execute(
                &format!("DELETE FROM sessions WHERE {condition}"),
                params_from_iter(values),
            )?;
            Ok(())
        })
        .await
    }

    pub async fn count_by_kind(&self, filter: &ScopeFilter) -> Result<BTreeMap<String, u64>> {
        let mut counts = BTreeMap::new();
        for segment in self.scroll_segments(filter).await? {
            *counts.entry(kind_name(&segment.kind)?).or_insert(0) += 1;
        }
        Ok(counts)
    }

    pub async fn upsert_session(&self, session: SessionRecord) -> Result<()> {
        self.run(move |connection| {
            let id = Uuid::new_v5(&SESSION_NAMESPACE, format!("{}/{}", session.harness, session.session).as_bytes()).to_string();
            let payload = serde_json::to_string(&session)?;
            connection.execute("INSERT INTO sessions(id,harness,project,session,payload) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET harness=excluded.harness,project=excluded.project,session=excluded.session,payload=excluded.payload", params![id, session.harness, session.project, session.session, payload])?;
            Ok(())
        }).await
    }

    pub async fn get_session(&self, harness: &str, session: &str) -> Result<Option<SessionRecord>> {
        let sessions = self
            .list_sessions_filtered(ScopeFilter {
                sessions: Some(vec![SessionKey {
                    harness: harness.into(),
                    session: session.into(),
                }]),
                ..Default::default()
            })
            .await?;
        ensure!(sessions.len() <= 1, "duplicate session records");
        Ok(sessions.into_iter().next())
    }

    pub async fn list_sessions(
        &self,
        harness: &str,
        project: Option<&str>,
    ) -> Result<Vec<SessionRecord>> {
        self.list_sessions_filtered(ScopeFilter {
            harness: Some(harness.into()),
            project: project.map(str::to_owned),
            ..Default::default()
        })
        .await
    }

    pub async fn session_links(&self) -> Result<Vec<(SessionKey, Option<SessionKey>)>> {
        Ok(self
            .list_sessions_filtered(ScopeFilter::default())
            .await?
            .into_iter()
            .map(|session| {
                let parent = session.parent.map(|parent| SessionKey {
                    harness: session
                        .parent_harness
                        .unwrap_or_else(|| session.harness.clone()),
                    session: parent,
                });
                (
                    SessionKey {
                        harness: session.harness,
                        session: session.session,
                    },
                    parent,
                )
            })
            .collect())
    }

    pub async fn upsert_continuation(&self, record: ContinuationRecord) -> Result<()> {
        let legacy_id = Uuid::new_v5(&CONTINUATION_NAMESPACE, record.project.as_bytes());
        if self
            .list_continuations(&record.project)
            .await?
            .iter()
            .any(|(id, existing)| {
                id == &PointId::from(legacy_id.to_string())
                    && existing.target_harness == record.target_harness
            })
        {
            self.delete_continuation(legacy_id.to_string().into())
                .await?;
        }
        let id = Uuid::new_v5(
            &CONTINUATION_NAMESPACE,
            format!(
                "{}\0{}",
                record.project,
                record.target_harness.as_deref().unwrap_or_default()
            )
            .as_bytes(),
        );
        self.write_continuation(id.to_string().into(), record).await
    }

    pub async fn write_continuation(&self, id: PointId, record: ContinuationRecord) -> Result<()> {
        let id = point_id(id)?;
        self.run(move |connection| {
            let payload = serde_json::to_string(&record)?;
            connection.execute("INSERT INTO continuations(id,project,payload) VALUES (?1,?2,?3) ON CONFLICT(id) DO UPDATE SET project=excluded.project,payload=excluded.payload", params![id, record.project, payload])?;
            Ok(())
        }).await
    }

    pub async fn list_continuations(
        &self,
        project: &str,
    ) -> Result<Vec<(PointId, ContinuationRecord)>> {
        let project = project.to_owned();
        self.run(move |connection| {
            let mut statement = connection
                .prepare("SELECT id,payload FROM continuations WHERE project=?1 ORDER BY id")?;
            let rows = statement.query_map([project], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.map(|row| {
                let (id, payload) = row?;
                Ok((
                    id.into(),
                    serde_json::from_str(&payload).context("decoding SQLite continuation")?,
                ))
            })
            .collect()
        })
        .await
    }

    pub async fn delete_continuation(&self, id: PointId) -> Result<()> {
        let id = point_id(id)?;
        self.run(move |connection| {
            connection.execute("DELETE FROM continuations WHERE id=?1", [id])?;
            Ok(())
        })
        .await
    }

    pub async fn delete_project_continuations(&self, project: &str) -> Result<()> {
        let project = project.to_owned();
        self.run(move |connection| {
            connection.execute("DELETE FROM continuations WHERE project=?1", [project])?;
            Ok(())
        })
        .await
    }

    pub async fn list_sessions_filtered(&self, filter: ScopeFilter) -> Result<Vec<SessionRecord>> {
        self.run(move |connection| {
            let (condition, values) = sql_filter(&filter)?;
            let mut statement = connection.prepare(&format!(
                "SELECT payload FROM sessions WHERE {condition} ORDER BY id"
            ))?;
            let rows =
                statement.query_map(params_from_iter(values), |row| row.get::<_, String>(0))?;
            rows.map(|row| serde_json::from_str(&row?).context("decoding SQLite session"))
                .collect()
        })
        .await
    }
}

fn point_id(id: PointId) -> Result<String> {
    match id.point_id_options {
        Some(PointIdOptions::Uuid(id)) => Ok(id),
        Some(PointIdOptions::Num(id)) => Ok(id.to_string()),
        None => Err(anyhow!("missing continuation point id")),
    }
}

fn validate_filter(filter: &ScopeFilter) -> Result<()> {
    if let Some(sessions) = &filter.sessions {
        ensure!(
            !sessions.is_empty(),
            "empty session filter would match nothing"
        );
    }
    if let Some(lineage) = &filter.lineage {
        ensure!(
            !lineage.is_empty(),
            "empty lineage filter would match nothing"
        );
    }
    if let Some(kinds) = &filter.kinds {
        ensure!(!kinds.is_empty(), "empty kind filter would match nothing");
    }
    Ok(())
}

fn sql_filter(filter: &ScopeFilter) -> Result<(String, Vec<Value>)> {
    validate_filter(filter)?;
    let mut clauses = vec!["1".to_owned()];
    let mut values = Vec::new();
    for (column, value) in [
        ("harness", &filter.harness),
        ("project", &filter.project),
        ("session", &filter.session),
    ] {
        if let Some(value) = value {
            clauses.push(format!("{column} = ?"));
            values.push(Value::Text(value.clone()));
        }
    }
    if let Some(sessions) = &filter.sessions {
        let mut pairs = Vec::new();
        for session in sessions {
            pairs.push("(harness = ? AND session = ?)");
            values.push(Value::Text(session.harness.clone()));
            values.push(Value::Text(session.session.clone()));
        }
        clauses.push(format!("({})", pairs.join(" OR ")));
    }
    if let Some(lineage) = &filter.lineage {
        let mut pairs = Vec::new();
        for ancestor in lineage {
            let mut pair = "(harness = ? AND session = ?".to_owned();
            values.push(Value::Text(ancestor.key.harness.clone()));
            values.push(Value::Text(ancestor.key.session.clone()));
            if let Some(cutoff) = ancestor.cutoff {
                pair.push_str(" AND (ts_seconds < ? OR (ts_seconds = ? AND ts_nanos <= ?))");
                values.push(Value::Integer(cutoff.timestamp()));
                values.push(Value::Integer(cutoff.timestamp()));
                values.push(Value::Integer(i64::from(cutoff.timestamp_subsec_nanos())));
            }
            pair.push(')');
            pairs.push(pair);
        }
        clauses.push(format!("({})", pairs.join(" OR ")));
    }
    if let Some(kinds) = &filter.kinds {
        clauses.push(format!("kind IN ({})", vec!["?"; kinds.len()].join(",")));
        for kind in kinds {
            values.push(Value::Text(kind_name(kind)?));
        }
    }
    Ok((clauses.join(" AND "), values))
}

struct StoredSegment {
    segment: Segment,
    vector: Vec<f32>,
    source_role: Option<String>,
    provenance: Option<String>,
}

fn write_segment(
    connection: &Connection,
    segment: &Segment,
    vector: &[f32],
    role: Option<&str>,
) -> Result<()> {
    ensure!(
        vector.len() == 384,
        "embedder returned incorrect vector dimensions"
    );
    let vector: Vec<u8> = vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let paths = serde_json::to_string(&segment.paths)?;
    let provenance = role.map(|_| segment.tool.as_deref().unwrap_or_default());
    connection.execute(
        "INSERT INTO segments(id,harness,project,session,kind,text,source_id,chunk_index,ts,ts_seconds,ts_nanos,tool,path,paths,source_role,provenance,vector)
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)
        ON CONFLICT(id) DO UPDATE SET harness=excluded.harness,project=excluded.project,session=excluded.session,kind=excluded.kind,text=excluded.text,source_id=excluded.source_id,chunk_index=excluded.chunk_index,ts=excluded.ts,ts_seconds=excluded.ts_seconds,ts_nanos=excluded.ts_nanos,tool=excluded.tool,path=excluded.path,paths=excluded.paths,source_role=excluded.source_role,provenance=excluded.provenance,vector=excluded.vector",
        params![segment.id.to_string(), segment.harness, segment.project, segment.session, kind_name(&segment.kind)?, segment.text, segment.source_id, segment.chunk_index, segment.ts.to_rfc3339(), segment.ts.timestamp(), segment.ts.timestamp_subsec_nanos(), segment.tool, segment.path, paths, role, provenance, vector],
    ).context("upserting SQLite segment")?;
    Ok(())
}

fn load_segments(connection: &Connection, filter: &ScopeFilter) -> Result<Vec<StoredSegment>> {
    let (condition, values) = sql_filter(filter)?;
    let sql = format!(
        "SELECT id,harness,project,session,kind,text,source_id,chunk_index,ts,tool,path,paths,source_role,provenance,vector FROM segments WHERE {condition} ORDER BY id"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(values), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, u32>(7)?,
            row.get::<_, String>(8)?,
            row.get::<_, Option<String>>(9)?,
            row.get::<_, Option<String>>(10)?,
            row.get::<_, String>(11)?,
            row.get::<_, Option<String>>(12)?,
            row.get::<_, Option<String>>(13)?,
            row.get::<_, Vec<u8>>(14)?,
        ))
    })?;
    rows.map(|row| {
        let (
            id,
            harness,
            project,
            session,
            kind,
            text,
            source_id,
            chunk_index,
            ts,
            tool,
            path,
            paths,
            source_role,
            provenance,
            vector,
        ) = row?;
        ensure!(vector.len() == 384 * 4, "invalid SQLite vector length");
        Ok(StoredSegment {
            segment: Segment {
                id: Uuid::parse_str(&id)?,
                harness,
                project,
                session,
                kind: serde_json::from_value(serde_json::Value::String(kind))?,
                text,
                source_id,
                chunk_index,
                ts: ts.parse()?,
                tool,
                path,
                paths: serde_json::from_str(&paths)?,
            },
            vector: vector
                .as_chunks::<4>()
                .0
                .iter()
                .map(|chunk| f32::from_le_bytes(*chunk))
                .collect(),
            source_role,
            provenance,
        })
    })
    .collect()
}

fn cosine(left: &[f32], right: &[f32]) -> f32 {
    let dot: f32 = left.iter().zip(right).map(|(a, b)| a * b).sum();
    let left_norm: f32 = left.iter().map(|value| value * value).sum();
    let right_norm: f32 = right.iter().map(|value| value * value).sum();
    if left_norm == 0.0 || right_norm == 0.0 {
        0.0
    } else {
        dot / (left_norm.sqrt() * right_norm.sqrt())
    }
}

fn fts_query(query: &str) -> String {
    query
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::FakeEmbedder;
    use memento_core::protocol::Kind;

    #[tokio::test]
    async fn fts_sanitizes_operators_and_empty_queries() {
        let store = SqliteStore::open(PathBuf::from(":memory:"), Arc::new(FakeEmbedder))
            .await
            .unwrap();
        store.ensure_schema().await.unwrap();
        for query in [
            "\"quoted\"",
            "AND OR NOT",
            "NEAR(alpha beta)",
            "*",
            "field:value",
            "",
        ] {
            store
                .hybrid_candidates(query, &ScopeFilter::default(), 50)
                .await
                .unwrap();
        }
        assert_eq!(
            fts_query("\"foo\" AND NEAR(bar)*:baz"),
            "\"foo\" OR \"AND\" OR \"NEAR\" OR \"bar\" OR \"baz\""
        );
        assert!(fts_query("* : \"").is_empty());
    }

    #[tokio::test]
    async fn lineage_cutoff_is_inclusive_and_subsecond_precise() {
        let store = SqliteStore::open(PathBuf::from(":memory:"), Arc::new(FakeEmbedder))
            .await
            .unwrap();
        store.ensure_schema().await.unwrap();
        let segment = |id: &str, timestamp: &str| Segment {
            id: Uuid::new_v5(&Uuid::NAMESPACE_OID, id.as_bytes()),
            harness: "h".into(),
            project: "p".into(),
            session: "parent".into(),
            kind: Kind::FileRef,
            text: id.into(),
            source_id: id.into(),
            chunk_index: 0,
            ts: timestamp.parse().unwrap(),
            tool: None,
            path: None,
            paths: vec![],
        };
        let boundary = segment("boundary", "2026-01-01T00:00:00.500Z");
        let after = segment("after", "2026-01-01T00:00:00.501Z");
        store
            .upsert_segments(&[boundary.clone(), after])
            .await
            .unwrap();
        let filter = ScopeFilter {
            lineage: Some(vec![Ancestor {
                key: SessionKey {
                    harness: "h".into(),
                    session: "parent".into(),
                },
                cutoff: Some("2026-01-01T00:00:00.500Z".parse().unwrap()),
            }]),
            ..Default::default()
        };
        assert_eq!(
            store.scroll_segments(&filter).await.unwrap(),
            vec![boundary]
        );
    }
}
