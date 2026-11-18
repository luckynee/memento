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
    let result = tokio::spawn(async move { run_workflow(&address).await }).await;
    server.abort();
    for collection in [&store.segments_collection, &store.sessions_collection] {
        client
            .delete_collection(DeleteCollectionBuilder::new(collection))
            .await
            .unwrap();
    }
    result.unwrap();
}

async fn run_workflow(address: &str) {
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
    assert_eq!(lineage["lineage"], json!(["child", "parent"]));
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
}
