use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Weak},
};

use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query, Request, State,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::{Method, StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use memento_core::{
    digest::{DigestInput, render_digest},
    lineage::{self, SessionKey, SessionLink},
    protocol::{
        ClaimRequest, ClaimResponse, ContinuationRecord, ContinuationRequest, DigestResponse,
        ErrorBody, ErrorDetail, ForgetRequest, ForgetResponse, ForgetScope, Health, IngestRequest,
        IngestResponse, Retention, Scope, SearchRequest, SearchResponse, SessionRecord,
    },
    rerank::{Candidate, RerankConfig, rerank},
    segment::{SegmentPolicy, segment_items},
};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

use crate::{
    config::Config,
    store::{ScopeFilter, Store},
};

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub config: Config,
    pub policy: SegmentPolicy,
    pub rerank: RerankConfig,
}

#[derive(Clone)]
struct RouterState {
    app: AppState,
    registry_lock: Arc<Mutex<()>>,
    operation_lock: Arc<RwLock<()>>,
    session_locks: Arc<std::sync::Mutex<HashMap<SessionKey, Weak<Mutex<()>>>>>,
}

fn validate_link(
    child: &SessionKey,
    parent: &SessionKey,
    links: &BTreeMap<SessionKey, Option<SessionKey>>,
) -> Result<(), ApiError> {
    if lineage::would_create_cycle(child, parent, |session| {
        links.get(session).cloned().flatten()
    }) {
        return Err(ApiError::cycle());
    }
    let mut updated_links = links.clone();
    updated_links.insert(child.clone(), Some(parent.clone()));
    let links = updated_links
        .iter()
        .map(|(key, parent)| SessionLink {
            key: key.clone(),
            parent: parent.clone(),
        })
        .collect::<Vec<_>>();
    for descendant in lineage::descendants(child, &links) {
        if let Err(error) = lineage::ancestors(&descendant, |session| {
            updated_links.get(session).cloned().flatten()
        }) {
            return Err(match error {
                lineage::LineageError::TooDeep { .. } => ApiError::too_deep(),
                lineage::LineageError::Cycle { .. } => ApiError::cycle(),
            });
        }
    }
    Ok(())
}

pub fn router(state: AppState) -> Router {
    let state = RouterState {
        app: state,
        registry_lock: Arc::new(Mutex::new(())),
        operation_lock: Arc::new(RwLock::new(())),
        session_locks: Arc::new(std::sync::Mutex::new(HashMap::new())),
    };
    Router::new()
        .route("/health", get(health))
        .route("/v1/sessions", get(list_sessions).put(put_session))
        .route("/v1/continuations", axum::routing::put(put_continuation))
        .route("/v1/continuations/claim", post(claim_continuation))
        .route(
            "/v1/sessions/{harness}/{session}/lineage",
            get(session_lineage),
        )
        .route(
            "/v1/ingest",
            post(ingest).layer(DefaultBodyLimit::max(16 * 1024 * 1024)),
        )
        .route("/v1/search", post(search))
        .route("/v1/digest", get(digest))
        .route("/v1/forget", post(forget))
        .route("/v1/stats", get(stats))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
}

impl ApiError {
    fn invalid(message: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request",
            message,
        }
    }

    fn cycle() -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: "cycle",
            message: "session link would create a cycle",
        }
    }

    fn cross_project() -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: "cross_project",
            message: "parent and child projects differ",
        }
    }

    fn too_deep() -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: "too_deep",
            message: "session lineage exceeds depth limit",
        }
    }

    fn internal(error: impl std::fmt::Debug) -> Self {
        tracing::error!(error = ?error, "internal request failure");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal",
            message: "internal server error",
        }
    }

    async fn store(store: &Store, error: anyhow::Error) -> Self {
        tracing::error!(error = ?error, "store request failure");
        let qdrant_available = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            store.client.health_check(),
        )
        .await
        .is_ok_and(|result| result.is_ok());
        if !qdrant_available {
            Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                code: "qdrant_unavailable",
                message: "Qdrant request failed",
            }
        } else {
            Self {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "internal",
                message: "internal server error",
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: ErrorDetail {
                    code: self.code.into(),
                    message: self.message.into(),
                },
            }),
        )
            .into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

async fn store_result<T>(state: &RouterState, result: anyhow::Result<T>) -> Result<T, ApiError> {
    match result {
        Ok(value) => Ok(value),
        Err(error) => Err(ApiError::store(&state.app.store, error).await),
    }
}

fn json_body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    body.map(|Json(value)| value)
        .map_err(|_| ApiError::invalid("invalid JSON request"))
}

fn query_params<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, ApiError> {
    query
        .map(|Query(value)| value)
        .map_err(|_| ApiError::invalid("invalid query parameters"))
}

fn required(value: &str) -> Result<(), ApiError> {
    if value.trim().is_empty() {
        Err(ApiError::invalid("required field must not be empty"))
    } else {
        Ok(())
    }
}

fn authorized(expected: Option<&str>, header: Option<&str>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    let Some(actual) = header.and_then(|value| value.strip_prefix("Bearer ")) else {
        return false;
    };
    let expected = expected.as_bytes();
    let actual = actual.as_bytes();
    let mut difference = expected.len() ^ actual.len();
    for index in 0..expected.len().max(actual.len()) {
        difference |= usize::from(
            expected.get(index).copied().unwrap_or_default()
                ^ actual.get(index).copied().unwrap_or_default(),
        );
    }
    difference == 0
}

async fn authorize(State(state): State<RouterState>, request: Request, next: Next) -> Response {
    if request.method() == Method::GET && request.uri().path() == "/health" {
        return next.run(request).await;
    }
    let header = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    if authorized(state.app.config.token.as_deref(), header) {
        next.run(request).await
    } else {
        ApiError {
            status: StatusCode::UNAUTHORIZED,
            code: "unauthorized",
            message: "authentication required",
        }
        .into_response()
    }
}

async fn not_found() -> ApiError {
    ApiError {
        status: StatusCode::NOT_FOUND,
        code: "not_found",
        message: "route not found",
    }
}

async fn health(State(state): State<RouterState>) -> Json<Health> {
    Json(Health {
        ok: true,
        qdrant: state.app.store.client.health_check().await.is_ok(),
    })
}

#[derive(Deserialize)]
struct SessionUpdate {
    harness: String,
    project: String,
    session: String,
    parent: Option<String>,
    parent_harness: Option<String>,
    retention: Option<Retention>,
    title: Option<String>,
}

async fn put_session(
    State(state): State<RouterState>,
    body: Result<Json<SessionUpdate>, JsonRejection>,
) -> ApiResult<SessionRecord> {
    let update = json_body(body)?;
    required(&update.harness)?;
    required(&update.project)?;
    required(&update.session)?;
    if let Some(parent) = &update.parent {
        required(parent)?;
    }
    if let Some(parent_harness) = &update.parent_harness {
        required(parent_harness)?;
    }
    let _guard = state.registry_lock.lock().await;
    let record = merge_session(&state, update).await?;
    Ok(Json(record))
}

async fn merge_session(
    state: &RouterState,
    update: SessionUpdate,
) -> Result<SessionRecord, ApiError> {
    let existing = state
        .app
        .store
        .get_session(&update.harness, &update.session)
        .await;
    let existing = store_result(state, existing).await?;
    if existing
        .as_ref()
        .is_some_and(|record| record.project != update.project)
    {
        return Err(ApiError::cross_project());
    }
    let links = state.app.store.session_links().await;
    let links = store_result(state, links)
        .await?
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    let mut record = existing.unwrap_or_else(|| SessionRecord {
        harness: update.harness.clone(),
        project: update.project.clone(),
        session: update.session.clone(),
        parent: None,
        parent_harness: None,
        retention: Retention::default(),
        title: None,
        created: Some(Utc::now()),
    });
    record.project = update.project;
    if let Some(parent) = update.parent {
        record.parent = Some(parent);
        record.parent_harness = Some(
            update
                .parent_harness
                .unwrap_or_else(|| record.harness.clone()),
        );
    } else if let Some(parent_harness) = update.parent_harness {
        record.parent_harness = Some(parent_harness);
    }
    if let Some(parent) = &record.parent {
        let child = SessionKey {
            harness: record.harness.clone(),
            session: record.session.clone(),
        };
        let parent = SessionKey {
            harness: record
                .parent_harness
                .clone()
                .unwrap_or_else(|| record.harness.clone()),
            session: parent.clone(),
        };
        let parent_record = state
            .app
            .store
            .get_session(&parent.harness, &parent.session)
            .await;
        if store_result(state, parent_record)
            .await?
            .is_some_and(|parent| parent.project != record.project)
        {
            return Err(ApiError::cross_project());
        }
        validate_link(&child, &parent, &links)?;
    }
    if let Some(retention) = update.retention {
        record.retention = retention;
    }
    if let Some(title) = update.title {
        record.title = Some(title);
    }
    let result = state.app.store.upsert_session(record.clone()).await;
    store_result(state, result).await?;
    Ok(record)
}

async fn put_continuation(
    State(state): State<RouterState>,
    body: Result<Json<ContinuationRequest>, JsonRejection>,
) -> ApiResult<ContinuationRecord> {
    let request = json_body(body)?;
    required(&request.project)?;
    required(&request.parent_harness)?;
    required(&request.parent_session)?;
    if let Some(target) = &request.target_harness {
        required(target)?;
    }
    let _guard = state.registry_lock.lock().await;
    let parent = state
        .app
        .store
        .get_session(&request.parent_harness, &request.parent_session)
        .await;
    let Some(parent) = store_result(&state, parent).await? else {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: "parent session not found",
        });
    };
    if parent.project != request.project {
        return Err(ApiError::cross_project());
    }
    let record = ContinuationRecord {
        project: request.project,
        parent_harness: request.parent_harness,
        parent_session: request.parent_session,
        target_harness: request.target_harness,
        created: Utc::now(),
        claimed_by: None,
        claimed_at: None,
    };
    let result = state.app.store.upsert_continuation(record.clone()).await;
    store_result(&state, result).await?;
    Ok(Json(record))
}

async fn claim_continuation(
    State(state): State<RouterState>,
    body: Result<Json<ClaimRequest>, JsonRejection>,
) -> ApiResult<ClaimResponse> {
    let request = json_body(body)?;
    required(&request.project)?;
    required(&request.harness)?;
    required(&request.session)?;
    let _guard = state.registry_lock.lock().await;
    let pending = state.app.store.get_continuation(&request.project).await;
    let Some(mut pending) = store_result(&state, pending).await? else {
        return Ok(Json(unclaimed()));
    };
    if Utc::now().signed_duration_since(pending.claimed_at.unwrap_or(pending.created))
        >= chrono::Duration::hours(24)
    {
        let result = state.app.store.delete_continuation(&request.project).await;
        store_result(&state, result).await?;
        return Ok(Json(unclaimed()));
    }
    if pending
        .target_harness
        .as_deref()
        .is_some_and(|target| target != request.harness)
    {
        return Ok(Json(unclaimed()));
    }
    let caller = SessionKey {
        harness: request.harness.clone(),
        session: request.session.clone(),
    };
    if pending
        .claimed_by
        .as_ref()
        .is_some_and(|claimed| claimed != &caller)
    {
        return Ok(Json(unclaimed()));
    }
    let parent = state
        .app
        .store
        .get_session(&pending.parent_harness, &pending.parent_session)
        .await;
    if store_result(&state, parent)
        .await?
        .is_some_and(|parent| parent.project != request.project)
    {
        return Err(ApiError::cross_project());
    }
    let links = state.app.store.session_links().await;
    let links = store_result(&state, links).await?.into_iter().collect();
    validate_link(
        &caller,
        &SessionKey {
            harness: pending.parent_harness.clone(),
            session: pending.parent_session.clone(),
        },
        &links,
    )?;
    if pending.claimed_by.is_none() {
        pending.claimed_by = Some(caller);
        pending.claimed_at = Some(Utc::now());
        let result = state.app.store.upsert_continuation(pending.clone()).await;
        store_result(&state, result).await?;
    }
    let record = merge_session(
        &state,
        SessionUpdate {
            harness: request.harness,
            project: request.project.clone(),
            session: request.session,
            parent: Some(pending.parent_session.clone()),
            parent_harness: Some(pending.parent_harness.clone()),
            retention: None,
            title: None,
        },
    )
    .await?;
    drop(_guard);
    let digest = render_session_digest(
        &state,
        &record.harness,
        &record.project,
        &record.session,
        None,
    )
    .await?;
    Ok(Json(ClaimResponse {
        claimed: true,
        parent_harness: Some(pending.parent_harness),
        parent: Some(pending.parent_session),
        digest: Some(digest.text),
    }))
}

fn unclaimed() -> ClaimResponse {
    ClaimResponse {
        claimed: false,
        parent_harness: None,
        parent: None,
        digest: None,
    }
}

#[derive(Deserialize)]
struct SessionQuery {
    harness: String,
    project: Option<String>,
}

#[derive(Serialize)]
struct SessionsResponse {
    sessions: Vec<SessionRecord>,
}

async fn list_sessions(
    State(state): State<RouterState>,
    query: Result<Query<SessionQuery>, QueryRejection>,
) -> ApiResult<SessionsResponse> {
    let query = query_params(query)?;
    required(&query.harness)?;
    let sessions = state
        .app
        .store
        .list_sessions(&query.harness, query.project.as_deref())
        .await;
    let mut sessions = store_result(&state, sessions).await?;
    sessions.sort_by(|left, right| {
        left.created
            .cmp(&right.created)
            .then_with(|| left.session.cmp(&right.session))
    });
    Ok(Json(SessionsResponse { sessions }))
}

async fn ancestors(
    state: &RouterState,
    harness: &str,
    session: &str,
) -> Result<Vec<SessionKey>, ApiError> {
    let links = state.app.store.session_links().await;
    let links = store_result(state, links)
        .await?
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    lineage::ancestors(
        &SessionKey {
            harness: harness.into(),
            session: session.into(),
        },
        |session| links.get(session).cloned().flatten(),
    )
    .map_err(ApiError::internal)
}

#[derive(Serialize)]
struct LineageResponse {
    lineage: Vec<SessionKey>,
}

async fn session_lineage(
    State(state): State<RouterState>,
    path: Result<Path<(String, String)>, PathRejection>,
) -> ApiResult<LineageResponse> {
    let Path((harness, session)) = path.map_err(|_| ApiError::invalid("invalid path"))?;
    required(&harness)?;
    required(&session)?;
    Ok(Json(LineageResponse {
        lineage: ancestors(&state, &harness, &session).await?,
    }))
}

async fn ingest(
    State(state): State<RouterState>,
    body: Result<Json<IngestRequest>, JsonRejection>,
) -> ApiResult<IngestResponse> {
    let request = json_body(body)?;
    required(&request.harness)?;
    required(&request.project)?;
    required(&request.session)?;
    let _operation = state.operation_lock.read().await;
    let key = SessionKey {
        harness: request.harness.clone(),
        session: request.session.clone(),
    };
    let session_lock = {
        let mut locks = state.session_locks.lock().map_err(ApiError::internal)?;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            lock
        } else {
            let lock = Arc::new(Mutex::new(()));
            locks.insert(key, Arc::downgrade(&lock));
            lock
        }
    };
    let _session = session_lock.lock().await;
    let _guard = state.registry_lock.lock().await;
    let existing = state
        .app
        .store
        .get_session(&request.harness, &request.session)
        .await;
    let existing = store_result(&state, existing).await?;
    if existing
        .as_ref()
        .is_some_and(|record| record.project != request.project)
    {
        return Err(ApiError::cross_project());
    }
    if existing.is_none() {
        let result = state
            .app
            .store
            .upsert_session(SessionRecord {
                harness: request.harness.clone(),
                project: request.project.clone(),
                session: request.session.clone(),
                parent: None,
                parent_harness: None,
                retention: Retention::default(),
                title: None,
                created: Some(Utc::now()),
            })
            .await;
        store_result(&state, result).await?;
    }
    drop(_guard);
    let outcome = segment_items(&request, &state.app.policy);
    let source_roles = request
        .items
        .iter()
        .map(|item| (item.source_id.clone(), item.role))
        .collect();
    let result = state
        .app
        .store
        .ingest_segments(
            &outcome.segments,
            &source_roles,
            state.app.config.duplicate_cosine_threshold,
            state.app.config.duplicate_jaccard_threshold,
        )
        .await;
    let (segments, duplicates) = store_result(&state, result).await?;
    let mut source_counts = BTreeMap::new();
    for segment in &outcome.segments {
        source_counts.insert(&segment.source_id, segment.chunk_index + 1);
    }
    for (source_id, count) in source_counts {
        let result = state
            .app
            .store
            .delete_stale_chunks(&request.harness, &request.session, source_id, count)
            .await;
        store_result(&state, result).await?;
    }
    Ok(Json(IngestResponse {
        segments,
        skipped: outcome.skipped,
        duplicates,
    }))
}

async fn search(
    State(state): State<RouterState>,
    body: Result<Json<SearchRequest>, JsonRejection>,
) -> ApiResult<SearchResponse> {
    let request = json_body(body)?;
    required(&request.harness)?;
    required(&request.project)?;
    required(&request.session)?;
    required(&request.query)?;
    if request.k > 50 {
        return Err(ApiError::invalid("k must not exceed 50"));
    }
    if request.kinds.as_ref().is_some_and(Vec::is_empty) {
        return Ok(Json(SearchResponse { hits: Vec::new() }));
    }
    let sessions = match request.scope {
        Scope::Session => Some(vec![SessionKey {
            harness: request.harness.clone(),
            session: request.session.clone(),
        }]),
        Scope::Lineage => Some(ancestors(&state, &request.harness, &request.session).await?),
        Scope::Project | Scope::Global => None,
    };
    let filter = ScopeFilter {
        harness: None,
        project: (request.scope == Scope::Project).then_some(request.project),
        sessions,
        kinds: request.kinds,
    };
    let candidates = state
        .app
        .store
        .hybrid_candidates(&request.query, &filter, 50)
        .await;
    let candidates = store_result(&state, candidates)
        .await?
        .into_iter()
        .map(|(segment, fused_score)| Candidate {
            segment,
            fused_score,
        })
        .collect();
    Ok(Json(SearchResponse {
        hits: rerank(&request.query, candidates, &state.app.rerank, request.k),
    }))
}

#[derive(Deserialize)]
struct DigestQuery {
    harness: String,
    project: String,
    session: String,
    budget: Option<usize>,
}

async fn digest(
    State(state): State<RouterState>,
    query: Result<Query<DigestQuery>, QueryRejection>,
) -> ApiResult<memento_core::protocol::DigestResponse> {
    let query = query_params(query)?;
    required(&query.harness)?;
    required(&query.project)?;
    required(&query.session)?;
    Ok(Json(
        render_session_digest(
            &state,
            &query.harness,
            &query.project,
            &query.session,
            query.budget,
        )
        .await?,
    ))
}

async fn render_session_digest(
    state: &RouterState,
    harness: &str,
    project: &str,
    session: &str,
    budget: Option<usize>,
) -> Result<DigestResponse, ApiError> {
    let lineage = ancestors(state, harness, session).await?;
    let segments = state
        .app
        .store
        .scroll_segments(&ScopeFilter {
            sessions: Some(lineage.clone()),
            project: Some(project.to_owned()),
            ..Default::default()
        })
        .await;
    let segments = store_result(state, segments).await?;
    Ok(render_digest(
        &DigestInput {
            harness: harness.into(),
            project: project.into(),
            lineage: lineage.iter().map(ToString::to_string).collect(),
            segments,
        },
        budget.unwrap_or(12000).clamp(1000, 100000),
    ))
}

async fn forget(
    State(state): State<RouterState>,
    body: Result<Json<ForgetRequest>, JsonRejection>,
) -> ApiResult<ForgetResponse> {
    let request = json_body(body)?;
    required(&request.harness)?;
    if let Some(project) = &request.project {
        required(project)?;
    }
    let _operation = state.operation_lock.write().await;
    let _guard = state.registry_lock.lock().await;
    let sessions = match request.scope {
        ForgetScope::Project => {
            required(request.project.as_deref().unwrap_or_default())?;
            None
        }
        ForgetScope::Session | ForgetScope::Lineage => {
            let session = request.session.as_deref().unwrap_or_default();
            required(session)?;
            if request.scope == ForgetScope::Session {
                Some(vec![SessionKey {
                    harness: request.harness.clone(),
                    session: session.to_owned(),
                }])
            } else {
                let links = state.app.store.session_links().await;
                let links = store_result(&state, links)
                    .await?
                    .into_iter()
                    .map(|(key, parent)| SessionLink { key, parent })
                    .collect::<Vec<_>>();
                Some(lineage::descendants(
                    &SessionKey {
                        harness: request.harness.clone(),
                        session: session.to_owned(),
                    },
                    &links,
                ))
            }
        }
    };
    let filter = ScopeFilter {
        harness: None,
        project: (request.scope == ForgetScope::Project)
            .then_some(request.project)
            .flatten(),
        sessions,
        ..Default::default()
    };
    let deleted = state.app.store.delete_segments(&filter).await;
    let deleted = store_result(&state, deleted).await?;
    let result = state.app.store.delete_sessions(&filter).await;
    store_result(&state, result).await?;
    Ok(Json(ForgetResponse {
        deleted: usize::try_from(deleted).map_err(ApiError::internal)?,
    }))
}

#[derive(Deserialize)]
struct StatsQuery {
    harness: String,
    project: Option<String>,
    session: Option<String>,
}

#[derive(Serialize)]
struct StatsResponse {
    total: u64,
    by_kind: BTreeMap<String, u64>,
}

async fn stats(
    State(state): State<RouterState>,
    query: Result<Query<StatsQuery>, QueryRejection>,
) -> ApiResult<StatsResponse> {
    let query = query_params(query)?;
    required(&query.harness)?;
    let by_kind = state
        .app
        .store
        .count_by_kind(&ScopeFilter {
            harness: Some(query.harness.clone()),
            project: query.project,
            sessions: query.session.map(|session| {
                vec![SessionKey {
                    harness: query.harness,
                    session,
                }]
            }),
            ..Default::default()
        })
        .await;
    let by_kind = store_result(&state, by_kind).await?;
    Ok(Json(StatsResponse {
        total: by_kind.values().sum(),
        by_kind,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_authentication() {
        assert!(authorized(None, None));
        assert!(!authorized(Some("token"), None));
        assert!(!authorized(Some("token"), Some("Bearer wrong")));
        assert!(!authorized(Some("token"), Some("Basic token")));
        assert!(authorized(Some("token"), Some("Bearer token")));
    }

    #[test]
    fn error_mapping_does_not_expose_details() {
        let error = ApiError::internal("sensitive internal detail");
        assert_eq!(error.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error.code, "internal");
        assert!(!error.message.contains("sensitive"));
        assert_eq!(ApiError::cycle().status, StatusCode::CONFLICT);
        assert_eq!(ApiError::invalid("bad").status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn link_depth_accounts_for_existing_descendants() {
        let key = |number: usize| SessionKey {
            harness: "h".into(),
            session: number.to_string(),
        };
        let mut links = BTreeMap::new();
        for number in 1..64 {
            links.insert(key(number), Some(key(number - 1)));
        }
        assert_eq!(
            validate_link(&key(64), &key(63), &links)
                .err()
                .unwrap()
                .code,
            "too_deep"
        );
        links.remove(&key(63));
        assert!(validate_link(&key(63), &key(62), &links).is_ok());
        links.insert(key(63), Some(key(62)));
        assert_eq!(
            validate_link(&key(0), &key(64), &links).err().unwrap().code,
            "too_deep"
        );
    }
}
