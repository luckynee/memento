use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, ensure};
use memento_core::{
    lineage::{Ancestor, SessionKey},
    protocol::{ContinuationRecord, Kind, Role, SessionRecord},
    segment::Segment,
};
use qdrant_client::{
    Payload, Qdrant,
    qdrant::{
        Condition, CountPointsBuilder, CreateCollectionBuilder, CreateFieldIndexCollectionBuilder,
        DatetimeRange, DeletePointsBuilder, Distance, Document, DocumentBuilder, FieldType, Filter,
        Fusion, KeywordIndexParamsBuilder, Modifier, PointId, PointStruct, PrefetchQueryBuilder,
        Query, QueryPointsBuilder, Range, ScrollPointsBuilder, SparseVectorParamsBuilder,
        SparseVectorsConfigBuilder, UpsertPointsBuilder, Vector, VectorParamsBuilder,
        VectorsConfigBuilder, point_id::PointIdOptions,
    },
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::embed::Embedder;

pub(crate) const SESSION_NAMESPACE: Uuid = Uuid::from_u128(0x2c450e36_854d_5a3c_aab2_743e57e955b8);
pub(crate) const CONTINUATION_NAMESPACE: Uuid =
    Uuid::from_u128(0x8bcc899f_a24d_53d6_83e1_80669ad0e156);

#[derive(Debug, Clone, Default)]
pub struct ScopeFilter {
    pub harness: Option<String>,
    pub project: Option<String>,
    pub session: Option<String>,
    pub sessions: Option<Vec<SessionKey>>,
    pub lineage: Option<Vec<Ancestor>>,
    pub kinds: Option<Vec<Kind>>,
}

impl ScopeFilter {
    pub fn to_filter(&self) -> Result<Filter> {
        let mut conditions = Vec::new();
        if let Some(harness) = &self.harness {
            conditions.push(Condition::matches("harness", harness.clone()));
        }
        if let Some(project) = &self.project {
            conditions.push(Condition::matches("project", project.clone()));
        }
        if let Some(session) = &self.session {
            conditions.push(Condition::matches("session", session.clone()));
        }
        if let Some(sessions) = &self.sessions {
            ensure!(
                !sessions.is_empty(),
                "empty session filter would match nothing"
            );
            let pairs = sessions.iter().map(|session| {
                Condition::from(Filter::must([
                    Condition::matches("harness", session.harness.clone()),
                    Condition::matches("session", session.session.clone()),
                ]))
            });
            conditions.push(Condition::from(Filter::should(pairs)));
        }
        if let Some(lineage) = &self.lineage {
            ensure!(
                !lineage.is_empty(),
                "empty lineage filter would match nothing"
            );
            let pairs = lineage.iter().map(|ancestor| {
                let mut conditions = vec![
                    Condition::matches("harness", ancestor.key.harness.clone()),
                    Condition::matches("session", ancestor.key.session.clone()),
                ];
                if let Some(cutoff) = ancestor.cutoff {
                    conditions.push(Condition::datetime_range(
                        "ts",
                        DatetimeRange {
                            lte: Some(qdrant_client::qdrant::Timestamp {
                                seconds: cutoff.timestamp(),
                                nanos: cutoff.timestamp_subsec_nanos() as i32,
                            }),
                            ..Default::default()
                        },
                    ));
                }
                Condition::from(Filter::must(conditions))
            });
            conditions.push(Condition::from(Filter::should(pairs)));
        }
        if let Some(kinds) = &self.kinds {
            ensure!(!kinds.is_empty(), "empty kind filter would match nothing");
            let names = kinds.iter().map(kind_name).collect::<Result<Vec<_>>>()?;
            conditions.push(Condition::matches("kind", names));
        }
        Ok(Filter::must(conditions))
    }
}

pub enum Store {
    Qdrant(QdrantStore),
    Sqlite(crate::sqlite_store::SqliteStore),
}

macro_rules! dispatch {
    ($self:expr, $method:ident ( $($arg:expr),* )) => {
        match $self {
            Store::Qdrant(store) => store.$method($($arg),*).await,
            Store::Sqlite(store) => store.$method($($arg),*).await,
        }
    };
}

impl Store {
    pub fn new(client: Qdrant, prefix: &str, embedder: Arc<dyn Embedder>) -> Self {
        Self::Qdrant(QdrantStore::new(client, prefix, embedder))
    }

    pub async fn sqlite(path: std::path::PathBuf, embedder: Arc<dyn Embedder>) -> Result<Self> {
        Ok(Self::Sqlite(
            crate::sqlite_store::SqliteStore::open(path, embedder).await?,
        ))
    }

    pub async fn health(&self) -> Result<()> {
        match self {
            Self::Qdrant(store) => {
                store
                    .client
                    .health_check()
                    .await
                    .context("checking Qdrant health")?;
                Ok(())
            }
            Self::Sqlite(store) => store.health().await,
        }
    }

    pub async fn ensure_schema(&self) -> Result<()> {
        dispatch!(self, ensure_schema())
    }

    pub async fn upsert_segments(&self, segments: &[Segment]) -> Result<()> {
        dispatch!(self, upsert_segments(segments))
    }

    pub async fn ingest_segments(
        &self,
        segments: &[Segment],
        roles: &HashMap<String, Role>,
        lineage: &[Ancestor],
        cosine: f32,
        jaccard: f32,
    ) -> Result<(usize, usize)> {
        dispatch!(
            self,
            ingest_segments(segments, roles, lineage, cosine, jaccard)
        )
    }

    pub async fn delete_stale_chunks(
        &self,
        harness: &str,
        session: &str,
        source_id: &str,
        count: u32,
    ) -> Result<()> {
        dispatch!(
            self,
            delete_stale_chunks(harness, session, source_id, count)
        )
    }

    pub async fn hybrid_candidates(
        &self,
        query: &str,
        filter: &ScopeFilter,
        limit: u64,
    ) -> Result<Vec<(Segment, f32)>> {
        dispatch!(self, hybrid_candidates(query, filter, limit))
    }

    pub async fn scroll_segments(&self, filter: &ScopeFilter) -> Result<Vec<Segment>> {
        dispatch!(self, scroll_segments(filter))
    }

    pub async fn count_segments(&self, filter: &ScopeFilter) -> Result<u64> {
        dispatch!(self, count_segments(filter))
    }

    pub async fn delete_segments(&self, filter: &ScopeFilter) -> Result<u64> {
        dispatch!(self, delete_segments(filter))
    }

    pub async fn delete_sessions(&self, filter: &ScopeFilter) -> Result<()> {
        dispatch!(self, delete_sessions(filter))
    }

    pub async fn count_by_kind(&self, filter: &ScopeFilter) -> Result<BTreeMap<String, u64>> {
        dispatch!(self, count_by_kind(filter))
    }

    pub async fn upsert_session(&self, session: SessionRecord) -> Result<()> {
        dispatch!(self, upsert_session(session))
    }

    pub async fn get_session(&self, harness: &str, session: &str) -> Result<Option<SessionRecord>> {
        dispatch!(self, get_session(harness, session))
    }

    pub async fn list_sessions(
        &self,
        harness: &str,
        project: Option<&str>,
    ) -> Result<Vec<SessionRecord>> {
        dispatch!(self, list_sessions(harness, project))
    }

    pub async fn session_links(&self) -> Result<Vec<(SessionKey, Option<SessionKey>)>> {
        dispatch!(self, session_links())
    }

    pub async fn upsert_continuation(&self, record: ContinuationRecord) -> Result<()> {
        dispatch!(self, upsert_continuation(record))
    }

    pub async fn write_continuation(&self, id: PointId, record: ContinuationRecord) -> Result<()> {
        dispatch!(self, write_continuation(id, record))
    }

    pub async fn list_continuations(
        &self,
        project: &str,
    ) -> Result<Vec<(PointId, ContinuationRecord)>> {
        dispatch!(self, list_continuations(project))
    }

    pub async fn delete_continuation(&self, id: PointId) -> Result<()> {
        dispatch!(self, delete_continuation(id))
    }

    pub async fn delete_project_continuations(&self, project: &str) -> Result<()> {
        dispatch!(self, delete_project_continuations(project))
    }

    pub async fn list_sessions_filtered(&self, filter: ScopeFilter) -> Result<Vec<SessionRecord>> {
        dispatch!(self, list_sessions_filtered(filter))
    }
}

pub struct QdrantStore {
    pub client: Qdrant,
    pub segments_collection: String,
    pub sessions_collection: String,
    pub continuations_collection: String,
    embedder: Arc<dyn Embedder>,
}

impl QdrantStore {
    pub fn new(client: Qdrant, prefix: &str, embedder: Arc<dyn Embedder>) -> Self {
        Self {
            client,
            segments_collection: format!("{prefix}_segments"),
            sessions_collection: format!("{prefix}_sessions"),
            continuations_collection: format!("{prefix}_continuations"),
            embedder,
        }
    }

    pub async fn ensure_schema(&self) -> Result<()> {
        ensure!(
            self.embedder.dimensions() == 384,
            "embedding dimensions must be 384"
        );
        if !self
            .client
            .collection_exists(&self.segments_collection)
            .await
            .context("checking segment collection")?
        {
            let mut dense = VectorsConfigBuilder::default();
            dense.add_named_vector_params("dense", VectorParamsBuilder::new(384, Distance::Cosine));
            let mut sparse = SparseVectorsConfigBuilder::default();
            sparse.add_named_vector_params(
                "bm25",
                SparseVectorParamsBuilder::default().modifier(Modifier::Idf),
            );
            self.client
                .create_collection(
                    CreateCollectionBuilder::new(&self.segments_collection)
                        .vectors_config(dense)
                        .sparse_vectors_config(sparse),
                )
                .await
                .context("creating segment collection")?;
        }
        if !self
            .client
            .collection_exists(&self.sessions_collection)
            .await
            .context("checking session collection")?
        {
            self.client
                .create_collection(CreateCollectionBuilder::new(&self.sessions_collection))
                .await
                .context("creating session collection")?;
        }
        if !self
            .client
            .collection_exists(&self.continuations_collection)
            .await
            .context("checking continuation collection")?
        {
            self.client
                .create_collection(CreateCollectionBuilder::new(&self.continuations_collection))
                .await
                .context("creating continuation collection")?;
        }
        for field in [
            "project",
            "harness",
            "session",
            "kind",
            "path",
            "paths",
            "source_id",
        ] {
            let mut request = CreateFieldIndexCollectionBuilder::new(
                &self.segments_collection,
                field,
                FieldType::Keyword,
            );
            if field == "project" {
                request = request
                    .field_index_params(KeywordIndexParamsBuilder::default().is_tenant(true));
            }
            self.client
                .create_field_index(request)
                .await
                .with_context(|| format!("indexing segment {field}"))?;
        }
        self.client
            .create_field_index(CreateFieldIndexCollectionBuilder::new(
                &self.segments_collection,
                "chunk_index",
                FieldType::Integer,
            ))
            .await
            .context("indexing segment chunk_index")?;
        self.client
            .create_field_index(CreateFieldIndexCollectionBuilder::new(
                &self.segments_collection,
                "ts",
                FieldType::Datetime,
            ))
            .await
            .context("indexing segment ts")?;
        for field in ["harness", "project", "session", "parent", "parent_harness"] {
            self.client
                .create_field_index(CreateFieldIndexCollectionBuilder::new(
                    &self.sessions_collection,
                    field,
                    FieldType::Keyword,
                ))
                .await
                .with_context(|| format!("indexing session {field}"))?;
        }
        self.client
            .create_field_index(CreateFieldIndexCollectionBuilder::new(
                &self.continuations_collection,
                "project",
                FieldType::Keyword,
            ))
            .await
            .context("indexing continuation project")?;
        Ok(())
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
            let mut points = Vec::with_capacity(batch.len());
            for (segment, vector) in batch.iter().zip(vectors) {
                ensure!(
                    vector.len() == self.embedder.dimensions(),
                    "embedder returned incorrect vector dimensions"
                );
                let vectors: HashMap<String, Vector> = HashMap::from([
                    ("dense".into(), vector.into()),
                    (
                        "bm25".into(),
                        DocumentBuilder::new(&segment.text, "qdrant/bm25")
                            .build()
                            .into(),
                    ),
                ]);
                points.push(PointStruct::new(
                    segment.id.to_string(),
                    vectors,
                    segment_payload(segment)?,
                ));
            }
            self.client
                .upsert_points(
                    UpsertPointsBuilder::new(&self.segments_collection, points).wait(true),
                )
                .await
                .context("upserting segment batch")?;
        }
        Ok(())
    }

    pub async fn ingest_segments(
        &self,
        segments: &[Segment],
        source_roles: &HashMap<String, Role>,
        lineage: &[Ancestor],
        cosine_threshold: f32,
        jaccard_threshold: f32,
    ) -> Result<(usize, usize)> {
        let lineage_filter = ScopeFilter {
            lineage: Some(lineage.to_vec()),
            ..Default::default()
        }
        .to_filter()
        .context("building duplicate lineage filter")?;
        let mut stored = 0;
        let mut duplicates = 0;
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
            for (segment, vector) in batch.iter().zip(vectors) {
                ensure!(
                    vector.len() == self.embedder.dimensions(),
                    "embedder returned incorrect vector dimensions"
                );
                if prose_kind(segment.kind)
                    && self
                        .is_duplicate(
                            segment,
                            source_roles.get(&segment.source_id).copied(),
                            &vector,
                            &lineage_filter,
                            cosine_threshold,
                            jaccard_threshold,
                        )
                        .await?
                {
                    duplicates += 1;
                    self.client
                        .delete_points(
                            DeletePointsBuilder::new(&self.segments_collection)
                                .points(vec![PointId::from(segment.id.to_string())])
                                .wait(true),
                        )
                        .await
                        .context("deleting duplicate source chunk")?;
                    continue;
                }
                let vectors: HashMap<String, Vector> = HashMap::from([
                    ("dense".into(), vector.into()),
                    (
                        "bm25".into(),
                        DocumentBuilder::new(&segment.text, "qdrant/bm25")
                            .build()
                            .into(),
                    ),
                ]);
                let point = PointStruct::new(
                    segment.id.to_string(),
                    vectors,
                    segment_payload_with_role(
                        segment,
                        source_roles.get(&segment.source_id).copied(),
                    )?,
                );
                self.client
                    .upsert_points(
                        UpsertPointsBuilder::new(&self.segments_collection, vec![point]).wait(true),
                    )
                    .await
                    .context("upserting segment")?;
                stored += 1;
            }
        }
        Ok((stored, duplicates))
    }

    async fn is_duplicate(
        &self,
        segment: &Segment,
        source_role: Option<Role>,
        vector: &[f32],
        lineage_filter: &Filter,
        cosine_threshold: f32,
        jaccard_threshold: f32,
    ) -> Result<bool> {
        let filter = Filter {
            must: vec![
                Condition::matches("project", segment.project.clone()),
                Condition::matches("kind", kind_name(&segment.kind)?),
                Condition::matches("source_role", role_name(source_role).to_owned()),
                Condition::matches("provenance", segment.tool.clone().unwrap_or_default()),
                Condition::from(lineage_filter.clone()),
            ],
            must_not: vec![Condition::matches("source_id", segment.source_id.clone())],
            ..Default::default()
        };
        let response = self
            .client
            .query(
                QueryPointsBuilder::new(&self.segments_collection)
                    .query(Query::new_nearest(vector.to_vec()))
                    .using("dense")
                    .filter(filter)
                    .limit(3u64)
                    .with_payload(true),
            )
            .await
            .context("querying duplicate candidates")?;
        for point in response.result {
            if point.score >= cosine_threshold {
                let candidate = segment_from_payload(point.id, point.payload)?;
                if shingle_jaccard(&segment.text, &candidate.text) >= jaccard_threshold {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    pub async fn delete_stale_chunks(
        &self,
        harness: &str,
        session: &str,
        source_id: &str,
        count: u32,
    ) -> Result<()> {
        let filter = stale_chunks_filter(harness, session, source_id, count);
        self.client
            .delete_points(
                DeletePointsBuilder::new(&self.segments_collection)
                    .points(filter)
                    .wait(true),
            )
            .await
            .with_context(|| format!("deleting stale chunks for source {source_id}"))?;
        Ok(())
    }

    pub async fn hybrid_candidates(
        &self,
        query: &str,
        filter: &ScopeFilter,
        limit: u64,
    ) -> Result<Vec<(Segment, f32)>> {
        let filter = filter.to_filter()?;
        let dense = self
            .embedder
            .embed_query(query.to_owned())
            .await
            .context("embedding query")?;
        let lexical = Document {
            text: query.to_owned(),
            model: "qdrant/bm25".into(),
            ..Default::default()
        };
        let response = self
            .client
            .query(
                QueryPointsBuilder::new(&self.segments_collection)
                    .add_prefetch(
                        PrefetchQueryBuilder::default()
                            .query(Query::new_nearest(dense))
                            .using("dense")
                            .filter(filter.clone())
                            .limit(50u64),
                    )
                    .add_prefetch(
                        PrefetchQueryBuilder::default()
                            .query(Query::new_nearest(lexical))
                            .using("bm25")
                            .filter(filter.clone())
                            .limit(50u64),
                    )
                    .query(Query::new_fusion(Fusion::Rrf))
                    .filter(filter)
                    .limit(limit)
                    .with_payload(true),
            )
            .await
            .context("querying hybrid candidates")?;
        response
            .result
            .into_iter()
            .map(|point| Ok((segment_from_payload(point.id, point.payload)?, point.score)))
            .collect()
    }

    async fn scroll_payloads(
        &self,
        collection: &str,
        filter: Filter,
    ) -> Result<
        Vec<(
            Option<PointId>,
            HashMap<String, qdrant_client::qdrant::Value>,
        )>,
    > {
        let mut offset = None;
        let mut points = Vec::new();
        loop {
            let mut request = ScrollPointsBuilder::new(collection)
                .filter(filter.clone())
                .limit(100u32)
                .with_payload(true);
            if let Some(id) = offset {
                request = request.offset(id);
            }
            let page = self
                .client
                .scroll(request)
                .await
                .with_context(|| format!("scrolling {collection}"))?;
            points.extend(
                page.result
                    .into_iter()
                    .map(|point| (point.id, point.payload)),
            );
            offset = page.next_page_offset;
            if offset.is_none() {
                break;
            }
        }
        Ok(points)
    }

    pub async fn scroll_segments(&self, filter: &ScopeFilter) -> Result<Vec<Segment>> {
        self.scroll_payloads(&self.segments_collection, filter.to_filter()?)
            .await?
            .into_iter()
            .map(|(id, payload)| segment_from_payload(id, payload))
            .collect()
    }

    pub async fn count_segments(&self, filter: &ScopeFilter) -> Result<u64> {
        let filter = filter.to_filter()?;
        Ok(self
            .client
            .count(
                CountPointsBuilder::new(&self.segments_collection)
                    .filter(filter)
                    .exact(true),
            )
            .await
            .context("counting segments")?
            .result
            .context("missing count result")?
            .count)
    }

    pub async fn delete_segments(&self, filter: &ScopeFilter) -> Result<u64> {
        let count = self.count_segments(filter).await?;
        self.client
            .delete_points(
                DeletePointsBuilder::new(&self.segments_collection)
                    .points(filter.to_filter()?)
                    .wait(true),
            )
            .await
            .context("deleting segments")?;
        Ok(count)
    }

    pub async fn delete_sessions(&self, filter: &ScopeFilter) -> Result<()> {
        self.client
            .delete_points(
                DeletePointsBuilder::new(&self.sessions_collection)
                    .points(filter.to_filter()?)
                    .wait(true),
            )
            .await
            .context("deleting sessions")?;
        Ok(())
    }

    pub async fn count_by_kind(&self, filter: &ScopeFilter) -> Result<BTreeMap<String, u64>> {
        let mut counts = BTreeMap::new();
        for segment in self.scroll_segments(filter).await? {
            *counts.entry(kind_name(&segment.kind)?).or_insert(0) += 1;
        }
        Ok(counts)
    }

    pub async fn upsert_session(&self, session: SessionRecord) -> Result<()> {
        let id = Uuid::new_v5(
            &SESSION_NAMESPACE,
            format!("{}/{}", session.harness, session.session).as_bytes(),
        );
        let payload =
            Payload::try_from(serde_json::to_value(session).context("serializing session")?)
                .context("encoding session payload")?;
        let point = PointStruct {
            id: Some(id.to_string().into()),
            payload: payload.into(),
            vectors: Some(HashMap::<String, Vector>::new().into()),
        };
        self.client
            .upsert_points(
                UpsertPointsBuilder::new(&self.sessions_collection, vec![point]).wait(true),
            )
            .await
            .context("upserting session")?;
        Ok(())
    }

    pub async fn get_session(&self, harness: &str, session: &str) -> Result<Option<SessionRecord>> {
        let filter = ScopeFilter {
            sessions: Some(vec![SessionKey {
                harness: harness.into(),
                session: session.into(),
            }]),
            ..Default::default()
        };
        let mut sessions = self.list_sessions_filtered(filter).await?;
        ensure!(sessions.len() <= 1, "duplicate session records");
        Ok(sessions.pop())
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
        let records = self.list_continuations(&record.project).await?;
        if records.iter().any(|(id, existing)| {
            id == &PointId::from(legacy_id.to_string())
                && existing.target_harness == record.target_harness
        }) {
            self.delete_continuation(PointId::from(legacy_id.to_string()))
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
        let payload =
            Payload::try_from(serde_json::to_value(record).context("serializing continuation")?)
                .context("encoding continuation payload")?;
        let point = PointStruct {
            id: Some(id),
            payload: payload.into(),
            vectors: Some(HashMap::<String, Vector>::new().into()),
        };
        self.client
            .upsert_points(
                UpsertPointsBuilder::new(&self.continuations_collection, vec![point]).wait(true),
            )
            .await
            .context("upserting continuation")?;
        Ok(())
    }

    pub async fn list_continuations(
        &self,
        project: &str,
    ) -> Result<Vec<(PointId, ContinuationRecord)>> {
        self.scroll_payloads(
            &self.continuations_collection,
            Filter::must([Condition::matches("project", project.to_owned())]),
        )
        .await?
        .into_iter()
        .map(|(id, payload)| {
            let id = id.context("missing continuation point id")?;
            let record = Payload::from(payload)
                .deserialize()
                .context("decoding continuation payload")?;
            Ok((id, record))
        })
        .collect()
    }

    pub async fn delete_continuation(&self, id: PointId) -> Result<()> {
        self.client
            .delete_points(
                DeletePointsBuilder::new(&self.continuations_collection)
                    .points(vec![id])
                    .wait(true),
            )
            .await
            .context("deleting continuation")?;
        Ok(())
    }

    pub async fn delete_project_continuations(&self, project: &str) -> Result<()> {
        self.client
            .delete_points(
                DeletePointsBuilder::new(&self.continuations_collection)
                    .points(Filter::must([Condition::matches(
                        "project",
                        project.to_owned(),
                    )]))
                    .wait(true),
            )
            .await
            .context("deleting project continuations")?;
        Ok(())
    }

    pub async fn list_sessions_filtered(&self, filter: ScopeFilter) -> Result<Vec<SessionRecord>> {
        self.scroll_payloads(&self.sessions_collection, filter.to_filter()?)
            .await?
            .into_iter()
            .map(|(_, payload)| {
                Payload::from(payload)
                    .deserialize()
                    .context("decoding session payload")
            })
            .collect()
    }
}

pub(crate) fn prose_kind(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::UserRequest | Kind::AssistantNote | Kind::Decision | Kind::Constraint | Kind::Task
    )
}

pub(crate) fn role_name(role: Option<Role>) -> &'static str {
    match role {
        Some(Role::User) => "user",
        Some(Role::Assistant) => "assistant",
        Some(Role::Tool) => "tool",
        None => "unknown",
    }
}

pub(crate) fn shingle_jaccard(left: &str, right: &str) -> f32 {
    fn shingles(text: &str) -> HashSet<Vec<String>> {
        let words = text
            .split(|character: char| !character.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
            .collect::<Vec<_>>();
        words.windows(3).map(|window| window.to_vec()).collect()
    }
    let left = shingles(left);
    let right = shingles(right);
    let union = left.union(&right).count();
    if union == 0 {
        return 0.0;
    }
    left.intersection(&right).count() as f32 / union as f32
}

fn stale_chunks_filter(harness: &str, session: &str, source_id: &str, count: u32) -> Filter {
    Filter::must([
        Condition::matches("harness", harness.to_owned()),
        Condition::matches("session", session.to_owned()),
        Condition::matches("source_id", source_id.to_owned()),
        Condition::range(
            "chunk_index",
            Range {
                gte: Some(f64::from(count)),
                ..Default::default()
            },
        ),
    ])
}

pub(crate) fn kind_name(kind: &Kind) -> Result<String> {
    serde_json::to_value(kind)?
        .as_str()
        .map(str::to_owned)
        .context("kind is not a string")
}

#[derive(Serialize, Deserialize)]
struct SegmentPayload {
    harness: String,
    project: String,
    session: String,
    kind: Kind,
    text: String,
    source_id: String,
    chunk_index: u32,
    ts: chrono::DateTime<chrono::Utc>,
    tool: Option<String>,
    path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    paths: Option<Vec<String>>,
}

fn segment_payload(segment: &Segment) -> Result<Payload> {
    let payload = SegmentPayload {
        harness: segment.harness.clone(),
        project: segment.project.clone(),
        session: segment.session.clone(),
        kind: segment.kind,
        text: segment.text.clone(),
        source_id: segment.source_id.clone(),
        chunk_index: segment.chunk_index,
        ts: segment.ts,
        tool: segment.tool.clone(),
        path: segment.path.clone(),
        paths: Some(segment.paths.clone()),
    };
    Payload::try_from(serde_json::to_value(payload).context("serializing segment")?)
        .context("encoding segment payload")
}

fn segment_payload_with_role(segment: &Segment, role: Option<Role>) -> Result<Payload> {
    let mut payload: HashMap<String, qdrant_client::qdrant::Value> =
        segment_payload(segment)?.into();
    payload.insert("source_role".into(), role_name(role).into());
    payload.insert(
        "provenance".into(),
        segment.tool.clone().unwrap_or_default().into(),
    );
    Ok(Payload::from(payload))
}

pub(crate) fn segment_from_payload(
    id: Option<PointId>,
    payload: HashMap<String, qdrant_client::qdrant::Value>,
) -> Result<Segment> {
    let id = match id.and_then(|id| id.point_id_options) {
        Some(PointIdOptions::Uuid(id)) => {
            Uuid::parse_str(&id).context("invalid segment point UUID")?
        }
        _ => return Err(anyhow!("missing segment point UUID")),
    };
    let payload: SegmentPayload = Payload::from(payload)
        .deserialize()
        .context("decoding segment payload")?;
    let paths = payload
        .paths
        .unwrap_or_else(|| payload.path.iter().cloned().collect());
    Ok(Segment {
        id,
        harness: payload.harness,
        project: payload.project,
        session: payload.session,
        kind: payload.kind,
        text: payload.text,
        source_id: payload.source_id,
        chunk_index: payload.chunk_index,
        ts: payload.ts,
        tool: payload.tool,
        path: payload.path,
        paths,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_round_trip() {
        let segment = Segment {
            id: Uuid::new_v5(&SESSION_NAMESPACE, b"test"),
            harness: "h".into(),
            project: "p".into(),
            session: "s".into(),
            kind: Kind::FileChange,
            text: "edit segment.rs".into(),
            source_id: "id".into(),
            chunk_index: 2,
            ts: "2026-11-18T10:00:00Z".parse().unwrap(),
            tool: Some("edit".into()),
            path: Some("segment.rs".into()),
            paths: vec!["segment.rs".into(), "other.rs".into()],
        };
        assert_eq!(
            segment,
            segment_from_payload(
                Some(segment.id.to_string().into()),
                segment_payload(&segment).unwrap().into()
            )
            .unwrap()
        );
        let mut legacy_payload: HashMap<_, _> = segment_payload(&segment).unwrap().into();
        legacy_payload.remove("paths");
        let legacy =
            segment_from_payload(Some(segment.id.to_string().into()), legacy_payload).unwrap();
        assert_eq!(legacy.path.as_deref(), Some("segment.rs"));
        assert_eq!(legacy.paths, ["segment.rs"]);
        let mut no_path_payload: HashMap<_, _> = segment_payload(&segment).unwrap().into();
        no_path_payload.remove("paths");
        no_path_payload.remove("path");
        let no_path =
            segment_from_payload(Some(segment.id.to_string().into()), no_path_payload).unwrap();
        assert!(no_path.paths.is_empty());
    }

    #[test]
    fn scope_filter_builds_all_conditions() {
        let filter = ScopeFilter {
            harness: Some("h".into()),
            project: Some("p".into()),
            session: Some("a".into()),
            sessions: Some(vec![
                SessionKey {
                    harness: "h".into(),
                    session: "a".into(),
                },
                SessionKey {
                    harness: "other".into(),
                    session: "b".into(),
                },
            ]),
            kinds: Some(vec![Kind::Task]),
            lineage: None,
        }
        .to_filter()
        .unwrap();
        assert_eq!(filter.must.len(), 5);
        assert!(
            ScopeFilter {
                sessions: Some(vec![]),
                ..Default::default()
            }
            .to_filter()
            .is_err()
        );
    }

    #[test]
    fn duplicate_shingles_are_normalized_and_need_three_words() {
        assert_eq!(
            shingle_jaccard("One, TWO three four", "one two THREE four"),
            1.0
        );
        assert_eq!(shingle_jaccard("one two", "one two"), 0.0);
        assert_eq!(shingle_jaccard("one two three", "different two three"), 0.0);
        assert!(prose_kind(Kind::Decision));
        assert!(!prose_kind(Kind::FileChange));
        assert_eq!(stale_chunks_filter("h", "s", "id", 2).must.len(), 4);
    }
}
