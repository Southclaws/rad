//! Execution observation seam.
//!
//! Statement execution emits one compact observation per statement through
//! this boundary. Observers must be cheap and must never fail the query:
//! implementations drop under pressure rather than block, and the engine
//! skips fingerprint computation entirely when the installed observer says
//! collection is disabled.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use bytes::Bytes;
use sha2::{Digest as _, Sha256};

use crate::engine::kv::{
    DescribedScan, Entry, KeyRange, KvIterator, KvView, Result as KvResult, ScanDescriptor,
    ScanPurpose, ScanRequest,
};
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
    pub join_operators: Vec<JoinOperatorMeasurement>,
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KvWork {
    pub gets: u64,
    pub puts: u64,
    pub deletes: u64,
    pub scans: u64,
    pub forward_seeks: u64,
    pub iterated: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
}

const LOGICAL_SCAN_TRACE_FORMAT: &str = "rad-logical-scan-trace-v1";
const MAX_LOGICAL_SCAN_RECORDS: usize = 64;
const MAX_LOGICAL_SCAN_BOUND_BYTES: usize = 512;
const MAX_LOGICAL_SCAN_SEEKS: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KvScanTrace {
    pub format: &'static str,
    pub scans: Vec<KvScanMeasurement>,
    pub dropped: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KvScanMeasurement {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_position: Option<String>,
    pub purpose: ScanPurpose,
    pub range: KvScanRangeMeasurement,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forward_seeks: Vec<KvScanBoundMeasurement>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub dropped_forward_seeks: u64,
    pub iterated: u64,
    pub bytes_read: u64,
    pub complete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KvScanRangeMeasurement {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<KvScanBoundMeasurement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<KvScanBoundMeasurement>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KvScanBoundMeasurement {
    pub byte_length: u64,
    pub sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base64: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinOperatorMeasurement {
    pub operator: &'static str,
    pub build_rows: u64,
    pub probe_rows: u64,
    pub lookup_requests: u64,
    pub key_comparisons: u64,
    pub residual_predicate_evaluations: u64,
    /// Owned join-key bytes and scalar payload bytes. Allocator metadata is excluded.
    pub peak_retained_bytes: u64,
    pub spill_bytes: u64,
    pub reduction_passes: u64,
    pub rows_before_reduction: u64,
    pub rows_after_reduction: u64,
    pub dangling_rows_removed: u64,
    pub expanded_rows: u64,
    pub filter_rows_scanned: u64,
    pub filter_insertions: u64,
    pub filter_checks: u64,
    pub filter_false_positives: u64,
    pub filter_false_positive_measurement_complete: bool,
    pub filter_rows_skipped: u64,
    pub filter_paths: u64,
    pub filter_pruned_paths: u64,
    pub filter_builds: u64,
    pub filter_shared_paths: u64,
    pub filter_build_cancellations: u64,
    pub filter_memory_cancellations: u64,
    pub filter_probe_cancellations: u64,
    pub filter_paths_canceled: u64,
    pub filter_blocks_scanned: u64,
    pub filter_blocks_skipped: u64,
    pub filter_min_max_checks: u64,
    pub filter_min_max_rows_skipped: u64,
    pub filter_input_scans: u64,
    pub filter_repeated_scans: u64,
    pub filter_bytes: u64,
    pub filter_storage_range_candidates: u64,
    pub filter_storage_ranges_applied: u64,
    pub filter_storage_scans_pruned: u64,
    pub filter_storage_empty_scans: u64,
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
    forward_seeks: AtomicU64,
    iterated: AtomicU64,
    bytes_read: AtomicU64,
    bytes_written: AtomicU64,
    capture_scan_trace: bool,
    scan_trace: Mutex<Vec<KvScanMeasurement>>,
    scan_trace_dropped: AtomicU64,
}

impl KvCounters {
    pub fn new(capture_scan_trace: bool) -> Self {
        Self {
            capture_scan_trace,
            ..Self::default()
        }
    }

    pub fn snapshot(&self) -> KvWork {
        KvWork {
            gets: self.gets.load(Ordering::Relaxed),
            puts: self.puts.load(Ordering::Relaxed),
            deletes: self.deletes.load(Ordering::Relaxed),
            scans: self.scans.load(Ordering::Relaxed),
            forward_seeks: self.forward_seeks.load(Ordering::Relaxed),
            iterated: self.iterated.load(Ordering::Relaxed),
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
            bytes_written: self.bytes_written.load(Ordering::Relaxed),
        }
    }

    pub fn scan_trace(&self) -> Option<KvScanTrace> {
        if !self.capture_scan_trace {
            return None;
        }
        let scans = self
            .scan_trace
            .lock()
            .expect("logical scan trace lock poisoned")
            .clone();
        let dropped = self.scan_trace_dropped.load(Ordering::Relaxed);
        (!scans.is_empty() || dropped > 0).then_some(KvScanTrace {
            format: LOGICAL_SCAN_TRACE_FORMAT,
            scans,
            dropped,
        })
    }

    fn start_scan(&self, descriptor: &ScanDescriptor) -> Option<usize> {
        if !self.capture_scan_trace {
            return None;
        }
        let mut scans = self
            .scan_trace
            .lock()
            .expect("logical scan trace lock poisoned");
        if scans.len() >= MAX_LOGICAL_SCAN_RECORDS {
            self.scan_trace_dropped.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let index = scans.len();
        scans.push(scan_measurement(descriptor));
        Some(index)
    }

    fn record_scan_entry(&self, index: usize, entry: &Entry) {
        let mut scans = self
            .scan_trace
            .lock()
            .expect("logical scan trace lock poisoned");
        let scan = scans.get_mut(index).expect("logical scan trace index");
        scan.iterated = scan.iterated.saturating_add(1);
        scan.bytes_read = scan
            .bytes_read
            .saturating_add((entry.key.len() + entry.value.len()) as u64);
    }

    fn record_scan_seek(&self, index: usize, next_key: &[u8]) {
        let mut scans = self
            .scan_trace
            .lock()
            .expect("logical scan trace lock poisoned");
        let scan = scans.get_mut(index).expect("logical scan trace index");
        if scan.forward_seeks.len() < MAX_LOGICAL_SCAN_SEEKS {
            scan.forward_seeks.push(scan_bound(next_key));
        } else {
            scan.dropped_forward_seeks = scan.dropped_forward_seeks.saturating_add(1);
        }
    }

    fn complete_scan(&self, index: usize) {
        self.scan_trace
            .lock()
            .expect("logical scan trace lock poisoned")
            .get_mut(index)
            .expect("logical scan trace index")
            .complete = true;
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
        Ok(self
            .scan_with_request(ScanRequest::access_path(range))
            .await?
            .iterator)
    }

    async fn scan_with_request<'b>(&'b self, request: ScanRequest) -> KvResult<DescribedScan<'b>> {
        self.counters.scans.fetch_add(1, Ordering::Relaxed);
        let scan = self.inner.scan_with_request(request).await?;
        let trace_index = self.counters.start_scan(&scan.descriptor);
        Ok(DescribedScan {
            descriptor: scan.descriptor,
            iterator: Box::new(ObservedIterator {
                inner: scan.iterator,
                counters: self.counters,
                trace_index,
            }),
        })
    }
}

struct ObservedIterator<'a> {
    inner: Box<dyn KvIterator + 'a>,
    counters: &'a KvCounters,
    trace_index: Option<usize>,
}

#[async_trait]
impl KvIterator for ObservedIterator<'_> {
    async fn seek_forward(&mut self, next_key: &[u8]) -> KvResult<()> {
        self.inner.seek_forward(next_key).await?;
        self.counters.forward_seeks.fetch_add(1, Ordering::Relaxed);
        if let Some(index) = self.trace_index {
            self.counters.record_scan_seek(index, next_key);
        }
        Ok(())
    }

    async fn next(&mut self) -> KvResult<Option<Entry>> {
        let entry = self.inner.next().await?;
        if let Some(entry) = &entry {
            self.counters.iterated.fetch_add(1, Ordering::Relaxed);
            self.counters.bytes_read.fetch_add(
                (entry.key.len() + entry.value.len()) as u64,
                Ordering::Relaxed,
            );
            if let Some(index) = self.trace_index {
                self.counters.record_scan_entry(index, entry);
            }
        } else if let Some(index) = self.trace_index {
            self.counters.complete_scan(index);
        }
        Ok(entry)
    }
}

fn scan_measurement(descriptor: &ScanDescriptor) -> KvScanMeasurement {
    KvScanMeasurement {
        snapshot_position: descriptor
            .position
            .as_ref()
            .map(|position| position.as_str().to_owned()),
        purpose: descriptor.request.purpose,
        range: KvScanRangeMeasurement {
            start: descriptor
                .request
                .range
                .start
                .as_ref()
                .map(|bound| scan_bound(bound)),
            end: descriptor
                .request
                .range
                .end
                .as_ref()
                .map(|bound| scan_bound(bound)),
        },
        forward_seeks: Vec::new(),
        dropped_forward_seeks: 0,
        iterated: 0,
        bytes_read: 0,
        complete: false,
    }
}

fn scan_bound(bound: &[u8]) -> KvScanBoundMeasurement {
    KvScanBoundMeasurement {
        byte_length: bound.len() as u64,
        sha256: format!("{:x}", Sha256::digest(bound)),
        base64: (bound.len() <= MAX_LOGICAL_SCAN_BOUND_BYTES).then(|| STANDARD.encode(bound)),
    }
}

const fn is_zero(value: &u64) -> bool {
    *value == 0
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

/// Where observations go. An absent observer means nothing is collecting, so
/// the engine skips the observation work.
#[derive(Clone, Copy, Default)]
pub struct Observation<'a> {
    pub observer: Option<&'a Arc<dyn ExecutionObserver>>,
}

impl Observation<'_> {
    pub fn enabled(&self) -> bool {
        self.observer.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::kv::slatedb::Store;
    use crate::engine::kv::{IsolationLevel, Kv, ScanRequest, TransactionView, TransactionalKv};

    #[tokio::test]
    async fn logical_scan_trace_keeps_the_exact_request_and_result_work() -> KvResult<()> {
        let store = Store::memory("logical-scan-trace").await?;
        for key in [b"a", b"b", b"c"] {
            Kv::put(
                &store,
                Bytes::copy_from_slice(key),
                Bytes::copy_from_slice(key),
            )
            .await?;
        }
        let transaction = store.begin(IsolationLevel::Snapshot).await?;
        let position = transaction.begin_position().as_str().to_owned();
        let counters = KvCounters::new(true);
        let request = ScanRequest::cascade_range(KeyRange::new(
            Bytes::from_static(b"b"),
            Bytes::from_static(b"d"),
        ));
        {
            let view = TransactionView(&*transaction);
            let observed = ObservedView::new(&view, &counters);
            let mut scan = observed.scan_with_request(request).await?;
            scan.iterator.seek_forward(b"c").await?;
            while scan.iterator.next().await?.is_some() {}
        }

        let trace = counters.scan_trace().expect("logical scan trace");
        assert_eq!(counters.snapshot().forward_seeks, 1);
        assert_eq!(trace.format, LOGICAL_SCAN_TRACE_FORMAT);
        assert_eq!(trace.dropped, 0);
        assert_eq!(trace.scans.len(), 1);
        let scan = &trace.scans[0];
        assert_eq!(scan.snapshot_position.as_deref(), Some(position.as_str()));
        assert_eq!(scan.purpose, ScanPurpose::CascadeRange);
        assert_eq!(scan.iterated, 1);
        assert_eq!(scan.bytes_read, 2);
        assert!(scan.complete);
        assert_eq!(scan.forward_seeks.len(), 1);
        assert_eq!(scan.forward_seeks[0].base64.as_deref(), Some("Yw=="));
        assert_eq!(scan.dropped_forward_seeks, 0);
        assert_eq!(
            scan.range
                .start
                .as_ref()
                .and_then(|bound| bound.base64.as_deref()),
            Some("Yg==")
        );
        assert_eq!(
            scan.range
                .end
                .as_ref()
                .and_then(|bound| bound.base64.as_deref()),
            Some("ZA==")
        );
        transaction.rollback();
        store.close().await
    }

    #[test]
    fn logical_scan_trace_bounds_records_and_values() {
        let counters = KvCounters::new(true);
        let descriptor = ScanDescriptor {
            request: ScanRequest::access_path(KeyRange::all()),
            position: None,
        };
        for _ in 0..=MAX_LOGICAL_SCAN_RECORDS {
            counters.start_scan(&descriptor);
        }
        for index in 0..=MAX_LOGICAL_SCAN_SEEKS {
            counters.record_scan_seek(0, &[index as u8]);
        }

        let trace = counters.scan_trace().expect("bounded logical scan trace");
        assert_eq!(trace.scans.len(), MAX_LOGICAL_SCAN_RECORDS);
        assert_eq!(trace.dropped, 1);
        assert_eq!(trace.scans[0].forward_seeks.len(), MAX_LOGICAL_SCAN_SEEKS);
        assert_eq!(trace.scans[0].dropped_forward_seeks, 1);
        assert_eq!(
            scan_bound(&Bytes::from(vec![0; MAX_LOGICAL_SCAN_BOUND_BYTES + 1])).base64,
            None
        );
    }
}
