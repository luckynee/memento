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
        &store.segments_collection,
        &store.sessions_collection,
        &store.continuations_collection,
    ] {
        if client.collection_exists(collection).await.unwrap() {
            client
                .delete_collection(DeleteCollectionBuilder::new(collection))
                .await
                .unwrap();
        }
    }
    result.unwrap();
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
