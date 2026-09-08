use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use ::slatedb as slate_db;
use async_trait::async_trait;
use bytes::Bytes;
use slate_db::config::{ObjectStoreCacheOptions, PreloadLevel, Settings, SstBlockSize};
use slate_db::db_cache::foyer::{FoyerCache, FoyerCacheOptions};
use slate_db::db_cache::{DbCache, SplitCache};
use slate_db::filter_policy::{BloomFilterPolicy, FilterPolicy};
use slate_db::object_store::{ObjectStore, memory::InMemory};
use tokio::sync::{Mutex as AsyncMutex, Notify, OnceCell};

use super::{
    Closure, DataPosition, Entry, Error, ErrorKind, IsolationLevel, KeyRange, Kv, KvIterator,
    Result, ScanOrder, ScanProfile, ScanRequest, Transaction, TransactionalKv,
};

mod telemetry;
mod metric_export;
mod object_store_telemetry;

pub(crate) use object_store_telemetry::observe_remote_object_store;
use telemetry::SlateTelemetry;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectCachePreload {
    None,
    L0,
    All,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Options {
    pub decoded_cache_size_mib: u64,
    pub scan_cache_blocks: bool,
    pub scan_read_ahead_kib: u64,
    pub scan_max_fetch_tasks: usize,
    pub flush_interval: Duration,
    pub l0_sst_size_mib: u64,
    pub max_wal_flushes_before_l0_flush: u64,
    pub l0_max_ssts: usize,
    pub l0_max_ssts_per_key: usize,
    pub l0_flush_parallelism: usize,
    pub max_unflushed_mib: u64,
    pub min_filter_keys: u32,
    pub bloom_bits_per_key: u32,
    pub sst_block_size_kib: u32,
    pub object_cache_path: Option<std::path::PathBuf>,
    pub object_cache_size_mib: u64,
    pub object_cache_part_size_kib: u64,
    pub object_cache_cache_on_flush: bool,
    pub object_cache_cache_on_compaction: bool,
    pub object_cache_preload: ObjectCachePreload,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            decoded_cache_size_mib: 128,
            scan_cache_blocks: false,
            scan_read_ahead_kib: 256,
            scan_max_fetch_tasks: 4,
            flush_interval: Duration::from_millis(100),
            l0_sst_size_mib: 64,
            max_wal_flushes_before_l0_flush: 4096,
            l0_max_ssts: 8,
            l0_max_ssts_per_key: 8,
            l0_flush_parallelism: 4,
            max_unflushed_mib: 1024,
            min_filter_keys: 1000,
            bloom_bits_per_key: 10,
            sst_block_size_kib: 4,
            object_cache_path: None,
            object_cache_size_mib: 16 * 1024,
            object_cache_part_size_kib: 4 * 1024,
            object_cache_cache_on_flush: false,
            object_cache_cache_on_compaction: false,
            object_cache_preload: ObjectCachePreload::None,
        }
    }
}

#[derive(Clone, Copy)]
struct ScanTuning {
    cache_blocks: bool,
    read_ahead_bytes: usize,
    max_fetch_tasks: usize,
}

impl Options {
    fn settings(&self) -> Result<Settings> {
        Ok(Settings {
            flush_interval: Some(self.flush_interval),
            l0_sst_size_bytes: mib_to_usize(self.l0_sst_size_mib)?,
            max_wal_flushes_before_l0_flush: self.max_wal_flushes_before_l0_flush,
            l0_max_ssts: self.l0_max_ssts,
            l0_max_ssts_per_key: self.l0_max_ssts_per_key,
            l0_flush_parallelism: self.l0_flush_parallelism,
            max_unflushed_bytes: mib_to_usize(self.max_unflushed_mib)?,
            min_filter_keys: self.min_filter_keys,
            object_store_cache_options: self.object_cache_options()?,
            ..Settings::default()
        })
    }

    fn reader_options(&self, poll_interval: Duration) -> Result<slate_db::config::DbReaderOptions> {
        Ok(slate_db::config::DbReaderOptions {
            manifest_poll_interval: poll_interval,
            checkpoint_lifetime: poll_interval
                .checked_mul(10)
                .unwrap_or(Duration::from_secs(60))
                .max(Duration::from_secs(1)),
            object_store_cache_options: self.object_cache_options()?,
            ..Default::default()
        })
    }

    fn object_cache_options(&self) -> Result<ObjectStoreCacheOptions> {
        Ok(ObjectStoreCacheOptions {
            root_folder: self.object_cache_path.clone(),
            max_cache_size_bytes: Some(mib_to_usize(self.object_cache_size_mib)?),
            part_size_bytes: kib_to_usize(self.object_cache_part_size_kib)?,
            cache_on_flush: self.object_cache_cache_on_flush,
            cache_on_compaction: self.object_cache_cache_on_compaction,
            preload_disk_cache_on_startup: match self.object_cache_preload {
                ObjectCachePreload::None => None,
                ObjectCachePreload::L0 => Some(PreloadLevel::L0Sst),
                ObjectCachePreload::All => Some(PreloadLevel::AllSst),
            },
            ..ObjectStoreCacheOptions::default()
        })
    }

    fn filter_policies(&self) -> Vec<Arc<dyn FilterPolicy>> {
        vec![Arc::new(BloomFilterPolicy::new(self.bloom_bits_per_key))]
    }

    fn sst_block_size(&self) -> Result<SstBlockSize> {
        match self.sst_block_size_kib {
            1 => Ok(SstBlockSize::Block1Kib),
            2 => Ok(SstBlockSize::Block2Kib),
            4 => Ok(SstBlockSize::Block4Kib),
            8 => Ok(SstBlockSize::Block8Kib),
            16 => Ok(SstBlockSize::Block16Kib),
            32 => Ok(SstBlockSize::Block32Kib),
            64 => Ok(SstBlockSize::Block64Kib),
            value => Err(Error::message(
                ErrorKind::Invalid,
                format!("unsupported Slate SST block size: {value} KiB"),
            )),
        }
    }

    fn scan_tuning(&self) -> Result<ScanTuning> {
        Ok(ScanTuning {
            cache_blocks: self.scan_cache_blocks,
            read_ahead_bytes: kib_to_usize(self.scan_read_ahead_kib)?,
            max_fetch_tasks: self.scan_max_fetch_tasks,
        })
    }
}

fn mib_to_usize(value: u64) -> Result<usize> {
    value
        .checked_mul(1024 * 1024)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| {
            Error::message(
                ErrorKind::Invalid,
                "Slate size exceeds this platform's address range",
            )
        })
}

fn kib_to_usize(value: u64) -> Result<usize> {
    value
        .checked_mul(1024)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| {
            Error::message(
                ErrorKind::Invalid,
                "Slate size exceeds this platform's address range",
            )
        })
}

pub struct Store {
    db: Arc<slate_db::Db>,
    cache: Arc<dyn DbCache>,
    lifecycle: Arc<Lifecycle>,
    telemetry: Arc<SlateTelemetry>,
    scan_tuning: ScanTuning,
}

/// A Slate checkpoint reader exposed through Rad's transactional read surface.
pub struct ReaderStore {
    reader: Arc<ReaderBackend>,
    lifecycle: Arc<Lifecycle>,
}

struct ReaderBackend {
    db: RwLock<Arc<slate_db::DbReader>>,
    path: slate_db::object_store::path::Path,
    object_store: Arc<dyn ObjectStore>,
    options: slate_db::config::DbReaderOptions,
    telemetry: Arc<SlateTelemetry>,
    cache: Arc<dyn DbCache>,
    slate_options: Options,
    scan_tuning: ScanTuning,
    reopen: AsyncMutex<()>,
}

impl ReaderStore {
    pub async fn open(
        path: impl Into<slate_db::object_store::path::Path> + Send,
        object_store: Arc<dyn ObjectStore>,
        poll_interval: Duration,
    ) -> Result<Self> {
        Self::open_with_options(path, object_store, poll_interval, Options::default()).await
    }

    pub async fn open_with_options(
        path: impl Into<slate_db::object_store::path::Path> + Send,
        object_store: Arc<dyn ObjectStore>,
        poll_interval: Duration,
        slate_options: Options,
    ) -> Result<Self> {
        let path = path.into();
        let options = slate_options.reader_options(poll_interval)?;
        let scan_tuning = slate_options.scan_tuning()?;
        let telemetry = SlateTelemetry::new();
        let cache = decoded_cache(slate_options.decoded_cache_size_mib);
        let db = slate_db::DbReader::builder(path.clone(), Arc::clone(&object_store))
            .with_options(options.clone())
            .with_filter_policies(slate_options.filter_policies())
            .with_metrics_recorder(telemetry.recorder())
            .with_db_cache(Arc::clone(&cache))
            .build()
            .await
            .map_err(map_operation_error)?;
        Ok(Self {
            reader: Arc::new(ReaderBackend {
                db: RwLock::new(Arc::new(db)),
                path,
                object_store,
                options,
                telemetry,
                cache,
                slate_options,
                scan_tuning,
                reopen: AsyncMutex::new(()),
            }),
            lifecycle: Arc::new(Lifecycle::default()),
        })
    }
}

impl ReaderBackend {
    fn current(&self) -> Arc<slate_db::DbReader> {
        Arc::clone(&self.db.read().expect("reader backend lock poisoned"))
    }

    async fn reopen_after(
        &self,
        failed: &Arc<slate_db::DbReader>,
    ) -> std::result::Result<Arc<slate_db::DbReader>, slate_db::Error> {
        let _guard = self.reopen.lock().await;
        let current = self.current();
        if !Arc::ptr_eq(&current, failed) {
            return Ok(current);
        }
        let replacement = Arc::new(
            slate_db::DbReader::builder(self.path.clone(), Arc::clone(&self.object_store))
                .with_options(self.options.clone())
                .with_filter_policies(self.slate_options.filter_policies())
                .with_metrics_recorder(self.telemetry.recorder())
                .with_db_cache(Arc::clone(&self.cache))
                .build()
                .await?,
        );
        *self.db.write().expect("reader backend lock poisoned") = Arc::clone(&replacement);
        let _ = current.close().await;
        Ok(replacement)
    }

    async fn get(&self, key: &[u8]) -> std::result::Result<Option<Bytes>, slate_db::Error> {
        let reader = self.current();
        match reader.get(key).await {
            Ok(value) => Ok(value),
            Err(error) if reader_must_reopen(&error) => {
                self.reopen_after(&reader).await?.get(key).await
            }
            Err(error) => Err(error),
        }
    }

    async fn scan(
        &self,
        request: ScanRequest,
    ) -> std::result::Result<slate_db::DbIterator, slate_db::Error> {
        let reader = self.current();
        let options = scan_options(&request, self.scan_tuning);
        match reader
            .scan_with_options(request.range.clone(), &options)
            .await
        {
            Ok(iterator) => Ok(iterator),
            Err(error) if reader_must_reopen(&error) => {
                self.reopen_after(&reader)
                    .await?
                    .scan_with_options(request.range, &options)
                    .await
            }
            Err(error) => Err(error),
        }
    }

    async fn close(&self) -> std::result::Result<(), slate_db::Error> {
        let _guard = self.reopen.lock().await;
        self.current().close().await?;
        self.cache.close().await
    }
}

fn scan_options(request: &ScanRequest, tuning: ScanTuning) -> slate_db::config::ScanOptions {
    let order = match request.order {
        ScanOrder::Ascending => slate_db::IterationOrder::Ascending,
        ScanOrder::Descending => slate_db::IterationOrder::Descending,
    };
    let options = slate_db::config::ScanOptions::default().with_order(order);
    match request.profile {
        ScanProfile::Latency => options,
        ScanProfile::Throughput => options
            .with_read_ahead_bytes(tuning.read_ahead_bytes)
            .with_max_fetch_tasks(tuning.max_fetch_tasks)
            .with_cache_blocks(tuning.cache_blocks),
    }
}

fn reader_must_reopen(error: &slate_db::Error) -> bool {
    matches!(error.kind(), slate_db::ErrorKind::Closed(_))
        || matches!(error.kind(), slate_db::ErrorKind::Data)
            && error.to_string().contains("checkpoint missing")
}

impl Store {
    pub async fn open(
        path: impl Into<slate_db::object_store::path::Path> + Send,
        object_store: Arc<dyn ObjectStore>,
    ) -> Result<Self> {
        Self::open_with_options(path, object_store, Options::default()).await
    }

    pub async fn open_with_options(
        path: impl Into<slate_db::object_store::path::Path> + Send,
        object_store: Arc<dyn ObjectStore>,
        options: Options,
    ) -> Result<Self> {
        let telemetry = SlateTelemetry::new();
        let cache = decoded_cache(options.decoded_cache_size_mib);
        let scan_tuning = options.scan_tuning()?;
        let db = slate_db::Db::builder(path, object_store)
            .with_settings(options.settings()?)
            .with_sst_block_size(options.sst_block_size()?)
            .with_filter_policies(options.filter_policies())
            .with_metrics_recorder(telemetry.recorder())
            .with_db_cache(Arc::clone(&cache))
            .build()
            .await
            .map_err(map_operation_error)?;
        Ok(Self {
            db: Arc::new(db),
            cache,
            lifecycle: Arc::new(Lifecycle::default()),
            telemetry,
            scan_tuning,
        })
    }

    pub async fn memory(path: &str) -> Result<Self> {
        Self::open(path, Arc::new(InMemory::new())).await
    }
}

fn decoded_cache(cache_size_mib: u64) -> Arc<dyn DbCache> {
    let (block_capacity, metadata_capacity) = decoded_cache_capacities(cache_size_mib);
    crate::telemetry::storage_cache_capacity("data_block", block_capacity);
    crate::telemetry::storage_cache_capacity("metadata", metadata_capacity);
    let cache = |max_capacity| {
        let options = FoyerCacheOptions {
            max_capacity,
            ..FoyerCacheOptions::default()
        };
        Arc::new(FoyerCache::new_with_opts(options)) as Arc<dyn DbCache>
    };
    Arc::new(
        SplitCache::new()
            .with_block_cache(Some(cache(block_capacity)))
            .with_meta_cache(Some(cache(metadata_capacity)))
            .build(),
    )
}

fn decoded_cache_capacities(cache_size_mib: u64) -> (u64, u64) {
    let capacity = cache_size_mib.saturating_mul(1024 * 1024);
    let metadata_capacity = capacity / 5;
    (
        capacity.saturating_sub(metadata_capacity),
        metadata_capacity,
    )
}

#[async_trait]
impl Kv for Store {
    async fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        let _lease = self.lifecycle.acquire()?;
        self.db.get(key).await.map_err(map_operation_error)
    }

    async fn put(&self, key: Bytes, value: Bytes) -> Result<()> {
        let _lease = self.lifecycle.acquire()?;
        self.db
            .put_bytes(key, value)
            .await
            .map(|_| ())
            .map_err(map_operation_error)
    }

    async fn delete(&self, key: &[u8]) -> Result<()> {
        let _lease = self.lifecycle.acquire()?;
        self.db
            .delete(key)
            .await
            .map(|_| ())
            .map_err(map_operation_error)
    }

    async fn scan(&self, range: KeyRange) -> Result<Box<dyn KvIterator>> {
        self.scan_ordered(range, ScanOrder::Ascending).await
    }

    async fn scan_ordered(&self, range: KeyRange, order: ScanOrder) -> Result<Box<dyn KvIterator>> {
        let lease = self.lifecycle.acquire()?;
        let iterator = self
            .db
            .scan_with_options(
                range.clone(),
                &scan_options(
                    &ScanRequest::access_path(range).with_order(order),
                    self.scan_tuning,
                ),
            )
            .await
            .map_err(map_operation_error)?;
        Ok(Box::new(SlateIterator {
            iterator,
            _lease: Some(lease),
        }))
    }

    async fn scan_requested(&self, request: ScanRequest) -> Result<Box<dyn KvIterator>> {
        let lease = self.lifecycle.acquire()?;
        let iterator = self
            .db
            .scan_with_options(
                request.range.clone(),
                &scan_options(&request, self.scan_tuning),
            )
            .await
            .map_err(map_operation_error)?;
        Ok(Box::new(SlateIterator {
            iterator,
            _lease: Some(lease),
        }))
    }
}

#[async_trait]
impl TransactionalKv for Store {
    async fn begin(&self, isolation: IsolationLevel) -> Result<Box<dyn Transaction>> {
        let lease = self.lifecycle.acquire()?;
        let transaction = self
            .db
            .begin(isolation.into())
            .await
            .map_err(map_operation_error)?;
        let begin_position = DataPosition::from_sequence(transaction.seqnum());
        Ok(Box::new(SlateTransaction {
            transaction,
            begin_position,
            scan_tuning: self.scan_tuning,
            _lease: lease,
        }))
    }

    async fn close(&self) -> Result<()> {
        self.lifecycle.start_closing();
        let result = self
            .lifecycle
            .close_result
            .get_or_init(|| async {
                self.lifecycle.wait_until_idle().await;
                match self.db.close().await {
                    Ok(()) => self.cache.close().await.map_err(map_operation_error),
                    Err(error) if matches!(error.kind(), slate_db::ErrorKind::Closed(_)) => {
                        self.cache.close().await.map_err(map_operation_error)
                    }
                    Err(error) => Err(map_operation_error(error)),
                }
            })
            .await
            .clone();
        self.lifecycle.finish_close();
        result
    }

    fn physical_telemetry(&self) -> Option<Arc<dyn super::telemetry::PhysicalTelemetry>> {
        Some(self.telemetry.clone())
    }
}

#[async_trait]
impl Kv for ReaderStore {
    async fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        let _lease = self.lifecycle.acquire()?;
        self.reader.get(key).await.map_err(map_operation_error)
    }

    async fn put(&self, _key: Bytes, _value: Bytes) -> Result<()> {
        Err(read_only_error())
    }

    async fn delete(&self, _key: &[u8]) -> Result<()> {
        Err(read_only_error())
    }

    async fn scan(&self, range: KeyRange) -> Result<Box<dyn KvIterator>> {
        self.scan_ordered(range, ScanOrder::Ascending).await
    }

    async fn scan_ordered(&self, range: KeyRange, order: ScanOrder) -> Result<Box<dyn KvIterator>> {
        let lease = self.lifecycle.acquire()?;
        let iterator = self
            .reader
            .scan(ScanRequest::access_path(range).with_order(order))
            .await
            .map_err(map_operation_error)?;
        Ok(Box::new(SlateIterator {
            iterator,
            _lease: Some(lease),
        }))
    }

    async fn scan_requested(&self, request: ScanRequest) -> Result<Box<dyn KvIterator>> {
        let lease = self.lifecycle.acquire()?;
        let iterator = self
            .reader
            .scan(request)
            .await
            .map_err(map_operation_error)?;
        Ok(Box::new(SlateIterator {
            iterator,
            _lease: Some(lease),
        }))
    }
}

#[async_trait]
impl TransactionalKv for ReaderStore {
    async fn begin(&self, _isolation: IsolationLevel) -> Result<Box<dyn Transaction>> {
        let lease = self.lifecycle.acquire()?;
        let snapshot = ReaderSnapshot::new(Arc::clone(&self.reader));
        let begin_position = DataPosition::from_sequence(snapshot.durable_sequence);
        Ok(Box::new(ReaderTransaction {
            snapshot,
            begin_position,
            _lease: lease,
        }))
    }

    async fn close(&self) -> Result<()> {
        self.lifecycle.start_closing();
        let result = self
            .lifecycle
            .close_result
            .get_or_init(|| async {
                self.lifecycle.wait_until_idle().await;
                match self.reader.close().await {
                    Ok(()) => Ok(()),
                    Err(error) => map_close_error(error),
                }
            })
            .await
            .clone();
        self.lifecycle.finish_close();
        result
    }

    fn physical_telemetry(&self) -> Option<Arc<dyn super::telemetry::PhysicalTelemetry>> {
        Some(self.reader.telemetry.clone())
    }
}

impl From<IsolationLevel> for slate_db::IsolationLevel {
    fn from(isolation: IsolationLevel) -> Self {
        match isolation {
            IsolationLevel::Snapshot => Self::Snapshot,
            IsolationLevel::SerializableSnapshot => Self::SerializableSnapshot,
        }
    }
}

impl slate_db::ByteRangeBounds for KeyRange {
    fn start_bound(&self) -> std::ops::Bound<&[u8]> {
        self.start
            .as_deref()
            .map_or(std::ops::Bound::Unbounded, std::ops::Bound::Included)
    }

    fn end_bound(&self) -> std::ops::Bound<&[u8]> {
        self.end
            .as_deref()
            .map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded)
    }
}

struct SlateTransaction {
    transaction: slate_db::DbTransaction,
    begin_position: DataPosition,
    scan_tuning: ScanTuning,
    _lease: Lease,
}

struct ReaderTransaction {
    snapshot: ReaderSnapshot,
    begin_position: DataPosition,
    _lease: Lease,
}

#[derive(Clone)]
struct ReaderSnapshot {
    backend: Arc<ReaderBackend>,
    reader: Arc<slate_db::DbReader>,
    durable_sequence: u64,
    scan_tuning: ScanTuning,
}

impl ReaderSnapshot {
    fn new(backend: Arc<ReaderBackend>) -> Self {
        let reader = backend.current();
        let durable_sequence = reader.status().durable_seq;
        let scan_tuning = backend.scan_tuning;
        Self {
            backend,
            reader,
            durable_sequence,
            scan_tuning,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.reader.status().durable_seq == self.durable_sequence {
            Ok(())
        } else {
            Err(Error::message(
                ErrorKind::Conflict,
                "reader snapshot changed during the transaction",
            ))
        }
    }

    async fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        self.validate()?;
        match self.reader.get(key).await {
            Ok(value) => {
                self.validate()?;
                Ok(value)
            }
            Err(error) => Err(self.operation_error(error).await),
        }
    }

    async fn scan(&self, request: &ScanRequest) -> Result<slate_db::DbIterator> {
        self.validate()?;
        match self
            .reader
            .scan_with_options(
                request.range.clone(),
                &scan_options(request, self.scan_tuning),
            )
            .await
        {
            Ok(iterator) => {
                self.validate()?;
                Ok(iterator)
            }
            Err(error) => Err(self.operation_error(error).await),
        }
    }

    async fn operation_error(&self, error: slate_db::Error) -> Error {
        if !reader_must_reopen(&error) {
            return map_operation_error(error);
        }
        match self.backend.reopen_after(&self.reader).await {
            Ok(_) => Error::source(
                ErrorKind::Conflict,
                "reader snapshot became unavailable during the transaction",
                error,
            ),
            Err(error) => map_operation_error(error),
        }
    }
}

#[async_trait]
impl Transaction for ReaderTransaction {
    fn begin_position(&self) -> &DataPosition {
        &self.begin_position
    }

    async fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        self.snapshot.get(key).await
    }

    fn put(&self, _key: Bytes, _value: Bytes) -> Result<()> {
        Err(read_only_error())
    }

    fn delete(&self, _key: &[u8]) -> Result<()> {
        Err(read_only_error())
    }

    fn untrack_write(&self, _key: &[u8]) -> Result<()> {
        Err(read_only_error())
    }

    async fn scan<'a>(&'a self, range: KeyRange) -> Result<Box<dyn KvIterator + 'a>> {
        self.scan_ordered(range, ScanOrder::Ascending).await
    }

    async fn scan_ordered<'a>(
        &'a self,
        range: KeyRange,
        order: ScanOrder,
    ) -> Result<Box<dyn KvIterator + 'a>> {
        let request = ScanRequest::access_path(range).with_order(order);
        let iterator = self.snapshot.scan(&request).await?;
        Ok(Box::new(ReaderSlateIterator {
            iterator,
            snapshot: self.snapshot.clone(),
        }))
    }

    async fn scan_requested<'a>(
        &'a self,
        request: ScanRequest,
    ) -> Result<Box<dyn KvIterator + 'a>> {
        let iterator = self.snapshot.scan(&request).await?;
        Ok(Box::new(ReaderSlateIterator {
            iterator,
            snapshot: self.snapshot.clone(),
        }))
    }

    async fn commit(self: Box<Self>) -> Result<()> {
        Ok(())
    }

    fn rollback(self: Box<Self>) {}
}

impl SlateTransaction {
    fn into_parts(self) -> (slate_db::DbTransaction, Lease) {
        let Self {
            transaction,
            begin_position: _,
            scan_tuning: _,
            _lease: lease,
        } = self;
        (transaction, lease)
    }
}

#[async_trait]
impl Transaction for SlateTransaction {
    fn begin_position(&self) -> &DataPosition {
        &self.begin_position
    }

    async fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        self.transaction.get(key).await.map_err(map_operation_error)
    }

    fn put(&self, key: Bytes, value: Bytes) -> Result<()> {
        self.transaction
            .put(key, value)
            .map_err(map_operation_error)
    }

    fn delete(&self, key: &[u8]) -> Result<()> {
        self.transaction.delete(key).map_err(map_operation_error)
    }

    fn untrack_write(&self, key: &[u8]) -> Result<()> {
        self.transaction
            .unmark_write([key])
            .map_err(map_operation_error)
    }

    async fn scan<'a>(&'a self, range: KeyRange) -> Result<Box<dyn KvIterator + 'a>> {
        self.scan_ordered(range, ScanOrder::Ascending).await
    }

    async fn scan_ordered<'a>(
        &'a self,
        range: KeyRange,
        order: ScanOrder,
    ) -> Result<Box<dyn KvIterator + 'a>> {
        let iterator = self
            .transaction
            .scan_with_options(
                range.clone(),
                &scan_options(
                    &ScanRequest::access_path(range).with_order(order),
                    self.scan_tuning,
                ),
            )
            .await
            .map_err(map_operation_error)?;
        Ok(Box::new(SlateIterator {
            iterator,
            _lease: None,
        }))
    }

    async fn scan_requested<'a>(
        &'a self,
        request: ScanRequest,
    ) -> Result<Box<dyn KvIterator + 'a>> {
        let iterator = self
            .transaction
            .scan_with_options(
                request.range.clone(),
                &scan_options(&request, self.scan_tuning),
            )
            .await
            .map_err(map_operation_error)?;
        Ok(Box::new(SlateIterator {
            iterator,
            _lease: None,
        }))
    }

    async fn commit(self: Box<Self>) -> Result<()> {
        let (transaction, lease) = (*self).into_parts();
        let result = transaction
            .commit()
            .await
            .map(|_| ())
            .map_err(map_commit_error);
        drop(lease);
        result
    }

    fn rollback(self: Box<Self>) {
        let (transaction, lease) = (*self).into_parts();
        transaction.rollback();
        drop(lease);
    }
}

struct SlateIterator {
    iterator: slate_db::DbIterator,
    _lease: Option<Lease>,
}

struct ReaderSlateIterator {
    iterator: slate_db::DbIterator,
    snapshot: ReaderSnapshot,
}

#[async_trait]
impl KvIterator for ReaderSlateIterator {
    async fn seek_forward(&mut self, next_key: &[u8]) -> Result<()> {
        match self.iterator.seek(next_key).await {
            Ok(()) => Ok(()),
            Err(error) => Err(self.snapshot.operation_error(error).await),
        }
    }

    async fn next(&mut self) -> Result<Option<Entry>> {
        match self.iterator.next().await {
            Ok(entry) => Ok(entry.map(slate_entry)),
            Err(error) => Err(self.snapshot.operation_error(error).await),
        }
    }

    async fn next_batch(&mut self, limit: usize, output: &mut Vec<Entry>) -> Result<()> {
        let target = output.len().saturating_add(limit);
        while output.len() < target {
            let Some(entry) = self.next().await? else {
                break;
            };
            output.push(entry);
        }
        Ok(())
    }
}

fn slate_entry(entry: slate_db::KeyValue) -> Entry {
    Entry {
        key: entry.key,
        value: entry.value,
    }
}

#[async_trait]
impl KvIterator for SlateIterator {
    async fn seek_forward(&mut self, next_key: &[u8]) -> Result<()> {
        self.iterator
            .seek(next_key)
            .await
            .map_err(map_operation_error)
    }

    async fn next(&mut self) -> Result<Option<Entry>> {
        let result = self.iterator.next().await;
        result
            .map(|entry| entry.map(slate_entry))
            .map_err(map_operation_error)
    }

    async fn next_batch(&mut self, limit: usize, output: &mut Vec<Entry>) -> Result<()> {
        let target = output.len().saturating_add(limit);
        while output.len() < target {
            let entry = self.iterator.next().await.map_err(map_operation_error)?;
            let Some(entry) = entry else {
                break;
            };
            output.push(Entry {
                key: entry.key,
                value: entry.value,
            });
        }
        Ok(())
    }
}

fn map_operation_error(error: slate_db::Error) -> Error {
    match error.kind() {
        slate_db::ErrorKind::Closed(reason) => {
            Error::closed(map_close_reason(reason), error.to_string(), error)
        }
        _ => Error::source(operation_error_kind(&error), error.to_string(), error),
    }
}

fn map_close_reason(reason: slate_db::CloseReason) -> Closure {
    match reason {
        slate_db::CloseReason::Fenced => Closure::Fenced,
        slate_db::CloseReason::Panic => Closure::Panicked,
        _ => Closure::Clean,
    }
}

fn map_close_error(error: slate_db::Error) -> Result<()> {
    if matches!(error.kind(), slate_db::ErrorKind::Closed(_)) {
        Ok(())
    } else {
        Err(map_operation_error(error))
    }
}

fn read_only_error() -> Error {
    Error::message(ErrorKind::ReadOnly, "database is read-only")
}

fn operation_error_kind(error: &slate_db::Error) -> ErrorKind {
    match error.kind() {
        slate_db::ErrorKind::Transaction => ErrorKind::Conflict,
        slate_db::ErrorKind::Closed(_) => ErrorKind::Closed,
        slate_db::ErrorKind::Unavailable => ErrorKind::Unavailable,
        slate_db::ErrorKind::Invalid => ErrorKind::Invalid,
        slate_db::ErrorKind::Data => ErrorKind::Data,
        _ => ErrorKind::Internal,
    }
}

fn map_commit_error(error: slate_db::Error) -> Error {
    if operation_error_kind(&error) == ErrorKind::Conflict {
        Error::source(ErrorKind::Conflict, error.to_string(), error)
    } else {
        Error::source(
            ErrorKind::CommitOutcomeUnknown,
            format!("transaction commit outcome is unknown: {error}"),
            error,
        )
    }
}

#[derive(Default)]
struct Lifecycle {
    state: Mutex<LifecycleState>,
    changed: Notify,
    close_result: OnceCell<Result<()>>,
}

#[derive(Default)]
struct LifecycleState {
    active: usize,
    status: Status,
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum Status {
    #[default]
    Open,
    Closing,
    Closed,
}

impl Lifecycle {
    fn acquire(self: &Arc<Self>) -> Result<Lease> {
        let mut state = self.state.lock().expect("lifecycle mutex poisoned");
        if state.status != Status::Open {
            return Err(Error::message(ErrorKind::Closed, "KV store is closing"));
        }
        state.active += 1;
        drop(state);
        Ok(Lease {
            lifecycle: Arc::clone(self),
        })
    }

    fn start_closing(&self) {
        let mut state = self.state.lock().expect("lifecycle mutex poisoned");
        if state.status == Status::Open {
            state.status = Status::Closing;
        }
    }

    async fn wait_until_idle(&self) {
        loop {
            let changed = self.changed.notified();
            if self.state.lock().expect("lifecycle mutex poisoned").active == 0 {
                return;
            }
            changed.await;
        }
    }

    fn finish_close(&self) {
        let mut state = self.state.lock().expect("lifecycle mutex poisoned");
        state.status = Status::Closed;
        drop(state);
        self.changed.notify_waiters();
    }
}

struct Lease {
    lifecycle: Arc<Lifecycle>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self
            .lifecycle
            .state
            .lock()
            .expect("lifecycle mutex poisoned");
        state.active -= 1;
        drop(state);
        self.lifecycle.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use std::fmt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures::stream::BoxStream;
    use slate_db::object_store::path::Path;
    use slate_db::object_store::{
        CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta,
        PutMultipartOptions, PutOptions, PutPayload, PutResult, Result as ObjectStoreResult,
    };
    use tokio::sync::oneshot;

    use super::*;

    #[test]
    fn decoded_cache_uses_the_configured_total_capacity() {
        let (blocks, metadata) = decoded_cache_capacities(128);
        assert_eq!(blocks + metadata, 128 * 1024 * 1024);
        assert_eq!(metadata, 128 * 1024 * 1024 / 5);
    }

    #[derive(Debug, Default)]
    struct FaultingReadStore {
        inner: InMemory,
        failures_remaining: AtomicUsize,
        read_attempts: AtomicUsize,
    }

    impl fmt::Display for FaultingReadStore {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("faulting-read-store")
        }
    }

    #[async_trait]
    impl ObjectStore for FaultingReadStore {
        async fn put_opts(
            &self,
            location: &Path,
            payload: PutPayload,
            options: PutOptions,
        ) -> ObjectStoreResult<PutResult> {
            self.inner.put_opts(location, payload, options).await
        }

        async fn put_multipart_opts(
            &self,
            location: &Path,
            options: PutMultipartOptions,
        ) -> ObjectStoreResult<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(location, options).await
        }

        async fn get_opts(
            &self,
            location: &Path,
            options: GetOptions,
        ) -> ObjectStoreResult<GetResult> {
            self.read_attempts.fetch_add(1, Ordering::Relaxed);
            if self
                .failures_remaining
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(slate_db::object_store::Error::NotFound {
                    path: location.to_string(),
                    source: Box::new(std::io::Error::other("injected missing object")),
                });
            }
            self.inner.get_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, ObjectStoreResult<Path>>,
        ) -> BoxStream<'static, ObjectStoreResult<Path>> {
            self.inner.delete_stream(locations)
        }

        fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, ObjectStoreResult<ObjectMeta>> {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&Path>,
        ) -> ObjectStoreResult<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &Path,
            to: &Path,
            options: CopyOptions,
        ) -> ObjectStoreResult<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    async fn collect(mut iterator: Box<dyn KvIterator + '_>) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();
        while let Some(entry) = iterator.next().await? {
            entries.push(entry);
        }
        Ok(entries)
    }

    #[tokio::test]
    async fn commit_is_visible_and_rollback_is_not() -> Result<()> {
        let store = Store::memory("commit-visibility").await?;
        let committed = store.begin(IsolationLevel::Snapshot).await?;
        committed.put(Bytes::from_static(b"key"), Bytes::from_static(b"committed"))?;
        committed.commit().await?;

        let rolled_back = store.begin(IsolationLevel::Snapshot).await?;
        rolled_back.put(
            Bytes::from_static(b"key"),
            Bytes::from_static(b"rolled-back"),
        )?;
        rolled_back.rollback();

        assert_eq!(
            store.get(b"key").await?,
            Some(Bytes::from_static(b"committed"))
        );
        store.close().await
    }

    #[tokio::test]
    async fn transaction_has_a_stable_snapshot_and_own_writes() -> Result<()> {
        let store = Store::memory("stable-snapshot").await?;
        store
            .put(Bytes::from_static(b"a"), Bytes::from_static(b"old"))
            .await?;
        let transaction = store.begin(IsolationLevel::Snapshot).await?;
        store
            .put(Bytes::from_static(b"a"), Bytes::from_static(b"new"))
            .await?;

        assert_eq!(
            transaction.get(b"a").await?,
            Some(Bytes::from_static(b"old"))
        );
        transaction.put(Bytes::from_static(b"b"), Bytes::from_static(b"own"))?;
        transaction.delete(b"a")?;
        assert_eq!(
            transaction.get(b"b").await?,
            Some(Bytes::from_static(b"own"))
        );
        assert_eq!(transaction.get(b"a").await?, None);
        assert_eq!(
            collect(transaction.scan(KeyRange::all()).await?).await?,
            vec![Entry {
                key: Bytes::from_static(b"b"),
                value: Bytes::from_static(b"own"),
            }]
        );
        transaction.rollback();
        store.close().await
    }

    #[tokio::test]
    async fn scans_are_ordered_and_half_open() -> Result<()> {
        let store = Store::memory("scan-order").await?;
        for key in [b"a", b"b", b"c", b"d"] {
            store
                .put(Bytes::copy_from_slice(key), Bytes::copy_from_slice(key))
                .await?;
        }
        let entries = collect(
            store
                .scan(KeyRange::new(
                    Bytes::from_static(b"b"),
                    Bytes::from_static(b"d"),
                ))
                .await?,
        )
        .await?;
        assert_eq!(
            entries
                .into_iter()
                .map(|entry| entry.key)
                .collect::<Vec<_>>(),
            vec![Bytes::from_static(b"b"), Bytes::from_static(b"c")]
        );
        store.close().await
    }

    #[test]
    fn scan_profiles_select_bounded_storage_work() -> Result<()> {
        let tuning = Options::default().scan_tuning()?;
        let latency = scan_options(&ScanRequest::access_path(KeyRange::all()), tuning);
        assert_eq!(latency.read_ahead_bytes, 1);
        assert_eq!(latency.max_fetch_tasks, 1);
        assert!(!latency.cache_blocks);

        let throughput = scan_options(
            &ScanRequest::access_path(KeyRange::all()).with_profile(ScanProfile::Throughput),
            tuning,
        );
        assert_eq!(throughput.read_ahead_bytes, 256 * 1024);
        assert_eq!(throughput.max_fetch_tasks, 4);
        assert!(!throughput.cache_blocks);
        Ok(())
    }

    #[tokio::test]
    async fn descending_scans_are_ordered_and_half_open() -> Result<()> {
        let store = Store::memory("descending-scan-order").await?;
        for key in [b"a", b"b", b"c", b"d"] {
            store
                .put(Bytes::copy_from_slice(key), Bytes::copy_from_slice(key))
                .await?;
        }
        let entries = collect(
            store
                .scan_ordered(
                    KeyRange::new(Bytes::from_static(b"b"), Bytes::from_static(b"d")),
                    ScanOrder::Descending,
                )
                .await?,
        )
        .await?;
        assert_eq!(
            entries
                .into_iter()
                .map(|entry| entry.key)
                .collect::<Vec<_>>(),
            vec![Bytes::from_static(b"c"), Bytes::from_static(b"b")]
        );
        store.close().await
    }

    #[tokio::test]
    async fn descending_transaction_scan_includes_buffered_writes() -> Result<()> {
        let store = Store::memory("descending-transaction-scan").await?;
        for key in [b"a", b"b", b"c", b"d"] {
            store
                .put(Bytes::copy_from_slice(key), Bytes::copy_from_slice(key))
                .await?;
        }
        let transaction = store.begin(IsolationLevel::Snapshot).await?;
        transaction.delete(b"c")?;
        transaction.put(Bytes::from_static(b"bb"), Bytes::from_static(b"bb"))?;
        let entries = collect(
            transaction
                .scan_ordered(
                    KeyRange::new(Bytes::from_static(b"b"), Bytes::from_static(b"d")),
                    ScanOrder::Descending,
                )
                .await?,
        )
        .await?;
        assert_eq!(
            entries
                .into_iter()
                .map(|entry| entry.key)
                .collect::<Vec<_>>(),
            vec![Bytes::from_static(b"bb"), Bytes::from_static(b"b")]
        );
        transaction.rollback();
        store.close().await
    }

    #[tokio::test]
    async fn scan_seek_moves_only_forward_inside_the_original_range() -> Result<()> {
        let store = Store::memory("scan-forward-seek").await?;
        for key in [b"a", b"b", b"c", b"d"] {
            store
                .put(Bytes::copy_from_slice(key), Bytes::copy_from_slice(key))
                .await?;
        }
        let mut iterator = store
            .scan(KeyRange::new(
                Bytes::from_static(b"a"),
                Bytes::from_static(b"e"),
            ))
            .await?;
        iterator.seek_forward(b"c").await?;
        assert_eq!(
            iterator.next().await?.map(|entry| entry.key),
            Some(Bytes::from_static(b"c"))
        );
        iterator.seek_forward(b"d").await?;
        assert_eq!(
            iterator.next().await?.map(|entry| entry.key),
            Some(Bytes::from_static(b"d"))
        );
        assert_eq!(
            iterator.seek_forward(b"c").await.unwrap_err().kind(),
            ErrorKind::Invalid
        );
        drop(iterator);
        store.close().await
    }

    #[tokio::test]
    async fn transaction_scan_descriptor_keeps_the_range_and_position() -> Result<()> {
        let store = Store::memory("scan-descriptor").await?;
        store
            .put(Bytes::from_static(b"b"), Bytes::from_static(b"value"))
            .await?;
        let transaction = store.begin(IsolationLevel::Snapshot).await?;
        let position = transaction.begin_position().clone();
        let request = crate::engine::kv::ScanRequest::cascade_range(KeyRange::new(
            Bytes::from_static(b"b"),
            Bytes::from_static(b"c"),
        ));
        {
            let scan = transaction.scan_with_request(request.clone()).await?;
            assert_eq!(scan.descriptor.request, request);
            assert_eq!(scan.descriptor.position, Some(position));
            assert_eq!(collect(scan.iterator).await?.len(), 1);
        }
        transaction.rollback();
        store.close().await
    }

    #[tokio::test]
    async fn transaction_cursor_allows_interleaved_point_reads() -> Result<()> {
        let store = Store::memory("interleaved-cursor-reads").await?;
        for (key, value) in [(b"i/a", b"row/a"), (b"i/b", b"row/b")] {
            store
                .put(Bytes::copy_from_slice(key), Bytes::copy_from_slice(value))
                .await?;
        }
        for key in [b"row/a", b"row/b"] {
            store
                .put(Bytes::copy_from_slice(key), Bytes::copy_from_slice(key))
                .await?;
        }

        let transaction = store.begin(IsolationLevel::Snapshot).await?;
        let mut cursor = transaction
            .scan(KeyRange::new(
                Bytes::from_static(b"i/"),
                Bytes::from_static(b"i0"),
            ))
            .await?;

        let first = cursor.next().await?.expect("first index entry");
        assert_eq!(
            transaction.get(&first.value).await?,
            Some(Bytes::from_static(b"row/a"))
        );
        let second = cursor.next().await?.expect("second index entry");
        assert_eq!(
            transaction.get(&second.value).await?,
            Some(Bytes::from_static(b"row/b"))
        );
        assert_eq!(cursor.next().await?, None);

        drop(cursor);
        transaction.rollback();
        store.close().await
    }

    #[tokio::test]
    async fn snapshot_detects_write_write_conflicts_but_allows_write_skew() -> Result<()> {
        let store = Store::memory("snapshot-conflicts").await?;
        store
            .put(Bytes::from_static(b"a"), Bytes::from_static(b"1"))
            .await?;
        store
            .put(Bytes::from_static(b"b"), Bytes::from_static(b"1"))
            .await?;

        let first = store.begin(IsolationLevel::Snapshot).await?;
        let second = store.begin(IsolationLevel::Snapshot).await?;
        first.put(Bytes::from_static(b"same"), Bytes::from_static(b"first"))?;
        second.put(Bytes::from_static(b"same"), Bytes::from_static(b"second"))?;
        first.commit().await?;
        assert_eq!(
            second.commit().await.unwrap_err().kind(),
            ErrorKind::Conflict
        );

        let first = store.begin(IsolationLevel::Snapshot).await?;
        let second = store.begin(IsolationLevel::Snapshot).await?;
        assert_eq!(first.get(b"b").await?, Some(Bytes::from_static(b"1")));
        assert_eq!(second.get(b"a").await?, Some(Bytes::from_static(b"1")));
        first.put(Bytes::from_static(b"a"), Bytes::from_static(b"0"))?;
        second.put(Bytes::from_static(b"b"), Bytes::from_static(b"0"))?;
        first.commit().await?;
        second.commit().await?;
        store.close().await
    }

    #[tokio::test]
    async fn serializable_snapshot_detects_point_and_empty_range_phantoms() -> Result<()> {
        let store = Store::memory("serializable-conflicts").await?;
        store
            .put(Bytes::from_static(b"watched"), Bytes::from_static(b"old"))
            .await?;

        let reader = store.begin(IsolationLevel::SerializableSnapshot).await?;
        assert_eq!(
            reader.get(b"watched").await?,
            Some(Bytes::from_static(b"old"))
        );
        let writer = store.begin(IsolationLevel::SerializableSnapshot).await?;
        writer.put(Bytes::from_static(b"watched"), Bytes::from_static(b"new"))?;
        writer.commit().await?;
        reader.put(Bytes::from_static(b"other"), Bytes::from_static(b"value"))?;
        assert_eq!(
            reader.commit().await.unwrap_err().kind(),
            ErrorKind::Conflict
        );

        let reader = store.begin(IsolationLevel::SerializableSnapshot).await?;
        assert!(
            collect(
                reader
                    .scan(KeyRange::new(
                        Bytes::from_static(b"m"),
                        Bytes::from_static(b"n"),
                    ))
                    .await?
            )
            .await?
            .is_empty()
        );
        let writer = store.begin(IsolationLevel::SerializableSnapshot).await?;
        writer.put(
            Bytes::from_static(b"middle"),
            Bytes::from_static(b"phantom"),
        )?;
        writer.commit().await?;
        assert_eq!(
            reader.commit().await.unwrap_err().kind(),
            ErrorKind::Conflict
        );
        store.close().await
    }

    #[tokio::test]
    async fn close_waits_for_transactions() -> Result<()> {
        let store = Store::memory("orderly-close").await?;
        let transaction = store.begin(IsolationLevel::Snapshot).await?;
        let (closed_tx, mut closed_rx) = oneshot::channel();
        let closing = tokio::spawn(async move {
            let result = store.close().await;
            closed_tx.send(result).ok();
        });

        tokio::task::yield_now().await;
        assert!(closed_rx.try_recv().is_err());
        transaction.rollback();
        closed_rx.await.expect("close task dropped")?;
        closing.await.expect("close task panicked");
        Ok(())
    }

    #[tokio::test]
    async fn close_is_idempotent_and_rejects_new_work() -> Result<()> {
        let store = Store::memory("closed-store").await?;
        store.close().await?;
        store.close().await?;
        assert_eq!(
            store.get(b"key").await.unwrap_err().kind(),
            ErrorKind::Closed
        );
        assert_eq!(
            match store.begin(IsolationLevel::Snapshot).await {
                Ok(_) => panic!("closed store accepted a transaction"),
                Err(error) => error.kind(),
            },
            ErrorKind::Closed
        );
        Ok(())
    }

    #[tokio::test]
    async fn reader_rejects_storage_mutations_and_closes_its_checkpoint_client() -> Result<()> {
        let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let writer = Store::open("reader-contract", Arc::clone(&objects)).await?;
        writer
            .put(Bytes::from_static(b"key"), Bytes::from_static(b"value"))
            .await?;
        let reader =
            ReaderStore::open("reader-contract", objects, Duration::from_millis(100)).await?;

        assert_eq!(
            reader
                .put(Bytes::from_static(b"other"), Bytes::from_static(b"value"))
                .await
                .unwrap_err()
                .kind(),
            ErrorKind::ReadOnly
        );
        assert_eq!(
            reader.delete(b"key").await.unwrap_err().kind(),
            ErrorKind::ReadOnly
        );

        let transaction = reader.begin(IsolationLevel::Snapshot).await?;
        assert_eq!(
            transaction
                .put(Bytes::from_static(b"other"), Bytes::from_static(b"value"))
                .unwrap_err()
                .kind(),
            ErrorKind::ReadOnly
        );
        assert_eq!(
            transaction.delete(b"key").unwrap_err().kind(),
            ErrorKind::ReadOnly
        );
        assert_eq!(
            transaction.untrack_write(b"key").unwrap_err().kind(),
            ErrorKind::ReadOnly
        );
        let position = transaction.begin_position().clone();
        {
            let scan = transaction
                .scan_with_request(crate::engine::kv::ScanRequest::access_path(KeyRange::all()))
                .await?;
            assert_eq!(scan.descriptor.position, Some(position));
        }
        transaction.rollback();

        let checkpoint_client = reader.reader.current();
        reader.close().await?;
        assert_eq!(
            reader.get(b"key").await.unwrap_err().kind(),
            ErrorKind::Closed
        );
        let error = checkpoint_client.get(b"key").await.unwrap_err();
        assert!(matches!(error.kind(), slate_db::ErrorKind::Closed(_)));
        writer.close().await
    }

    #[tokio::test]
    async fn reader_transaction_rejects_a_refresh_between_index_and_base_reads() -> Result<()> {
        let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let writer = Store::open("reader-stable-view", Arc::clone(&objects)).await?;
        let initial = writer.begin(IsolationLevel::SerializableSnapshot).await?;
        initial.put(
            Bytes::from_static(b"index/active/a"),
            Bytes::from_static(b"row/a"),
        )?;
        initial.put(Bytes::from_static(b"row/a"), Bytes::from_static(b"value-a"))?;
        initial.commit().await?;

        let reader =
            ReaderStore::open("reader-stable-view", objects, Duration::from_millis(10)).await?;
        wait_for_reader_value(&reader, b"row/a", Some(b"value-a")).await?;

        let transaction = reader.begin(IsolationLevel::Snapshot).await?;
        let mut index = transaction
            .scan(KeyRange::new(
                Bytes::from_static(b"index/active/"),
                Bytes::from_static(b"index/active0"),
            ))
            .await?;
        let entry = index.next().await?.expect("reader did not see index entry");
        assert_eq!(entry.value, Bytes::from_static(b"row/a"));
        drop(index);

        let replacement = writer.begin(IsolationLevel::SerializableSnapshot).await?;
        replacement.delete(b"index/active/a")?;
        replacement.delete(b"row/a")?;
        replacement.put(
            Bytes::from_static(b"index/active/b"),
            Bytes::from_static(b"row/b"),
        )?;
        replacement.put(Bytes::from_static(b"row/b"), Bytes::from_static(b"value-b"))?;
        replacement.commit().await?;
        wait_for_reader_value(&reader, b"row/a", None).await?;

        assert_eq!(
            transaction.get(&entry.value).await.unwrap_err().kind(),
            ErrorKind::Conflict
        );
        transaction.rollback();
        reader.close().await?;
        writer.close().await
    }

    #[tokio::test]
    async fn concurrent_reader_transactions_return_rows_or_snapshot_conflicts() -> Result<()> {
        let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let writer = Arc::new(Store::open("reader-concurrent-view", Arc::clone(&objects)).await?);
        let initial = writer.begin(IsolationLevel::SerializableSnapshot).await?;
        initial.put(
            Bytes::from_static(b"index/active/item"),
            Bytes::from_static(b"row/0000"),
        )?;
        initial.put(
            Bytes::from_static(b"row/0000"),
            Bytes::from_static(b"row/0000"),
        )?;
        initial.commit().await?;

        let reader = Arc::new(
            ReaderStore::open("reader-concurrent-view", objects, Duration::from_millis(1)).await?,
        );
        wait_for_reader_value(&reader, b"row/0000", Some(b"row/0000")).await?;

        let writer_task = {
            let writer = Arc::clone(&writer);
            tokio::spawn(async move {
                for version in 1..=128_u16 {
                    let old_row = Bytes::from(format!("row/{:04}", version - 1));
                    let new_row = Bytes::from(format!("row/{version:04}"));
                    let transaction = writer.begin(IsolationLevel::SerializableSnapshot).await?;
                    transaction.delete(b"index/active/item")?;
                    transaction.delete(&old_row)?;
                    transaction.put(Bytes::from_static(b"index/active/item"), new_row.clone())?;
                    transaction.put(new_row.clone(), new_row)?;
                    transaction.commit().await?;
                    tokio::task::yield_now().await;
                }
                Result::<()>::Ok(())
            })
        };
        let mut reader_tasks = Vec::new();
        for _ in 0..8 {
            let reader = Arc::clone(&reader);
            reader_tasks.push(tokio::spawn(async move {
                for _ in 0..64 {
                    let transaction = reader.begin(IsolationLevel::Snapshot).await?;
                    let mut index = match transaction
                        .scan(KeyRange::new(
                            Bytes::from_static(b"index/active/"),
                            Bytes::from_static(b"index/active0"),
                        ))
                        .await
                    {
                        Ok(index) => index,
                        Err(error) if error.kind() == ErrorKind::Conflict => continue,
                        Err(error) => return Err(error),
                    };
                    let entry = match index.next().await {
                        Ok(Some(entry)) => entry,
                        Ok(None) => panic!("active index entry is missing"),
                        Err(error) if error.kind() == ErrorKind::Conflict => continue,
                        Err(error) => return Err(error),
                    };
                    drop(index);
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    match transaction.get(&entry.value).await {
                        Ok(value) => assert_eq!(value, Some(entry.value)),
                        Err(error) if error.kind() == ErrorKind::Conflict => {}
                        Err(error) => return Err(error),
                    }
                    transaction.rollback();
                }
                Result::<()>::Ok(())
            }));
        }

        writer_task.await.expect("writer task panicked")?;
        for task in reader_tasks {
            task.await.expect("reader task panicked")?;
        }
        reader.close().await?;
        writer.close().await
    }

    #[tokio::test]
    async fn reader_transaction_rejects_a_refresh_after_a_flush() -> Result<()> {
        use slate_db::config::{FlushOptions, FlushType};

        let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let writer = Store::open("reader-checkpoint-view", Arc::clone(&objects)).await?;
        writer
            .put(Bytes::from_static(b"row/old"), Bytes::from_static(b"old"))
            .await?;
        writer
            .db
            .flush_with_options(FlushOptions {
                flush_type: FlushType::MemTable,
            })
            .await
            .map_err(map_operation_error)?;
        let reader = ReaderStore::open(
            "reader-checkpoint-view",
            Arc::clone(&objects),
            Duration::from_millis(10),
        )
        .await?;
        wait_for_reader_value(&reader, b"row/old", Some(b"old")).await?;

        let snapshot = reader.begin(IsolationLevel::Snapshot).await?;
        let replacement = writer.begin(IsolationLevel::SerializableSnapshot).await?;
        replacement.delete(b"row/old")?;
        replacement.put(Bytes::from_static(b"row/new"), Bytes::from_static(b"new"))?;
        replacement.commit().await?;
        writer
            .db
            .flush_with_options(FlushOptions {
                flush_type: FlushType::MemTable,
            })
            .await
            .map_err(map_operation_error)?;
        wait_for_reader_value(&reader, b"row/old", None).await?;
        assert_eq!(
            snapshot.get(b"row/old").await.unwrap_err().kind(),
            ErrorKind::Conflict
        );
        snapshot.rollback();
        reader.close().await?;
        writer.close().await
    }

    #[tokio::test]
    async fn reader_scan_keeps_its_open_snapshot_across_a_refresh() -> Result<()> {
        let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let writer = Store::open("reader-open-scan", Arc::clone(&objects)).await?;
        let initial = writer.begin(IsolationLevel::SerializableSnapshot).await?;
        initial.put(Bytes::from_static(b"row/a"), Bytes::from_static(b"old-a"))?;
        initial.put(Bytes::from_static(b"row/b"), Bytes::from_static(b"old-b"))?;
        initial.commit().await?;

        let reader =
            ReaderStore::open("reader-open-scan", objects, Duration::from_millis(10)).await?;
        wait_for_reader_value(&reader, b"row/a", Some(b"old-a")).await?;

        let transaction = reader.begin(IsolationLevel::Snapshot).await?;
        let mut scan = transaction
            .scan(KeyRange::new(
                Bytes::from_static(b"row/"),
                Bytes::from_static(b"row0"),
            ))
            .await?;
        let first = scan.next().await?.expect("first row is missing");
        assert_eq!(first.value, Bytes::from_static(b"old-a"));

        let replacement = writer.begin(IsolationLevel::SerializableSnapshot).await?;
        replacement.put(Bytes::from_static(b"row/b"), Bytes::from_static(b"new-b"))?;
        replacement.commit().await?;
        wait_for_reader_value(&reader, b"row/b", Some(b"new-b")).await?;

        let second = scan.next().await?.expect("second row is missing");
        assert_eq!(second.value, Bytes::from_static(b"old-b"));
        assert!(scan.next().await?.is_none());
        drop(scan);
        assert_eq!(
            transaction.get(b"row/b").await.unwrap_err().kind(),
            ErrorKind::Conflict
        );
        transaction.rollback();
        reader.close().await?;
        writer.close().await
    }

    async fn wait_for_reader_value(
        reader: &ReaderStore,
        key: &[u8],
        expected: Option<&'static [u8]>,
    ) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let observed = reader.get(key).await?;
                if observed.as_deref() == expected {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| Error::message(ErrorKind::Unavailable, "reader did not refresh"))?
    }

    #[tokio::test]
    async fn reader_reopens_after_its_checkpoint_client_closes() -> Result<()> {
        let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let writer = Store::open("reader-reopen", Arc::clone(&objects)).await?;
        writer
            .put(Bytes::from_static(b"key"), Bytes::from_static(b"value"))
            .await?;
        let reader =
            ReaderStore::open("reader-reopen", objects, Duration::from_millis(100)).await?;
        assert_eq!(
            reader.reader.options.manifest_poll_interval,
            Duration::from_millis(100)
        );
        assert_eq!(
            reader.reader.options.checkpoint_lifetime,
            Duration::from_secs(1)
        );

        reader
            .reader
            .current()
            .close()
            .await
            .map_err(map_operation_error)?;
        assert_eq!(
            reader.get(b"key").await?,
            Some(Bytes::from_static(b"value"))
        );

        reader
            .reader
            .current()
            .close()
            .await
            .map_err(map_operation_error)?;
        let entries = collect(
            reader
                .scan(KeyRange::new(
                    Bytes::from_static(b"key"),
                    Bytes::from_static(b"kez"),
                ))
                .await?,
        )
        .await?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, Bytes::from_static(b"key"));
        assert_eq!(entries[0].value, Bytes::from_static(b"value"));

        reader.close().await?;
        writer.close().await
    }

    #[tokio::test]
    async fn reader_snapshot_reopens_a_missing_checkpoint_as_a_conflict() -> Result<()> {
        let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let writer = Store::open("reader-snapshot-reopen", Arc::clone(&objects)).await?;
        writer
            .put(Bytes::from_static(b"key"), Bytes::from_static(b"value"))
            .await?;
        let reader = ReaderStore::open(
            "reader-snapshot-reopen",
            objects,
            Duration::from_millis(100),
        )
        .await?;
        let failed = reader.reader.current();
        let snapshot = ReaderSnapshot::new(Arc::clone(&reader.reader));

        let error = snapshot
            .operation_error(slate_db::Error::data(
                "checkpoint missing during refresh".into(),
            ))
            .await;

        assert_eq!(error.kind(), ErrorKind::Conflict);
        assert!(!Arc::ptr_eq(&failed, &reader.reader.current()));
        assert_eq!(
            reader.get(b"key").await?,
            Some(Bytes::from_static(b"value"))
        );
        reader.close().await?;
        writer.close().await
    }

    #[test]
    fn reader_reopen_classifier_rejects_unrelated_errors() {
        assert!(reader_must_reopen(&slate_db::Error::closed(
            "closed".into(),
            slate_db::CloseReason::Clean,
        )));
        assert!(reader_must_reopen(&slate_db::Error::data(
            "checkpoint missing during refresh".into(),
        )));
        assert!(!reader_must_reopen(&slate_db::Error::data(
            "checksum mismatch".into(),
        )));
        assert!(!reader_must_reopen(&slate_db::Error::unavailable(
            "temporary object-store failure".into(),
        )));
    }

    #[test]
    fn reader_close_ignores_only_an_already_closed_client() {
        assert!(
            map_close_error(slate_db::Error::closed(
                "closed".into(),
                slate_db::CloseReason::Clean,
            ))
            .is_ok()
        );
        assert_eq!(
            map_close_error(slate_db::Error::unavailable(
                "temporary object-store failure".into(),
            ))
            .unwrap_err()
            .kind(),
            ErrorKind::Unavailable,
        );
    }

    #[tokio::test]
    async fn reader_get_propagates_unrelated_data_errors() -> Result<()> {
        let objects = Arc::new(FaultingReadStore::default());
        let writer = Store::open(
            "reader-unavailable",
            Arc::clone(&objects) as Arc<dyn ObjectStore>,
        )
        .await?;
        writer
            .put(Bytes::from_static(b"key"), Bytes::from_static(b"value"))
            .await?;
        writer.close().await?;
        let reader = ReaderStore::open(
            "reader-unavailable",
            Arc::clone(&objects) as Arc<dyn ObjectStore>,
            Duration::from_secs(60),
        )
        .await?;

        objects.failures_remaining.store(2, Ordering::Relaxed);
        let before_get = objects.read_attempts.load(Ordering::Relaxed);
        assert_eq!(
            reader.get(b"key").await.unwrap_err().kind(),
            ErrorKind::Data
        );
        assert_eq!(
            objects.read_attempts.load(Ordering::Relaxed) - before_get,
            2,
            "unrelated data error from get attempted to reopen the reader",
        );

        reader.close().await
    }
}
