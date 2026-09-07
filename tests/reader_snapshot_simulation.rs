use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use rad::engine::kv::slatedb::{ReaderStore, Store};
use rad::engine::kv::{ErrorKind, IsolationLevel, KeyRange, Kv, TransactionalKv};
use slatedb::object_store::ObjectStore;
use slatedb::object_store::memory::InMemory;

#[test]
fn reader_snapshot_conflicts_are_deterministic_for_task_schedules()
-> Result<(), Box<dyn std::error::Error>> {
    for seed in 0..16 {
        run_case(seed)?;
    }
    Ok(())
}

fn run_case(seed: u64) -> Result<(), Box<dyn std::error::Error>> {
    let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let phase = Arc::new(AtomicUsize::new(0));
    let path = format!("reader-snapshot-schedule-{seed}");

    let mut builder = turmoil::Builder::new();
    builder
        .rng_seed(seed)
        .enable_random_order()
        .tick_duration(Duration::from_millis(1))
        .simulation_duration(Duration::from_secs(10));
    let mut simulation = builder.build();
    simulation.host("writer", {
        let objects = Arc::clone(&objects);
        let phase = Arc::clone(&phase);
        let path = path.clone();
        move || {
            let objects = Arc::clone(&objects);
            let phase = Arc::clone(&phase);
            let path = path.clone();
            async move {
                let writer = Store::open(path, objects).await?;
                let initial = writer.begin(IsolationLevel::SerializableSnapshot).await?;
                initial.put(
                    Bytes::from_static(b"index/active/item"),
                    Bytes::from_static(b"row/old"),
                )?;
                initial.put(
                    Bytes::from_static(b"row/old"),
                    Bytes::from_static(b"value/old"),
                )?;
                initial.commit().await?;
                phase.store(1, Ordering::SeqCst);

                wait_for_phase(&phase, 2).await;
                let replacement = writer.begin(IsolationLevel::SerializableSnapshot).await?;
                replacement.delete(b"index/active/item")?;
                replacement.delete(b"row/old")?;
                replacement.put(
                    Bytes::from_static(b"index/active/item"),
                    Bytes::from_static(b"row/new"),
                )?;
                replacement.put(
                    Bytes::from_static(b"row/new"),
                    Bytes::from_static(b"value/new"),
                )?;
                replacement.commit().await?;
                phase.store(3, Ordering::SeqCst);

                wait_for_phase(&phase, 4).await;
                writer.close().await?;
                phase.store(5, Ordering::SeqCst);
                Ok(())
            }
        }
    });
    simulation.host("reader", {
        let objects = Arc::clone(&objects);
        let phase = Arc::clone(&phase);
        move || {
            let objects = Arc::clone(&objects);
            let phase = Arc::clone(&phase);
            let path = path.clone();
            async move {
                wait_for_phase(&phase, 1).await;
                let reader = ReaderStore::open(path, objects, Duration::from_millis(1)).await?;
                wait_for_value(&reader, b"row/old", Some(b"value/old")).await?;

                let transaction = reader.begin(IsolationLevel::Snapshot).await?;
                let mut index = transaction
                    .scan(KeyRange::new(
                        Bytes::from_static(b"index/active/"),
                        Bytes::from_static(b"index/active0"),
                    ))
                    .await?;
                let entry = index.next().await?.expect("active index entry is missing");
                drop(index);
                phase.store(2, Ordering::SeqCst);

                wait_for_phase(&phase, 3).await;
                wait_for_value(&reader, b"row/old", None).await?;
                assert_eq!(
                    transaction.get(&entry.value).await.unwrap_err().kind(),
                    ErrorKind::Conflict
                );
                transaction.rollback();
                reader.close().await?;
                phase.store(4, Ordering::SeqCst);
                Ok(())
            }
        }
    });

    for _ in 0..10_000 {
        simulation.step()?;
        if phase.load(Ordering::SeqCst) == 5 {
            return Ok(());
        }
    }
    Err(format!("reader snapshot schedule {seed} did not complete").into())
}

async fn wait_for_phase(phase: &AtomicUsize, expected: usize) {
    while phase.load(Ordering::SeqCst) < expected {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

async fn wait_for_value(
    reader: &ReaderStore,
    key: &[u8],
    expected: Option<&'static [u8]>,
) -> rad::engine::kv::Result<()> {
    loop {
        if Kv::get(reader, key).await?.as_deref() == expected {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}
