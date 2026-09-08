use std::collections::{HashMap, VecDeque};
use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use crate::engine::catalog;
use crate::engine::catalog::model::Table;
use crate::engine::kv::{DataPosition, KvView};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SnapshotTableKey {
    // A logical table name can resolve to different physical metadata at two
    // storage positions. The position keeps both meanings correct while old
    // and current snapshot transactions overlap.
    position: DataPosition,
    name: String,
}

#[derive(Clone)]
enum FlightResult {
    Success(Option<Arc<Table>>),
    Cancelled,
}

struct Flight {
    result: watch::Sender<Option<FlightResult>>,
}

impl Flight {
    fn new() -> Self {
        let (result, _) = watch::channel(None);
        Self { result }
    }

    async fn wait(mut receiver: watch::Receiver<Option<FlightResult>>) -> FlightResult {
        loop {
            let result = receiver.borrow_and_update().clone();
            if let Some(result) = result {
                return result;
            }
            if receiver.changed().await.is_err() {
                return FlightResult::Cancelled;
            }
        }
    }
}

struct Entry {
    table: Arc<Table>,
    retained_bytes: usize,
}

#[derive(Default)]
struct State {
    entries: HashMap<SnapshotTableKey, Entry>,
    insertion_order: VecDeque<SnapshotTableKey>,
    flights: HashMap<SnapshotTableKey, Arc<Flight>>,
    retained_bytes: usize,
}

#[derive(Default)]
struct Metrics {
    hits: AtomicU64,
    misses: AtomicU64,
    coalesced: AtomicU64,
    evictions: AtomicU64,
    rejected_too_large: AtomicU64,
}

pub(super) struct SnapshotCatalogCache {
    state: Mutex<State>,
    metrics: Metrics,
    entry_limit: usize,
    byte_limit: usize,
}

impl SnapshotCatalogCache {
    pub(super) fn new(entry_limit: usize, byte_limit: usize) -> Self {
        crate::telemetry::relation_cache_catalog_residency(0, 0);
        Self {
            state: Mutex::new(State::default()),
            metrics: Metrics::default(),
            entry_limit: entry_limit.max(1),
            // Catalog metadata is auxiliary cache state. Its complete byte
            // budget is no larger than one permitted relation result.
            byte_limit: byte_limit.max(1),
        }
    }

    pub(super) async fn get_table(
        &self,
        view: &dyn KvView,
        name: &str,
    ) -> catalog::Result<Option<Table>> {
        self.get_table_arc(view, name)
            .await
            .map(|table| table.map(|table| (*table).clone()))
    }

    pub(super) async fn table_matches(
        &self,
        view: &dyn KvView,
        name: &str,
        id: &crate::engine::catalog::identity::TableId,
        definition_generation: crate::engine::catalog::identity::DefinitionGeneration,
    ) -> catalog::Result<bool> {
        Ok(self.get_table_arc(view, name).await?.is_some_and(|table| {
            table.id == *id && table.definition_generation == definition_generation
        }))
    }

    async fn get_table_arc(
        &self,
        view: &dyn KvView,
        name: &str,
    ) -> catalog::Result<Option<Arc<Table>>> {
        let Some(position) = view.begin_position().cloned() else {
            crate::telemetry::relation_cache_catalog_lookup("unpositioned");
            return catalog::store::get_table(view, name)
                .await
                .map(|table| table.map(Arc::new));
        };
        let key = SnapshotTableKey {
            position,
            name: name.to_owned(),
        };
        loop {
            let lookup = {
                let mut state = self
                    .state
                    .lock()
                    .expect("snapshot catalog cache lock poisoned");
                if let Some(entry) = state.entries.get(&key) {
                    Lookup::Hit(entry.table.clone())
                } else if let Some(flight) = state.flights.get(&key) {
                    Lookup::Wait(flight.result.subscribe())
                } else {
                    let flight = Arc::new(Flight::new());
                    state.flights.insert(key.clone(), flight.clone());
                    Lookup::Read(flight)
                }
            };
            match lookup {
                Lookup::Hit(table) => {
                    self.metrics.hits.fetch_add(1, Ordering::Relaxed);
                    crate::telemetry::relation_cache_catalog_lookup("hit");
                    return Ok(Some(table));
                }
                Lookup::Wait(receiver) => {
                    self.metrics.coalesced.fetch_add(1, Ordering::Relaxed);
                    crate::telemetry::relation_cache_catalog_lookup("coalesced");
                    match Flight::wait(receiver).await {
                        FlightResult::Success(table) => {
                            return Ok(table);
                        }
                        FlightResult::Cancelled => continue,
                    }
                }
                Lookup::Read(flight) => {
                    self.metrics.misses.fetch_add(1, Ordering::Relaxed);
                    crate::telemetry::relation_cache_catalog_lookup("miss");
                    let mut owner = FlightOwner::new(self, key.clone(), flight);
                    match catalog::store::get_table(view, name).await {
                        Ok(table) => {
                            let table = table.map(Arc::new);
                            if let Some(table) = table.as_ref() {
                                self.insert(key.clone(), table.clone());
                            }
                            owner.finish(FlightResult::Success(table.clone()));
                            return Ok(table);
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
        }
    }

    fn insert(&self, key: SnapshotTableKey, table: Arc<Table>) {
        let retained_bytes = retained_bytes(&key, &table);
        if retained_bytes > self.byte_limit {
            self.metrics
                .rejected_too_large
                .fetch_add(1, Ordering::Relaxed);
            crate::telemetry::relation_cache_catalog_lookup("too_large");
            return;
        }
        let mut state = self
            .state
            .lock()
            .expect("snapshot catalog cache lock poisoned");
        if state.entries.contains_key(&key) {
            return;
        }
        let mut evictions = 0u64;
        // FIFO uses only request event order. Wall time cannot change cache
        // behavior in deterministic scheduling tests. Old positions enter
        // first, so pressure normally removes metadata for old snapshots.
        while state.entries.len() >= self.entry_limit
            || state.retained_bytes.saturating_add(retained_bytes) > self.byte_limit
        {
            let oldest = state
                .insertion_order
                .pop_front()
                .expect("snapshot catalog insertion order is complete");
            if let Some(removed) = state.entries.remove(&oldest) {
                state.retained_bytes = state.retained_bytes.saturating_sub(removed.retained_bytes);
                evictions = evictions.saturating_add(1);
            }
        }
        state.retained_bytes = state.retained_bytes.saturating_add(retained_bytes);
        state.insertion_order.push_back(key.clone());
        state.entries.insert(
            key,
            Entry {
                table,
                retained_bytes,
            },
        );
        if evictions > 0 {
            self.metrics
                .evictions
                .fetch_add(evictions, Ordering::Relaxed);
            crate::telemetry::relation_cache_catalog_eviction(evictions);
        }
        crate::telemetry::relation_cache_catalog_residency(
            state.entries.len() as u64,
            state.retained_bytes as u64,
        );
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use bytes::Bytes;

    use crate::engine::catalog::identity::{
        DefinitionGeneration, ExistenceGeneration, SchemaId, StorageGeneration, ValueGeneration,
        WriteProtocolGeneration,
    };
    use crate::engine::catalog::model::{Column, ScalarType};
    use crate::engine::exec::observe::{KvCounters, ObservedView};
    use crate::engine::kv::slatedb::Store;
    use crate::engine::kv::{
        IsolationLevel, KeyRange, KvIterator, TransactionView, TransactionalKv,
    };

    use super::*;

    fn table(id: &str, schema_id: u32, name: &str) -> Table {
        Table {
            id: id.into(),
            schema_id: SchemaId::new(schema_id).unwrap(),
            name: name.into(),
            definition_generation: DefinitionGeneration::ZERO,
            existence_generation: ExistenceGeneration::from(1),
            write_protocol_generation: WriteProtocolGeneration::from(1),
            storage_generation: StorageGeneration::INITIAL,
            columns: vec![Column {
                id: format!("c{schema_id}").into(),
                schema_id: SchemaId::new(schema_id).unwrap(),
                name: "id".into(),
                value_generation: ValueGeneration::from(1),
                scalar_type: ScalarType::Text,
                nullable: false,
                format: "uuid".into(),
                insert_default: None,
                missing_value: None,
            }],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            constraints: Vec::new(),
        }
    }

    async fn save(store: &Store, mut table: Table) {
        let transaction = store
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        {
            let mut view = TransactionView(&*transaction);
            catalog::store::save_table(&mut view, &mut table)
                .await
                .unwrap();
            catalog::store::save_table_name(&view, &table.name, &table.id)
                .await
                .unwrap();
        }
        transaction.commit().await.unwrap();
    }

    struct SlowView<'a> {
        inner: &'a dyn KvView,
    }

    #[async_trait]
    impl KvView for SlowView<'_> {
        fn begin_position(&self) -> Option<&DataPosition> {
            self.inner.begin_position()
        }

        async fn get(&self, key: &[u8]) -> crate::engine::kv::Result<Option<Bytes>> {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            self.inner.get(key).await
        }

        async fn put(&self, key: Bytes, value: Bytes) -> crate::engine::kv::Result<()> {
            self.inner.put(key, value).await
        }

        async fn delete(&self, key: &[u8]) -> crate::engine::kv::Result<()> {
            self.inner.delete(key).await
        }

        async fn scan<'a>(
            &'a self,
            range: KeyRange,
        ) -> crate::engine::kv::Result<Box<dyn KvIterator + 'a>> {
            self.inner.scan(range).await
        }
    }

    #[tokio::test]
    async fn repeated_table_reads_use_the_same_snapshot_entry() {
        let store = Store::memory("snapshot-catalog-repeat").await.unwrap();
        save(&store, table("t1", 1, "items")).await;
        let cache = SnapshotCatalogCache::new(16, 64 * 1024);
        let transaction = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let view = TransactionView(&*transaction);
        let counters = KvCounters::new(false);
        let observed = ObservedView::new(&view, &counters);

        let first = cache.get_table(&observed, "items").await.unwrap().unwrap();
        let second = cache.get_table(&observed, "items").await.unwrap().unwrap();

        assert_eq!(first, second);
        assert_eq!(counters.snapshot().gets, 2);
        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.entries, 1);
        assert!(stats.retained_bytes > 0);
        transaction.rollback();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn old_and_current_snapshot_metadata_can_coexist() {
        let store = Store::memory("snapshot-catalog-positions").await.unwrap();
        save(&store, table("t1", 1, "items")).await;
        let cache = SnapshotCatalogCache::new(16, 64 * 1024);
        let old = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let old_view = TransactionView(&*old);
        let old_table = cache.get_table(&old_view, "items").await.unwrap().unwrap();

        let writer = store
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        {
            let mut writer_view = TransactionView(&*writer);
            let mut changed = catalog::store::get_table(&writer_view, "items")
                .await
                .unwrap()
                .unwrap();
            changed.columns[0].format = "email".into();
            catalog::store::save_table(&mut writer_view, &mut changed)
                .await
                .unwrap();
        }
        writer.commit().await.unwrap();

        let current = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let current_view = TransactionView(&*current);
        let current_table = cache
            .get_table(&current_view, "items")
            .await
            .unwrap()
            .unwrap();
        let old_again = cache.get_table(&old_view, "items").await.unwrap().unwrap();

        assert_eq!(old_table.columns[0].format, "uuid");
        assert_eq!(current_table.columns[0].format, "email");
        assert_eq!(old_again.columns[0].format, "uuid");
        assert_eq!(cache.stats().entries, 2);
        current.rollback();
        old.rollback();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_table_reads_share_one_storage_read() {
        let store = Store::memory("snapshot-catalog-concurrent").await.unwrap();
        save(&store, table("t1", 1, "items")).await;
        let cache = SnapshotCatalogCache::new(16, 64 * 1024);
        let transaction = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let view = TransactionView(&*transaction);
        let counters = KvCounters::new(false);
        let observed = ObservedView::new(&view, &counters);
        let slow = SlowView { inner: &observed };

        let (first, second) = tokio::join!(
            cache.get_table(&slow, "items"),
            cache.get_table(&slow, "items")
        );

        assert_eq!(first.unwrap(), second.unwrap());
        assert_eq!(counters.snapshot().gets, 2);
        assert_eq!(cache.stats().coalesced, 1);
        transaction.rollback();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn missing_tables_are_not_retained() {
        let store = Store::memory("snapshot-catalog-missing").await.unwrap();
        let cache = SnapshotCatalogCache::new(16, 64 * 1024);
        let transaction = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let view = TransactionView(&*transaction);

        assert!(cache.get_table(&view, "missing").await.unwrap().is_none());
        assert!(cache.get_table(&view, "missing").await.unwrap().is_none());

        let stats = cache.stats();
        assert_eq!(stats.misses, 2);
        assert_eq!(stats.entries, 0);
        transaction.rollback();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn entry_and_byte_limits_bound_catalog_metadata() {
        let store = Store::memory("snapshot-catalog-limits").await.unwrap();
        save(&store, table("t1", 1, "items")).await;
        save(&store, table("t2", 2, "orders")).await;
        let transaction = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let view = TransactionView(&*transaction);
        let cache = SnapshotCatalogCache::new(1, 64 * 1024);

        cache.get_table(&view, "items").await.unwrap().unwrap();
        cache.get_table(&view, "orders").await.unwrap().unwrap();
        cache.get_table(&view, "items").await.unwrap().unwrap();

        let stats = cache.stats();
        assert_eq!(stats.entries, 1);
        assert_eq!(stats.evictions, 2);

        let too_small = SnapshotCatalogCache::new(16, 1);
        too_small.get_table(&view, "items").await.unwrap().unwrap();
        too_small.get_table(&view, "items").await.unwrap().unwrap();
        let stats = too_small.stats();
        assert_eq!(stats.entries, 0);
        assert_eq!(stats.rejected_too_large, 2);
        transaction.rollback();
        store.close().await.unwrap();
    }
}

enum Lookup {
    Hit(Arc<Table>),
    Wait(watch::Receiver<Option<FlightResult>>),
    Read(Arc<Flight>),
}

/// The size is conservative for decoded metadata. JSON length accounts for
/// string and collection contents. The fixed sizes account for decoded
/// containers, reference counts, and the lookup key. Allocator metadata is
/// not available and is not included.
fn retained_bytes(key: &SnapshotTableKey, table: &Table) -> usize {
    let encoded = serde_json::to_vec(table)
        .map(|value| value.len())
        .unwrap_or(usize::MAX);
    size_of::<SnapshotTableKey>()
        .saturating_add(key.position.as_str().len())
        .saturating_add(key.name.capacity())
        .saturating_add(size_of::<Entry>())
        .saturating_add(size_of::<Table>())
        .saturating_add(encoded)
}

struct FlightOwner<'a> {
    cache: &'a SnapshotCatalogCache,
    key: SnapshotTableKey,
    flight: Arc<Flight>,
    finished: bool,
}

impl<'a> FlightOwner<'a> {
    fn new(cache: &'a SnapshotCatalogCache, key: SnapshotTableKey, flight: Arc<Flight>) -> Self {
        Self {
            cache,
            key,
            flight,
            finished: false,
        }
    }

    fn finish(&mut self, result: FlightResult) {
        self.flight.result.send_replace(Some(result));
        self.remove();
        self.finished = true;
    }

    fn remove(&self) {
        let mut state = self
            .cache
            .state
            .lock()
            .expect("snapshot catalog cache lock poisoned");
        if state
            .flights
            .get(&self.key)
            .is_some_and(|flight| Arc::ptr_eq(flight, &self.flight))
        {
            state.flights.remove(&self.key);
        }
    }
}

impl Drop for FlightOwner<'_> {
    fn drop(&mut self) {
        if !self.finished {
            // An error or cancelled owner does not create retained state.
            // Waiters retry through their own pinned views.
            self.flight
                .result
                .send_replace(Some(FlightResult::Cancelled));
            self.remove();
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct SnapshotCatalogStats {
    pub hits: u64,
    pub misses: u64,
    pub coalesced: u64,
    pub evictions: u64,
    pub rejected_too_large: u64,
    pub entries: u64,
    pub retained_bytes: u64,
}

#[cfg(test)]
impl SnapshotCatalogCache {
    pub(super) fn stats(&self) -> SnapshotCatalogStats {
        let state = self
            .state
            .lock()
            .expect("snapshot catalog cache lock poisoned");
        SnapshotCatalogStats {
            hits: self.metrics.hits.load(Ordering::Relaxed),
            misses: self.metrics.misses.load(Ordering::Relaxed),
            coalesced: self.metrics.coalesced.load(Ordering::Relaxed),
            evictions: self.metrics.evictions.load(Ordering::Relaxed),
            rejected_too_large: self.metrics.rejected_too_large.load(Ordering::Relaxed),
            entries: state.entries.len() as u64,
            retained_bytes: state.retained_bytes as u64,
        }
    }
}
