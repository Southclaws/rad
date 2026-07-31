mod error;
pub mod fault;
pub mod key_encoding;
pub mod slatedb;

use async_trait::async_trait;
use bytes::Bytes;

pub use error::{Error, ErrorKind, Result};

/// The isolation guarantees requested from a transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IsolationLevel {
    /// Detects write-write conflicts while permitting write skew.
    Snapshot,
    /// Also detects read-write conflicts, including phantoms in empty ranges.
    SerializableSnapshot,
}

impl IsolationLevel {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::SerializableSnapshot => "serializable_snapshot",
        }
    }
}

/// An opaque storage position identifying the snapshot at transaction start.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DataPosition(String);

impl DataPosition {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn from_sequence(sequence: u64) -> Self {
        Self(sequence.to_string())
    }
}

/// An owned, half-open key range: `[start, end)`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct KeyRange {
    pub start: Option<Bytes>,
    pub end: Option<Bytes>,
}

impl KeyRange {
    pub fn new(start: impl Into<Bytes>, end: impl Into<Bytes>) -> Self {
        Self {
            start: Some(start.into()),
            end: Some(end.into()),
        }
    }

    pub fn from_start(start: impl Into<Bytes>) -> Self {
        Self {
            start: Some(start.into()),
            end: None,
        }
    }

    pub fn to_end(end: impl Into<Bytes>) -> Self {
        Self {
            start: None,
            end: Some(end.into()),
        }
    }

    pub fn all() -> Self {
        Self::default()
    }
}

/// A key-value entry yielded by an ordered scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    pub key: Bytes,
    pub value: Bytes,
}

#[async_trait]
pub trait KvIterator: Send {
    async fn next(&mut self) -> Result<Option<Entry>>;
}

/// The common read/write surface shared by a database and an open
/// transaction. Catalog persistence is deliberately written against this
/// view so the exact same key and durable-value logic is used for bootstrap,
/// snapshots, and serializable mutations.
#[async_trait]
pub trait KvView: Send + Sync {
    fn begin_position(&self) -> Option<&DataPosition> {
        None
    }

    async fn get(&self, key: &[u8]) -> Result<Option<Bytes>>;
    async fn put(&self, key: Bytes, value: Bytes) -> Result<()>;
    async fn delete(&self, key: &[u8]) -> Result<()>;
    /// Exclude a write to `key` from transaction conflict detection.
    ///
    /// Stores without an enclosing transaction have no conflict set, so the
    /// default implementation is a no-op. Callers must retain a separately
    /// tracked semantic fence for every compatibility boundary they relax.
    fn untrack_write(&self, _key: &[u8]) -> Result<()> {
        Ok(())
    }
    /// Open a cursor with owned iteration state over this view's snapshot.
    ///
    /// Its shared borrow permits interleaved point reads while preventing the
    /// transaction from being consumed before the cursor is dropped.
    async fn scan<'a>(&'a self, range: KeyRange) -> Result<Box<dyn KvIterator + 'a>>;
}

/// An ordered byte key-value store.
#[async_trait]
pub trait Kv: Send + Sync {
    async fn get(&self, key: &[u8]) -> Result<Option<Bytes>>;
    async fn put(&self, key: Bytes, value: Bytes) -> Result<()>;
    async fn delete(&self, key: &[u8]) -> Result<()>;
    async fn scan(&self, range: KeyRange) -> Result<Box<dyn KvIterator>>;
}

/// A transaction with a stable snapshot plus read-your-own-writes behavior.
///
/// `commit` must be driven to completion once it has been polled. Cancelling a
/// commit can leave the caller unable to tell whether the write became durable.
#[async_trait]
pub trait Transaction: Send + Sync {
    fn begin_position(&self) -> &DataPosition;
    async fn get(&self, key: &[u8]) -> Result<Option<Bytes>>;
    fn put(&self, key: Bytes, value: Bytes) -> Result<()>;
    fn delete(&self, key: &[u8]) -> Result<()>;
    fn untrack_write(&self, key: &[u8]) -> Result<()>;
    async fn scan<'a>(&'a self, range: KeyRange) -> Result<Box<dyn KvIterator + 'a>>;
    async fn commit(self: Box<Self>) -> Result<()>;
    fn rollback(self: Box<Self>);
}

/// A KV store capable of opening isolated transactions and orderly shutdown.
#[async_trait]
pub trait TransactionalKv: Kv {
    async fn begin(&self, isolation: IsolationLevel) -> Result<Box<dyn Transaction>>;
    async fn close(&self) -> Result<()>;
}

#[async_trait]
impl<T> KvView for T
where
    T: Kv + ?Sized,
{
    async fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        Kv::get(self, key).await
    }

    async fn put(&self, key: Bytes, value: Bytes) -> Result<()> {
        Kv::put(self, key, value).await
    }

    async fn delete(&self, key: &[u8]) -> Result<()> {
        Kv::delete(self, key).await
    }

    async fn scan<'a>(&'a self, range: KeyRange) -> Result<Box<dyn KvIterator + 'a>> {
        Kv::scan(self, range).await
    }
}

/// Adapter for dynamic transactions. The transaction's buffered writes are
/// synchronous, but the common view keeps one async API for stores and
/// transactions.
pub struct TransactionView<'a>(pub &'a dyn Transaction);

#[async_trait]
impl KvView for TransactionView<'_> {
    fn begin_position(&self) -> Option<&DataPosition> {
        Some(self.0.begin_position())
    }

    async fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        self.0.get(key).await
    }

    async fn put(&self, key: Bytes, value: Bytes) -> Result<()> {
        self.0.put(key, value)
    }

    async fn delete(&self, key: &[u8]) -> Result<()> {
        self.0.delete(key)
    }

    fn untrack_write(&self, key: &[u8]) -> Result<()> {
        self.0.untrack_write(key)
    }

    async fn scan<'a>(&'a self, range: KeyRange) -> Result<Box<dyn KvIterator + 'a>> {
        self.0.scan(range).await
    }
}
