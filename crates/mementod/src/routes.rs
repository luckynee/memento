use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
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
use chrono::{DateTime, Duration, Utc};
use memento_core::{
    digest::{DigestInput, render_digest},
    lineage::{self, SessionKey, SessionLink},
    protocol::{
        ClaimRequest, ClaimResponse, ContinuationRecord, ContinuationRequest, DigestResponse,
        ErrorBody, ErrorDetail, ForgetRequest, ForgetResponse, ForgetScope, Health, IngestRequest,
        IngestResponse, ReleaseRequest, ReleaseResponse, Retention, Scope, SearchRequest,
        SearchResponse, SessionRecord, SweepRequest, SweepResponse,
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
    if state.app.config.stale_days > 0 {
        let sweep_state = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(
                sweep_state.app.config.stale_sweep_secs.max(1),
            ));
            interval.tick().await;
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            loop {
                if sweep_projects(&sweep_state, None, false, Utc::now())
                    .await
                    .is_err()
                {
                    tracing::error!("background stale sweep failed");
                }
                interval.tick().await;
            }
        });
    }
    Router::new()
        .route("/health", get(health))
        .route("/v1/sessions", get(list_sessions).put(put_session))
        .route("/v1/sessions/release", post(release_session))
        .route("/v1/maintenance/sweep", post(maintenance_sweep))
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
    #[serde(default)]
    require_parent: bool,
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

fn session_key(record: &SessionRecord) -> SessionKey {
    SessionKey {
        harness: record.harness.clone(),
        session: record.session.clone(),
    }
}

fn refresh_activity(record: &mut SessionRecord, now: DateTime<Utc>) -> bool {
    if record.deleted_at.is_some()
        || record
            .last_active_at
            .is_some_and(|last| now.signed_duration_since(last) < Duration::minutes(15))
    {
        return false;
    }
    record.last_active_at = Some(now);
    true
}

async fn record_activity(
    state: &RouterState,
    harness: &str,
    project: &str,
    session: &str,
) -> anyhow::Result<()> {
    let _guard = state.registry_lock.lock().await;
    if let Some(mut record) = state.app.store.get_session(harness, session).await?
        && record.project == project
        && refresh_activity(&mut record, Utc::now())
    {
        state.app.store.upsert_session(record).await?;
    }
    Ok(())
}

async fn read_activity(state: &RouterState, harness: &str, project: &str, session: &str) {
    if let Err(error) = record_activity(state, harness, project, session).await {
        tracing::warn!(?error, "recording read activity failed");
    }
}

pub async fn backfill_activity(store: &Store, now: DateTime<Utc>) -> anyhow::Result<usize> {
    let records = store.list_sessions_filtered(ScopeFilter::default()).await?;
    let mut updated = 0;
    for mut record in records {
        if record.last_active_at.is_none() {
            record.last_active_at = Some(now);
            store.upsert_session(record).await?;
            updated += 1;
        }
    }
    Ok(updated)
}

fn stale_sessions(
    records: &[SessionRecord],
    slots: &[ContinuationRecord],
    now: DateTime<Utc>,
    stale_days: u64,
) -> Vec<SessionKey> {
    if stale_days == 0 {
        return Vec::new();
    }
    let threshold = i64::try_from(stale_days)
        .ok()
        .and_then(Duration::try_days)
        .and_then(|duration| now.checked_sub_signed(duration))
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    let records = records
        .iter()
        .filter(|record| record.collected_at.is_none())
        .collect::<Vec<_>>();
    let links = records
        .iter()
        .map(|record| SessionLink {
            key: session_key(record),
            parent: record.parent.as_ref().map(|parent| SessionKey {
                harness: record
                    .parent_harness
                    .clone()
                    .unwrap_or_else(|| record.harness.clone()),
                session: parent.clone(),
            }),
        })
        .collect::<Vec<_>>();
    let active = records
        .iter()
        .filter(|record| {
            record.last_active_at.is_none_or(|last| last >= threshold)
                || slots.iter().any(|slot| {
                    slot.parent_harness == record.harness
                        && slot.parent_session == record.session
                        && slot.claimed_by.is_none()
                        && now.signed_duration_since(slot.created) < Duration::hours(24)
                })
        })
        .map(|record| session_key(record))
        .collect::<BTreeSet<_>>();
    let mut stale = records
        .iter()
        .filter(|record| record.deleted_at.is_none() && record.retention != Retention::Keep)
        .filter(|record| {
            lineage::descendants(&session_key(record), &links)
                .iter()
                .all(|key| !active.contains(key))
        })
        .map(|record| session_key(record))
        .collect::<Vec<_>>();
    stale.sort();
    stale
}

async fn maintenance_sweep(
    State(state): State<RouterState>,
    body: Result<Json<SweepRequest>, JsonRejection>,
) -> ApiResult<SweepResponse> {
    let request = json_body(body)?;
    if let Some(project) = &request.project {
        required(project)?;
    }
    Ok(Json(
        sweep_projects(
            &state,
            request.project.as_deref(),
            request.dry_run,
            Utc::now(),
        )
        .await?,
    ))
}

async fn sweep_projects(
    state: &RouterState,
    project: Option<&str>,
    dry_run: bool,
    now: DateTime<Utc>,
) -> Result<SweepResponse, ApiError> {
    let mut response = SweepResponse {
        stale: Vec::new(),
        removed: Vec::new(),
    };
    if state.app.config.stale_days == 0 {
        return Ok(response);
    }
    let _operation = state.operation_lock.write().await;
    let _guard = state.registry_lock.lock().await;
    let records = state
        .app
        .store
        .list_sessions_filtered(ScopeFilter {
            project: project.map(str::to_owned),
            ..Default::default()
        })
        .await;
    let records = store_result(state, records).await?;
    let projects = records
        .iter()
        .map(|record| record.project.clone())
        .collect::<BTreeSet<_>>();
    for project in projects {
        let project_records = records
            .iter()
            .filter(|record| record.project == project)
            .cloned()
            .collect::<Vec<_>>();
        let slots = state.app.store.list_continuations(&project).await;
        let slots = store_result(state, slots)
            .await?
            .into_iter()
            .map(|(_, slot)| slot)
            .collect::<Vec<_>>();
        let stale = stale_sessions(&project_records, &slots, now, state.app.config.stale_days);
        if !dry_run {
            for mut record in project_records {
                if stale.contains(&session_key(&record)) {
                    record.deleted_at = Some(now);
                    record.deleted_reason = Some("stale".into());
                    let result = state.app.store.upsert_session(record).await;
                    store_result(state, result).await?;
                }
            }
            response
                .removed
                .extend(collect_project(state, &project).await?);
        }
        response.stale.extend(stale);
    }
    Ok(response)
}

fn collectible(
    record: &SessionRecord,
    records: &[SessionRecord],
    continuations: &[ContinuationRecord],
    now: chrono::DateTime<Utc>,
) -> bool {
    let key = session_key(record);
    record.deleted_at.is_some()
        && record.collected_at.is_none()
        && record.retention != Retention::Keep
        && !records.iter().any(|child| {
            child.collected_at.is_none()
                && child.parent.as_deref() == Some(&key.session)
                && child.parent_harness.as_deref().unwrap_or(&child.harness) == key.harness
        })
        && !continuations.iter().any(|slot| {
            slot.parent_harness == key.harness
                && slot.parent_session == key.session
                && slot.claimed_by.is_none()
                && now.signed_duration_since(slot.created) < chrono::Duration::hours(24)
        })
}

async fn collect_project(state: &RouterState, project: &str) -> Result<Vec<SessionKey>, ApiError> {
    let records = state
        .app
        .store
        .list_sessions_filtered(ScopeFilter {
            project: Some(project.to_owned()),
            ..Default::default()
        })
        .await;
    let mut records = store_result(state, records).await?;
    let slots = state.app.store.list_continuations(project).await;
    let mut slots = store_result(state, slots).await?;
    let now = Utc::now();
    let mut removed = Vec::new();
    loop {
        let continuation_records = slots
            .iter()
            .map(|(_, slot)| slot.clone())
            .collect::<Vec<_>>();
        let Some(index) = records
            .iter()
            .position(|record| collectible(record, &records, &continuation_records, now))
        else {
            break;
        };
        let key = session_key(&records[index]);
        let result = state
            .app
            .store
            .delete_segments(&ScopeFilter {
                project: Some(project.to_owned()),
                sessions: Some(vec![key.clone()]),
                ..Default::default()
            })
            .await;
        store_result(state, result).await?;
        let mut remaining = Vec::new();
        for (id, slot) in slots {
            if (slot.parent_harness == key.harness && slot.parent_session == key.session)
                || slot.claimed_by.as_ref() == Some(&key)
            {
                let result = state.app.store.delete_continuation(id).await;
                store_result(state, result).await?;
            } else {
                remaining.push((id, slot));
            }
        }
        slots = remaining;
        records[index].collected_at = Some(now);
        let result = state.app.store.upsert_session(records[index].clone()).await;
        store_result(state, result).await?;
        removed.push(key);
    }
    let expired = records
        .into_iter()
        .filter(|record| {
            record.collected_at.is_some_and(|collected| {
                now.signed_duration_since(collected) > chrono::Duration::days(7)
            })
        })
        .map(|record| session_key(&record))
        .collect::<Vec<_>>();
    if !expired.is_empty() {
        let result = state
            .app
            .store
            .delete_sessions(&ScopeFilter {
                project: Some(project.to_owned()),
                sessions: Some(expired),
                ..Default::default()
            })
            .await;
        store_result(state, result).await?;
    }
    Ok(removed)
}

async fn release_session(
    State(state): State<RouterState>,
    body: Result<Json<ReleaseRequest>, JsonRejection>,
) -> ApiResult<ReleaseResponse> {
    let request = json_body(body)?;
    required(&request.harness)?;
    required(&request.project)?;
    required(&request.session)?;
    let _operation = state.operation_lock.write().await;
    let _guard = state.registry_lock.lock().await;
    let record = state
        .app
        .store
        .get_session(&request.harness, &request.session)
        .await;
    let Some(mut record) = store_result(&state, record).await? else {
        return Ok(Json(ReleaseResponse {
            released: false,
            removed: Vec::new(),
        }));
    };
    if record.project != request.project {
        return Ok(Json(ReleaseResponse {
            released: false,
            removed: Vec::new(),
        }));
    }
    if record.deleted_at.is_none() {
        record.deleted_at = Some(Utc::now());
        record.deleted_reason = Some(request.reason.unwrap_or_else(|| "deleted".into()));
        let result = state.app.store.upsert_session(record).await;
        store_result(&state, result).await?;
    }
    let removed = collect_project(&state, &request.project).await?;
    Ok(Json(ReleaseResponse {
        released: true,
        removed,
    }))
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
        .is_some_and(|record| record.deleted_at.is_some())
    {
        return Ok(existing.expect("deleted session exists"));
    }
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
    let original = existing.clone();
    let mut record = existing.unwrap_or_else(|| SessionRecord {
        harness: update.harness.clone(),
        project: update.project.clone(),
        session: update.session.clone(),
        parent: None,
        parent_harness: None,
        retention: Retention::default(),
        title: None,
        created: Some(Utc::now()),
        last_active_at: None,
        deleted_at: None,
        deleted_reason: None,
        collected_at: None,
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
        let parent_record = store_result(state, parent_record).await?;
        if update.require_parent
            && parent_record
                .as_ref()
                .is_none_or(|parent| parent.deleted_at.is_some())
        {
            return Err(ApiError {
                status: StatusCode::CONFLICT,
                code: "parent_unavailable",
                message: "parent session is missing or deleted",
            });
        }
        if parent_record.is_some_and(|parent| parent.project != record.project) {
            return Err(ApiError::cross_project());
        }
        validate_link(&child, &parent, &links)?;
    } else if update.require_parent {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            code: "parent_unavailable",
            message: "parent session is missing or deleted",
        });
    }
    if let Some(retention) = update.retention {
        record.retention = retention;
    }
    if let Some(title) = update.title {
        record.title = Some(title);
    }
    refresh_activity(&mut record, Utc::now());
    if original.as_ref() != Some(&record) {
        let result = state.app.store.upsert_session(record.clone()).await;
        store_result(state, result).await?;
    }
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
    if parent.deleted_at.is_some() {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            code: "parent_deleted",
            message: "parent session is deleted",
        });
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
    let _operation = state.operation_lock.read().await;
    let _guard = state.registry_lock.lock().await;
    let caller = SessionKey {
        harness: request.harness.clone(),
        session: request.session.clone(),
    };
    let records = state.app.store.list_continuations(&request.project).await;
    let mut records = store_result(&state, records).await?;
    records.retain(|(_, pending)| {
        pending.target_harness.is_none()
            || pending.target_harness.as_deref() == Some(&request.harness)
    });
    records.sort_by_key(|(_, pending)| {
        (
            pending.claimed_by.as_ref() != Some(&caller),
            pending.target_harness.is_none(),
        )
    });
    let mut selected = None;
    for (id, pending) in records {
        if Utc::now().signed_duration_since(pending.claimed_at.unwrap_or(pending.created))
            >= chrono::Duration::hours(24)
        {
            let result = state.app.store.delete_continuation(id).await;
            store_result(&state, result).await?;
            continue;
        }
        if pending
            .claimed_by
            .as_ref()
            .is_some_and(|claimed| claimed != &caller)
        {
            continue;
        }
        selected = Some((id, pending));
        break;
    }
    let Some((id, mut pending)) = selected else {
        return Ok(Json(unclaimed()));
    };
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
        let result = state
            .app
            .store
            .write_continuation(id, pending.clone())
            .await;
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
            require_parent: false,
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
    #[serde(default)]
    include_deleted: bool,
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
    if !query.include_deleted {
        sessions.retain(|record| record.deleted_at.is_none());
    }
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
        .is_some_and(|record| record.deleted_at.is_some())
    {
        return Ok(Json(IngestResponse {
            segments: 0,
            skipped: request.items.len(),
            duplicates: 0,
        }));
    }
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
                last_active_at: Some(Utc::now()),
                deleted_at: None,
                deleted_reason: None,
                collected_at: None,
            })
            .await;
        store_result(&state, result).await?;
    } else if let Some(mut record) = existing
        && refresh_activity(&mut record, Utc::now())
    {
        let result = state.app.store.upsert_session(record).await;
        store_result(&state, result).await?;
    }
    let lineage = ancestors(&state, &request.harness, &request.session).await?;
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
            &lineage,
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
    let _operation = state.operation_lock.read().await;
    read_activity(&state, &request.harness, &request.project, &request.session).await;
    if request.kinds.as_ref().is_some_and(Vec::is_empty) {
        return Ok(Json(SearchResponse { hits: Vec::new() }));
    }
    let mut sessions = match request.scope {
        Scope::Session => Some(vec![SessionKey {
            harness: request.harness.clone(),
            session: request.session.clone(),
        }]),
        Scope::Lineage => Some(ancestors(&state, &request.harness, &request.session).await?),
        Scope::Project | Scope::Global => None,
    };
    let records = state
        .app
        .store
        .list_sessions_filtered(ScopeFilter {
            project: (request.scope != Scope::Global).then_some(request.project.clone()),
            ..Default::default()
        })
        .await;
    let records = store_result(&state, records).await?;
    let hidden = records
        .iter()
        .filter(|record| {
            record.collected_at.is_some()
                || (matches!(request.scope, Scope::Project | Scope::Global)
                    && record.deleted_at.is_some())
        })
        .map(|record| SessionKey {
            harness: record.harness.clone(),
            session: record.session.clone(),
        })
        .collect::<std::collections::HashSet<_>>();
    if let Some(keys) = &mut sessions {
        keys.retain(|key| !hidden.contains(key));
        if keys.is_empty() {
            return Ok(Json(SearchResponse { hits: Vec::new() }));
        }
    }
    let filter = ScopeFilter {
        harness: None,
        project: (request.scope != Scope::Global).then_some(request.project),
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
        .filter(|(segment, _)| {
            !hidden.contains(&SessionKey {
                harness: segment.harness.clone(),
                session: segment.session.clone(),
            })
        })
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
    let _operation = state.operation_lock.read().await;
    read_activity(&state, &query.harness, &query.project, &query.session).await;
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
    let mut lineage = ancestors(state, harness, session).await?;
    let records = state
        .app
        .store
        .list_sessions_filtered(ScopeFilter {
            project: Some(project.to_owned()),
            ..Default::default()
        })
        .await;
    let records = store_result(state, records).await?;
    lineage.retain(|key| {
        !records.iter().any(|record| {
            record.harness == key.harness
                && record.session == key.session
                && record.collected_at.is_some()
        })
    });
    if lineage.is_empty() {
        return Ok(render_digest(
            &DigestInput {
                harness: harness.into(),
                project: project.into(),
                lineage: Vec::new(),
                segments: Vec::new(),
            },
            budget.unwrap_or(12000).clamp(1000, 100000),
        ));
    }
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
    let affected_projects = if let Some(project) = &request.project {
        vec![project.clone()]
    } else {
        let records = state
            .app
            .store
            .list_sessions_filtered(ScopeFilter {
                sessions: sessions.clone(),
                ..Default::default()
            })
            .await;
        store_result(&state, records)
            .await?
            .into_iter()
            .map(|record| record.project)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    let filter = ScopeFilter {
        harness: None,
        project: request.project,
        sessions,
        ..Default::default()
    };
    let deleted = state.app.store.delete_segments(&filter).await;
    let deleted = store_result(&state, deleted).await?;
    let result = state.app.store.delete_sessions(&filter).await;
    store_result(&state, result).await?;
    if request.scope == ForgetScope::Project {
        let result = state
            .app
            .store
            .delete_project_continuations(filter.project.as_deref().unwrap_or_default())
            .await;
        store_result(&state, result).await?;
    }
    for project in affected_projects {
        collect_project(&state, &project).await?;
    }
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

    fn record(harness: &str, session: &str, parent: Option<(&str, &str)>) -> SessionRecord {
        SessionRecord {
            harness: harness.into(),
            project: "project".into(),
            session: session.into(),
            parent: parent.map(|(_, session)| session.into()),
            parent_harness: parent.map(|(harness, _)| harness.into()),
            retention: Retention::Continuation,
            title: None,
            created: Some(Utc::now()),
            last_active_at: None,
            deleted_at: Some(Utc::now()),
            deleted_reason: None,
            collected_at: None,
        }
    }

    #[test]
    fn activity_is_throttled_and_deleted_records_are_untouched() {
        let now = Utc::now();
        let mut session = record("h", "s", None);
        session.deleted_at = None;
        session.last_active_at = Some(now - Duration::minutes(16));
        assert!(refresh_activity(&mut session, now));
        assert_eq!(session.last_active_at, Some(now));
        assert!(!refresh_activity(&mut session, now + Duration::minutes(14)));
        assert!(refresh_activity(&mut session, now + Duration::minutes(15)));
        session.deleted_at = Some(now);
        assert!(!refresh_activity(&mut session, now + Duration::days(1)));
    }

    #[test]
    fn stale_selection_protects_active_descendants_and_pending_slots() {
        let now = Utc::now();
        let mut parent = record("opencode", "parent", None);
        let mut child = record("codex", "child", Some(("opencode", "parent")));
        parent.deleted_at = None;
        child.deleted_at = None;
        parent.last_active_at = Some(now - Duration::days(8));
        child.last_active_at = Some(now);
        let records = vec![parent.clone(), child.clone()];
        assert!(stale_sessions(&records, &[], now, 7).is_empty());
        child.last_active_at = Some(now - Duration::days(8));
        let records = vec![parent.clone(), child.clone()];
        assert_eq!(stale_sessions(&records, &[], now, 7).len(), 2);
        assert!(stale_sessions(&records, &[], now, 0).is_empty());
        let slot = ContinuationRecord {
            project: "project".into(),
            parent_harness: "codex".into(),
            parent_session: "child".into(),
            target_harness: None,
            created: now,
            claimed_by: None,
            claimed_at: None,
        };
        assert!(stale_sessions(&records, &[slot], now, 7).is_empty());
        child.retention = Retention::Keep;
        assert_eq!(
            stale_sessions(&[parent.clone(), child], &[], now, 7),
            vec![session_key(&parent)]
        );
    }

    #[test]
    fn collection_respects_dependents_retention_and_pending_slots() {
        let now = Utc::now();
        let parent = record("opencode", "parent", None);
        let mut child = record("codex", "child", Some(("opencode", "parent")));
        assert!(collectible(
            &parent,
            std::slice::from_ref(&parent),
            &[],
            now
        ));
        child.deleted_at = None;
        assert!(!collectible(
            &parent,
            &[parent.clone(), child.clone()],
            &[],
            now
        ));
        child.deleted_at = Some(now);
        assert!(!collectible(
            &parent,
            &[parent.clone(), child.clone()],
            &[],
            now
        ));
        child.collected_at = Some(now);
        assert!(collectible(&parent, &[parent.clone(), child], &[], now));
        let mut kept = parent.clone();
        kept.retention = Retention::Keep;
        assert!(!collectible(&kept, std::slice::from_ref(&kept), &[], now));
        let slot = ContinuationRecord {
            project: "project".into(),
            parent_harness: "opencode".into(),
            parent_session: "parent".into(),
            target_harness: None,
            created: now,
            claimed_by: None,
            claimed_at: None,
        };
        assert!(!collectible(
            &parent,
            std::slice::from_ref(&parent),
            &[slot],
            now
        ));
    }
}
