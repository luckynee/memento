use std::sync::Arc;

use mementod::{
    config::Config,
    embed::FakeEmbedder,
    routes::{AppState, backfill_activity, router},
    store::Store,
};
use qdrant_client::{Qdrant, qdrant::DeleteCollectionBuilder};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use uuid::Uuid;

async fn request(
    address: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> (u16, Value) {
    let mut stream = TcpStream::connect(address).await.unwrap();
    let authorization = token
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let message = format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\n{authorization}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(message.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8(response).unwrap();
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    let status = headers
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    (status, serde_json::from_str(body).unwrap())
}

#[tokio::test]
async fn route_workflow() {
    let Ok(url) = std::env::var("MEMENTO_TEST_QDRANT_URL") else {
        eprintln!("skipping Qdrant route test: MEMENTO_TEST_QDRANT_URL is unset");
        return;
    };
    let client = Qdrant::from_url(&url).build().unwrap();
    let prefix = format!(
        "memento_routes_{}",
        Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            format!("{:?}", std::time::SystemTime::now()).as_bytes()
        )
        .simple()
    );
    let store = Arc::new(Store::new(client.clone(), &prefix, Arc::new(FakeEmbedder)));
    store.ensure_schema().await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let app = router(AppState {
        store: Arc::clone(&store),
        config: Config {
            bind: listener.local_addr().unwrap(),
            qdrant_url: url,
            token: Some("test-token".into()),
            collection_prefix: prefix,
            cache_dir: None,
            duplicate_cosine_threshold: 0.95,
            duplicate_jaccard_threshold: 0.8,
            stale_days: 7,
            stale_sweep_secs: 3600,
        },
        policy: Default::default(),
        rerank: Default::default(),
    });
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let test_store = Arc::clone(&store);
    let result = tokio::spawn(async move { run_workflow(&address, &test_store).await }).await;
    server.abort();
    for collection in [
        &store.segments_collection,
        &store.sessions_collection,
        &store.continuations_collection,
    ] {
        client
            .delete_collection(DeleteCollectionBuilder::new(collection))
            .await
            .unwrap();
    }
    result.unwrap();
}

async fn run_workflow(address: &str, store: &Store) {
    let call = |method: &'static str, path: &'static str, body: Value| async move {
        request(address, method, path, Some("test-token"), &body.to_string()).await
    };
    assert_eq!(request(address, "GET", "/health", None, "").await.0, 200);
    let (status, body) = request(address, "GET", "/v1/stats?harness=h", None, "").await;
    assert_eq!(status, 401);
    assert_eq!(body["error"]["code"], "unauthorized");
    let (status, body) = request(address, "POST", "/v1/search", Some("test-token"), "{").await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_request");
    let (status, body) = request(
        address,
        "GET",
        "/v1/digest?harness=h&project=p&session=s&budget=bad",
        Some("test-token"),
        "",
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_request");
    let (status, body) = request(address, "GET", "/missing", Some("test-token"), "").await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "not_found");
    assert_eq!(call("GET", "/v1/stats?harness=h", json!(null)).await.0, 200);
    let parent = json!({
        "harness":"h","project":"p","session":"parent","items":[
            {"source_id":"a","ts":"2026-11-18T10:00:00Z","role":"user","text":"Please fix unique_identifier"},
            {"source_id":"b","ts":"2026-11-18T10:00:01Z","role":"tool","tool":{"name":"edit","input":{"filePath":"src/example.rs"},"output":"","ok":true}}
        ]
    });
    assert_eq!(call("POST", "/v1/ingest", parent).await.1["segments"], 2);
    let link = json!({"harness":"h","project":"p","session":"child","parent":"parent","title":"first","retention":"keep"});
    let (status, linked) = call("PUT", "/v1/sessions", link).await;
    assert_eq!(status, 200);
    let created = linked["created"].clone();
    let merged = call(
        "PUT",
        "/v1/sessions",
        json!({"harness":"h","project":"p","session":"child","title":"second"}),
    )
    .await
    .1;
    assert_eq!(merged["parent"], "parent");
    assert_eq!(merged["retention"], "keep");
    assert_eq!(merged["created"], created);
    let (status, cycle) = call(
        "PUT",
        "/v1/sessions",
        json!({"harness":"h","project":"p","session":"parent","parent":"child"}),
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(cycle["error"]["code"], "cycle");
    let lineage = call("GET", "/v1/sessions/h/child/lineage", json!(null))
        .await
        .1;
    assert_eq!(
        lineage["lineage"],
        json!([{"harness":"h","session":"child"}, {"harness":"h","session":"parent"}])
    );
    let search = json!({"harness":"h","project":"p","session":"child","query":"unique_identifier","scope":"lineage"});
    let hits = call("POST", "/v1/search", search.clone()).await.1;
    assert!(
        hits["hits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|hit| hit["session"] == "parent"
                && hit["text"].as_str().unwrap().contains("unique_identifier"))
    );
    let mut session_search = search;
    session_search["scope"] = json!("session");
    assert_eq!(
        call("POST", "/v1/search", session_search).await.1["hits"],
        json!([])
    );
    let digest = call(
        "GET",
        "/v1/digest?harness=h&project=p&session=child",
        json!(null),
    )
    .await
    .1;
    assert!(
        digest["text"]
            .as_str()
            .unwrap()
            .contains("Please fix unique_identifier")
    );
    assert!(digest["text"].as_str().unwrap().contains("src/example.rs"));
    let child = json!({"harness":"h","project":"p","session":"child","items":[{"source_id":"c","ts":"2026-11-18T10:00:02Z","role":"user","text":"child request"}]});
    assert_eq!(call("POST", "/v1/ingest", child).await.1["segments"], 1);
    let stats = call("GET", "/v1/stats?harness=h&project=p", json!(null))
        .await
        .1;
    assert_eq!(stats["total"], 3);
    assert_eq!(stats["by_kind"]["user_request"], 2);
    assert_eq!(stats["by_kind"]["file_change"], 1);
    let forgotten = call(
        "POST",
        "/v1/forget",
        json!({"harness":"h","session":"child","scope":"lineage"}),
    )
    .await
    .1;
    assert_eq!(forgotten["deleted"], 1);
    let sessions = call("GET", "/v1/sessions?harness=h&project=p", json!(null))
        .await
        .1;
    assert_eq!(sessions["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(sessions["sessions"][0]["session"], "parent");
    let stats = call("GET", "/v1/stats?harness=h", json!(null)).await.1;
    assert_eq!(stats["total"], 2);
    cross_harness_workflow(address, store).await;
    regression_workflow(address, store).await;
    continuation_slots_workflow(address, store).await;
    release_workflow(address).await;
    stale_workflow(address, store).await;
}

async fn stale_workflow(address: &str, store: &Store) {
    use chrono::{Duration, Utc};

    let call = |method: &'static str, path: &'static str, body: Value| async move {
        request(address, method, path, Some("test-token"), &body.to_string()).await
    };
    let project = "stale-project";
    for session in [
        json!({"harness":"h","project":project,"session":"stale-parent"}),
        json!({"harness":"other","project":project,"session":"stale-child","parent":"stale-parent","parent_harness":"h"}),
        json!({"harness":"h","project":project,"session":"kept","retention":"keep"}),
        json!({"harness":"h","project":project,"session":"pending"}),
        json!({"harness":"h","project":project,"session":"active-parent"}),
        json!({"harness":"other","project":project,"session":"active-child","parent":"active-parent","parent_harness":"h"}),
    ] {
        assert_eq!(call("PUT", "/v1/sessions", session).await.0, 200);
    }
    for (harness, session) in [
        ("h", "stale-parent"),
        ("other", "stale-child"),
        ("h", "kept"),
        ("h", "pending"),
        ("h", "active-parent"),
    ] {
        let mut record = store.get_session(harness, session).await.unwrap().unwrap();
        record.last_active_at = Some(Utc::now() - Duration::days(8));
        store.upsert_session(record).await.unwrap();
    }
    assert_eq!(
        call(
            "PUT",
            "/v1/continuations",
            json!({
                "project":project,"parent_harness":"h","parent_session":"pending"
            })
        )
        .await
        .0,
        200
    );
    let dry = call(
        "POST",
        "/v1/maintenance/sweep",
        json!({"project":project,"dry_run":true}),
    )
    .await;
    assert_eq!(dry.0, 200);
    assert_eq!(dry.1["stale"].as_array().unwrap().len(), 2);
    assert_eq!(dry.1["removed"], json!([]));
    assert!(
        store
            .get_session("h", "stale-parent")
            .await
            .unwrap()
            .unwrap()
            .deleted_at
            .is_none()
    );
    let sweep = call("POST", "/v1/maintenance/sweep", json!({"project":project})).await;
    assert_eq!(sweep.0, 200);
    assert_eq!(
        sweep.1["removed"],
        json!([
            {"harness":"other","session":"stale-child"},
            {"harness":"h","session":"stale-parent"}
        ])
    );
    assert_eq!(
        store
            .get_session("h", "stale-parent")
            .await
            .unwrap()
            .unwrap()
            .deleted_reason
            .as_deref(),
        Some("stale")
    );
    for session in ["kept", "pending", "active-parent"] {
        assert!(
            store
                .get_session("h", session)
                .await
                .unwrap()
                .unwrap()
                .deleted_at
                .is_none()
        );
    }
    let mut backfill = store.get_session("h", "kept").await.unwrap().unwrap();
    backfill.last_active_at = None;
    store.upsert_session(backfill).await.unwrap();
    let now = Utc::now();
    assert_eq!(backfill_activity(store, now).await.unwrap(), 1);
    assert_eq!(backfill_activity(store, now).await.unwrap(), 0);
    assert_eq!(
        store
            .get_session("h", "kept")
            .await
            .unwrap()
            .unwrap()
            .last_active_at,
        Some(now)
    );
    let old = Utc::now() - Duration::hours(1);
    let mut active = store
        .get_session("other", "active-child")
        .await
        .unwrap()
        .unwrap();
    active.last_active_at = Some(old);
    store.upsert_session(active).await.unwrap();
    assert_eq!(call("POST", "/v1/search", json!({
        "harness":"other","project":project,"session":"active-child","query":"anything","kinds":[]
    })).await.0, 200);
    let after_search = store
        .get_session("other", "active-child")
        .await
        .unwrap()
        .unwrap()
        .last_active_at
        .unwrap();
    assert!(after_search > old);
    assert_eq!(
        call(
            "POST",
            "/v1/ingest",
            json!({
                "harness":"other","project":project,"session":"active-child","items":[]
            })
        )
        .await
        .0,
        200
    );
    assert_eq!(
        store
            .get_session("other", "active-child")
            .await
            .unwrap()
            .unwrap()
            .last_active_at,
        Some(after_search)
    );
    let mut active = store
        .get_session("other", "active-child")
        .await
        .unwrap()
        .unwrap();
    active.last_active_at = Some(old);
    store.upsert_session(active).await.unwrap();
    assert_eq!(
        call(
            "POST",
            "/v1/ingest",
            json!({
                "harness":"other","project":project,"session":"active-child","items":[]
            })
        )
        .await
        .0,
        200
    );
    assert!(
        store
            .get_session("other", "active-child")
            .await
            .unwrap()
            .unwrap()
            .last_active_at
            .unwrap()
            > old
    );
    assert_eq!(
        call(
            "POST",
            "/v1/continuations/claim",
            json!({
                "project":project,"harness":"new","session":"claimant"
            })
        )
        .await
        .1["claimed"],
        true
    );
    assert!(
        store
            .get_session("new", "claimant")
            .await
            .unwrap()
            .unwrap()
            .last_active_at
            .is_some()
    );
}

async fn release_workflow(address: &str) {
    let call = |method: &'static str, path: &'static str, body: Value| async move {
        request(address, method, path, Some("test-token"), &body.to_string()).await
    };
    let parent = json!({"harness":"opencode","project":"gc-project","session":"parent"});
    let child = json!({"harness":"codex","project":"gc-project","session":"child","parent":"parent","parent_harness":"opencode","require_parent":true});
    assert_eq!(call("PUT", "/v1/sessions", parent.clone()).await.0, 200);
    assert_eq!(call("PUT", "/v1/sessions", child).await.0, 200);
    let ingest = json!({"harness":"opencode","project":"gc-project","session":"parent","items":[{"source_id":"item","ts":"2026-11-18T10:00:00Z","role":"user","text":"unique parent memory"}]});
    assert_eq!(call("POST", "/v1/ingest", ingest.clone()).await.0, 200);
    let release = call("POST", "/v1/sessions/release", json!({"harness":"opencode","project":"gc-project","session":"parent","reason":"test-reason"})).await;
    assert_eq!(release.1["released"], true);
    assert_eq!(release.1["removed"], json!([]));
    let deleted = request(
        address,
        "GET",
        "/v1/sessions?harness=opencode&project=gc-project&include_deleted=true",
        Some("test-token"),
        "",
    )
    .await;
    assert_eq!(deleted.1["sessions"][0]["deleted_reason"], "test-reason");
    let digest = request(
        address,
        "GET",
        "/v1/digest?harness=codex&project=gc-project&session=child",
        Some("test-token"),
        "",
    )
    .await;
    assert!(
        digest.1["text"]
            .as_str()
            .unwrap()
            .contains("unique parent memory")
    );
    let list = request(
        address,
        "GET",
        "/v1/sessions?harness=opencode&project=gc-project",
        Some("test-token"),
        "",
    )
    .await;
    assert_eq!(list.1["sessions"], json!([]));
    let update = call(
        "PUT",
        "/v1/sessions",
        json!({"harness":"opencode","project":"gc-project","session":"parent","title":"changed"}),
    )
    .await;
    assert!(update.1["title"].is_null());
    assert_eq!(call("POST", "/v1/ingest", ingest).await.1["segments"], 0);
    let strict = call("PUT", "/v1/sessions", json!({"harness":"opencode","project":"gc-project","session":"fork","parent":"parent","require_parent":true})).await;
    assert_eq!(strict.0, 409);
    assert_eq!(strict.1["error"]["code"], "parent_unavailable");
    let continuation = call(
        "PUT",
        "/v1/continuations",
        json!({"project":"gc-project","parent_harness":"opencode","parent_session":"parent"}),
    )
    .await;
    assert_eq!(continuation.1["error"]["code"], "parent_deleted");
    let search = call("POST", "/v1/search", json!({"harness":"codex","project":"gc-project","session":"child","query":"unique parent memory","scope":"project"})).await;
    assert_eq!(search.1["hits"], json!([]));
    let forget = call(
        "POST",
        "/v1/forget",
        json!({"harness":"codex","project":"gc-project","session":"child","scope":"session"}),
    )
    .await;
    assert_eq!(forget.0, 200);
    let list = request(
        address,
        "GET",
        "/v1/sessions?harness=opencode&project=gc-project&include_deleted=true",
        Some("test-token"),
        "",
    )
    .await;
    assert!(list.1["sessions"][0]["collected_at"].is_string());
}

async fn regression_workflow(address: &str, store: &Store) {
    use chrono::Utc;
    use memento_core::{lineage::SessionKey, protocol::ContinuationRecord};

    let call = |method: &'static str, path: &'static str, body: Value| async move {
        request(address, method, path, Some("test-token"), &body.to_string()).await
    };
    assert_eq!(
        call(
            "PUT",
            "/v1/sessions",
            json!({"harness":"reg","project":"one","session":"parent"})
        )
        .await
        .0,
        200
    );
    let invalid_link = call(
        "PUT",
        "/v1/sessions",
        json!({"harness":"reg","project":"two","session":"child","parent":"parent"}),
    )
    .await;
    assert_eq!(
        (invalid_link.0, &invalid_link.1["error"]["code"]),
        (409, &json!("cross_project"))
    );
    let changed_project = call(
        "PUT",
        "/v1/sessions",
        json!({"harness":"reg","project":"two","session":"parent"}),
    )
    .await;
    assert_eq!(changed_project.1["error"]["code"], "cross_project");
    let invalid_pending = call(
        "PUT",
        "/v1/continuations",
        json!({"project":"two","parent_harness":"reg","parent_session":"parent"}),
    )
    .await;
    assert_eq!(
        (invalid_pending.0, &invalid_pending.1["error"]["code"]),
        (409, &json!("cross_project"))
    );
    store
        .upsert_continuation(ContinuationRecord {
            project: "two".into(),
            parent_harness: "reg".into(),
            parent_session: "parent".into(),
            target_harness: None,
            created: Utc::now(),
            claimed_by: None,
            claimed_at: None,
        })
        .await
        .unwrap();
    let invalid_claim = call(
        "POST",
        "/v1/continuations/claim",
        json!({"project":"two","harness":"reg","session":"child"}),
    )
    .await;
    assert_eq!(
        (invalid_claim.0, &invalid_claim.1["error"]["code"]),
        (409, &json!("cross_project"))
    );
    assert_eq!(
        call(
            "PUT",
            "/v1/continuations",
            json!({"project":"one","parent_harness":"reg","parent_session":"parent"})
        )
        .await
        .0,
        200
    );
    let claim = json!({"project":"one","harness":"reg","session":"continuation"});
    let first = call("POST", "/v1/continuations/claim", claim.clone()).await;
    assert_eq!(first.1["claimed"], true);
    let retained = store
        .list_continuations("one")
        .await
        .unwrap()
        .pop()
        .unwrap()
        .1;
    assert_eq!(
        retained.claimed_by,
        Some(SessionKey {
            harness: "reg".into(),
            session: "continuation".into()
        })
    );
    assert!(retained.claimed_at.is_some());
    assert_eq!(
        call(
            "POST",
            "/v1/continuations/claim",
            json!({"project":"one","harness":"reg","session":"another"})
        )
        .await
        .1,
        json!({"claimed":false})
    );
    let content = |project: &str, session: &str, source: &str, text: &str| {
        json!({
            "harness":"reg", "project":project, "session":session,
            "items":[{"source_id":source,"ts":"2026-11-18T10:00:00Z","role":"user","text":text}]
        })
    };
    assert_eq!(
        call(
            "POST",
            "/v1/ingest",
            content("one", "parent", "fresh", "A newly added digest message")
        )
        .await
        .1["segments"],
        1
    );
    let repeated = call("POST", "/v1/continuations/claim", claim).await;
    assert_eq!(repeated.1["claimed"], true);
    assert!(
        repeated.1["digest"]
            .as_str()
            .unwrap()
            .contains("A newly added digest message")
    );

    let long = (0..500).map(|i| format!("word{i} ")).collect::<String>();
    assert!(
        call(
            "POST",
            "/v1/ingest",
            content("chunks", "s", "source", &long)
        )
        .await
        .1["segments"]
            .as_u64()
            .unwrap()
            > 1
    );
    assert_eq!(
        call(
            "POST",
            "/v1/ingest",
            content("chunks", "s", "source", "short replacement words here")
        )
        .await
        .1["segments"],
        1
    );
    let chunks = store
        .scroll_segments(&mementod::store::ScopeFilter {
            project: Some("chunks".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].text, "short replacement words here");
    let phrase = "The quick brown fox jumps over the lazy dog today";
    assert_eq!(
        call(
            "POST",
            "/v1/ingest",
            content("dedupe", "dedupe-session", "first", phrase)
        )
        .await
        .1["segments"],
        1
    );
    let unlinked = call(
        "POST",
        "/v1/ingest",
        content("dedupe", "unlinked", "unlinked-source", phrase),
    )
    .await;
    assert_eq!(unlinked.0, 200);
    assert_eq!(unlinked.1["segments"], 1);
    assert_eq!(unlinked.1["duplicates"], 0);
    let unlinked_digest = call(
        "GET",
        "/v1/digest?harness=reg&project=dedupe&session=unlinked",
        json!(null),
    )
    .await;
    assert!(unlinked_digest.1["text"].as_str().unwrap().contains(phrase));
    assert_eq!(
        call(
            "PUT",
            "/v1/sessions",
            json!({"harness":"other","project":"dedupe","session":"echo-child","parent_harness":"reg","parent":"dedupe-session"})
        )
        .await
        .0,
        200
    );
    let echo = json!({"harness":"other","project":"dedupe","session":"echo-child","items":[
        {"source_id":"echo","ts":"2026-11-18T10:00:00Z","role":"user","text":phrase}
    ]});
    let child_echo = call("POST", "/v1/ingest", echo).await;
    assert_eq!(child_echo.0, 200);
    assert_eq!(child_echo.1["segments"], 0);
    assert_eq!(child_echo.1["duplicates"], 1);
    for scope in ["session", "lineage"] {
        let search = call(
            "POST",
            "/v1/search",
            json!({"harness":"reg","project":"other-project","session":"unlinked","query":phrase,"scope":scope}),
        )
        .await;
        assert_eq!(search.0, 200);
        assert_eq!(search.1["hits"], json!([]));
        let forgotten = call(
            "POST",
            "/v1/forget",
            json!({"harness":"reg","project":"other-project","session":"unlinked","scope":scope}),
        )
        .await;
        assert_eq!(forgotten.0, 200);
        assert_eq!(forgotten.1["deleted"], 0);
    }
    assert!(
        store
            .get_session("reg", "unlinked")
            .await
            .unwrap()
            .is_some()
    );
    let unlinked_digest = call(
        "GET",
        "/v1/digest?harness=reg&project=dedupe&session=unlinked",
        json!(null),
    )
    .await;
    assert!(unlinked_digest.1["text"].as_str().unwrap().contains(phrase));
    let assistant = json!({"harness":"reg","project":"dedupe","session":"dedupe-session","items":[
        {"source_id":"assistant","ts":"2026-11-18T10:00:00Z","role":"assistant","kind_hint":"decision","text":phrase}
    ]});
    let user_decision = json!({"harness":"reg","project":"dedupe","session":"dedupe-session","items":[
        {"source_id":"user-decision","ts":"2026-11-18T10:00:00Z","role":"user","kind_hint":"decision","text":phrase}
    ]});
    assert_eq!(call("POST", "/v1/ingest", assistant).await.1["segments"], 1);
    assert_eq!(
        call("POST", "/v1/ingest", user_decision).await.1["segments"],
        1
    );
    let duplicate = call(
        "POST",
        "/v1/ingest",
        content("dedupe", "dedupe-session", "second", phrase),
    )
    .await
    .1;
    assert_eq!(duplicate["segments"], 0);
    assert_eq!(duplicate["duplicates"], 1);
    assert_eq!(
        call(
            "POST",
            "/v1/ingest",
            content("dedupe", "dedupe-session", "first", phrase)
        )
        .await
        .1["segments"],
        1
    );
    assert_eq!(
        call(
            "POST",
            "/v1/ingest",
            content("another-project", "s2", "third", phrase)
        )
        .await
        .1["segments"],
        1
    );
    let tool = json!({"harness":"reg","project":"dedupe","session":"dedupe-session","items":[
        {"source_id":"tool1","ts":"2026-11-18T10:00:00Z","role":"tool","tool":{"name":"read","input":{"path":"same"},"ok":true}},
        {"source_id":"tool2","ts":"2026-11-18T10:00:00Z","role":"tool","tool":{"name":"read","input":{"path":"same"},"ok":true}}
    ]});
    assert_eq!(call("POST", "/v1/ingest", tool).await.1["segments"], 2);
}

async fn cross_harness_workflow(address: &str, store: &Store) {
    use chrono::{Duration, Utc};
    use memento_core::protocol::ContinuationRecord;

    let call = |method: &'static str, path: &'static str, body: Value| async move {
        request(address, method, path, Some("test-token"), &body.to_string()).await
    };
    let ingest = |harness: &str, project: &str, session: &str, text: &str| {
        json!({
            "harness": harness, "project": project, "session": session,
            "items": [{"source_id":"same","ts":"2026-11-18T10:00:00Z","role":"user","text":text}]
        })
    };
    assert_eq!(
        call(
            "POST",
            "/v1/ingest",
            ingest("opencode", "cross", "same", "crossparent unique_marker")
        )
        .await
        .0,
        200
    );
    assert_eq!(
        call(
            "POST",
            "/v1/ingest",
            ingest("claude-code", "cross", "same", "crosschild unique_marker")
        )
        .await
        .0,
        200
    );
    assert_eq!(
        call(
            "POST",
            "/v1/ingest",
            ingest("codex", "other", "other", "global_marker unique_marker")
        )
        .await
        .0,
        200
    );
    let linked = call(
        "PUT",
        "/v1/sessions",
        json!({
            "harness":"claude-code","project":"cross","session":"same",
            "parent_harness":"opencode","parent":"same"
        }),
    )
    .await;
    assert_eq!(linked.0, 200);
    assert_eq!(linked.1["parent_harness"], "opencode");
    let merged = call(
        "PUT",
        "/v1/sessions",
        json!({
            "harness":"claude-code","project":"cross","session":"same","title":"kept"
        }),
    )
    .await;
    assert_eq!(merged.1["parent_harness"], "opencode");
    assert_eq!(
        call(
            "PUT",
            "/v1/sessions",
            json!({
                "harness":"opencode","project":"cross","session":"same",
                "parent_harness":"claude-code","parent":"same"
            })
        )
        .await
        .0,
        409
    );
    let lineage = call("GET", "/v1/sessions/claude-code/same/lineage", json!(null))
        .await
        .1;
    assert_eq!(
        lineage["lineage"],
        json!([
            {"harness":"claude-code","session":"same"},
            {"harness":"opencode","session":"same"}
        ])
    );
    let search = |scope| json!({"harness":"claude-code","project":"cross","session":"same","query":"unique_marker","scope":scope});
    let lineage_hits = call("POST", "/v1/search", search("lineage")).await.1;
    assert_eq!(lineage_hits["hits"].as_array().unwrap().len(), 2);
    assert!(
        lineage_hits["hits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|hit| hit["text"].as_str().unwrap().contains("crossparent"))
    );
    assert_eq!(
        call("POST", "/v1/search", search("session")).await.1["hits"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        call(
            "POST",
            "/v1/ingest",
            ingest("opencode", "cross", "other", "decoy unique_marker")
        )
        .await
        .0,
        200
    );
    assert_eq!(
        call(
            "POST",
            "/v1/ingest",
            ingest("codex", "cross", "same", "decoy unique_marker")
        )
        .await
        .0,
        200
    );
    assert_eq!(
        call("POST", "/v1/search", search("lineage")).await.1["hits"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        call("POST", "/v1/search", search("project")).await.1["hits"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(
        call("POST", "/v1/search", search("global")).await.1["hits"]
            .as_array()
            .unwrap()
            .len(),
        7
    );
    let project_hits = call(
        "POST",
        "/v1/search",
        json!({"harness":"","project":"cross","session":"","query":"unique_marker","scope":"project"}),
    )
    .await;
    assert_eq!(project_hits.0, 200);
    assert_eq!(project_hits.1["hits"].as_array().unwrap().len(), 4);
    assert!(
        project_hits.1["hits"]
            .as_array()
            .unwrap()
            .iter()
            .all(|hit| {
                hit["harness"]
                    .as_str()
                    .is_some_and(|harness| !harness.is_empty())
                    && hit["session"]
                        .as_str()
                        .is_some_and(|session| !session.is_empty())
            })
    );
    assert_eq!(
        call(
            "POST",
            "/v1/search",
            json!({"harness":"","project":"cross","session":"","query":"unique_marker","scope":"global"})
        )
        .await
        .1["hits"]
            .as_array()
            .unwrap()
            .len(),
        7
    );
    for scope in ["session", "lineage"] {
        assert_eq!(
            call(
                "POST",
                "/v1/search",
                json!({"harness":"","project":"cross","session":"","query":"unique_marker","scope":scope})
            )
            .await
            .0,
            400
        );
    }
    assert_eq!(
        call("GET", "/v1/sessions?project=cross", json!(null))
            .await
            .1["sessions"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(
        call("GET", "/v1/stats?project=cross", json!(null)).await.1["total"],
        4
    );
    assert_eq!(
        call("GET", "/v1/stats?project=cross&session=same", json!(null))
            .await
            .1["total"],
        3
    );
    assert_eq!(
        call(
            "GET",
            "/v1/stats?harness=opencode&project=cross&session=same",
            json!(null)
        )
        .await
        .1["total"],
        1
    );
    let digest = call(
        "GET",
        "/v1/digest?harness=claude-code&project=cross&session=same",
        json!(null),
    )
    .await
    .1;
    let text = digest["text"].as_str().unwrap();
    assert!(text.contains("claude-code/same <- opencode/same"));
    assert!(text.contains("crossparent") && text.contains("crosschild"));

    assert_eq!(
        call(
            "PUT",
            "/v1/continuations",
            json!({"project":"cross","parent_harness":"none","parent_session":"missing"})
        )
        .await
        .0,
        404
    );
    let pending = json!({"project":"cross","parent_harness":"opencode","parent_session":"same","target_harness":"codex"});
    assert_eq!(
        call("PUT", "/v1/continuations", pending.clone()).await.0,
        200
    );
    assert_eq!(
        call(
            "POST",
            "/v1/continuations/claim",
            json!({"project":"cross","harness":"claude-code","session":"new"})
        )
        .await
        .1,
        json!({"claimed":false})
    );
    let claimed = call(
        "POST",
        "/v1/continuations/claim",
        json!({"project":"cross","harness":"codex","session":"new"}),
    )
    .await;
    assert_eq!(claimed.0, 200);
    assert_eq!(claimed.1["claimed"], true);
    assert_eq!(claimed.1["parent_harness"], "opencode");
    assert_eq!(claimed.1["parent"], "same");
    assert!(
        claimed.1["digest"]
            .as_str()
            .unwrap()
            .contains("crossparent")
    );
    assert_eq!(
        call(
            "POST",
            "/v1/continuations/claim",
            json!({"project":"cross","harness":"codex","session":"new"})
        )
        .await
        .1["claimed"],
        true
    );
    assert_eq!(call("PUT", "/v1/continuations", json!({"project":"cross","parent_harness":"opencode","parent_session":"same","target_harness":"claude-code"})).await.0, 200);
    assert_eq!(
        call("PUT", "/v1/continuations", pending.clone()).await.0,
        200
    );
    let (first, second) = tokio::join!(
        call(
            "POST",
            "/v1/continuations/claim",
            json!({"project":"cross","harness":"codex","session":"race1"})
        ),
        call(
            "POST",
            "/v1/continuations/claim",
            json!({"project":"cross","harness":"codex","session":"race2"})
        )
    );
    assert_eq!(
        usize::from(first.1["claimed"] == true) + usize::from(second.1["claimed"] == true),
        1
    );
    store
        .upsert_continuation(ContinuationRecord {
            project: "cross".into(),
            parent_harness: "opencode".into(),
            parent_session: "same".into(),
            target_harness: None,
            created: Utc::now() - Duration::hours(25),
            claimed_by: None,
            claimed_at: None,
        })
        .await
        .unwrap();
    assert_eq!(
        call(
            "POST",
            "/v1/continuations/claim",
            json!({"project":"cross","harness":"codex","session":"expired"})
        )
        .await
        .1,
        json!({"claimed":false})
    );
    assert!(
        store
            .list_continuations("cross")
            .await
            .unwrap()
            .iter()
            .all(|(_, record)| record.target_harness.is_some())
    );

    assert_eq!(
        call(
            "PUT",
            "/v1/continuations",
            json!({"project":"cross","parent_harness":"claude-code","parent_session":"same"})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        call(
            "POST",
            "/v1/continuations/claim",
            json!({"project":"cross","harness":"opencode","session":"same"})
        )
        .await
        .0,
        409
    );
    assert!(
        store
            .list_continuations("cross")
            .await
            .unwrap()
            .iter()
            .any(|(_, record)| record.target_harness.is_none())
    );

    let forgotten = call(
        "POST",
        "/v1/forget",
        json!({"harness":"claude-code","session":"same","scope":"lineage"}),
    )
    .await
    .1;
    assert_eq!(forgotten["deleted"], 1);
    assert!(
        store
            .get_session("opencode", "same")
            .await
            .unwrap()
            .is_some()
    );
    assert!(store.get_session("codex", "new").await.unwrap().is_some());
    assert!(
        store
            .get_session("claude-code", "same")
            .await
            .unwrap()
            .is_none()
    );
    let forgotten = call(
        "POST",
        "/v1/forget",
        json!({"harness":"opencode","session":"same","scope":"lineage"}),
    )
    .await
    .1;
    assert_eq!(forgotten["deleted"], 1);
    assert!(store.get_session("codex", "new").await.unwrap().is_none());
    assert!(store.get_session("codex", "other").await.unwrap().is_some());
}

async fn continuation_slots_workflow(address: &str, store: &Store) {
    use std::collections::HashMap;

    use chrono::{Duration, Utc};
    use memento_core::protocol::ContinuationRecord;
    use qdrant_client::{
        Payload,
        qdrant::{PointStruct, UpsertPointsBuilder, Vector},
    };

    let call = |method: &'static str, path: &'static str, body: Value| async move {
        request(address, method, path, Some("test-token"), &body.to_string()).await
    };
    for session in ["parent-a", "parent-b"] {
        assert_eq!(
            call(
                "PUT",
                "/v1/sessions",
                json!({"harness":"opencode","project":"slots","session":session})
            )
            .await
            .0,
            200
        );
    }
    let pending = |parent, target: Option<&str>| json!({"project":"slots","parent_harness":"opencode","parent_session":parent,"target_harness":target});
    let claim = |harness, session| json!({"project":"slots","harness":harness,"session":session});
    for target in ["claude-code", "codex"] {
        assert_eq!(
            call(
                "PUT",
                "/v1/continuations",
                pending("parent-a", Some(target))
            )
            .await
            .0,
            200
        );
    }
    assert_eq!(
        call(
            "PUT",
            "/v1/continuations",
            pending("parent-b", Some("codex"))
        )
        .await
        .0,
        200
    );
    for (harness, parent) in [("claude-code", "parent-a"), ("codex", "parent-b")] {
        let response = call("POST", "/v1/continuations/claim", claim(harness, "first")).await;
        assert_eq!(response.1["claimed"], true);
        assert_eq!(response.1["parent"], parent);
    }
    assert_eq!(
        call("PUT", "/v1/continuations", pending("parent-a", None))
            .await
            .0,
        200
    );
    let fallback = call(
        "POST",
        "/v1/continuations/claim",
        claim("codex", "fallback"),
    )
    .await;
    assert_eq!(fallback.1["claimed"], true);
    assert_eq!(fallback.1["parent"], "parent-a");
    assert_eq!(
        call(
            "PUT",
            "/v1/continuations",
            pending("parent-b", Some("codex"))
        )
        .await
        .0,
        200
    );
    let repeated = call(
        "POST",
        "/v1/continuations/claim",
        claim("codex", "fallback"),
    )
    .await;
    assert_eq!(repeated.1["parent"], "parent-a");
    assert_eq!(
        call("PUT", "/v1/continuations", pending("parent-a", None))
            .await
            .0,
        200
    );
    let preferred = call(
        "POST",
        "/v1/continuations/claim",
        claim("codex", "preferred"),
    )
    .await;
    assert_eq!(preferred.1["parent"], "parent-b");
    let fallback = call(
        "POST",
        "/v1/continuations/claim",
        claim("claude-code", "fallback"),
    )
    .await;
    assert_eq!(fallback.1["parent"], "parent-a");

    store
        .upsert_continuation(ContinuationRecord {
            project: "slots".into(),
            parent_harness: "opencode".into(),
            parent_session: "parent-b".into(),
            target_harness: Some("codex".into()),
            created: Utc::now() - Duration::hours(25),
            claimed_by: None,
            claimed_at: None,
        })
        .await
        .unwrap();
    assert_eq!(
        call("PUT", "/v1/continuations", pending("parent-a", None))
            .await
            .0,
        200
    );
    let expired = call(
        "POST",
        "/v1/continuations/claim",
        claim("codex", "expired-target"),
    )
    .await;
    assert_eq!(expired.1["parent"], "parent-a");
    assert!(
        store
            .list_continuations("slots")
            .await
            .unwrap()
            .iter()
            .all(|(_, record)| record.target_harness.as_deref() != Some("codex"))
    );
    assert!(
        store
            .list_continuations("slots")
            .await
            .unwrap()
            .iter()
            .any(|(_, record)| record.target_harness.as_deref() == Some("claude-code"))
    );

    let legacy = ContinuationRecord {
        project: "slots".into(),
        parent_harness: "opencode".into(),
        parent_session: "parent-b".into(),
        target_harness: Some("legacy".into()),
        created: Utc::now(),
        claimed_by: None,
        claimed_at: None,
    };
    let legacy_id = Uuid::new_v5(
        &Uuid::from_u128(0x8bcc899f_a24d_53d6_83e1_80669ad0e156),
        b"slots",
    );
    let payload = Payload::try_from(serde_json::to_value(legacy).unwrap()).unwrap();
    store
        .client
        .upsert_points(
            UpsertPointsBuilder::new(
                &store.continuations_collection,
                vec![PointStruct {
                    id: Some(legacy_id.to_string().into()),
                    payload: payload.into(),
                    vectors: Some(HashMap::<String, Vector>::new().into()),
                }],
            )
            .wait(true),
        )
        .await
        .unwrap();
    let legacy_claim = call("POST", "/v1/continuations/claim", claim("legacy", "first")).await;
    assert_eq!(legacy_claim.1["parent"], "parent-b");
    assert_eq!(
        call("POST", "/v1/continuations/claim", claim("legacy", "first"))
            .await
            .1["claimed"],
        true
    );
    assert_eq!(
        call(
            "PUT",
            "/v1/continuations",
            pending("parent-a", Some("legacy"))
        )
        .await
        .0,
        200
    );
    assert_eq!(
        store
            .list_continuations("slots")
            .await
            .unwrap()
            .iter()
            .filter(|(_, record)| record.target_harness.as_deref() == Some("legacy"))
            .count(),
        1
    );
    assert_eq!(
        call(
            "POST",
            "/v1/continuations/claim",
            claim("legacy", "replacement")
        )
        .await
        .1["parent"],
        "parent-a"
    );
    assert_eq!(
        call(
            "POST",
            "/v1/forget",
            json!({"harness":"opencode","project":"slots","scope":"project"})
        )
        .await
        .0,
        200
    );
    assert!(store.list_continuations("slots").await.unwrap().is_empty());
}
