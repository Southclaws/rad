#![no_main]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use rad::engine::kv::slatedb::{ReaderStore, Store};
use rad::engine::kv::{ErrorKind, IsolationLevel, KeyRange, Kv, ScanOrder, TransactionalKv};
use slatedb::bytes::Bytes;
use slatedb::object_store::ObjectStore;
use slatedb::object_store::memory::InMemory;
use tokio::runtime::Runtime;

struct Harness {
    writer: Arc<Store>,
    reader: Arc<ReaderStore>,
}

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static HARNESS: OnceLock<Arc<Harness>> = OnceLock::new();
static CASE: AtomicU64 = AtomicU64::new(0);

fuzz_target!(|data: &[u8]| {
    if data.len() > 64 {
        return;
    }
    let runtime = RUNTIME.get_or_init(|| Runtime::new().expect("Tokio runtime creation failed"));
    let harness = Arc::clone(HARNESS.get_or_init(|| {
        runtime.block_on(async {
            let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            let writer = Arc::new(
                Store::open("reader-snapshot-fuzz", Arc::clone(&objects))
                    .await
                    .expect("writer open failed"),
            );
            let reader = Arc::new(
                ReaderStore::open("reader-snapshot-fuzz", objects, Duration::from_millis(1))
                    .await
                    .expect("reader open failed"),
            );
            Arc::new(Harness { writer, reader })
        })
    }));
    runtime.block_on(run_case(harness, data));
});

async fn run_case(harness: Arc<Harness>, data: &[u8]) {
    let case = CASE.fetch_add(1, Ordering::Relaxed);
    let rows = data.first().map_or(1, |value| usize::from(value % 8) + 1);
    let generations = data.get(1).map_or(1, |value| usize::from(value % 4) + 1);
    let prefix = format!("case/{case:016x}/");
    let initial = harness
        .writer
        .begin(IsolationLevel::SerializableSnapshot)
        .await
        .expect("initial transaction failed");
    for row in 0..rows {
        let index = Bytes::from(format!("{prefix}index/{row:02}"));
        let base = Bytes::from(format!("{prefix}row/00/{row:02}"));
        initial
            .put(index, base.clone())
            .expect("initial index write failed");
        initial
            .put(base.clone(), base)
            .expect("initial base write failed");
    }
    initial.commit().await.expect("initial commit failed");
    let initial_sentinel = format!("{prefix}row/00/00");
    wait_for_value(&harness.reader, initial_sentinel.as_bytes(), true).await;

    let snapshot = harness
        .reader
        .begin(IsolationLevel::Snapshot)
        .await
        .expect("reader snapshot failed");
    let range = KeyRange::new(
        Bytes::from(format!("{prefix}index/")),
        Bytes::from(format!("{prefix}index0")),
    );
    let order = if data.get(2).is_some_and(|value| value & 1 == 1) {
        ScanOrder::Descending
    } else {
        ScanOrder::Ascending
    };
    let mut index = snapshot
        .scan_ordered(range.clone(), order)
        .await
        .expect("snapshot index scan failed");
    let mut entries = Vec::new();
    while let Some(entry) = index.next().await.expect("snapshot index read failed") {
        entries.push(entry);
    }
    drop(index);
    assert_eq!(entries.len(), rows);

    for generation in 1..=generations {
        let replacement = harness
            .writer
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .expect("replacement transaction failed");
        for row in 0..rows {
            let index = Bytes::from(format!("{prefix}index/{row:02}"));
            let old = Bytes::from(format!("{prefix}row/{:02}/{row:02}", generation - 1));
            let new = Bytes::from(format!("{prefix}row/{generation:02}/{row:02}"));
            replacement
                .delete(&index)
                .expect("replacement index delete failed");
            replacement
                .delete(&old)
                .expect("replacement base delete failed");
            replacement
                .put(index, new.clone())
                .expect("replacement index write failed");
            replacement
                .put(new.clone(), new)
                .expect("replacement base write failed");
        }
        replacement
            .commit()
            .await
            .expect("replacement commit failed");
    }
    wait_for_value(&harness.reader, initial_sentinel.as_bytes(), false).await;

    for entry in &entries {
        match snapshot.get(&entry.value).await {
            Ok(value) => assert_eq!(value, Some(entry.value.clone())),
            Err(error) if error.kind() == ErrorKind::Conflict => {}
            Err(error) => panic!("snapshot base read failed: {error}"),
        }
    }
    let error = match snapshot.scan_ordered(range, order).await {
        Ok(_) => panic!("the repeated snapshot scan did not detect the refresh"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ErrorKind::Conflict);
    snapshot.rollback();
}

async fn wait_for_value(reader: &ReaderStore, key: &[u8], present: bool) {
    for _ in 0..10_000 {
        let value = Kv::get(reader, key).await.expect("reader refresh failed");
        if value.is_some() == present {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("reader did not refresh");
}
