use std::sync::Arc;

use bytes::Bytes;
use rad::engine::kv::fault::{
    FaultAction, FaultController, FaultRule, FaultingKv, Operation, TraceEvent, TraceOutcome,
    TracePhase,
};
use rad::engine::kv::slatedb::Store;
use rad::engine::kv::{
    Entry, ErrorKind, IsolationLevel, KeyRange, Kv, KvIterator, Result, TransactionalKv,
};
use slatedb::object_store::ObjectStore;
use slatedb::object_store::local::LocalFileSystem;
use tempfile::TempDir;

mod support;

use support::s3::{RustFs, TestResult, object_store};

async fn collect(mut iterator: Box<dyn KvIterator + '_>) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    while let Some(entry) = iterator.next().await? {
        entries.push(entry);
    }
    Ok(entries)
}

async fn qualify_backend(store: Arc<dyn TransactionalKv>) -> Result<()> {
    let committed = store.begin(IsolationLevel::Snapshot).await?;
    committed.put(
        Bytes::from_static(b"atomic/committed"),
        Bytes::from_static(b"yes"),
    )?;
    committed.put(
        Bytes::from_static(b"atomic/together"),
        Bytes::from_static(b"yes"),
    )?;
    committed.commit().await?;
    let rolled_back = store.begin(IsolationLevel::Snapshot).await?;
    rolled_back.put(
        Bytes::from_static(b"atomic/committed"),
        Bytes::from_static(b"no"),
    )?;
    rolled_back.delete(b"atomic/together")?;
    rolled_back.rollback();
    assert_eq!(
        Kv::get(&*store, b"atomic/committed").await?,
        Some(Bytes::from_static(b"yes"))
    );
    assert_eq!(
        Kv::get(&*store, b"atomic/together").await?,
        Some(Bytes::from_static(b"yes"))
    );

    let before_delete = store.begin(IsolationLevel::Snapshot).await?;
    let deletion = store.begin(IsolationLevel::SerializableSnapshot).await?;
    deletion.delete(b"atomic/together")?;
    deletion.commit().await?;
    assert_eq!(Kv::get(&*store, b"atomic/together").await?, None);
    assert_eq!(
        before_delete.get(b"atomic/together").await?,
        Some(Bytes::from_static(b"yes"))
    );
    before_delete.rollback();

    Kv::put(
        &*store,
        Bytes::from_static(b"snapshot/value"),
        Bytes::from_static(b"old"),
    )
    .await?;
    let snapshot = store.begin(IsolationLevel::Snapshot).await?;
    let position = snapshot.begin_position().as_str().to_owned();
    Kv::put(
        &*store,
        Bytes::from_static(b"snapshot/value"),
        Bytes::from_static(b"new"),
    )
    .await?;
    assert_eq!(
        snapshot.get(b"snapshot/value").await?,
        Some(Bytes::from_static(b"old"))
    );
    snapshot.put(
        Bytes::from_static(b"snapshot/own"),
        Bytes::from_static(b"visible"),
    )?;
    assert_eq!(
        snapshot.get(b"snapshot/own").await?,
        Some(Bytes::from_static(b"visible"))
    );
    assert!(!position.is_empty());
    snapshot.rollback();

    for key in [b"scan/a", b"scan/b", b"scan/c", b"scan/d"] {
        Kv::put(
            &*store,
            Bytes::copy_from_slice(key),
            Bytes::copy_from_slice(key),
        )
        .await?;
    }
    let opened = Kv::scan(
        &*store,
        KeyRange::new(Bytes::from_static(b"scan/b"), Bytes::from_static(b"scan/d")),
    )
    .await?;
    Kv::put(
        &*store,
        Bytes::from_static(b"scan/bb"),
        Bytes::from_static(b"late"),
    )
    .await?;
    let entries = collect(opened).await?;
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.key.clone())
            .collect::<Vec<_>>(),
        vec![Bytes::from_static(b"scan/b"), Bytes::from_static(b"scan/c")]
    );

    let bulk = store.begin(IsolationLevel::SerializableSnapshot).await?;
    for value in 0..256 {
        let key = Bytes::from(format!("bulk/{value:04}"));
        bulk.put(key.clone(), key)?;
    }
    bulk.commit().await?;
    let entries = collect(
        Kv::scan(
            &*store,
            KeyRange::new(Bytes::from_static(b"bulk/"), Bytes::from_static(b"bulk0")),
        )
        .await?,
    )
    .await?;
    assert_eq!(entries.len(), 256);
    assert_eq!(
        entries.first().unwrap().key,
        Bytes::from_static(b"bulk/0000")
    );
    assert_eq!(
        entries.last().unwrap().key,
        Bytes::from_static(b"bulk/0255")
    );

    let first = store.begin(IsolationLevel::Snapshot).await?;
    let second = store.begin(IsolationLevel::Snapshot).await?;
    first.put(
        Bytes::from_static(b"conflict/same"),
        Bytes::from_static(b"first"),
    )?;
    second.put(
        Bytes::from_static(b"conflict/same"),
        Bytes::from_static(b"second"),
    )?;
    first.commit().await?;
    assert_eq!(
        second.commit().await.unwrap_err().kind(),
        ErrorKind::Conflict
    );

    Kv::put(
        &*store,
        Bytes::from_static(b"skew/a"),
        Bytes::from_static(b"1"),
    )
    .await?;
    Kv::put(
        &*store,
        Bytes::from_static(b"skew/b"),
        Bytes::from_static(b"1"),
    )
    .await?;
    let first = store.begin(IsolationLevel::Snapshot).await?;
    let second = store.begin(IsolationLevel::Snapshot).await?;
    assert_eq!(first.get(b"skew/b").await?, Some(Bytes::from_static(b"1")));
    assert_eq!(second.get(b"skew/a").await?, Some(Bytes::from_static(b"1")));
    first.put(Bytes::from_static(b"skew/a"), Bytes::from_static(b"0"))?;
    second.put(Bytes::from_static(b"skew/b"), Bytes::from_static(b"0"))?;
    first.commit().await?;
    second.commit().await?;

    Kv::put(
        &*store,
        Bytes::from_static(b"serial/watched"),
        Bytes::from_static(b"old"),
    )
    .await?;
    let reader = store.begin(IsolationLevel::SerializableSnapshot).await?;
    assert_eq!(
        reader.get(b"serial/watched").await?,
        Some(Bytes::from_static(b"old"))
    );
    let writer = store.begin(IsolationLevel::SerializableSnapshot).await?;
    writer.put(
        Bytes::from_static(b"serial/watched"),
        Bytes::from_static(b"new"),
    )?;
    writer.commit().await?;
    reader.put(
        Bytes::from_static(b"serial/other"),
        Bytes::from_static(b"value"),
    )?;
    assert_eq!(
        reader.commit().await.unwrap_err().kind(),
        ErrorKind::Conflict
    );

    let reader = store.begin(IsolationLevel::SerializableSnapshot).await?;
    assert!(
        collect(
            reader
                .scan(KeyRange::new(
                    Bytes::from_static(b"phantom/"),
                    Bytes::from_static(b"phantom0"),
                ))
                .await?
        )
        .await?
        .is_empty()
    );
    let writer = store.begin(IsolationLevel::SerializableSnapshot).await?;
    writer.put(
        Bytes::from_static(b"phantom/key"),
        Bytes::from_static(b"value"),
    )?;
    writer.commit().await?;
    assert_eq!(
        reader.commit().await.unwrap_err().kind(),
        ErrorKind::Conflict
    );

    store.close().await
}

async fn qualify_reopen(objects: Arc<dyn ObjectStore>, path: &str) -> Result<()> {
    let store = Store::open(path, objects.clone()).await?;
    let transaction = store.begin(IsolationLevel::SerializableSnapshot).await?;
    transaction.put(
        Bytes::from_static(b"durable/committed"),
        Bytes::from_static(b"present"),
    )?;
    transaction.commit().await?;
    let rolled_back = store.begin(IsolationLevel::SerializableSnapshot).await?;
    rolled_back.put(
        Bytes::from_static(b"durable/rolled-back"),
        Bytes::from_static(b"absent"),
    )?;
    rolled_back.rollback();
    store.close().await?;

    let reopened = Store::open(path, objects).await?;
    assert_eq!(
        reopened.get(b"durable/committed").await?,
        Some(Bytes::from_static(b"present"))
    );
    assert_eq!(reopened.get(b"durable/rolled-back").await?, None);
    reopened.close().await
}

async fn qualify_writer_fencing(objects: Arc<dyn ObjectStore>, path: &str) -> Result<()> {
    let first = Store::open(path, objects.clone()).await?;
    let seed = first.begin(IsolationLevel::SerializableSnapshot).await?;
    seed.put(
        Bytes::from_static(b"writer/value"),
        Bytes::from_static(b"first"),
    )?;
    seed.commit().await?;
    let stale = first.begin(IsolationLevel::SerializableSnapshot).await?;
    stale.put(
        Bytes::from_static(b"writer/value"),
        Bytes::from_static(b"stale"),
    )?;

    let second = Store::open(path, objects).await?;
    let winner = second.begin(IsolationLevel::SerializableSnapshot).await?;
    winner.put(
        Bytes::from_static(b"writer/value"),
        Bytes::from_static(b"second"),
    )?;
    winner.commit().await?;

    assert_eq!(
        stale.commit().await.unwrap_err().kind(),
        ErrorKind::CommitOutcomeUnknown
    );
    let fenced = match first.begin(IsolationLevel::SerializableSnapshot).await {
        Ok(_) => panic!("fenced writer admitted a new transaction"),
        Err(error) => error,
    };
    assert_eq!(fenced.kind(), ErrorKind::Closed);
    assert_eq!(
        second.get(b"writer/value").await?,
        Some(Bytes::from_static(b"second"))
    );
    let _ = first.close().await;
    second.close().await
}

#[tokio::test]
async fn memory_backend_satisfies_transaction_contract() -> Result<()> {
    qualify_backend(Arc::new(Store::memory("qualification-memory").await?)).await
}

#[tokio::test]
async fn local_backend_satisfies_transaction_contract_and_reopens() -> Result<()> {
    let directory = TempDir::new().expect("temporary backend directory");
    let objects: Arc<dyn ObjectStore> = Arc::new(
        LocalFileSystem::new_with_prefix(directory.path()).expect("local object-store root"),
    );
    qualify_backend(Arc::new(Store::open("contract", objects.clone()).await?)).await?;
    qualify_reopen(objects, "reopen").await
}

#[tokio::test]
async fn a_new_local_writer_fences_the_old_writer() -> Result<()> {
    let directory = TempDir::new().expect("temporary backend directory");
    let objects: Arc<dyn ObjectStore> = Arc::new(
        LocalFileSystem::new_with_prefix(directory.path()).expect("local object-store root"),
    );
    qualify_writer_fencing(objects, "writer-fence").await
}

#[tokio::test]
#[ignore = "requires a Docker daemon or RAD_TEST_S3_ENDPOINT"]
async fn rustfs_backend_satisfies_the_storage_qualification_suite() -> TestResult {
    let rustfs = RustFs::start_or_external().await?;
    let objects = object_store(&rustfs.config)?;

    qualify_backend(Arc::new(Store::open("contract", objects.clone()).await?)).await?;
    qualify_reopen(objects.clone(), "reopen").await?;
    qualify_writer_fencing(objects, "writer-fence").await?;

    Ok(())
}

#[tokio::test]
async fn injected_commit_outcomes_are_classified_and_traced() -> Result<()> {
    let underlying = Arc::new(Store::memory("qualification-fault-commit").await?);
    let before = FaultController::new(vec![FaultRule {
        operation: Operation::Commit,
        occurrence: 1,
        action: FaultAction::ErrorBefore(ErrorKind::Unavailable),
    }]);
    let blocked = FaultingKv::new(underlying.clone(), before.clone());
    let transaction = blocked.begin(IsolationLevel::SerializableSnapshot).await?;
    transaction.put(
        Bytes::from_static(b"fault/before"),
        Bytes::from_static(b"absent"),
    )?;
    assert_eq!(
        transaction.commit().await.unwrap_err().kind(),
        ErrorKind::Unavailable
    );
    assert_eq!(underlying.get(b"fault/before").await?, None);

    let after = FaultController::new(vec![FaultRule {
        operation: Operation::Commit,
        occurrence: 1,
        action: FaultAction::ErrorAfter(ErrorKind::CommitOutcomeUnknown),
    }]);
    let unknown = FaultingKv::new(underlying.clone(), after.clone());
    let transaction = unknown.begin(IsolationLevel::SerializableSnapshot).await?;
    transaction.put(
        Bytes::from_static(b"fault/after"),
        Bytes::from_static(b"committed"),
    )?;
    assert_eq!(
        transaction.commit().await.unwrap_err().kind(),
        ErrorKind::CommitOutcomeUnknown
    );
    assert_eq!(
        underlying.get(b"fault/after").await?,
        Some(Bytes::from_static(b"committed"))
    );
    let trace = after.trace();
    assert_eq!(
        trace.last().map(|event| (event.phase, event.outcome)),
        Some((
            TracePhase::Finished,
            TraceOutcome::Injected(ErrorKind::CommitOutcomeUnknown)
        ))
    );
    let json = serde_json::to_vec(&trace).expect("trace serializes");
    assert_eq!(
        serde_json::from_slice::<Vec<TraceEvent>>(&json).expect("trace deserializes"),
        trace
    );
    underlying.close().await
}

#[tokio::test]
async fn iterator_faults_have_replayable_occurrences() -> Result<()> {
    let underlying = Arc::new(Store::memory("qualification-fault-iterator").await?);
    for key in [b"iterator/a", b"iterator/b"] {
        underlying
            .put(Bytes::copy_from_slice(key), Bytes::copy_from_slice(key))
            .await?;
    }
    let controller = FaultController::new(vec![FaultRule {
        operation: Operation::IteratorNext,
        occurrence: 2,
        action: FaultAction::ErrorBefore(ErrorKind::Unavailable),
    }]);
    let faulting = FaultingKv::new(underlying.clone(), controller.clone());
    let mut iterator = faulting
        .scan(KeyRange::new(
            Bytes::from_static(b"iterator/"),
            Bytes::from_static(b"iterator0"),
        ))
        .await?;
    assert_eq!(
        iterator.next().await?.map(|entry| entry.key),
        Some(Bytes::from_static(b"iterator/a"))
    );
    assert_eq!(
        iterator.next().await.unwrap_err().kind(),
        ErrorKind::Unavailable
    );
    let trace = controller.trace();
    assert!(
        trace
            .windows(2)
            .all(|events| events[0].sequence < events[1].sequence)
    );
    assert!(trace.iter().any(|event| {
        event.operation == Operation::IteratorNext
            && event.occurrence == 2
            && event.outcome == TraceOutcome::Injected(ErrorKind::Unavailable)
    }));
    drop(iterator);
    underlying.close().await
}
