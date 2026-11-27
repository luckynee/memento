use std::sync::Arc;

use memento_core::{
    lineage::SessionKey,
    protocol::{Kind, Retention, SessionRecord},
    segment::Segment,
};
use mementod::{
    embed::FakeEmbedder,
    store::{ScopeFilter, Store},
};
use qdrant_client::{Qdrant, qdrant::DeleteCollectionBuilder};
use uuid::Uuid;

#[tokio::test]
async fn qdrant_storage_round_trip() {
    let Ok(url) = std::env::var("MEMENTO_TEST_QDRANT_URL") else {
        eprintln!("skipping Qdrant integration test: MEMENTO_TEST_QDRANT_URL is unset");
        return;
    };
    let client = Qdrant::from_url(&url).build().unwrap();
    let prefix = format!(
        "memento_test_{}",
        Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            format!("{:?}", std::time::SystemTime::now()).as_bytes()
        )
        .simple()
    );
    let store = Store::new(client.clone(), &prefix, Arc::new(FakeEmbedder));
    let result = run_test(&store).await;
    for collection in [
        format!("{prefix}_segments"),
        format!("{prefix}_sessions"),
        format!("{prefix}_continuations"),
    ] {
        if client.collection_exists(&collection).await.unwrap() {
            client
                .delete_collection(DeleteCollectionBuilder::new(collection))
                .await
                .unwrap();
        }
    }
    result.unwrap();
}

#[tokio::test]
async fn sqlite_storage_round_trip() {
    let path = std::env::temp_dir().join(format!(
        "memento-store-{}.db",
        Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            format!("{:?}", std::time::SystemTime::now()).as_bytes()
        )
    ));
    let store = Store::sqlite(path.clone(), Arc::new(FakeEmbedder))
        .await
        .unwrap();
    run_test(&store).await.unwrap();
    drop(store);
    let store = Store::sqlite(path.clone(), Arc::new(FakeEmbedder))
        .await
        .unwrap();
    store.ensure_schema().await.unwrap();
    assert_eq!(
        store.count_segments(&ScopeFilter::default()).await.unwrap(),
        1
    );
    drop(store);
    std::fs::remove_file(path).unwrap();
}

async fn run_test(store: &Store) -> anyhow::Result<()> {
    store.ensure_schema().await?;
    store.ensure_schema().await?;
    let segment = |source: &str, session: &str, text: &str| Segment {
        id: Uuid::new_v5(&Uuid::NAMESPACE_OID, source.as_bytes()),
        harness: "h".into(),
        project: "p".into(),
        session: session.into(),
        kind: Kind::FileRef,
        text: text.into(),
        source_id: source.into(),
        chunk_index: 0,
        ts: "2026-11-18T10:00:00Z".parse().unwrap(),
        tool: Some("read".into()),
        path: None,
        paths: Vec::new(),
    };
    let lexical = segment("lexical", "s1", "read src/segment.rs");
    let semantic = segment("semantic", "s2", "semantic memory vector embedding");
    store
        .upsert_segments(&[lexical.clone(), semantic.clone()])
        .await?;
    let all = ScopeFilter {
        harness: Some("h".into()),
        project: Some("p".into()),
        ..Default::default()
    };
    let hits = store.hybrid_candidates("segment.rs", &all, 50).await?;
    assert!(hits.iter().any(|(segment, _)| segment.id == lexical.id));
    let hits = store
        .hybrid_candidates("semantic memory vector embedding", &all, 50)
        .await?;
    assert!(hits.iter().any(|(segment, _)| segment.id == semantic.id));
    assert_eq!(store.scroll_segments(&all).await?.len(), 2);
    assert_eq!(store.count_by_kind(&all).await?.get("file_ref"), Some(&2));
    let session = SessionRecord {
        harness: "h".into(),
        project: "p".into(),
        session: "s1".into(),
        parent: Some("parent".into()),
        parent_harness: None,
        parent_cutoff: Some("2026-11-18T10:00:00Z".parse().unwrap()),
        retention: Retention::Keep,
        title: Some("title".into()),
        created: Some("2026-11-18T10:00:00Z".parse().unwrap()),
        last_active_at: None,
        deleted_at: None,
        deleted_reason: None,
        collected_at: None,
    };
    store.upsert_session(session.clone()).await?;
    assert_eq!(store.get_session("h", "s1").await?, Some(session.clone()));
    assert_eq!(store.list_sessions("h", Some("p")).await?, vec![session]);
    assert_eq!(
        store.session_links().await?,
        vec![(
            SessionKey {
                harness: "h".into(),
                session: "s1".into()
            },
            Some(SessionKey {
                harness: "h".into(),
                session: "parent".into()
            })
        )]
    );
    let filtered = ScopeFilter {
        sessions: Some(vec![SessionKey {
            harness: "h".into(),
            session: "s1".into(),
        }]),
        ..all.clone()
    };
    assert_eq!(store.delete_segments(&filtered).await?, 1);
    assert_eq!(store.scroll_segments(&all).await?, vec![semantic]);
    Ok(())
}

struct BatchSlowEmbedder;

#[async_trait::async_trait]
impl mementod::embed::Embedder for BatchSlowEmbedder {
    fn dimensions(&self) -> usize {
        mementod::embed::DIMENSIONS
    }

    async fn embed_documents(&self, texts: Vec<String>) -> anyhow::Result<Vec<Vec<f32>>> {
        if texts.len() > 1 {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
        FakeEmbedder.embed_documents(texts).await
    }
}

#[tokio::test]
async fn sqlite_search_completes_while_a_large_ingest_is_embedding() {
    use memento_core::{lineage::Ancestor, protocol::Role};
    let path =
        std::env::temp_dir().join(format!("memento-ingest-search-{}.db", std::process::id()));
    let store = Arc::new(
        Store::sqlite(path.clone(), Arc::new(BatchSlowEmbedder))
            .await
            .unwrap(),
    );
    store.ensure_schema().await.unwrap();
    let segments: Vec<Segment> = (0..320)
        .map(|index| Segment {
            id: Uuid::new_v5(
                &Uuid::NAMESPACE_OID,
                format!("ingest-search-{index}").as_bytes(),
            ),
            harness: "h".into(),
            project: "p".into(),
            session: "s".into(),
            kind: Kind::UserRequest,
            text: format!("request number {index} about topic{index} widget{index}"),
            source_id: format!("m{index}"),
            chunk_index: 0,
            ts: "2026-11-18T10:00:00Z".parse().unwrap(),
            tool: None,
            path: None,
            paths: Vec::new(),
        })
        .collect();
    let roles = segments
        .iter()
        .map(|segment| (segment.source_id.clone(), Role::User))
        .collect();
    let lineage = vec![Ancestor {
        key: SessionKey {
            harness: "h".into(),
            session: "s".into(),
        },
        cutoff: None,
    }];
    let ingesting = {
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            store
                .ingest_segments(&segments, &roles, &lineage, 0.95, 0.9)
                .await
                .unwrap()
        })
    };
    let filter = ScopeFilter {
        harness: Some("h".into()),
        project: Some("p".into()),
        ..Default::default()
    };
    let mut latencies = Vec::new();
    while !ingesting.is_finished() {
        let started = std::time::Instant::now();
        store
            .hybrid_candidates("topic7 widget7", &filter, 10)
            .await
            .unwrap();
        latencies.push(started.elapsed());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let (written, _) = ingesting.await.unwrap();
    assert_eq!(written, 320);
    assert!(
        latencies.len() >= 4,
        "only {} searches ran",
        latencies.len()
    );
    let slowest = latencies.iter().max().unwrap();
    assert!(
        *slowest < std::time::Duration::from_millis(300),
        "search blocked for {slowest:?} during ingest"
    );
    drop(store);
    let _ = std::fs::remove_file(path);
}
