use std::sync::{Arc, Mutex};

use ::slatedb as slate_db;
use async_trait::async_trait;
use bytes::Bytes;
use slate_db::object_store::{ObjectStore, memory::InMemory};
use tokio::sync::{Notify, OnceCell};

use super::{
    DataPosition, Entry, Error, ErrorKind, IsolationLevel, KeyRange, Kv, KvIterator, Result,
    Transaction, TransactionalKv,
};

pub struct Store {
    db: Arc<slate_db::Db>,
    lifecycle: Arc<Lifecycle>,
}

impl Store {
    pub async fn open(
        path: impl Into<slate_db::object_store::path::Path> + Send,
        object_store: Arc<dyn ObjectStore>,
    ) -> Result<Self> {
        let db = slate_db::Db::open(path, object_store)
            .await
            .map_err(map_operation_error)?;
        Ok(Self {
            db: Arc::new(db),
            lifecycle: Arc::new(Lifecycle::default()),
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
    Error::source(operation_error_kind(&error), error.to_string(), error)
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
    use tokio::sync::oneshot;

    use super::*;

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
}
