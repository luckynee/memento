use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, ensure};
use memento_core::{
    protocol::{Kind, SessionRecord},
    segment::Segment,
};
use qdrant_client::{
    Payload, Qdrant,
    qdrant::{
        Condition, CountPointsBuilder, CreateCollectionBuilder, CreateFieldIndexCollectionBuilder,
        DeletePointsBuilder, Distance, Document, DocumentBuilder, FieldType, Filter, Fusion,
        KeywordIndexParamsBuilder, Modifier, PointId, PointStruct, PrefetchQueryBuilder, Query,
        QueryPointsBuilder, ScrollPointsBuilder, SparseVectorParamsBuilder,
        SparseVectorsConfigBuilder, UpsertPointsBuilder, Vector, VectorParamsBuilder,
        VectorsConfigBuilder, point_id::PointIdOptions,
    },
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::embed::Embedder;

const SESSION_NAMESPACE: Uuid = Uuid::from_u128(0x2c450e36_854d_5a3c_aab2_743e57e955b8);

#[derive(Debug, Clone, Default)]
pub struct ScopeFilter {
    pub harness: Option<String>,
    pub project: Option<String>,
    pub sessions: Option<Vec<String>>,
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
        if let Some(sessions) = &self.sessions {
            ensure!(
                !sessions.is_empty(),
                "empty session filter would match nothing"
            );
            conditions.push(Condition::matches("session", sessions.clone()));
        }
        if let Some(kinds) = &self.kinds {
            ensure!(!kinds.is_empty(), "empty kind filter would match nothing");
            let names = kinds.iter().map(kind_name).collect::<Result<Vec<_>>>()?;
            conditions.push(Condition::matches("kind", names));
        }
        Ok(Filter::must(conditions))
    }
}

pub struct Store {
    pub client: Qdrant,
    pub segments_collection: String,
    pub sessions_collection: String,
    embedder: Arc<dyn Embedder>,
}

impl Store {
    pub fn new(client: Qdrant, prefix: &str, embedder: Arc<dyn Embedder>) -> Self {
        Self {
            client,
            segments_collection: format!("{prefix}_segments"),
            sessions_collection: format!("{prefix}_sessions"),
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
        for field in ["project", "harness", "session", "kind", "path"] {
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
                "ts",
                FieldType::Datetime,
            ))
            .await
            .context("indexing segment ts")?;
        for field in ["harness", "project", "session", "parent"] {
            self.client
                .create_field_index(CreateFieldIndexCollectionBuilder::new(
                    &self.sessions_collection,
                    field,
                    FieldType::Keyword,
                ))
                .await
                .with_context(|| format!("indexing session {field}"))?;
        }
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

    pub async fn delete_segments(&self, filter: &ScopeFilter) -> Result<u64> {
        let filter = filter.to_filter()?;
        let count = self
            .client
            .count(
                CountPointsBuilder::new(&self.segments_collection)
                    .filter(filter.clone())
                    .exact(true),
            )
            .await
            .context("counting segments before deletion")?
            .result
            .context("missing count result")?
            .count;
        self.client
            .delete_points(
                DeletePointsBuilder::new(&self.segments_collection)
                    .points(filter)
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
            harness: Some(harness.into()),
            sessions: Some(vec![session.into()]),
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

    pub async fn session_links(
        &self,
        harness: &str,
        project: Option<&str>,
    ) -> Result<Vec<(String, Option<String>)>> {
        Ok(self
            .list_sessions(harness, project)
            .await?
            .into_iter()
            .map(|session| (session.session, session.parent))
            .collect())
    }

    async fn list_sessions_filtered(&self, filter: ScopeFilter) -> Result<Vec<SessionRecord>> {
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

fn kind_name(kind: &Kind) -> Result<String> {
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
    };
    Payload::try_from(serde_json::to_value(payload).context("serializing segment")?)
        .context("encoding segment payload")
}

fn segment_from_payload(
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
        };
        assert_eq!(
            segment,
            segment_from_payload(
                Some(segment.id.to_string().into()),
                segment_payload(&segment).unwrap().into()
            )
            .unwrap()
        );
    }

    #[test]
    fn scope_filter_builds_all_conditions() {
        let filter = ScopeFilter {
            harness: Some("h".into()),
            project: Some("p".into()),
            sessions: Some(vec!["a".into(), "b".into()]),
            kinds: Some(vec![Kind::Task]),
        }
        .to_filter()
        .unwrap();
        assert_eq!(filter.must.len(), 4);
        assert!(
            ScopeFilter {
                sessions: Some(vec![]),
                ..Default::default()
            }
            .to_filter()
            .is_err()
        );
    }
}
