use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use ::slatedb as slate_db;
use async_trait::async_trait;
use bytes::Bytes;
use slate_db::object_store::{ObjectStore, memory::InMemory};
use tokio::sync::{Mutex as AsyncMutex, Notify, OnceCell};

use super::{
    Closure, DataPosition, Entry, Error, ErrorKind, IsolationLevel, KeyRange, Kv, KvIterator,
    Result, Transaction, TransactionalKv,
};

mod telemetry;

use telemetry::SlateTelemetry;

pub struct Store {
    db: Arc<slate_db::Db>,
    lifecycle: Arc<Lifecycle>,
    telemetry: Arc<SlateTelemetry>,
}

/// A Slate checkpoint reader exposed through Rad's transactional read surface.
/// Transactions are stable only for the duration of each individual operation;
/// the engine rejects every effectful program before it opens this view.
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
    reopen: AsyncMutex<()>,
}

impl ReaderStore {
    pub async fn open(
        path: impl Into<slate_db::object_store::path::Path> + Send,
        object_store: Arc<dyn ObjectStore>,
        poll_interval: Duration,
    ) -> Result<Self> {
        let path = path.into();
        let options = slate_db::config::DbReaderOptions {
            manifest_poll_interval: poll_interval,
            checkpoint_lifetime: poll_interval
                .checked_mul(10)
                .unwrap_or(Duration::from_secs(60))
                .max(Duration::from_secs(1)),
            ..Default::default()
        };
        let telemetry = SlateTelemetry::new();
        let db = slate_db::DbReader::builder(path.clone(), Arc::clone(&object_store))
            .with_options(options.clone())
            .with_metrics_recorder(telemetry.recorder())
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
                .with_metrics_recorder(self.telemetry.recorder())
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
        range: KeyRange,
    ) -> std::result::Result<slate_db::DbIterator, slate_db::Error> {
        let reader = self.current();
        match reader.scan(range.clone()).await {
            Ok(iterator) => Ok(iterator),
            Err(error) if reader_must_reopen(&error) => {
                self.reopen_after(&reader).await?.scan(range).await
            }
            Err(error) => Err(error),
        }
    }

    async fn close(&self) -> std::result::Result<(), slate_db::Error> {
        let _guard = self.reopen.lock().await;
        self.current().close().await
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
        let telemetry = SlateTelemetry::new();
        let db = slate_db::Db::builder(path, object_store)
            .with_metrics_recorder(telemetry.recorder())
            .build()
            .await
            .map_err(map_operation_error)?;
        Ok(Self {
            db: Arc::new(db),
            lifecycle: Arc::new(Lifecycle::default()),
            telemetry,
        })
    }

    pub async fn memory(path: &str) -> Result<Self> {
        Self::open(path, Arc::new(InMemory::new())).await
    }
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
        let lease = self.lifecycle.acquire()?;
        let iterator = self.db.scan(range).await.map_err(map_operation_error)?;
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
                    Ok(()) => Ok(()),
                    Err(error) if matches!(error.kind(), slate_db::ErrorKind::Closed(_)) => Ok(()),
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
        let lease = self.lifecycle.acquire()?;
        let iterator = self.reader.scan(range).await.map_err(map_operation_error)?;
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
        Ok(Box::new(ReaderTransaction {
            reader: Arc::clone(&self.reader),
            begin_position: DataPosition::reader(),
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
    _lease: Lease,
}

struct ReaderTransaction {
    reader: Arc<ReaderBackend>,
    begin_position: DataPosition,
    _lease: Lease,
}

#[async_trait]
impl Transaction for ReaderTransaction {
    fn begin_position(&self) -> &DataPosition {
        &self.begin_position
    }

    fn scan_position(&self) -> Option<&DataPosition> {
        // A reader can refresh between operations. Its begin position does not
        // identify the state that builds this scan.
        None
    }

    async fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        self.reader.get(key).await.map_err(map_operation_error)
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
        let iterator = self.reader.scan(range).await.map_err(map_operation_error)?;
        Ok(Box::new(SlateIterator {
            iterator,
            _lease: None,
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
        let iterator = self
            .transaction
            .scan(range)
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

#[async_trait]
impl KvIterator for SlateIterator {
    async fn seek_forward(&mut self, next_key: &[u8]) -> Result<()> {
        self.iterator
            .seek(next_key)
            .await
            .map_err(map_operation_error)
    }

    async fn next(&mut self) -> Result<Option<Entry>> {
        self.iterator
            .next()
            .await
            .map(|entry| {
                entry.map(|entry| Entry {
                    key: entry.key,
                    value: entry.value,
                })
            })
            .map_err(map_operation_error)
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
        {
            let scan = transaction
                .scan_with_request(crate::engine::kv::ScanRequest::access_path(KeyRange::all()))
                .await?;
            assert_eq!(scan.descriptor.position, None);
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
    async fn reader_reopen_policy_propagates_unrelated_data_errors() -> Result<()> {
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

        objects.failures_remaining.store(2, Ordering::Relaxed);
        let before_scan = objects.read_attempts.load(Ordering::Relaxed);
        assert_eq!(
            match reader.scan(KeyRange::all()).await {
                Ok(_) => panic!("scan unexpectedly succeeded through an unavailable store"),
                Err(error) => error.kind(),
            },
            ErrorKind::Data,
        );
        assert_eq!(
            objects.read_attempts.load(Ordering::Relaxed) - before_scan,
            2,
            "unrelated data error from scan attempted to reopen the reader",
        );

        reader.close().await
    }
}
