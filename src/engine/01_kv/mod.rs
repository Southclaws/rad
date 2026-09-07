mod error;
pub mod fault;
pub mod key_encoding;
pub mod keys;
pub mod keyspace;
pub mod manifest;
pub mod slatedb;
pub mod telemetry;

use async_trait::async_trait;
use bytes::Bytes;

pub use error::{Closure, Error, ErrorKind, Result};

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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanPurpose {
    #[default]
    AccessPath,
    CascadeRange,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanOrder {
    #[default]
    Ascending,
    Descending,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ScanProfile {
    #[default]
    Latency,
    Throughput,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanRequest {
    pub range: KeyRange,
    pub purpose: ScanPurpose,
    pub order: ScanOrder,
    pub profile: ScanProfile,
}

impl ScanRequest {
    pub fn access_path(range: KeyRange) -> Self {
        Self {
            range,
            purpose: ScanPurpose::AccessPath,
            order: ScanOrder::Ascending,
            profile: ScanProfile::Latency,
        }
    }

    pub fn cascade_range(range: KeyRange) -> Self {
        Self {
            range,
            purpose: ScanPurpose::CascadeRange,
            order: ScanOrder::Ascending,
            profile: ScanProfile::Latency,
        }
    }

    pub fn with_order(mut self, order: ScanOrder) -> Self {
        self.order = order;
        self
    }

    pub fn with_profile(mut self, profile: ScanProfile) -> Self {
        self.profile = profile;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanDescriptor {
    pub request: ScanRequest,
    pub position: Option<DataPosition>,
}

pub struct DescribedScan<'a> {
    pub descriptor: ScanDescriptor,
    pub iterator: Box<dyn KvIterator + 'a>,
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
    /// Move to the first key at or after `next_key`.
    ///
    /// `next_key` must be inside the original scan range and after the last
    /// returned key.
    async fn seek_forward(&mut self, next_key: &[u8]) -> Result<()>;
    async fn next(&mut self) -> Result<Option<Entry>>;

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

    async fn scan_ordered<'a>(
        &'a self,
        range: KeyRange,
        order: ScanOrder,
    ) -> Result<Box<dyn KvIterator + 'a>> {
        match order {
            ScanOrder::Ascending => self.scan(range).await,
            ScanOrder::Descending => Err(Error::message(
                ErrorKind::Invalid,
                "descending scan is not supported by this KV view",
            )),
        }
    }

    async fn scan_with_request<'a>(&'a self, request: ScanRequest) -> Result<DescribedScan<'a>> {
        let descriptor = ScanDescriptor {
            request: request.clone(),
            position: None,
        };
        let iterator = self.scan_ordered(request.range, request.order).await?;
        Ok(DescribedScan {
            descriptor,
            iterator,
        })
    }
}

/// An ordered byte key-value store.
#[async_trait]
pub trait Kv: Send + Sync {
    async fn get(&self, key: &[u8]) -> Result<Option<Bytes>>;
    async fn put(&self, key: Bytes, value: Bytes) -> Result<()>;
    async fn delete(&self, key: &[u8]) -> Result<()>;
    async fn scan(&self, range: KeyRange) -> Result<Box<dyn KvIterator>>;

    async fn scan_ordered(&self, range: KeyRange, order: ScanOrder) -> Result<Box<dyn KvIterator>> {
        match order {
            ScanOrder::Ascending => self.scan(range).await,
            ScanOrder::Descending => Err(Error::message(
                ErrorKind::Invalid,
                "descending scan is not supported by this KV store",
            )),
        }
    }

    async fn scan_requested(&self, request: ScanRequest) -> Result<Box<dyn KvIterator>> {
        self.scan_ordered(request.range, request.order).await
    }
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
    async fn scan_ordered<'a>(
        &'a self,
        range: KeyRange,
        order: ScanOrder,
    ) -> Result<Box<dyn KvIterator + 'a>> {
        match order {
            ScanOrder::Ascending => self.scan(range).await,
            ScanOrder::Descending => Err(Error::message(
                ErrorKind::Invalid,
                "descending scan is not supported by this transaction",
            )),
        }
    }
    async fn scan_requested<'a>(
        &'a self,
        request: ScanRequest,
    ) -> Result<Box<dyn KvIterator + 'a>> {
        self.scan_ordered(request.range, request.order).await
    }
    fn scan_position(&self) -> Option<&DataPosition> {
        Some(self.begin_position())
    }
    async fn scan_with_request<'a>(&'a self, request: ScanRequest) -> Result<DescribedScan<'a>> {
        let descriptor = ScanDescriptor {
            request: request.clone(),
            position: self.scan_position().cloned(),
        };
        let iterator = self.scan_requested(request.clone()).await?;
        Ok(DescribedScan {
            descriptor,
            iterator,
        })
    }
    async fn commit(self: Box<Self>) -> Result<()>;
    fn rollback(self: Box<Self>);
}

/// A KV store capable of opening isolated transactions and orderly shutdown.
#[async_trait]
pub trait TransactionalKv: Kv {
    async fn begin(&self, isolation: IsolationLevel) -> Result<Box<dyn Transaction>>;
    async fn close(&self) -> Result<()>;

    fn physical_telemetry(&self) -> Option<std::sync::Arc<dyn telemetry::PhysicalTelemetry>> {
        None
    }
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

    async fn scan_ordered<'a>(
        &'a self,
        range: KeyRange,
        order: ScanOrder,
    ) -> Result<Box<dyn KvIterator + 'a>> {
        Kv::scan_ordered(self, range, order).await
    }

    async fn scan_with_request<'a>(&'a self, request: ScanRequest) -> Result<DescribedScan<'a>> {
        let descriptor = ScanDescriptor {
            request: request.clone(),
            position: None,
        };
        let iterator = Kv::scan_requested(self, request).await?;
        Ok(DescribedScan {
            descriptor,
            iterator,
        })
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

    async fn scan_ordered<'a>(
        &'a self,
        range: KeyRange,
        order: ScanOrder,
    ) -> Result<Box<dyn KvIterator + 'a>> {
        self.0.scan_ordered(range, order).await
    }

    async fn scan_with_request<'a>(&'a self, request: ScanRequest) -> Result<DescribedScan<'a>> {
        self.0.scan_with_request(request).await
    }
}
