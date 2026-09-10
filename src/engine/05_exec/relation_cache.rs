//! Bounded materialized relation cache.
//!
//! Cache identity is a correctness proof. It combines the exact bound LIR
//! fingerprint with all catalog, storage, access, and data generations that
//! can affect the relation value. The key does not use a database-wide data
//! position. A commit to an unrelated table therefore does not change the
//! key. Entries for different visible generations can remain resident at the
//! same time. Eviction removes them by cache policy; mutation does not remove
//! them directly.
//!
//! Every persistent read added to LIR must also add its complete catalog
//! dependency set. An omitted dependency can permit a stale cache hit. Every
//! logical row mutation must advance its table data generation in the same
//! transaction as the row writes. These two rules are required for cache
//! correctness.

mod policy;
mod prepared_read;
mod snapshot_catalog;

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::mem::size_of;
use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use foyer::{Cache, CacheProperties, Event, EventListener, LfuConfig};
use sha2::{Digest as _, Sha256};
use tokio::sync::watch;

use crate::engine::catalog::identity::{
    AccessGeneration, ColumnId, ExistenceGeneration, IndexId, StorageGeneration, TableId,
    ValueGeneration, WriteProtocolGeneration,
};
use crate::engine::catalog::model::CatalogDependencies;
use crate::engine::catalog::store;
use crate::engine::catalog::store::TableDataGeneration;
use crate::engine::kv::{DataPosition, KvView};
use crate::engine::lir::eval::Env;
use crate::engine::lir::fingerprint::Fingerprint;
use crate::engine::lir::{Datum, ObjectField, RootCardinality, RowType, Value};

use super::observe::KvWork;
use super::observe::StatementSource;
use super::{Error, ErrorKind, ErrorReason, Result};
use policy::{CohortToken, RelationCachePolicy};
use prepared_read::PreparedReadCache;
pub(super) use prepared_read::PreparedReadResult;
use snapshot_catalog::SnapshotCatalogCache;

const DEFAULT_BYTE_LIMIT: usize = 128 * 1024 * 1024;
const DEFAULT_ENTRY_LIMIT: usize = 4_096;
const DEFAULT_RESULT_BYTE_LIMIT: usize = 8 * 1024 * 1024;
const CACHE_SHARDS: usize = 8;
const ACCOUNTING_PENDING: u8 = 0;
const ACCOUNTING_RETAINED: u8 = 1;
const ACCOUNTING_REMOVED: u8 = 2;
const QUERY_VALIDATOR_DOMAIN: &[u8] = b"rad-query-validator";
const QUERY_RESULT_REPRESENTATION: &[u8] = b"application/json;rad-datum-v1";

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct QueryValidator([u8; 32]);

impl QueryValidator {
    fn from_key_parts(exact: Fingerprint, dependencies: &[DependencyGeneration]) -> QueryValidator {
        // The encoding has explicit tags, lengths, and big-endian integers.
        // Rust layout and Hash implementations are not stable protocol inputs.
        let mut hash = Sha256::new();
        hash_bytes(&mut hash, 1, QUERY_VALIDATOR_DOMAIN);
        hash_bytes(&mut hash, 2, &exact.to_bytes());
        hash_u64(&mut hash, 3, dependencies.len() as u64);
        for dependency in dependencies {
            dependency.hash_query_validator(&mut hash);
        }
        hash_bytes(&mut hash, 4, QUERY_RESULT_REPRESENTATION);
        QueryValidator(hash.finalize().into())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RelationCacheLimits {
    pub byte_limit: usize,
    pub entry_limit: usize,
    pub result_byte_limit: usize,
}

impl Default for RelationCacheLimits {
    fn default() -> Self {
        Self {
            byte_limit: DEFAULT_BYTE_LIMIT,
            entry_limit: DEFAULT_ENTRY_LIMIT,
            result_byte_limit: DEFAULT_RESULT_BYTE_LIMIT,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DependencyValidation {
    /// The caller cannot write through this transaction. A stable storage
    /// position can identify a completed dependency validation.
    Snapshot,
    /// The caller can add writes after this read. Each dependency fence must
    /// enter the transaction read set even when the relation result is cached.
    Transaction,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum CatalogDependencyGeneration {
    Table {
        table_id: TableId,
        existence_generation: ExistenceGeneration,
        storage_generation: StorageGeneration,
    },
    Column {
        table_id: TableId,
        column_id: ColumnId,
        generation: ValueGeneration,
    },
    Index {
        table_id: TableId,
        index_id: IndexId,
        generation: AccessGeneration,
    },
    WriteProtocol {
        table_id: TableId,
        generation: WriteProtocolGeneration,
    },
}

impl CatalogDependencyGeneration {
    fn collect(dependencies: &CatalogDependencies) -> Vec<Self> {
        let mut generations = Vec::with_capacity(
            dependencies.table_existence.len()
                + dependencies.column_values.len()
                + dependencies.index_access.len()
                + dependencies.write_protocols.len(),
        );
        generations.extend(
            dependencies
                .table_existence
                .iter()
                .map(|dependency| Self::Table {
                    table_id: dependency.table_id.clone(),
                    existence_generation: dependency.generation,
                    storage_generation: dependency.storage_generation,
                }),
        );
        generations.extend(
            dependencies
                .column_values
                .iter()
                .map(|dependency| Self::Column {
                    table_id: dependency.table_id.clone(),
                    column_id: dependency.column_id.clone(),
                    generation: dependency.generation,
                }),
        );
        generations.extend(
            dependencies
                .index_access
                .iter()
                .map(|dependency| Self::Index {
                    table_id: dependency.table_id.clone(),
                    index_id: dependency.index_id.clone(),
                    generation: dependency.generation,
                }),
        );
        generations.extend(dependencies.write_protocols.iter().map(|dependency| {
            Self::WriteProtocol {
                table_id: dependency.table_id.clone(),
                generation: dependency.generation,
            }
        }));
        generations.sort_unstable();
        generations.dedup();
        generations
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SnapshotDependencyKey {
    // The position is only a scope for dependency validation. It is not part
    // of RelationCacheKey, so unrelated commits do not change result identity.
    position: DataPosition,
    exact: Fingerprint,
    // One exact relation can receive different access plans from different
    // statistics. Include the physical dependency set so one plan cannot use
    // validation completed for another plan.
    dependencies: Vec<CatalogDependencyGeneration>,
}

impl SnapshotDependencyKey {
    fn new(position: DataPosition, exact: Fingerprint, dependencies: &CatalogDependencies) -> Self {
        Self {
            position,
            exact,
            dependencies: CatalogDependencyGeneration::collect(dependencies),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum DependencyGeneration {
    Table {
        table_id: TableId,
        existence_generation: ExistenceGeneration,
        storage_generation: StorageGeneration,
        data_generation: TableDataGeneration,
    },
    Column {
        table_id: TableId,
        column_id: ColumnId,
        generation: ValueGeneration,
    },
    Index {
        table_id: TableId,
        index_id: IndexId,
        generation: AccessGeneration,
    },
    WriteProtocol {
        table_id: TableId,
        generation: WriteProtocolGeneration,
    },
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct RelationCacheKey {
    exact: Fingerprint,
    dependencies: Vec<DependencyGeneration>,
    // Snapshot dependency reuse can return this key for every request at one
    // storage position. Store the derived validator with the key so a
    // conditional hit does not repeat the canonical SHA-256 calculation.
    query_validator: QueryValidator,
}

impl RelationCacheKey {
    /// Read data generations through the relation's pinned view. Reading a
    /// latest value outside this view can give an old transaction a key for
    /// data that it cannot observe.
    pub async fn for_view(
        exact: Fingerprint,
        view: &dyn KvView,
        dependencies: &CatalogDependencies,
    ) -> Result<Self> {
        // A cache hit skips the executor and its dependency admission. Read
        // all catalog fences here so the hit has the same conflict protection
        // as execution. This is important when an explicit transaction reads
        // from the cache and then writes before commit.
        store::admit_catalog_dependencies(view, dependencies).await?;
        let mut data_generations = HashMap::with_capacity(dependencies.table_existence.len());
        for dependency in &dependencies.table_existence {
            // This tracked range read is also the logical data conflict fence
            // for a cache hit. Every committed row mutation writes at least
            // one stripe in this range.
            let generation = store::read_table_data_generation(view, &dependency.table_id).await?;
            data_generations.insert(dependency.table_id.clone(), generation);
        }
        Ok(Self::from_generations(
            exact,
            dependencies,
            &data_generations,
        ))
    }

    fn from_generations(
        exact: Fingerprint,
        dependencies: &CatalogDependencies,
        data_generations: &HashMap<TableId, TableDataGeneration>,
    ) -> Self {
        let mut generations = Vec::with_capacity(
            dependencies.table_existence.len()
                + dependencies.column_values.len()
                + dependencies.index_access.len()
                + dependencies.write_protocols.len(),
        );
        generations.extend(dependencies.table_existence.iter().map(|dependency| {
            DependencyGeneration::Table {
                table_id: dependency.table_id.clone(),
                existence_generation: dependency.generation,
                storage_generation: dependency.storage_generation,
                data_generation: *data_generations
                    .get(&dependency.table_id)
                    .expect("table data generation is present"),
            }
        }));
        generations.extend(dependencies.column_values.iter().map(|dependency| {
            DependencyGeneration::Column {
                table_id: dependency.table_id.clone(),
                column_id: dependency.column_id.clone(),
                generation: dependency.generation,
            }
        }));
        generations.extend(dependencies.index_access.iter().map(|dependency| {
            DependencyGeneration::Index {
                table_id: dependency.table_id.clone(),
                index_id: dependency.index_id.clone(),
                generation: dependency.generation,
            }
        }));
        generations.extend(dependencies.write_protocols.iter().map(|dependency| {
            DependencyGeneration::WriteProtocol {
                table_id: dependency.table_id.clone(),
                generation: dependency.generation,
            }
        }));
        Self::from_key_parts(exact, generations)
    }

    fn from_key_parts(exact: Fingerprint, mut dependencies: Vec<DependencyGeneration>) -> Self {
        // Catalog dependency collection order is not part of relation
        // identity. Canonical order also removes duplicate dependency records.
        dependencies.sort_unstable();
        dependencies.dedup();
        let query_validator = QueryValidator::from_key_parts(exact, &dependencies);
        Self {
            exact,
            dependencies,
            query_validator,
        }
    }

    fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(
                self.dependencies
                    .capacity()
                    .saturating_mul(size_of::<DependencyGeneration>()),
            )
            .saturating_add(
                self.dependencies
                    .iter()
                    .map(DependencyGeneration::dynamic_bytes)
                    .fold(0usize, usize::saturating_add),
            )
    }

    pub(super) fn query_validator(&self) -> QueryValidator {
        self.query_validator
    }
}

impl DependencyGeneration {
    fn dynamic_bytes(&self) -> usize {
        match self {
            Self::Table { table_id, .. } | Self::WriteProtocol { table_id, .. } => {
                table_id.as_str().len()
            }
            Self::Column {
                table_id,
                column_id,
                ..
            } => table_id
                .as_str()
                .len()
                .saturating_add(column_id.as_str().len()),
            Self::Index {
                table_id, index_id, ..
            } => table_id
                .as_str()
                .len()
                .saturating_add(index_id.as_str().len()),
        }
    }

    fn hash_query_validator(&self, hash: &mut Sha256) {
        match self {
            Self::Table {
                table_id,
                existence_generation,
                storage_generation,
                data_generation,
            } => {
                hash_u8(hash, 10, 1);
                hash_bytes(hash, 11, table_id.as_str().as_bytes());
                hash_u64(hash, 12, existence_generation.get());
                hash_u64(hash, 13, storage_generation.get());
                hash_u64(hash, 14, data_generation.stripes().len() as u64);
                for generation in data_generation.stripes() {
                    hash_u64(hash, 20, generation.get());
                }
            }
            Self::Column {
                table_id,
                column_id,
                generation,
            } => {
                hash_u8(hash, 10, 2);
                hash_bytes(hash, 11, table_id.as_str().as_bytes());
                hash_bytes(hash, 15, column_id.as_str().as_bytes());
                hash_u64(hash, 16, generation.get());
            }
            Self::Index {
                table_id,
                index_id,
                generation,
            } => {
                hash_u8(hash, 10, 3);
                hash_bytes(hash, 11, table_id.as_str().as_bytes());
                hash_bytes(hash, 17, index_id.as_str().as_bytes());
                hash_u64(hash, 18, generation.get());
            }
            Self::WriteProtocol {
                table_id,
                generation,
            } => {
                hash_u8(hash, 10, 4);
                hash_bytes(hash, 11, table_id.as_str().as_bytes());
                hash_u64(hash, 19, generation.get());
            }
        }
    }
}

fn hash_u8(hash: &mut Sha256, tag: u8, value: u8) {
    hash.update([tag]);
    hash.update(1u64.to_be_bytes());
    hash.update([value]);
}

fn hash_u64(hash: &mut Sha256, tag: u8, value: u64) {
    hash.update([tag]);
    hash.update(8u64.to_be_bytes());
    hash.update(value.to_be_bytes());
}

fn hash_bytes(hash: &mut Sha256, tag: u8, value: &[u8]) {
    hash.update([tag]);
    hash.update((value.len() as u64).to_be_bytes());
    hash.update(value);
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct CachedWork {
    pub kv: KvWork,
    pub execution: Duration,
}

#[derive(Debug)]
struct CachedRelation {
    rows: Vec<Box<[Datum]>>,
    result_bytes: usize,
    work: CachedWork,
    accounting_state: AtomicU8,
}

impl CachedRelation {
    fn from_frames(output: &RowType, frames: &[Env], work: CachedWork) -> Self {
        // Execution slots are local planner identities. The same exact LIR can
        // receive different slots in another execution. Store values in output
        // field order, then map them to the current output slots on a hit.
        let mut rows = Vec::with_capacity(frames.len());
        for frame in frames {
            let mut row = Vec::with_capacity(output.fields.len());
            for field in &output.fields {
                row.push(frame.get(field.slot).cloned().unwrap_or(Datum::Null));
            }
            rows.push(row.into_boxed_slice());
        }
        let result_bytes = retained_row_bytes(&rows, rows.capacity());
        Self {
            rows,
            result_bytes,
            work,
            accounting_state: AtomicU8::new(ACCOUNTING_PENDING),
        }
    }

    fn restore(&self, output: &RowType) -> Vec<Env> {
        self.rows
            .iter()
            .map(|row| {
                let mut frame = Env::new();
                for (field, datum) in output.fields.iter().zip(row.iter()) {
                    frame.insert(field.slot, datum.clone());
                }
                frame
            })
            .collect()
    }

    fn shape(&self, cardinality: RootCardinality, output: &RowType) -> Result<Datum> {
        super::frames::validate_frame_cardinality(cardinality, self.rows.len())?;
        match cardinality {
            RootCardinality::Many => Ok(Datum::Array(
                self.rows
                    .iter()
                    .map(|row| cached_row_object(output, row))
                    .collect(),
            )),
            RootCardinality::First => Ok(self
                .rows
                .first()
                .map(|row| cached_row_object(output, row))
                .unwrap_or(Datum::Null)),
            RootCardinality::ExactlyOne => Ok(cached_row_object(
                output,
                self.rows.first().expect("cardinality is valid"),
            )),
            RootCardinality::Scalar => Ok(self
                .rows
                .first()
                .and_then(|row| row.first())
                .cloned()
                .unwrap_or(Datum::Null)),
        }
    }

    fn retained_bytes(&self) -> usize {
        size_of::<Self>().saturating_add(self.result_bytes)
    }
}

fn cached_row_object(output: &RowType, row: &[Datum]) -> Datum {
    Datum::Object(
        output
            .fields
            .iter()
            .enumerate()
            .map(|(index, field)| ObjectField {
                name: field.name.clone(),
                datum: row.get(index).cloned().unwrap_or(Datum::Null),
            })
            .collect(),
    )
}

/// Estimate the owned row copy before allocation. The estimate uses logical
/// lengths because the copy does not preserve spare source capacity. The
/// cache checks the allocated value again before admission. An allocator can
/// therefore cause a conservative late rejection, but it cannot let the
/// allocated value exceed the result limit.
fn estimated_result_bytes(output: &RowType, frames: &[Env]) -> usize {
    frames
        .len()
        .saturating_mul(size_of::<Box<[Datum]>>())
        .saturating_add(
            frames
                .iter()
                .map(|frame| {
                    output
                        .fields
                        .len()
                        .saturating_mul(size_of::<Datum>())
                        .saturating_add(
                            output
                                .fields
                                .iter()
                                .filter_map(|field| frame.get(field.slot))
                                .map(cloned_datum_dynamic_bytes)
                                .fold(0usize, usize::saturating_add),
                        )
                })
                .fold(0usize, usize::saturating_add),
        )
}

fn retained_row_bytes(rows: &[Box<[Datum]>], row_capacity: usize) -> usize {
    row_capacity
        .saturating_mul(size_of::<Box<[Datum]>>())
        .saturating_add(
            rows.iter()
                .map(|row| {
                    row.len().saturating_mul(size_of::<Datum>()).saturating_add(
                        row.iter()
                            .map(datum_dynamic_bytes)
                            .fold(0usize, usize::saturating_add),
                    )
                })
                .fold(0usize, usize::saturating_add),
        )
}

fn datum_dynamic_bytes(datum: &Datum) -> usize {
    match datum {
        Datum::Null => 0,
        Datum::Scalar(Value::Text(value)) => value.capacity(),
        Datum::Scalar(_) => 0,
        Datum::Object(fields) => fields
            .capacity()
            .saturating_mul(size_of::<ObjectField>())
            .saturating_add(
                fields
                    .iter()
                    .map(|field| {
                        field
                            .name
                            .capacity()
                            .saturating_add(datum_dynamic_bytes(&field.datum))
                    })
                    .fold(0usize, usize::saturating_add),
            ),
        Datum::Array(elements) => elements
            .capacity()
            .saturating_mul(size_of::<Datum>())
            .saturating_add(
                elements
                    .iter()
                    .map(datum_dynamic_bytes)
                    .fold(0usize, usize::saturating_add),
            ),
    }
}

fn cloned_datum_dynamic_bytes(datum: &Datum) -> usize {
    match datum {
        Datum::Null => 0,
        Datum::Scalar(Value::Text(value)) => value.len(),
        Datum::Scalar(_) => 0,
        Datum::Object(fields) => fields
            .len()
            .saturating_mul(size_of::<ObjectField>())
            .saturating_add(
                fields
                    .iter()
                    .map(|field| {
                        field
                            .name
                            .len()
                            .saturating_add(cloned_datum_dynamic_bytes(&field.datum))
                    })
                    .fold(0usize, usize::saturating_add),
            ),
        Datum::Array(elements) => elements
            .len()
            .saturating_mul(size_of::<Datum>())
            .saturating_add(
                elements
                    .iter()
                    .map(cloned_datum_dynamic_bytes)
                    .fold(0usize, usize::saturating_add),
            ),
    }
}

#[derive(Clone)]
enum FlightResult {
    Success(Arc<CachedRelation>),
    Failure(CachedError),
    Cancelled,
}

#[derive(Clone)]
enum SnapshotFlightResult {
    Success(Arc<RelationCacheKey>),
    Failure(CachedError),
    Cancelled,
}

struct SnapshotFlight {
    result: watch::Sender<Option<SnapshotFlightResult>>,
}

impl SnapshotFlight {
    fn new() -> Self {
        let (result, _) = watch::channel(None);
        Self { result }
    }

    async fn wait(
        mut receiver: watch::Receiver<Option<SnapshotFlightResult>>,
    ) -> SnapshotFlightResult {
        loop {
            let result = receiver.borrow_and_update().clone();
            if let Some(result) = result {
                return result;
            }
            if receiver.changed().await.is_err() {
                return SnapshotFlightResult::Cancelled;
            }
        }
    }
}

#[derive(Default)]
struct SnapshotDependencyState {
    entries: HashMap<SnapshotDependencyKey, Arc<RelationCacheKey>>,
    insertion_order: VecDeque<SnapshotDependencyKey>,
    flights: HashMap<SnapshotDependencyKey, Arc<SnapshotFlight>>,
}

struct SnapshotDependencyCache {
    state: Mutex<SnapshotDependencyState>,
    entry_limit: usize,
}

impl SnapshotDependencyCache {
    fn new(entry_limit: usize) -> Self {
        Self {
            state: Mutex::new(SnapshotDependencyState::default()),
            entry_limit: entry_limit.max(1),
        }
    }

    fn get(&self, key: &SnapshotDependencyKey) -> Option<Arc<RelationCacheKey>> {
        self.state
            .lock()
            .expect("snapshot dependency cache lock poisoned")
            .entries
            .get(key)
            .cloned()
    }

    fn insert(&self, key: SnapshotDependencyKey, value: Arc<RelationCacheKey>) -> usize {
        let mut state = self
            .state
            .lock()
            .expect("snapshot dependency cache lock poisoned");
        if state.entries.contains_key(&key) {
            return 0;
        }
        let mut evictions = 0usize;
        // Old storage positions enter the queue before new positions. FIFO
        // removal therefore gives current snapshots capacity without making
        // position age a correctness rule. An evicted old transaction reads
        // its fences again and can still use the relation result cache.
        while state.entries.len() >= self.entry_limit {
            let oldest = state
                .insertion_order
                .pop_front()
                .expect("snapshot dependency insertion order is complete");
            if state.entries.remove(&oldest).is_some() {
                evictions = evictions.saturating_add(1);
            }
        }
        state.insertion_order.push_back(key.clone());
        state.entries.insert(key, value);
        evictions
    }
}

struct Flight {
    result: watch::Sender<Option<FlightResult>>,
    waiters: AtomicUsize,
}

impl Flight {
    fn new() -> Self {
        let (result, _) = watch::channel(None);
        Self {
            result,
            waiters: AtomicUsize::new(0),
        }
    }

    fn subscribe(&self) -> watch::Receiver<Option<FlightResult>> {
        self.waiters.fetch_add(1, Ordering::Relaxed);
        self.result.subscribe()
    }

    async fn wait(mut receiver: watch::Receiver<Option<FlightResult>>) -> FlightResult {
        // A cancelled owner publishes Cancelled from Drop. A waiter retries
        // ownership instead of returning cancellation as a query failure.
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

#[derive(Clone)]
struct CachedError {
    kind: ErrorKind,
    reason: ErrorReason,
    message: String,
}

impl CachedError {
    fn capture(error: &Error) -> Self {
        Self {
            kind: error.kind(),
            reason: error.reason(),
            message: error.to_string(),
        }
    }

    fn restore(self) -> Error {
        Error::with_reason(self.kind, self.reason, self.message)
    }
}

#[derive(Default)]
struct RelationCacheMetrics {
    hits: AtomicU64,
    misses: AtomicU64,
    admissions: AtomicU64,
    rejected_too_large: AtomicU64,
    rejected_by_policy: AtomicU64,
    evictions: AtomicU64,
    coalesced: AtomicU64,
    retained_bytes: AtomicI64,
    dependency_hits: AtomicU64,
    dependency_misses: AtomicU64,
    dependency_coalesced: AtomicU64,
    dependency_evictions: AtomicU64,
}

struct CacheEvents {
    metrics: Arc<RelationCacheMetrics>,
}

impl EventListener for CacheEvents {
    type Key = RelationCacheKey;
    type Value = Arc<CachedRelation>;

    fn on_leave(&self, event: Event, key: &Self::Key, value: &Self::Value) {
        let bytes = key.retained_bytes().saturating_add(value.retained_bytes()) as u64;
        // Foyer can call on_leave before insert returns. The state exchange
        // prevents a late admission path from adding bytes for an entry that
        // is already absent. It also makes each removal subtract at most once.
        if value
            .accounting_state
            .swap(ACCOUNTING_REMOVED, Ordering::AcqRel)
            == ACCOUNTING_RETAINED
        {
            self.metrics
                .retained_bytes
                .fetch_sub(bytes as i64, Ordering::Relaxed);
        }
        let cause = match event {
            Event::Evict => {
                self.metrics.evictions.fetch_add(1, Ordering::Relaxed);
                "capacity"
            }
            Event::Replace => "replace",
            Event::Remove => "remove",
            Event::Clear => "clear",
        };
        crate::telemetry::relation_cache_eviction(cause);
    }
}

pub(super) struct RelationCache {
    entries: Cache<RelationCacheKey, Arc<CachedRelation>>,
    flights: Mutex<HashMap<RelationCacheKey, Arc<Flight>>>,
    snapshot_dependencies: SnapshotDependencyCache,
    snapshot_catalog: SnapshotCatalogCache,
    prepared_reads: PreparedReadCache,
    policy: RelationCachePolicy,
    metrics: Arc<RelationCacheMetrics>,
    limits: RelationCacheLimits,
    entry_weight: usize,
    result_byte_limit: usize,
}

impl Default for RelationCache {
    fn default() -> Self {
        Self::new(RelationCacheLimits::default())
    }
}

impl RelationCache {
    pub fn new(config: RelationCacheLimits) -> Self {
        let byte_limit = config.byte_limit.clamp(1, i64::MAX as usize);
        let entry_limit = config.entry_limit.max(1);
        // A minimum weight converts the byte capacity into an entry limit.
        // Larger entries still use their estimated retained byte size.
        let entry_weight = byte_limit.div_ceil(entry_limit);
        let shards = CACHE_SHARDS.min(entry_limit).min(byte_limit);
        let metrics = Arc::new(RelationCacheMetrics::default());
        let entries = Cache::builder(byte_limit)
            .with_shards(shards)
            .with_eviction_config(LfuConfig::default())
            .with_weighter(move |key: &RelationCacheKey, value: &Arc<CachedRelation>| {
                key.retained_bytes()
                    .saturating_add(value.retained_bytes())
                    .max(entry_weight)
            })
            .with_event_listener(Arc::new(CacheEvents {
                metrics: metrics.clone(),
            }))
            .build::<CacheProperties>();
        let limits = RelationCacheLimits {
            byte_limit,
            entry_limit,
            result_byte_limit: config.result_byte_limit,
        };
        crate::telemetry::relation_cache_limits(
            limits.entry_limit,
            limits.byte_limit as u64,
            limits.result_byte_limit as u64,
        );
        crate::telemetry::relation_cache_residency(0, 0);
        Self {
            entries,
            flights: Mutex::new(HashMap::new()),
            snapshot_dependencies: SnapshotDependencyCache::new(entry_limit),
            snapshot_catalog: SnapshotCatalogCache::new(entry_limit, config.result_byte_limit),
            prepared_reads: PreparedReadCache::new(entry_limit, config.result_byte_limit),
            policy: RelationCachePolicy::new(entry_limit),
            metrics,
            limits,
            entry_weight,
            result_byte_limit: config.result_byte_limit,
        }
    }

    /// Resolve table metadata through the caller's pinned snapshot.
    ///
    /// Only implicit read-only execution uses this path. An explicit
    /// serializable transaction must read the catalog keys through its own
    /// view so those keys enter its conflict set.
    pub async fn catalog_table_for_snapshot(
        &self,
        view: &dyn KvView,
        name: &str,
    ) -> crate::engine::catalog::Result<Option<crate::engine::catalog::model::Table>> {
        self.snapshot_catalog.get_table(view, name).await
    }

    async fn catalog_table_matches_snapshot(
        &self,
        view: &dyn KvView,
        name: &str,
        id: &TableId,
        definition_generation: crate::engine::catalog::identity::DefinitionGeneration,
    ) -> crate::engine::catalog::Result<bool> {
        self.snapshot_catalog
            .table_matches(view, name, id, definition_generation)
            .await
    }

    /// Reuse binding and physical planning for one exact read request.
    ///
    /// Each candidate keeps its complete physical dependency set. Lookup
    /// validates that set through the caller's pinned view before it returns
    /// the plan. Data changes can reuse a plan and receive a new relation
    /// result key. Catalog changes can reuse a plan only when all dependencies
    /// still match. Explicit transactions do not call this method.
    pub(super) async fn get_or_prepare_read<F, Fut>(
        &self,
        view: &dyn KvView,
        query: &crate::engine::lir::Query,
        statistics: Option<&crate::engine::planner::models::PlannerStats>,
        options: crate::engine::planner::PlanOptions,
        prepare: F,
    ) -> Result<PreparedReadResult>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<crate::engine::planner::bind::BoundStatement>>,
    {
        self.prepared_reads
            .get_or_prepare(self, view, query, statistics, options, prepare)
            .await
    }

    /// Resolve the correctness key for one bound physical relation.
    ///
    /// Snapshot validation can be shared only when the view has the same
    /// storage position, exact relation, and physical dependency set. A memo
    /// hit skips storage reads but does not skip any value check: the stored
    /// key contains the data generations read through that same snapshot.
    /// Failed validation is sent to current waiters and is not stored.
    pub async fn key_for_view(
        &self,
        exact: Fingerprint,
        view: &dyn KvView,
        dependencies: &CatalogDependencies,
        validation: DependencyValidation,
    ) -> Result<RelationCacheKey> {
        if validation == DependencyValidation::Transaction {
            crate::telemetry::relation_cache_dependency_lookup("tracked");
            return RelationCacheKey::for_view(exact, view, dependencies).await;
        }
        let Some(position) = view.begin_position().cloned() else {
            crate::telemetry::relation_cache_dependency_lookup("unpositioned");
            return RelationCacheKey::for_view(exact, view, dependencies).await;
        };
        let snapshot_key = SnapshotDependencyKey::new(position, exact, dependencies);
        loop {
            if let Some(key) = self.snapshot_dependencies.get(&snapshot_key) {
                self.metrics.dependency_hits.fetch_add(1, Ordering::Relaxed);
                crate::telemetry::relation_cache_dependency_lookup("hit");
                return Ok((*key).clone());
            }
            self.metrics
                .dependency_misses
                .fetch_add(1, Ordering::Relaxed);
            crate::telemetry::relation_cache_dependency_lookup("miss");
            let (flight, owner, receiver) = {
                let mut state = self
                    .snapshot_dependencies
                    .state
                    .lock()
                    .expect("snapshot dependency cache lock poisoned");
                if let Some(flight) = state.flights.get(&snapshot_key) {
                    (flight.clone(), false, Some(flight.result.subscribe()))
                } else {
                    let flight = Arc::new(SnapshotFlight::new());
                    state.flights.insert(snapshot_key.clone(), flight.clone());
                    (flight, true, None)
                }
            };
            if !owner {
                self.metrics
                    .dependency_coalesced
                    .fetch_add(1, Ordering::Relaxed);
                crate::telemetry::relation_cache_dependency_lookup("coalesced");
                match SnapshotFlight::wait(receiver.expect("a waiter has a receiver")).await {
                    SnapshotFlightResult::Success(key) => return Ok((*key).clone()),
                    SnapshotFlightResult::Failure(error) => return Err(error.restore()),
                    SnapshotFlightResult::Cancelled => continue,
                }
            }
            let mut owner =
                SnapshotFlightOwner::new(&self.snapshot_dependencies, snapshot_key.clone(), flight);
            // Read through the caller's view. A latest generation from outside
            // this view can identify data that the caller cannot observe.
            match RelationCacheKey::for_view(exact, view, dependencies).await {
                Ok(key) => {
                    let key = Arc::new(key);
                    let evictions = self
                        .snapshot_dependencies
                        .insert(snapshot_key.clone(), key.clone());
                    if evictions > 0 {
                        self.metrics
                            .dependency_evictions
                            .fetch_add(evictions as u64, Ordering::Relaxed);
                        crate::telemetry::relation_cache_dependency_eviction(evictions as u64);
                    }
                    owner.finish(SnapshotFlightResult::Success(key.clone()));
                    return Ok((*key).clone());
                }
                Err(error) => {
                    owner.finish(SnapshotFlightResult::Failure(CachedError::capture(&error)));
                    return Err(error);
                }
            }
        }
    }

    pub async fn get_or_fill<F, Fut>(
        &self,
        key: RelationCacheKey,
        output: &RowType,
        fill: F,
    ) -> Result<RelationCacheResult>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(Vec<Env>, CachedWork)>>,
    {
        let cohort = self.policy.observe_request(&key);
        if let Some(relation) = self.get(&key, cohort) {
            return Ok(RelationCacheResult {
                rows: RelationRows::Cached(relation),
                source: StatementSource::RelationCache,
            });
        }
        self.metrics.misses.fetch_add(1, Ordering::Relaxed);
        crate::telemetry::relation_cache_lookup("miss");
        let mut fill = Some(fill);
        loop {
            // The flight map coalesces only equal correctness keys. Do not use
            // the query fingerprint alone here because visible generations can
            // differ between concurrent transactions.
            let (flight, owner, receiver) = {
                let mut flights = self
                    .flights
                    .lock()
                    .expect("relation cache flight lock poisoned");
                if let Some(flight) = flights.get(&key) {
                    let receiver = flight.subscribe();
                    (flight.clone(), false, Some(receiver))
                } else {
                    let flight = Arc::new(Flight::new());
                    flights.insert(key.clone(), flight.clone());
                    (flight, true, None)
                }
            };
            if !owner {
                self.metrics.coalesced.fetch_add(1, Ordering::Relaxed);
                crate::telemetry::relation_cache_coalesced();
                match Flight::wait(receiver.expect("a flight waiter has a receiver")).await {
                    FlightResult::Success(relation) => {
                        self.policy.observe_coalesced_reuse(cohort);
                        Self::record_avoided(relation.work);
                        return Ok(RelationCacheResult {
                            rows: RelationRows::Cached(relation),
                            source: StatementSource::RelationCache,
                        });
                    }
                    FlightResult::Failure(error) => return Err(error.restore()),
                    FlightResult::Cancelled => continue,
                }
            }
            let mut owner = FlightOwner::new(self, key.clone(), flight);
            // Another owner can admit the key between the first cache lookup
            // and flight ownership. Check again before the expensive read.
            if let Some(relation) = self.get_after_miss(&key, cohort) {
                owner.finish(FlightResult::Success(relation.clone()));
                return Ok(RelationCacheResult {
                    rows: RelationRows::Cached(relation),
                    source: StatementSource::RelationCache,
                });
            }
            let result = fill.take().expect("relation cache fill runs once")().await;
            match result {
                Ok((frames, work)) => {
                    let estimated_bytes = estimated_result_bytes(output, &frames);
                    crate::telemetry::relation_cache_result_size(estimated_bytes);
                    if estimated_bytes > self.result_byte_limit {
                        self.policy.observe_fill(
                            cohort,
                            work,
                            estimated_bytes,
                            key.retained_bytes()
                                .saturating_add(size_of::<CachedRelation>())
                                .saturating_add(estimated_bytes),
                            self.result_byte_limit,
                        );
                        self.reject_too_large();
                        // Waiter registration and flight removal use the same
                        // lock. After detach returns zero, no waiter can still
                        // require a portable copy of this result.
                        if owner.detach() > 0 {
                            let relation =
                                Arc::new(CachedRelation::from_frames(output, &frames, work));
                            owner.publish(FlightResult::Success(relation));
                        }
                        return Ok(RelationCacheResult {
                            rows: RelationRows::Frames(frames),
                            source: StatementSource::Executed,
                        });
                    }
                    let relation = Arc::new(CachedRelation::from_frames(output, &frames, work));
                    self.policy.observe_fill(
                        cohort,
                        work,
                        relation.result_bytes,
                        key.retained_bytes()
                            .saturating_add(relation.retained_bytes()),
                        self.result_byte_limit,
                    );
                    self.admit(key.clone(), relation.clone());
                    // Admission policy can reject this value. Current waiters
                    // still share the successful result. A new request fills
                    // the key again if no entry remains.
                    owner.finish(FlightResult::Success(relation));
                    return Ok(RelationCacheResult {
                        rows: RelationRows::Frames(frames),
                        source: StatementSource::Executed,
                    });
                }
                Err(error) => {
                    // A failed read is shared with current waiters but is never
                    // stored in the relation cache.
                    owner.finish(FlightResult::Failure(CachedError::capture(&error)));
                    return Err(error);
                }
            }
        }
    }

    fn get(&self, key: &RelationCacheKey, cohort: CohortToken) -> Option<Arc<CachedRelation>> {
        let relation = self.entries.get(key)?.value().clone();
        self.policy.observe_cache_hit(cohort);
        self.metrics.hits.fetch_add(1, Ordering::Relaxed);
        crate::telemetry::relation_cache_lookup("hit");
        Self::record_avoided(relation.work);
        Some(relation)
    }

    fn get_after_miss(
        &self,
        key: &RelationCacheKey,
        cohort: CohortToken,
    ) -> Option<Arc<CachedRelation>> {
        let relation = self.entries.get(key)?.value().clone();
        self.policy.observe_cache_hit(cohort);
        Self::record_avoided(relation.work);
        Some(relation)
    }

    fn admit(&self, key: RelationCacheKey, relation: Arc<CachedRelation>) {
        if relation.result_bytes > self.result_byte_limit {
            self.reject_too_large();
            return;
        }
        let retained = key
            .retained_bytes()
            .saturating_add(relation.retained_bytes()) as u64;
        let weight = (retained as usize).max(self.entry_weight);
        let shards = self.entries.shards();
        let shard = self.entries.hash(&key) as usize % shards;
        let shard_capacity =
            self.limits.byte_limit / shards + usize::from(shard < self.limits.byte_limit % shards);
        // Foyer retains one item that is larger than its selected shard. That
        // behavior can make total retained bytes exceed the configured bound.
        // Reject the item before insertion so each shard stays within its
        // share of the total byte limit.
        if weight > shard_capacity {
            self.reject_by_policy();
            return;
        }
        let entry = self.entries.insert(key, relation);
        let admitted = !entry.is_outdated()
            && entry
                .value()
                .accounting_state
                .compare_exchange(
                    ACCOUNTING_PENDING,
                    ACCOUNTING_RETAINED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok();
        if !admitted {
            self.reject_by_policy();
        } else {
            self.metrics.admissions.fetch_add(1, Ordering::Relaxed);
            self.metrics
                .retained_bytes
                .fetch_add(retained as i64, Ordering::Relaxed);
            crate::telemetry::relation_cache_admission("admitted");
        }
        self.record_residency();
    }

    fn reject_too_large(&self) {
        self.metrics
            .rejected_too_large
            .fetch_add(1, Ordering::Relaxed);
        crate::telemetry::relation_cache_admission("too_large");
    }

    fn reject_by_policy(&self) {
        self.metrics
            .rejected_by_policy
            .fetch_add(1, Ordering::Relaxed);
        crate::telemetry::relation_cache_admission("policy_rejected");
    }

    fn record_avoided(work: CachedWork) {
        let reads = work.kv.gets.saturating_add(work.kv.iterated);
        crate::telemetry::relation_cache_avoided(reads, work.kv.bytes_read, work.execution);
    }

    fn record_residency(&self) {
        crate::telemetry::relation_cache_limits(
            self.limits.entry_limit,
            self.limits.byte_limit as u64,
            self.limits.result_byte_limit as u64,
        );
        crate::telemetry::relation_cache_residency(self.entries.entries(), self.retained_bytes());
    }

    fn retained_bytes(&self) -> u64 {
        self.metrics.retained_bytes.load(Ordering::Relaxed).max(0) as u64
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use async_trait::async_trait;
    use bytes::Bytes;
    use tokio::sync::Notify;

    use crate::engine::catalog::model::TableExistenceDependency;
    use crate::engine::exec::observe::{KvCounters, ObservedView};
    use crate::engine::kv::slatedb::Store;
    use crate::engine::kv::{
        IsolationLevel, KeyRange, KvIterator, KvView, TransactionView, TransactionalKv,
    };
    use crate::engine::lir::{Field, Kind, SlotId, Type};

    use super::*;

    fn fingerprint(seed: u8) -> Fingerprint {
        Fingerprint {
            canonicalization_version: 1,
            hash_algorithm: 1,
            digest: [seed; 16],
        }
    }

    fn dependencies(storage_generation: u64) -> CatalogDependencies {
        let mut dependencies = CatalogDependencies::default();
        dependencies.table_existence.push(TableExistenceDependency {
            table_id: "t1".into(),
            table_name: "items".into(),
            generation: 2.into(),
            storage_generation: storage_generation.into(),
        });
        dependencies
    }

    fn absent_table_dependencies() -> CatalogDependencies {
        let mut dependencies = dependencies(0);
        dependencies.table_existence[0].generation = 0.into();
        dependencies
    }

    fn key(seed: u8, data_generation: u64) -> RelationCacheKey {
        RelationCacheKey::from_generations(
            fingerprint(seed),
            &dependencies(3),
            &HashMap::from([("t1".into(), data_generation.into())]),
        )
    }

    fn validator_key() -> RelationCacheKey {
        RelationCacheKey::from_key_parts(
            fingerprint(1),
            vec![
                DependencyGeneration::Table {
                    table_id: "t1".into(),
                    existence_generation: 2.into(),
                    storage_generation: 3.into(),
                    data_generation: 4.into(),
                },
                DependencyGeneration::Column {
                    table_id: "t1".into(),
                    column_id: "c1".into(),
                    generation: 5.into(),
                },
                DependencyGeneration::Index {
                    table_id: "t1".into(),
                    index_id: "i1".into(),
                    generation: 6.into(),
                },
                DependencyGeneration::WriteProtocol {
                    table_id: "t1".into(),
                    generation: 7.into(),
                },
            ],
        )
    }

    #[test]
    fn query_validator_covers_every_correctness_key_field() {
        let original = validator_key();
        let validator = original.query_validator();
        assert_eq!(validator, original.clone().query_validator());

        let changed =
            RelationCacheKey::from_key_parts(fingerprint(2), original.dependencies.clone());
        assert_ne!(validator, changed.query_validator());

        let replacements = [
            DependencyGeneration::Table {
                table_id: "t1".into(),
                existence_generation: 8.into(),
                storage_generation: 3.into(),
                data_generation: 4.into(),
            },
            DependencyGeneration::Table {
                table_id: "t1".into(),
                existence_generation: 2.into(),
                storage_generation: 8.into(),
                data_generation: 4.into(),
            },
            DependencyGeneration::Table {
                table_id: "t1".into(),
                existence_generation: 2.into(),
                storage_generation: 3.into(),
                data_generation: 8.into(),
            },
        ];
        for replacement in replacements {
            let mut dependencies = original.dependencies.clone();
            dependencies[0] = replacement;
            let changed = RelationCacheKey::from_key_parts(original.exact, dependencies);
            assert_ne!(validator, changed.query_validator());
        }

        let mut dependencies = original.dependencies.clone();
        dependencies[1] = DependencyGeneration::Column {
            table_id: "t1".into(),
            column_id: "c1".into(),
            generation: 8.into(),
        };
        let changed = RelationCacheKey::from_key_parts(original.exact, dependencies);
        assert_ne!(validator, changed.query_validator());

        let mut dependencies = original.dependencies.clone();
        dependencies[2] = DependencyGeneration::Index {
            table_id: "t1".into(),
            index_id: "i1".into(),
            generation: 8.into(),
        };
        let changed = RelationCacheKey::from_key_parts(original.exact, dependencies);
        assert_ne!(validator, changed.query_validator());

        let mut dependencies = original.dependencies.clone();
        dependencies[3] = DependencyGeneration::WriteProtocol {
            table_id: "t1".into(),
            generation: 8.into(),
        };
        let changed = RelationCacheKey::from_key_parts(original.exact, dependencies);
        assert_ne!(validator, changed.query_validator());

        for index in 1..original.dependencies.len() {
            let mut dependencies = original.dependencies.clone();
            dependencies.remove(index);
            let changed = RelationCacheKey::from_key_parts(original.exact, dependencies);
            assert_ne!(validator, changed.query_validator());
        }
    }

    fn output(slot: usize) -> RowType {
        RowType {
            fields: vec![Field {
                name: "value".into(),
                slot: SlotId(slot),
                value_type: Type::scalar(Kind::Int64, false),
            }],
        }
    }

    fn frames(slot: usize, value: i64) -> Vec<Env> {
        let mut frame = Env::new();
        frame.insert(SlotId(slot), Datum::scalar(Value::Int64(value)));
        vec![frame]
    }

    fn text_frames(slot: usize, value: String) -> Vec<Env> {
        let mut frame = Env::new();
        frame.insert(SlotId(slot), Datum::scalar(Value::Text(value)));
        vec![frame]
    }

    fn config() -> RelationCacheLimits {
        RelationCacheLimits {
            byte_limit: 64 * 1024,
            entry_limit: 16,
            result_byte_limit: 16 * 1024,
        }
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
            tokio::time::sleep(Duration::from_millis(10)).await;
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
    async fn snapshot_dependency_keys_reuse_only_the_same_snapshot() {
        let store = Store::memory("relation-cache-snapshot-dependencies")
            .await
            .unwrap();
        let cache = RelationCache::new(config());
        let dependencies = absent_table_dependencies();
        let pinned = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let pinned_view = TransactionView(&*pinned);
        let pinned_counters = KvCounters::new(false);
        let pinned_observed = ObservedView::new(&pinned_view, &pinned_counters);

        let first = cache
            .key_for_view(
                fingerprint(1),
                &pinned_observed,
                &dependencies,
                DependencyValidation::Snapshot,
            )
            .await
            .unwrap();
        let second = cache
            .key_for_view(
                fingerprint(1),
                &pinned_observed,
                &dependencies,
                DependencyValidation::Snapshot,
            )
            .await
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(pinned_counters.snapshot().gets, 1);
        assert_eq!(pinned_counters.snapshot().scans, 1);

        let writer = store
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        {
            let mut writer_view = TransactionView(&*writer);
            store::advance_table_data_generation(
                &mut writer_view,
                &TableId::from("t1"),
                [&b"row-1"[..]],
            )
            .await
            .unwrap();
        }
        writer.commit().await.unwrap();

        let current = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let current_view = TransactionView(&*current);
        let current_counters = KvCounters::new(false);
        let current_observed = ObservedView::new(&current_view, &current_counters);
        let third = cache
            .key_for_view(
                fingerprint(1),
                &current_observed,
                &dependencies,
                DependencyValidation::Snapshot,
            )
            .await
            .unwrap();
        assert_ne!(first, third);
        let old_again = cache
            .key_for_view(
                fingerprint(1),
                &pinned_observed,
                &dependencies,
                DependencyValidation::Snapshot,
            )
            .await
            .unwrap();
        assert_eq!(first, old_again);

        let stats = cache.stats();
        assert_eq!(stats.dependency_hits, 2);
        assert_eq!(stats.dependency_misses, 2);
        assert_eq!(pinned_counters.snapshot().gets, 1);
        assert_eq!(pinned_counters.snapshot().scans, 1);
        assert_eq!(current_counters.snapshot().gets, 1);
        assert_eq!(current_counters.snapshot().scans, 1);
        current.rollback();
        pinned.rollback();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn transaction_dependency_validation_always_reads_fences() {
        let store = Store::memory("relation-cache-transaction-dependencies")
            .await
            .unwrap();
        let cache = RelationCache::new(config());
        let transaction = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let transaction_view = TransactionView(&*transaction);
        let counters = KvCounters::new(false);
        let view = ObservedView::new(&transaction_view, &counters);
        let dependencies = absent_table_dependencies();
        for _ in 0..2 {
            cache
                .key_for_view(
                    fingerprint(1),
                    &view,
                    &dependencies,
                    DependencyValidation::Transaction,
                )
                .await
                .unwrap();
        }

        let stats = cache.stats();
        assert_eq!(stats.dependency_hits, 0);
        assert_eq!(stats.dependency_misses, 0);
        assert_eq!(counters.snapshot().gets, 2);
        assert_eq!(counters.snapshot().scans, 2);
        transaction.rollback();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_snapshot_dependency_reads_share_one_validation() {
        let store = Store::memory("relation-cache-concurrent-dependencies")
            .await
            .unwrap();
        let cache = RelationCache::new(config());
        let transaction = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let transaction_view = TransactionView(&*transaction);
        let counters = KvCounters::new(false);
        let observed = ObservedView::new(&transaction_view, &counters);
        let slow = SlowView { inner: &observed };
        let dependencies = absent_table_dependencies();

        let (first, second) = tokio::join!(
            cache.key_for_view(
                fingerprint(1),
                &slow,
                &dependencies,
                DependencyValidation::Snapshot,
            ),
            cache.key_for_view(
                fingerprint(1),
                &slow,
                &dependencies,
                DependencyValidation::Snapshot,
            )
        );
        assert_eq!(first.unwrap(), second.unwrap());
        assert_eq!(counters.snapshot().gets, 1);
        assert_eq!(counters.snapshot().scans, 1);
        assert_eq!(cache.stats().dependency_coalesced, 1);

        transaction.rollback();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn failed_snapshot_dependency_reads_are_not_stored() {
        let store = Store::memory("relation-cache-failed-dependencies")
            .await
            .unwrap();
        let cache = RelationCache::new(config());
        let transaction = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let view = TransactionView(&*transaction);
        for _ in 0..2 {
            assert!(
                cache
                    .key_for_view(
                        fingerprint(1),
                        &view,
                        &dependencies(0),
                        DependencyValidation::Snapshot,
                    )
                    .await
                    .is_err()
            );
        }
        assert_eq!(cache.stats().dependency_misses, 2);
        assert_eq!(cache.stats().dependency_hits, 0);

        transaction.rollback();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn hit_restores_values_to_current_slots() {
        let cache = RelationCache::new(config());
        let cache_key = key(1, 1);
        cache
            .get_or_fill(cache_key.clone(), &output(0), || async {
                Ok((frames(0, 42), CachedWork::default()))
            })
            .await
            .unwrap();

        let hit = cache
            .get_or_fill(cache_key, &output(7), || async {
                Err(Error::message(ErrorKind::Internal, "fill must not run"))
            })
            .await
            .unwrap();
        assert_eq!(
            hit.shape(RootCardinality::Many, &output(7)).unwrap(),
            Datum::Array(vec![Datum::Object(vec![ObjectField {
                name: "value".into(),
                datum: Datum::scalar(Value::Int64(42)),
            }])])
        );
        let hit = hit.into_frames(&output(7));

        assert_eq!(
            hit[0].get(SlotId(7)),
            Some(&Datum::scalar(Value::Int64(42)))
        );
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().misses, 1);
        let policy = cache.policy.stats();
        assert_eq!(policy.requests, 2);
        assert_eq!(policy.reuse_opportunities, 1);
        assert_eq!(policy.cache_hits, 1);
        assert_eq!(policy.fills, 1);
    }

    #[test]
    fn candidate_size_matches_the_owned_materialization() {
        let output = output(0);
        let frames = text_frames(0, "value".repeat(20));
        let estimated = estimated_result_bytes(&output, &frames);
        let relation = CachedRelation::from_frames(&output, &frames, CachedWork::default());

        assert_eq!(estimated, relation.result_bytes);
    }

    #[test]
    fn key_changes_with_identity_data_and_physical_generations() {
        let first_dependencies = dependencies(3);
        let second_dependencies = dependencies(4);
        let data_generations = HashMap::from([("t1".into(), TableDataGeneration::test_value(7))]);
        let first = RelationCacheKey::from_generations(
            fingerprint(1),
            &first_dependencies,
            &data_generations,
        );

        assert_ne!(first, key(2, 7));
        assert_ne!(first, key(1, 8));
        assert_ne!(
            first,
            RelationCacheKey::from_generations(
                fingerprint(1),
                &second_dependencies,
                &data_generations,
            )
        );
    }

    #[tokio::test]
    async fn oversized_results_are_not_admitted() {
        let cache = RelationCache::new(RelationCacheLimits {
            result_byte_limit: 1,
            ..config()
        });
        let cache_key = key(1, 1);
        for value in [1, 2] {
            cache
                .get_or_fill(cache_key.clone(), &output(0), || async move {
                    Ok((frames(0, value), CachedWork::default()))
                })
                .await
                .unwrap();
        }

        let stats = cache.stats();
        assert_eq!(stats.misses, 2);
        assert_eq!(stats.rejected_too_large, 2);
        assert_eq!(stats.entries, 0);
    }

    #[tokio::test]
    async fn failed_fills_are_not_admitted() {
        let cache = RelationCache::new(config());
        let cache_key = key(1, 1);
        for _ in 0..2 {
            let error = cache
                .get_or_fill(cache_key.clone(), &output(0), || async {
                    Err(Error::message(ErrorKind::Runtime, "expected failure"))
                })
                .await
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Runtime);
        }

        let stats = cache.stats();
        assert_eq!(stats.misses, 2);
        assert_eq!(stats.admissions, 0);
        assert_eq!(stats.entries, 0);
        let policy = cache.policy.stats();
        assert_eq!(policy.requests, 0);
        assert_eq!(policy.reuse_opportunities, 0);
        assert_eq!(policy.fills, 0);
    }

    #[tokio::test]
    async fn entry_limit_evicts_or_rejects_overflow() {
        let cache = RelationCache::new(RelationCacheLimits {
            entry_limit: 2,
            ..config()
        });
        for seed in 1..=8 {
            cache
                .get_or_fill(key(seed, 1), &output(0), || async {
                    Ok((frames(0, 1), CachedWork::default()))
                })
                .await
                .unwrap();
        }

        let stats = cache.stats();
        assert!(stats.entries <= 2);
        assert!(stats.evictions + stats.rejected_by_policy > 0);
        assert!(stats.retained_bytes <= config().byte_limit as u64);
    }

    #[tokio::test]
    async fn byte_limit_evicts_or_rejects_overflow() {
        let byte_limit = 4 * 1024;
        let cache = RelationCache::new(RelationCacheLimits {
            byte_limit,
            entry_limit: 4_096,
            result_byte_limit: 1024,
        });
        for seed in 1..=64 {
            cache
                .get_or_fill(key(seed, 1), &output(0), || async move {
                    Ok((text_frames(0, "x".repeat(200)), CachedWork::default()))
                })
                .await
                .unwrap();
        }

        let stats = cache.stats();
        assert!(stats.entries < 64);
        assert!(stats.evictions + stats.rejected_by_policy > 0);
        assert!(
            stats.retained_bytes <= byte_limit as u64,
            "relation cache exceeds its byte limit: {stats:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_misses_share_one_fill() {
        let cache = Arc::new(RelationCache::new(config()));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let fills = Arc::new(AtomicUsize::new(0));

        let first = {
            let cache = cache.clone();
            let started = started.clone();
            let release = release.clone();
            let fills = fills.clone();
            tokio::spawn(async move {
                cache
                    .get_or_fill(key(1, 1), &output(0), || async move {
                        fills.fetch_add(1, Ordering::Relaxed);
                        started.notify_one();
                        release.notified().await;
                        Ok((frames(0, 9), CachedWork::default()))
                    })
                    .await
            })
        };
        started.notified().await;
        let second = {
            let cache = cache.clone();
            let fills = fills.clone();
            tokio::spawn(async move {
                cache
                    .get_or_fill(key(1, 1), &output(0), || async move {
                        fills.fetch_add(1, Ordering::Relaxed);
                        Ok((frames(0, 10), CachedWork::default()))
                    })
                    .await
            })
        };
        tokio::task::yield_now().await;
        release.notify_waiters();

        assert_eq!(first.await.unwrap().unwrap().len(), 1);
        assert_eq!(second.await.unwrap().unwrap().len(), 1);
        assert_eq!(fills.load(Ordering::Relaxed), 1);
        assert_eq!(cache.stats().coalesced, 1);
        let policy = cache.policy.stats();
        assert_eq!(policy.requests, 2);
        assert_eq!(policy.reuse_opportunities, 1);
        assert_eq!(policy.coalesced_reuses, 1);
        assert_eq!(policy.fills, 1);
    }

    #[tokio::test]
    async fn concurrent_oversized_results_share_one_transient_copy() {
        let cache = Arc::new(RelationCache::new(RelationCacheLimits {
            result_byte_limit: 1,
            ..config()
        }));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let fills = Arc::new(AtomicUsize::new(0));

        let first = {
            let cache = cache.clone();
            let started = started.clone();
            let release = release.clone();
            let fills = fills.clone();
            tokio::spawn(async move {
                cache
                    .get_or_fill(key(1, 1), &output(0), || async move {
                        fills.fetch_add(1, Ordering::Relaxed);
                        started.notify_one();
                        release.notified().await;
                        Ok((frames(0, 9), CachedWork::default()))
                    })
                    .await
            })
        };
        started.notified().await;
        let second = {
            let cache = cache.clone();
            let fills = fills.clone();
            tokio::spawn(async move {
                cache
                    .get_or_fill(key(1, 1), &output(0), || async move {
                        fills.fetch_add(1, Ordering::Relaxed);
                        Ok((frames(0, 10), CachedWork::default()))
                    })
                    .await
            })
        };
        tokio::task::yield_now().await;
        release.notify_waiters();

        let first = first.await.unwrap().unwrap();
        let second = second.await.unwrap().unwrap();
        assert_eq!(
            first.into_frames(&output(0)),
            second.into_frames(&output(0))
        );
        assert_eq!(fills.load(Ordering::Relaxed), 1);
        assert_eq!(cache.stats().entries, 0);
        assert_eq!(cache.stats().rejected_too_large, 1);
    }

    #[tokio::test]
    async fn cancelled_fill_releases_the_key() {
        let cache = Arc::new(RelationCache::new(config()));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let first = {
            let cache = cache.clone();
            let started = started.clone();
            let release = release.clone();
            tokio::spawn(async move {
                cache
                    .get_or_fill(key(1, 1), &output(0), || async move {
                        started.notify_one();
                        release.notified().await;
                        Ok((frames(0, 1), CachedWork::default()))
                    })
                    .await
            })
        };
        started.notified().await;
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());

        let result = cache
            .get_or_fill(key(1, 1), &output(0), || async {
                Ok((frames(0, 2), CachedWork::default()))
            })
            .await
            .unwrap();
        let result = result.into_frames(&output(0));
        assert_eq!(
            result[0].get(SlotId(0)),
            Some(&Datum::scalar(Value::Int64(2)))
        );
        assert_eq!(cache.stats().admissions, 1);
    }
}

struct FlightOwner<'a> {
    cache: &'a RelationCache,
    key: RelationCacheKey,
    flight: Arc<Flight>,
    finished: bool,
}

impl<'a> FlightOwner<'a> {
    fn new(cache: &'a RelationCache, key: RelationCacheKey, flight: Arc<Flight>) -> Self {
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

    fn detach(&mut self) -> usize {
        self.remove();
        self.finished = true;
        self.flight.waiters.load(Ordering::Acquire)
    }

    fn publish(&self, result: FlightResult) {
        debug_assert!(self.finished);
        self.flight.result.send_replace(Some(result));
    }

    fn remove(&self) {
        let mut flights = self
            .cache
            .flights
            .lock()
            .expect("relation cache flight lock poisoned");
        if flights
            .get(&self.key)
            .is_some_and(|flight| Arc::ptr_eq(flight, &self.flight))
        {
            flights.remove(&self.key);
        }
    }
}

impl Drop for FlightOwner<'_> {
    fn drop(&mut self) {
        if !self.finished {
            // Cancellation includes task abort and panic unwind. Wake all
            // waiters so one of them can become the next fill owner.
            self.flight
                .result
                .send_replace(Some(FlightResult::Cancelled));
            self.remove();
        }
    }
}

struct SnapshotFlightOwner<'a> {
    cache: &'a SnapshotDependencyCache,
    key: SnapshotDependencyKey,
    flight: Arc<SnapshotFlight>,
    finished: bool,
}

impl<'a> SnapshotFlightOwner<'a> {
    fn new(
        cache: &'a SnapshotDependencyCache,
        key: SnapshotDependencyKey,
        flight: Arc<SnapshotFlight>,
    ) -> Self {
        Self {
            cache,
            key,
            flight,
            finished: false,
        }
    }

    fn finish(&mut self, result: SnapshotFlightResult) {
        self.flight.result.send_replace(Some(result));
        self.remove();
        self.finished = true;
    }

    fn remove(&self) {
        let mut state = self
            .cache
            .state
            .lock()
            .expect("snapshot dependency cache lock poisoned");
        if state
            .flights
            .get(&self.key)
            .is_some_and(|flight| Arc::ptr_eq(flight, &self.flight))
        {
            state.flights.remove(&self.key);
        }
    }
}

impl Drop for SnapshotFlightOwner<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.flight
                .result
                .send_replace(Some(SnapshotFlightResult::Cancelled));
            self.remove();
        }
    }
}

#[derive(Debug)]
pub(super) struct RelationCacheResult {
    rows: RelationRows,
    pub source: StatementSource,
}

// Keep a cache hit in slot-independent field order until the caller knows how
// it consumes the result. Final result shaping needs field order and names,
// but a later statement needs request-local execution slots. Restoring an Env
// before final shaping copies every datum twice.
#[derive(Debug)]
enum RelationRows {
    Frames(Vec<Env>),
    Cached(Arc<CachedRelation>),
}

impl RelationCacheResult {
    pub fn executed(frames: Vec<Env>) -> Self {
        Self {
            rows: RelationRows::Frames(frames),
            source: StatementSource::Executed,
        }
    }

    pub fn len(&self) -> usize {
        match &self.rows {
            RelationRows::Frames(frames) => frames.len(),
            RelationRows::Cached(relation) => relation.rows.len(),
        }
    }

    pub fn shape(&self, cardinality: RootCardinality, output: &RowType) -> Result<Datum> {
        match &self.rows {
            RelationRows::Frames(frames) => super::shape_frames(cardinality, output, frames),
            RelationRows::Cached(relation) => relation.shape(cardinality, output),
        }
    }

    pub fn into_frames(self, output: &RowType) -> Vec<Env> {
        match self.rows {
            RelationRows::Frames(frames) => frames,
            RelationRows::Cached(relation) => relation.restore(output),
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct RelationCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub admissions: u64,
    pub rejected_too_large: u64,
    pub rejected_by_policy: u64,
    pub evictions: u64,
    pub coalesced: u64,
    pub entries: u64,
    pub retained_bytes: u64,
    pub dependency_hits: u64,
    pub dependency_misses: u64,
    pub dependency_coalesced: u64,
    pub dependency_evictions: u64,
    pub catalog_hits: u64,
    pub catalog_misses: u64,
    pub catalog_coalesced: u64,
    pub catalog_evictions: u64,
    pub catalog_entries: u64,
    pub catalog_retained_bytes: u64,
    pub prepared_hits: u64,
    pub prepared_misses: u64,
    pub prepared_coalesced: u64,
    pub prepared_admissions: u64,
    pub prepared_evictions: u64,
    pub prepared_rejected_too_large: u64,
    pub prepared_superseded: u64,
    pub prepared_entries: u64,
    pub prepared_retained_bytes: u64,
}

#[cfg(test)]
impl RelationCache {
    pub fn stats(&self) -> RelationCacheStats {
        let catalog = self.snapshot_catalog.stats();
        let prepared = self.prepared_reads.stats();
        RelationCacheStats {
            hits: self.metrics.hits.load(Ordering::Relaxed),
            misses: self.metrics.misses.load(Ordering::Relaxed),
            admissions: self.metrics.admissions.load(Ordering::Relaxed),
            rejected_too_large: self.metrics.rejected_too_large.load(Ordering::Relaxed),
            rejected_by_policy: self.metrics.rejected_by_policy.load(Ordering::Relaxed),
            evictions: self.metrics.evictions.load(Ordering::Relaxed),
            coalesced: self.metrics.coalesced.load(Ordering::Relaxed),
            entries: self.entries.entries() as u64,
            retained_bytes: self.retained_bytes(),
            dependency_hits: self.metrics.dependency_hits.load(Ordering::Relaxed),
            dependency_misses: self.metrics.dependency_misses.load(Ordering::Relaxed),
            dependency_coalesced: self.metrics.dependency_coalesced.load(Ordering::Relaxed),
            dependency_evictions: self.metrics.dependency_evictions.load(Ordering::Relaxed),
            catalog_hits: catalog.hits,
            catalog_misses: catalog.misses,
            catalog_coalesced: catalog.coalesced,
            catalog_evictions: catalog.evictions,
            catalog_entries: catalog.entries,
            catalog_retained_bytes: catalog.retained_bytes,
            prepared_hits: prepared.hits,
            prepared_misses: prepared.misses,
            prepared_coalesced: prepared.coalesced,
            prepared_admissions: prepared.admissions,
            prepared_evictions: prepared.evictions,
            prepared_rejected_too_large: prepared.rejected_too_large,
            prepared_superseded: prepared.superseded,
            prepared_entries: prepared.entries,
            prepared_retained_bytes: prepared.retained_bytes,
        }
    }
}
