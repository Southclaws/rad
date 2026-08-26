//! Backend-neutral physical storage telemetry.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const PHYSICAL_TELEMETRY_FORMAT: u32 = 1;
pub const PHYSICAL_REQUEST_TRACE_FORMAT: &str = "rad-physical-storage-trace-v1";

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhysicalTelemetryIdentity {
    pub backend: String,
    pub format: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct PhysicalTelemetryCapabilities {
    pub request_latency: bool,
    pub request_bytes: bool,
    pub request_concurrency: bool,
    pub cache_tiers: bool,
    pub access_locality: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PhysicalRequestClass {
    Read,
    RangeRead,
    MetadataRead,
    Write,
    Delete,
    List,
}

impl PhysicalRequestClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::RangeRead => "range_read",
            Self::MetadataRead => "metadata_read",
            Self::Write => "write",
            Self::Delete => "delete",
            Self::List => "list",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PhysicalCacheTier {
    Memory,
    Local,
}

impl PhysicalCacheTier {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Local => "local",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PhysicalServiceTier {
    Memory,
    Local,
    Remote,
}

impl PhysicalServiceTier {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhysicalRequestCondition {
    pub size_upper_bound: Option<u64>,
    pub concurrency_upper_bound: Option<u32>,
    pub service_tier: Option<PhysicalServiceTier>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhysicalRequestTrace {
    pub format: &'static str,
    pub backend: String,
    pub scope: &'static str,
    pub coverage: &'static str,
    pub caches: Vec<PhysicalCacheMeasurement>,
    pub requests: Vec<PhysicalRequestMeasurement>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhysicalCacheMeasurement {
    pub tier: PhysicalCacheTier,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_kind: Option<String>,
    pub accesses: u64,
    pub hits: u64,
    pub misses: u64,
    pub errors: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhysicalRequestMeasurement {
    pub service_tier: PhysicalServiceTier,
    pub class: PhysicalRequestClass,
    pub requests: u64,
    pub completed: u64,
    pub errors: u64,
    pub bytes: u64,
    pub duration_micros: u64,
}

#[derive(Default)]
struct PhysicalCacheAccumulator {
    accesses: u64,
    hits: u64,
    misses: u64,
    errors: u64,
}

#[derive(Default)]
struct PhysicalRequestAccumulator {
    requests: u64,
    completed: u64,
    errors: u64,
    bytes: u64,
    duration_micros: u64,
}

#[derive(Default)]
struct PhysicalRequestState {
    backend: String,
    caches: BTreeMap<(PhysicalCacheTier, Option<String>), PhysicalCacheAccumulator>,
    requests: BTreeMap<(PhysicalServiceTier, PhysicalRequestClass), PhysicalRequestAccumulator>,
}

pub(crate) struct PhysicalRequestRecorder {
    state: Mutex<PhysicalRequestState>,
}

tokio::task_local! {
    static PHYSICAL_REQUEST: Arc<PhysicalRequestRecorder>;
}

pub(crate) async fn observe_request<F>(
    backend: &str,
    future: F,
) -> (F::Output, PhysicalRequestTrace)
where
    F: Future,
{
    let recorder = Arc::new(PhysicalRequestRecorder {
        state: Mutex::new(PhysicalRequestState {
            backend: backend.to_owned(),
            ..PhysicalRequestState::default()
        }),
    });
    let output = PHYSICAL_REQUEST.scope(Arc::clone(&recorder), future).await;
    (output, recorder.snapshot())
}

pub(crate) fn current_request_recorder() -> Option<Arc<PhysicalRequestRecorder>> {
    PHYSICAL_REQUEST.try_with(Arc::clone).ok()
}

pub(crate) fn record_cache(
    tier: PhysicalCacheTier,
    entry_kind: Option<&str>,
    accesses: u64,
    hits: u64,
    misses: u64,
    errors: u64,
) {
    crate::telemetry::storage_cache_observed(
        tier.as_str(),
        entry_kind,
        accesses,
        hits,
        misses,
        errors,
    );
    let _ = PHYSICAL_REQUEST.try_with(|recorder| {
        recorder.record_cache(tier, entry_kind, accesses, hits, misses, errors);
    });
}

impl PhysicalRequestRecorder {
    fn record_cache(
        &self,
        tier: PhysicalCacheTier,
        entry_kind: Option<&str>,
        accesses: u64,
        hits: u64,
        misses: u64,
        errors: u64,
    ) {
        let mut state = self.state.lock().expect("physical request lock poisoned");
        let cache = state
            .caches
            .entry((tier, entry_kind.map(str::to_owned)))
            .or_default();
        cache.accesses = cache.accesses.saturating_add(accesses);
        cache.hits = cache.hits.saturating_add(hits);
        cache.misses = cache.misses.saturating_add(misses);
        cache.errors = cache.errors.saturating_add(errors);
    }

    pub(crate) fn record_request(
        &self,
        service_tier: PhysicalServiceTier,
        class: PhysicalRequestClass,
        completed: bool,
        error: bool,
        bytes: u64,
        duration: Duration,
    ) {
        crate::telemetry::storage_request_finished(
            service_tier.as_str(),
            class.as_str(),
            completed,
            error,
            bytes,
            duration,
        );
        let mut state = self.state.lock().expect("physical request lock poisoned");
        let request = state.requests.entry((service_tier, class)).or_default();
        request.requests = request.requests.saturating_add(1);
        request.completed = request.completed.saturating_add(u64::from(completed));
        request.errors = request.errors.saturating_add(u64::from(error));
        request.bytes = request.bytes.saturating_add(bytes);
        request.duration_micros = request
            .duration_micros
            .saturating_add(duration.as_micros().min(u128::from(u64::MAX)) as u64);
    }

    fn snapshot(&self) -> PhysicalRequestTrace {
        let state = self.state.lock().expect("physical request lock poisoned");
        PhysicalRequestTrace {
            format: PHYSICAL_REQUEST_TRACE_FORMAT,
            backend: state.backend.clone(),
            scope: "foreground_task",
            coverage: "cache_and_backing_reads",
            caches: state
                .caches
                .iter()
                .map(|((tier, entry_kind), cache)| PhysicalCacheMeasurement {
                    tier: *tier,
                    entry_kind: entry_kind.clone(),
                    accesses: cache.accesses,
                    hits: cache.hits,
                    misses: if *tier == PhysicalCacheTier::Local {
                        cache.accesses.saturating_sub(cache.hits)
                    } else {
                        cache.misses
                    },
                    errors: cache.errors,
                })
                .collect(),
            requests: state
                .requests
                .iter()
                .map(
                    |((service_tier, class), request)| PhysicalRequestMeasurement {
                        service_tier: *service_tier,
                        class: *class,
                        requests: request.requests,
                        completed: request.completed,
                        errors: request.errors,
                        bytes: request.bytes,
                        duration_micros: request.duration_micros,
                    },
                )
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CumulativeHistogram {
    pub boundaries: Vec<u64>,
    pub bucket_counts: Vec<u64>,
    pub count: u64,
    pub maximum: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PhysicalRequestSnapshot {
    pub class: PhysicalRequestClass,
    pub condition: PhysicalRequestCondition,
    pub requests: u64,
    pub errors: u64,
    pub latency_micros: Option<CumulativeHistogram>,
    pub bytes: Option<CumulativeHistogram>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PhysicalCacheSnapshot {
    pub tier: PhysicalCacheTier,
    pub accesses: u64,
    pub hits: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PhysicalTelemetrySnapshot {
    pub identity: PhysicalTelemetryIdentity,
    pub capabilities: PhysicalTelemetryCapabilities,
    pub requests: Vec<PhysicalRequestSnapshot>,
    pub caches: Vec<PhysicalCacheSnapshot>,
}

pub trait PhysicalTelemetry: Send + Sync {
    fn snapshot(&self) -> PhysicalTelemetrySnapshot;
}

pub type SharedPhysicalTelemetry = Arc<dyn PhysicalTelemetry>;
