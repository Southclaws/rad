//! Execution observation seam.
//!
//! Statement execution emits one compact observation per statement through
//! this boundary. Observers must be cheap and must never fail the query:
//! implementations drop under pressure rather than block, and the engine
//! skips fingerprint computation entirely when the installed observer says
//! collection is disabled.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;

use bytes::Bytes;

use crate::engine::kv::{Entry, KeyRange, KvIterator, KvView, Result as KvResult};
use crate::engine::lir::fingerprint::{Fingerprint, QueryFingerprints};

#[derive(Clone, Debug)]
pub struct StatementObservation {
    /// Fingerprints of the bound statement: exact/family roots, per-subtree
    /// digests in canonical order, and the logical dependency set.
    pub query: QueryFingerprints,
    /// Structural identity of the physical plan that executed, when one was
    /// planned (mutation statements execute their input relation plan).
    pub plan: Option<Fingerprint>,
    pub phase: PhaseTimings,
    /// Rows produced by the statement's relation before result shaping.
    pub rows: u64,
    /// The estimate that was available when this statement was planned,
    /// paired with `rows` so estimate quality is measurable. Absent when no
    /// model snapshot was installed.
    pub estimate: Option<crate::engine::planner::estimator::Estimate>,
    /// Catalog generations this statement was planned against.
    pub stamp: crate::engine::planner::models::DependencyStamp,
    /// Actual cardinalities for relations inside the statement whose output
    /// the plan materialized under a stable identity. Fused operators have
    /// no logical boundary and are absent rather than approximated.
    pub relations: Vec<RelationObservation>,
    /// Rows affected as reported in the statement summary.
    pub affected: u64,
    /// Logical identity of the table this statement mutated, if any.
    pub mutated: Option<crate::engine::catalog::identity::SchemaId>,
    /// KV work charged while the statement executed.
    pub kv: KvWork,
    /// Terminal outcome: `None` is success; a reason label otherwise.
    pub failure: Option<&'static str>,
}

/// One relation inside a statement, with what it actually produced and what
/// was estimated for it.
#[derive(Clone, Debug)]
pub struct RelationObservation {
    pub family: Fingerprint,
    pub rows: u64,
    pub estimate: Option<crate::engine::planner::estimator::Estimate>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KvWork {
    pub gets: u64,
    pub puts: u64,
    pub deletes: u64,
    pub scans: u64,
    pub iterated: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
}

/// Shared counters charged by [`ObservedView`]. One instance lives per
/// observed statement; reading it after execution yields the statement's
/// [`KvWork`].
#[derive(Debug, Default)]
pub struct KvCounters {
    gets: AtomicU64,
    puts: AtomicU64,
    deletes: AtomicU64,
    scans: AtomicU64,
    iterated: AtomicU64,
    bytes_read: AtomicU64,
    bytes_written: AtomicU64,
}

impl KvCounters {
    pub fn snapshot(&self) -> KvWork {
        KvWork {
            gets: self.gets.load(Ordering::Relaxed),
            puts: self.puts.load(Ordering::Relaxed),
            deletes: self.deletes.load(Ordering::Relaxed),
            scans: self.scans.load(Ordering::Relaxed),
            iterated: self.iterated.load(Ordering::Relaxed),
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
            bytes_written: self.bytes_written.load(Ordering::Relaxed),
        }
    }
}

/// KV decorator charging every operation to shared counters.
pub struct ObservedView<'a> {
    inner: &'a dyn KvView,
    counters: &'a KvCounters,
}

impl<'a> ObservedView<'a> {
    pub fn new(inner: &'a dyn KvView, counters: &'a KvCounters) -> Self {
        Self { inner, counters }
    }
}

#[async_trait]
impl KvView for ObservedView<'_> {
    fn begin_position(&self) -> Option<&crate::engine::kv::DataPosition> {
        self.inner.begin_position()
    }

    async fn get(&self, key: &[u8]) -> KvResult<Option<Bytes>> {
        self.counters.gets.fetch_add(1, Ordering::Relaxed);
        let value = self.inner.get(key).await?;
        if let Some(value) = &value {
            self.counters
                .bytes_read
                .fetch_add(value.len() as u64, Ordering::Relaxed);
        }
        Ok(value)
    }

    async fn put(&self, key: Bytes, value: Bytes) -> KvResult<()> {
        self.counters.puts.fetch_add(1, Ordering::Relaxed);
        self.counters
            .bytes_written
            .fetch_add((key.len() + value.len()) as u64, Ordering::Relaxed);
        self.inner.put(key, value).await
    }

    async fn delete(&self, key: &[u8]) -> KvResult<()> {
        self.counters.deletes.fetch_add(1, Ordering::Relaxed);
        self.inner.delete(key).await
    }

    fn untrack_write(&self, key: &[u8]) -> KvResult<()> {
        self.inner.untrack_write(key)
    }

    async fn scan<'b>(&'b self, range: KeyRange) -> KvResult<Box<dyn KvIterator + 'b>> {
        self.counters.scans.fetch_add(1, Ordering::Relaxed);
        let inner = self.inner.scan(range).await?;
        Ok(Box::new(ObservedIterator {
            inner,
            counters: self.counters,
        }))
    }
}

struct ObservedIterator<'a> {
    inner: Box<dyn KvIterator + 'a>,
    counters: &'a KvCounters,
}

#[async_trait]
impl KvIterator for ObservedIterator<'_> {
    async fn next(&mut self) -> KvResult<Option<Entry>> {
        let entry = self.inner.next().await?;
        if let Some(entry) = &entry {
            self.counters.iterated.fetch_add(1, Ordering::Relaxed);
            self.counters.bytes_read.fetch_add(
                (entry.key.len() + entry.value.len()) as u64,
                Ordering::Relaxed,
            );
        }
        Ok(entry)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PhaseTimings {
    pub bind: Duration,
    pub execute: Duration,
}

/// One executed program captured for the workload corpus: the canonical
/// document bytes (content-addressed, stored once per hash) and the
/// execution-log fields.
///
/// Serializable because an instance that cannot publish relays these to one
/// that can. The bytes contain the literals of the program that ran, so they
/// travel only over a transport shown to be confidential.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct ProgramRecord {
    /// Canonical JSON bytes of the wire program: key-sorted, whitespace-free.
    pub canonical: Vec<u8>,
    /// 16-byte content hash of the canonical bytes.
    pub content_hash: [u8; 16],
    pub at_unix_micros: u64,
    pub statements: u32,
    #[serde(default)]
    pub outcomes: Vec<ProgramStatementOutcome>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ProgramStatementOutcome {
    pub name: String,
    pub rows: u64,
}

pub trait ExecutionObserver: Send + Sync {
    fn statement(&self, observation: StatementObservation);

    fn captures_programs(&self) -> bool {
        false
    }

    /// A program submitted for execution, captured for the workload corpus.
    /// Called regardless of the execution outcome: failed programs are
    /// workload too. The default ignores it.
    fn program(&self, _record: ProgramRecord) {}

    fn program_skipped_oversize(&self) {}
}

/// Where observations go and which model snapshot the recorded estimate comes
/// from. An absent observer means nothing is collecting, so the engine skips
/// the work rather than computing statistics no one reads.
#[derive(Clone, Copy, Default)]
pub struct Observation<'a> {
    pub observer: Option<&'a Arc<dyn ExecutionObserver>>,
    pub statistics: Option<&'a Arc<dyn crate::engine::planner::estimator::StatisticsProvider>>,
}

impl Observation<'_> {
    pub fn enabled(&self) -> bool {
        self.observer.is_some()
    }
}
