use std::sync::Arc;

use mementod::{
    config::Config,
    embed::FakeEmbedder,
    routes::{AppState, router},
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
        .1,
        json!({"claimed":false})
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
    assert!(store.get_continuation("cross").await.unwrap().is_none());

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
    assert!(store.get_continuation("cross").await.unwrap().is_some());

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
