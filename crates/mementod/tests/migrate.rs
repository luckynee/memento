use std::{path::PathBuf, sync::Arc};

use memento_core::{
    protocol::{ContinuationRecord, Kind, Retention, SessionRecord},
    segment::Segment,
};
use mementod::{
    embed::FakeEmbedder,
    migrate::migrate,
    store::{ScopeFilter, Store},
};
use qdrant_client::{Qdrant, qdrant::DeleteCollectionBuilder};
use uuid::Uuid;

fn temporary_path() -> PathBuf {
    let name = Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!("{:?}-{}", std::time::SystemTime::now(), std::process::id()).as_bytes(),
    );
    std::env::temp_dir().join(format!("memento-migrate-test-{name}.db"))
}

#[tokio::test]
async fn migration_refuses_existing_destinations_and_sidecars() {
    let path = temporary_path();
    std::fs::write(&path, b"unchanged").unwrap();
    let error = migrate(
        "http://127.0.0.1:1",
        "unused",
        &path,
        false,
        Arc::new(FakeEmbedder),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("already exists"));
    let sidecar = PathBuf::from(format!("{}-wal", path.display()));
    std::fs::write(&sidecar, b"").unwrap();
    let error = migrate(
        "http://127.0.0.1:1",
        "unused",
        &path,
        true,
        Arc::new(FakeEmbedder),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("stop the daemon"));
    assert_eq!(std::fs::read(&path).unwrap(), b"unchanged");
    std::fs::remove_file(sidecar).unwrap();
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn migration_cleans_temporary_database_on_source_failure() {
    let directory = temporary_path();
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("destination.db");
    let result = migrate(
        "http://127.0.0.1:1",
        "unused",
        &path,
        false,
        Arc::new(FakeEmbedder),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
    std::fs::remove_dir(directory).unwrap();
}

#[tokio::test]
async fn migration_refuses_locked_destination() {
    let path = temporary_path();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute_batch("CREATE TABLE active(value); BEGIN EXCLUSIVE")
        .unwrap();
    let error = migrate(
        "http://127.0.0.1:1",
        "unused",
        &path,
        true,
        Arc::new(FakeEmbedder),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("stop the daemon"));
    connection.execute_batch("ROLLBACK").unwrap();
    drop(connection);
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn qdrant_migration_round_trip() -> anyhow::Result<()> {
    let Ok(url) = std::env::var("MEMENTO_TEST_QDRANT_URL") else {
        eprintln!("skipping Qdrant migration test: MEMENTO_TEST_QDRANT_URL is unset");
        return Ok(());
    };
    let path = temporary_path();
    let prefix = format!(
        "memento_migration_{}",
        Uuid::new_v5(&Uuid::NAMESPACE_OID, path.to_string_lossy().as_bytes()).simple()
    );
    let client = Qdrant::from_url(&url).build()?;
    let source = Store::new(client.clone(), &prefix, Arc::new(FakeEmbedder));
    source.ensure_schema().await?;
    let segment = Segment {
        id: Uuid::new_v5(&Uuid::NAMESPACE_OID, b"migrate segment"),
        harness: "h".into(),
        project: "p".into(),
        session: "s".into(),
        kind: Kind::FileRef,
        text: "read src/segment.rs".into(),
        source_id: "source".into(),
        chunk_index: 0,
        ts: "2026-11-18T10:00:00Z".parse()?,
        tool: Some("read".into()),
        path: Some("src/segment.rs".into()),
        paths: vec!["src/segment.rs".into()],
    };
    let session = SessionRecord {
        harness: "h".into(),
        project: "p".into(),
        session: "s".into(),
        parent: None,
        parent_harness: None,
        parent_cutoff: None,
        retention: Retention::Keep,
        title: Some("kept tombstone".into()),
        created: Some("2026-11-18T10:00:00Z".parse()?),
        last_active_at: None,
        deleted_at: Some("2026-11-19T10:00:00Z".parse()?),
        deleted_reason: Some("deleted".into()),
        collected_at: None,
    };
    let continuation = ContinuationRecord {
        project: "p".into(),
        parent_harness: "h".into(),
        parent_session: "s".into(),
        target_harness: Some("other".into()),
        created: "2026-11-18T10:00:00Z".parse()?,
        claimed_by: None,
        claimed_at: None,
    };
    source
        .upsert_segments(std::slice::from_ref(&segment))
        .await?;
    source.upsert_session(session.clone()).await?;
    source.upsert_continuation(continuation.clone()).await?;
    let source_continuations = source.list_continuations("p").await?;
    let report = migrate(&url, &prefix, &path, false, Arc::new(FakeEmbedder)).await?;
    assert_eq!(report.counts, [1, 1, 1]);
    let destination = Store::sqlite(path.clone(), Arc::new(FakeEmbedder)).await?;
    assert_eq!(
        destination.scroll_segments(&ScopeFilter::default()).await?,
        vec![segment]
    );
    assert_eq!(destination.get_session("h", "s").await?, Some(session));
    assert_eq!(
        destination.list_continuations("p").await?,
        source_continuations
    );
    drop(destination);
    for collection in ["segments", "sessions", "continuations"] {
        client
            .delete_collection(DeleteCollectionBuilder::new(format!(
                "{prefix}_{collection}"
            )))
            .await?;
    }
    std::fs::remove_file(path)?;
    Ok(())
}
