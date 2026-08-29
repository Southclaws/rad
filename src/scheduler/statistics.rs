//! Statistics collection: the execution-side collector and the hot registry.
//!
//! The collector implements the engine's observation seam and does one thing
//! on the execution path: a non-blocking send into a bounded channel.
//! Overflow drops the observation and counts the drop — statistics loss is
//! acceptable, blocking execution is not. The registry is the single-consumer
//! side: a frequency sketch over every subtree family fingerprint plus a
//! bounded map of per-query models for the hot set.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::engine::exec::observe::{ExecutionObserver, ProgramRecord, StatementObservation};
use crate::engine::exec::survey::{SurveyRequest, SurveyResult};
use crate::engine::lir::fingerprint::Fingerprint;
pub use crate::engine::planner::models::{
    CorpusCaptureStats, CorpusMaintenanceStats, FeedbackModel, FingerprintCardinalitySketch,
    FrequencySketch, Log2Histogram, ObservationModelKind, PlanProfile, PlannerStats, SynopsisModel,
};

pub const DEFAULT_CHANNEL_CAPACITY: usize = 4096;
pub(crate) const CORPUS_MAX_PROGRAM_BYTES: usize = 1024 * 1024;
/// Relayed batches awaiting merge. Small: each holds a whole interval of
/// another instance's evidence, and a sender that finds the queue full retries
/// rather than loses.
const RELAY_QUEUE_CAPACITY: usize = 32;
pub const DEFAULT_REGISTRY_CAPACITY: usize = 4096;
/// The corpus canonicalization version stamped into corpus keys.
pub const CANONICAL_FORMAT_VERSION: u64 = 1;
const STATISTICS_MODEL_FORMAT: u32 = 1;
const STATISTICS_MODEL_CATALOG_FORMAT: u32 = 1;
const STATISTICS_SYNOPSIS_FORMAT: u32 = 5;
const STATISTICS_FREQUENCY_FORMAT: u32 = 1;
const RETENTION_EPOCH_MICROS: u64 = 7 * 24 * 60 * 60 * 1_000_000;
const CORPUS_MAX_EXECUTIONS: usize = 10_000;
const CORPUS_MAX_BYTES: usize = 64 * 1024 * 1024;
const CORPUS_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const DEFAULT_SURVEY_ROW_BUDGET: u64 = 50_000;
const DEFAULT_SURVEY_BYTE_BUDGET: u64 = 64 * 1024 * 1024;

#[derive(Default)]
struct PhysicalRequestCalibration {
    observed_requests: u64,
    errors: u64,
    latency_micros: Log2Histogram,
    bytes: Log2Histogram,
}

#[derive(Default)]
struct PhysicalCacheCalibration {
    accesses: u64,
    hits: u64,
}

#[derive(Default)]
struct PhysicalCostRegistry {
    identity: Option<crate::engine::kv::telemetry::PhysicalTelemetryIdentity>,
    capabilities: crate::engine::kv::telemetry::PhysicalTelemetryCapabilities,
    previous: Option<crate::engine::kv::telemetry::PhysicalTelemetrySnapshot>,
    requests: BTreeMap<
        (
            crate::engine::kv::telemetry::PhysicalRequestClass,
            crate::engine::kv::telemetry::PhysicalRequestCondition,
        ),
        PhysicalRequestCalibration,
    >,
    caches: BTreeMap<crate::engine::kv::telemetry::PhysicalCacheTier, PhysicalCacheCalibration>,
}

impl PhysicalCostRegistry {
    fn observe(
        &mut self,
        snapshot: crate::engine::kv::telemetry::PhysicalTelemetrySnapshot,
    ) -> bool {
        if self.identity.as_ref() != Some(&snapshot.identity) {
            self.identity = Some(snapshot.identity.clone());
            self.capabilities = snapshot.capabilities;
            self.previous = Some(snapshot);
            self.requests.clear();
            self.caches.clear();
            return false;
        }
        let Some(previous) = self.previous.take() else {
            self.previous = Some(snapshot);
            return false;
        };
        let mut next_previous = snapshot.clone();
        self.capabilities = snapshot.capabilities;
        let mut changed = false;
        for current in &snapshot.requests {
            let earlier = previous.requests.iter().find(|request| {
                request.class == current.class && request.condition == current.condition
            });
            let requests = current
                .requests
                .saturating_sub(earlier.map_or(0, |request| request.requests));
            let errors = current
                .errors
                .saturating_sub(earlier.map_or(0, |request| request.errors));
            let calibration = self
                .requests
                .entry((current.class, current.condition))
                .or_default();
            calibration.observed_requests = calibration.observed_requests.saturating_add(requests);
            calibration.errors = calibration.errors.saturating_add(errors);
            let next = next_previous
                .requests
                .iter_mut()
                .find(|request| {
                    request.class == current.class && request.condition == current.condition
                })
                .expect("request copied from the current snapshot");
            observe_histogram(
                &mut calibration.latency_micros,
                &mut next.latency_micros,
                current.latency_micros.as_ref(),
                earlier.and_then(|request| request.latency_micros.as_ref()),
                earlier.is_none(),
            );
            observe_histogram(
                &mut calibration.bytes,
                &mut next.bytes,
                current.bytes.as_ref(),
                earlier.and_then(|request| request.bytes.as_ref()),
                earlier.is_none(),
            );
            changed |= requests > 0 || errors > 0;
        }
        for current in &snapshot.caches {
            let earlier = previous
                .caches
                .iter()
                .find(|cache| cache.tier == current.tier);
            let accesses = current
                .accesses
                .saturating_sub(earlier.map_or(0, |cache| cache.accesses));
            let hits = current
                .hits
                .saturating_sub(earlier.map_or(0, |cache| cache.hits));
            let calibration = self.caches.entry(current.tier).or_default();
            calibration.accesses = calibration.accesses.saturating_add(accesses);
            calibration.hits = calibration.hits.saturating_add(hits);
            changed |= accesses > 0 || hits > 0;
        }
        self.previous = Some(next_previous);
        changed
    }

    fn model(&self) -> Option<crate::engine::planner::models::PhysicalCostModel> {
        let identity = self.identity.clone()?;
        let requests = self
            .requests
            .iter()
            .filter(|(_, calibration)| calibration.observed_requests > 0)
            .map(|((class, condition), calibration)| {
                crate::engine::planner::models::PhysicalRequestCost {
                    class: *class,
                    size_upper_bound: condition.size_upper_bound,
                    concurrency_upper_bound: condition.concurrency_upper_bound,
                    service_tier: condition.service_tier,
                    observed_requests: calibration.observed_requests,
                    errors: calibration.errors.min(calibration.observed_requests),
                    latency_micros: crate::engine::planner::models::PhysicalCostMetric::of(
                        &calibration.latency_micros,
                    ),
                    bytes: crate::engine::planner::models::PhysicalCostMetric::of(
                        &calibration.bytes,
                    ),
                }
            })
            .collect::<Vec<_>>();
        let caches = self
            .caches
            .iter()
            .filter(|(_, calibration)| calibration.accesses > 0)
            .map(|(tier, calibration)| {
                let hits = calibration.hits.min(calibration.accesses);
                crate::engine::planner::models::PhysicalCacheCost {
                    tier: *tier,
                    accesses: calibration.accesses,
                    hits,
                    hit_rate_ppm: hits.saturating_mul(1_000_000) / calibration.accesses.max(1),
                }
            })
            .collect::<Vec<_>>();
        (!requests.is_empty() || !caches.is_empty()).then_some(
            crate::engine::planner::models::PhysicalCostModel {
                basis: "backend_physical_telemetry",
                backend: identity.backend,
                telemetry_format: identity.format,
                capabilities: self.capabilities,
                requests,
                caches,
            },
        )
    }
}

fn observe_histogram(
    output: &mut Log2Histogram,
    next: &mut Option<crate::engine::kv::telemetry::CumulativeHistogram>,
    current: Option<&crate::engine::kv::telemetry::CumulativeHistogram>,
    previous: Option<&crate::engine::kv::telemetry::CumulativeHistogram>,
    request_is_new: bool,
) {
    let Some(current) = current.filter(|histogram| histogram_is_valid(histogram)) else {
        *next = previous.cloned();
        return;
    };
    if let Some(previous) = previous.filter(|histogram| histogram_is_valid(histogram)) {
        if previous.boundaries == current.boundaries
            && previous.bucket_counts.len() == current.bucket_counts.len()
        {
            record_histogram_delta(output, current, Some(previous));
        }
    } else if request_is_new {
        record_histogram_delta(output, current, None);
    }
}

fn histogram_is_valid(histogram: &crate::engine::kv::telemetry::CumulativeHistogram) -> bool {
    histogram.bucket_counts.len() == histogram.boundaries.len().saturating_add(1)
        && histogram
            .bucket_counts
            .iter()
            .fold(0u64, |sum, count| sum.saturating_add(*count))
            == histogram.count
}

fn record_histogram_delta(
    output: &mut Log2Histogram,
    current: &crate::engine::kv::telemetry::CumulativeHistogram,
    previous: Option<&crate::engine::kv::telemetry::CumulativeHistogram>,
) {
    for (index, count) in current.bucket_counts.iter().enumerate() {
        let earlier = previous.map_or(0, |previous| previous.bucket_counts[index]);
        let delta = count.saturating_sub(earlier);
        if delta == 0 {
            continue;
        }
        let upper_bound = current
            .boundaries
            .get(index)
            .copied()
            .unwrap_or(current.maximum);
        output.record_many(upper_bound, delta);
    }
}

fn decay_frequency(count: u32, epochs: u64) -> u32 {
    u32::try_from(epochs)
        .ok()
        .and_then(|epochs| count.checked_shr(epochs))
        .unwrap_or(0)
}

pub enum StatisticsEvent {
    Statement(Box<StatementObservation>),
    Program(ProgramRecord),
}

pub struct StatisticsCollector {
    sender: mpsc::Sender<StatisticsEvent>,
    dropped: AtomicU64,
    corpus_captured: AtomicU64,
    corpus_skipped_oversize: AtomicU64,
    corpus_dropped_queue: AtomicU64,
    capture_programs: bool,
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
struct CollectorReport {
    corpus_enabled: bool,
    dropped: u64,
    corpus_captured: u64,
    corpus_skipped_oversize: u64,
    corpus_dropped_queue: u64,
}

impl StatisticsCollector {
    /// Returns the collector to install on the engine and the receiver the
    /// registry runner drains.
    pub fn channel(capacity: usize) -> (Arc<Self>, mpsc::Receiver<StatisticsEvent>) {
        Self::channel_with_programs(capacity, false)
    }

    fn channel_with_programs(
        capacity: usize,
        capture_programs: bool,
    ) -> (Arc<Self>, mpsc::Receiver<StatisticsEvent>) {
        let (sender, receiver) = mpsc::channel(capacity);
        let collector = Arc::new(Self {
            sender,
            dropped: AtomicU64::new(0),
            corpus_captured: AtomicU64::new(0),
            corpus_skipped_oversize: AtomicU64::new(0),
            corpus_dropped_queue: AtomicU64::new(0),
            capture_programs,
        });
        (collector, receiver)
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn report(&self) -> CollectorReport {
        CollectorReport {
            corpus_enabled: self.capture_programs,
            dropped: self.dropped.load(Ordering::Relaxed),
            corpus_captured: self.corpus_captured.load(Ordering::Relaxed),
            corpus_skipped_oversize: self.corpus_skipped_oversize.load(Ordering::Relaxed),
            corpus_dropped_queue: self.corpus_dropped_queue.load(Ordering::Relaxed),
        }
    }
}

impl ExecutionObserver for StatisticsCollector {
    fn statement(&self, observation: StatementObservation) {
        if self
            .sender
            .try_send(StatisticsEvent::Statement(Box::new(observation)))
            .is_err()
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn captures_programs(&self) -> bool {
        self.capture_programs
    }

    fn program(&self, record: ProgramRecord) {
        if !self.capture_programs {
            return;
        }
        if record.canonical.len() > CORPUS_MAX_PROGRAM_BYTES {
            self.corpus_skipped_oversize.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self
            .sender
            .try_send(StatisticsEvent::Program(record))
            .is_err()
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            self.corpus_dropped_queue.fetch_add(1, Ordering::Relaxed);
        } else {
            self.corpus_captured.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn program_skipped_oversize(&self) {
        if self.capture_programs {
            self.corpus_skipped_oversize.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Accumulated evidence for one observed family, in the form the registry
/// builds and stores. Distilling to a [`FeedbackModel`] discards the
/// distributions, so this is what persists: an instance loads its own models
/// back and keeps extending them instead of starting over.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct QueryModel {
    pub family: Fingerprint,
    pub kind: ObservationModelKind,
    pub executions: u64,
    /// Wall time since the Unix epoch.
    pub last_seen: Duration,
    pub rows: Log2Histogram,
    pub execute_micros: Log2Histogram,
    pub bind_micros: Log2Histogram,
    #[serde(
        default,
        skip_serializing_if = "crate::engine::planner::models::KvResourceDistribution::is_empty"
    )]
    pub resources: crate::engine::planner::models::KvResourceDistribution,
    pub duration_ewma_micros: f64,
    /// Distinct plan fingerprints observed for this family, with counts.
    pub plans: Vec<(Fingerprint, u64)>,
    #[serde(default)]
    pub plan_profiles: Vec<PlanProfile>,
    pub exact_variants: FingerprintCardinalitySketch,
    pub estimated_executions: u64,
    pub q_errors: Log2Histogram,
    pub q_error_max_x100: u64,
    pub stamp: crate::engine::planner::models::DependencyStamp,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct ModelId {
    kind: ObservationModelKind,
    family: Fingerprint,
}

impl ModelId {
    fn new(kind: ObservationModelKind, family: Fingerprint) -> Self {
        Self { kind, family }
    }

    fn of(model: &QueryModel) -> Self {
        Self::new(model.kind, model.family)
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct StoredModel {
    format: u32,
    model: QueryModel,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct StoredFrequency {
    format: u32,
    record: StoredFrequencyRecord,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct StoredFrequencyRecord {
    count: u32,
    epoch: u64,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct ModelCatalog {
    format: u32,
    entries: Vec<ModelCatalogEntry>,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct ModelCatalogEntry {
    family: Fingerprint,
    kind: ObservationModelKind,
    executions: u64,
    last_seen_unix_micros: u64,
}

impl ModelCatalogEntry {
    fn of(model: &QueryModel) -> Self {
        Self {
            family: model.family,
            kind: model.kind,
            executions: model.executions,
            last_seen_unix_micros: model.last_seen.as_micros().min(u128::from(u64::MAX)) as u64,
        }
    }

    fn id(&self) -> ModelId {
        ModelId::new(self.kind, self.family)
    }
}

fn model_retention_rank(
    entry: &ModelCatalogEntry,
    newest_unix_micros: u64,
) -> (u64, u64, u8, u64, ModelId) {
    let age_epochs =
        newest_unix_micros.saturating_sub(entry.last_seen_unix_micros) / RETENTION_EPOCH_MICROS;
    let score = entry.executions >> age_epochs.min(63);
    let relation_priority = u8::from(entry.kind == ObservationModelKind::Relation);
    (
        score,
        entry.last_seen_unix_micros,
        relation_priority,
        entry.executions,
        entry.id(),
    )
}

fn decode_stored_model(
    value: &[u8],
    family: Fingerprint,
    kind: ObservationModelKind,
) -> Option<QueryModel> {
    let stored = serde_json::from_slice::<StoredModel>(value).ok()?;
    (stored.format == STATISTICS_MODEL_FORMAT
        && stored.model.family == family
        && stored.model.kind == kind)
        .then_some(stored.model)
}

fn decode_model_catalog(value: &[u8]) -> Option<ModelCatalog> {
    let catalog = serde_json::from_slice::<ModelCatalog>(value).ok()?;
    (catalog.format == STATISTICS_MODEL_CATALOG_FORMAT).then_some(catalog)
}

fn decode_stored_frequency(value: &[u8], current_epoch: u64) -> Option<u32> {
    let stored = serde_json::from_slice::<StoredFrequency>(value).ok()?;
    (stored.format == STATISTICS_FREQUENCY_FORMAT && stored.record.epoch <= current_epoch)
        .then(|| decay_frequency(stored.record.count, current_epoch - stored.record.epoch))
}

fn model_storage_key(id: ModelId) -> Vec<u8> {
    use crate::engine::kv::keys;

    match id.kind {
        ObservationModelKind::Relation => {
            keys::statistics_relation_model_key(&id.family.to_bytes())
        }
        ObservationModelKind::Statement => keys::statistics_model_key(&id.family.to_bytes()),
    }
}

impl QueryModel {
    pub(super) fn new(family: Fingerprint, kind: ObservationModelKind) -> Self {
        Self {
            family,
            kind,
            executions: 0,
            last_seen: Duration::ZERO,
            rows: Log2Histogram::default(),
            execute_micros: Log2Histogram::default(),
            bind_micros: Log2Histogram::default(),
            resources: Default::default(),
            duration_ewma_micros: 0.0,
            plans: Vec::new(),
            plan_profiles: Vec::new(),
            exact_variants: FingerprintCardinalitySketch::default(),
            estimated_executions: 0,
            q_errors: Log2Histogram::default(),
            q_error_max_x100: 0,
            stamp: crate::engine::planner::models::DependencyStamp::default(),
        }
    }

    /// Reset when the stamp moved: the shape is unchanged but what it denotes
    /// is not, so blending the two populations would describe neither.
    fn restart_if_redefined(&mut self, stamp: crate::engine::planner::models::DependencyStamp) {
        if self.executions > 0 && self.stamp.semantic != stamp.semantic {
            *self = Self::new(self.family, self.kind);
        }
        self.stamp = stamp;
    }

    fn record_statement(&mut self, observation: &StatementObservation, at: Duration) {
        self.restart_if_redefined(observation.stamp);
        self.executions += 1;
        self.last_seen = at;
        self.rows.record(observation.rows);
        let execute_micros = observation.phase.execute.as_micros() as u64;
        self.execute_micros.record(execute_micros);
        self.bind_micros
            .record(observation.phase.bind.as_micros() as u64);
        let resources = crate::engine::planner::models::KvResourceSample {
            gets: observation.kv.gets,
            puts: observation.kv.puts,
            deletes: observation.kv.deletes,
            scans: observation.kv.scans,
            iterated: observation.kv.iterated,
            bytes_read: observation.kv.bytes_read,
            bytes_written: observation.kv.bytes_written,
        };
        self.resources.record(resources);
        self.duration_ewma_micros = if self.executions == 1 {
            execute_micros as f64
        } else {
            EWMA_ALPHA * execute_micros as f64 + (1.0 - EWMA_ALPHA) * self.duration_ewma_micros
        };
        if let Some(plan) = observation.plan {
            self.record_plan(
                plan,
                observation.stamp.access,
                observation.rows,
                execute_micros,
                resources,
            );
        }
        if let Some(estimate) = &observation.estimate {
            self.record_estimate(estimate.cardinality, observation.rows);
        }
        self.exact_variants.record(&observation.query.exact);
    }

    fn record_relation(
        &mut self,
        relation: &crate::engine::exec::observe::RelationObservation,
        stamp: crate::engine::planner::models::DependencyStamp,
        at: Duration,
    ) {
        self.restart_if_redefined(stamp);
        self.executions += 1;
        self.last_seen = at;
        self.rows.record(relation.rows);
        if let Some(estimate) = &relation.estimate {
            self.record_estimate(estimate.cardinality, relation.rows);
        }
    }

    fn record_plan(
        &mut self,
        plan: Fingerprint,
        access_stamp: u64,
        rows: u64,
        execute_micros: u64,
        resources: crate::engine::planner::models::KvResourceSample,
    ) {
        if let Some((_, count)) = self.plans.iter_mut().find(|(known, _)| *known == plan) {
            *count = count.saturating_add(1);
        } else if self.plans.len() < MAX_PLANS_PER_MODEL {
            self.plans.push((plan, 1));
        }
        if let Some(profile) = self
            .plan_profiles
            .iter_mut()
            .find(|known| known.plan == plan && known.access_stamp == access_stamp)
        {
            profile.record_with_resources(rows, execute_micros, resources);
        } else if self.plan_profiles.len() < MAX_PLANS_PER_MODEL {
            let mut profile = PlanProfile::new(plan, access_stamp);
            profile.record_with_resources(rows, execute_micros, resources);
            self.plan_profiles.push(profile);
        }
    }

    fn record_estimate(&mut self, estimated: u64, actual: u64) {
        let q = crate::engine::planner::models::q_error_x100(estimated, actual);
        self.estimated_executions += 1;
        self.q_errors.record(q);
        self.q_error_max_x100 = self.q_error_max_x100.max(q);
    }

    /// Fold newer evidence for the same family into this model.
    ///
    /// `incoming` is treated as the later of the two, which decides the fields
    /// that cannot be added. A moved semantic stamp discards what came before
    /// rather than blending it, exactly as observing the change would.
    ///
    /// `duration_ewma_micros` cannot be combined. The value with the latest
    /// wall time wins. An equal-time tie uses the larger value.
    pub fn merge(&mut self, incoming: &QueryModel) {
        if self.kind != incoming.kind {
            return;
        }
        if self.executions > 0 && self.stamp.semantic != incoming.stamp.semantic {
            let family = self.family;
            *self = incoming.clone();
            self.family = family;
            return;
        }
        let replace_ewma = incoming.last_seen > self.last_seen
            || (incoming.last_seen == self.last_seen
                && incoming.duration_ewma_micros > self.duration_ewma_micros);
        self.stamp = incoming.stamp;
        self.executions += incoming.executions;
        self.last_seen = self.last_seen.max(incoming.last_seen);
        self.rows.merge(&incoming.rows);
        self.execute_micros.merge(&incoming.execute_micros);
        self.bind_micros.merge(&incoming.bind_micros);
        self.resources.merge(&incoming.resources);
        self.q_errors.merge(&incoming.q_errors);
        self.q_error_max_x100 = self.q_error_max_x100.max(incoming.q_error_max_x100);
        self.estimated_executions += incoming.estimated_executions;
        self.exact_variants.merge(&incoming.exact_variants);
        if incoming.duration_ewma_micros > 0.0 && replace_ewma {
            self.duration_ewma_micros = incoming.duration_ewma_micros;
        }
        for (plan, count) in &incoming.plans {
            if let Some((_, known)) = self.plans.iter_mut().find(|(seen, _)| seen == plan) {
                *known = known.saturating_add(*count);
            } else if self.plans.len() < MAX_PLANS_PER_MODEL {
                self.plans.push((*plan, *count));
            }
        }
        for incoming in &incoming.plan_profiles {
            if let Some(profile) = self.plan_profiles.iter_mut().find(|known| {
                known.plan == incoming.plan && known.access_stamp == incoming.access_stamp
            }) {
                profile.merge(incoming);
            } else if self.plan_profiles.len() < MAX_PLANS_PER_MODEL {
                self.plan_profiles.push(incoming.clone());
            }
        }
    }
}

const MAX_PLANS_PER_MODEL: usize = 8;
const EWMA_ALPHA: f64 = 0.1;

/// An observation updates two models: the cumulative one this instance plans
/// from, and the delta that will be published. They see the same recording
/// calls, so a field cannot be accumulated in one and forgotten in the other.
struct ObservedPair<'a> {
    cumulative: &'a mut QueryModel,
    delta: &'a mut QueryModel,
}

impl ObservedPair<'_> {
    fn record_statement(&mut self, observation: &StatementObservation, at: Duration) {
        self.cumulative.record_statement(observation, at);
        self.delta.record_statement(observation, at);
    }

    fn record_relation(
        &mut self,
        relation: &crate::engine::exec::observe::RelationObservation,
        stamp: crate::engine::planner::models::DependencyStamp,
        at: Duration,
    ) {
        self.cumulative.record_relation(relation, stamp, at);
        self.delta.record_relation(relation, stamp, at);
    }
}

pub struct HotRegistry {
    sketch: FrequencySketch,
    models: HashMap<ModelId, QueryModel>,
    capacity: usize,
    absorbed: u64,
    evicted: u64,
    /// Un-flushed per-table modification deltas, keyed by logical identity.
    table_change_deltas: HashMap<crate::engine::catalog::identity::SchemaId, u64>,
    survey_changes: HashMap<crate::engine::catalog::identity::SchemaId, u64>,
    /// Un-flushed workload corpus records, bounded; overflow drops the
    /// oldest first (the content store is what matters long-term and hot
    /// documents reappear immediately).
    pending_programs: Vec<ProgramRecord>,
    /// Latest survey results, replacing wholesale each survey pass.
    synopses: HashMap<crate::engine::catalog::identity::SchemaId, SynopsisModel>,
    synopses_dirty: bool,
    /// Evidence gathered since the last successful publication, as a delta per
    /// family. Publishing sends deltas rather than whole models, so whoever
    /// receives them merges rather than replaces and never has to know what
    /// this instance holds in memory.
    pending: HashMap<ModelId, QueryModel>,
    /// Appearances counted since the last publication. The sketch itself
    /// cannot be published: reading a count back out of it needs the
    /// fingerprint, so what travels is the counts, not the sketch.
    pending_frequency: HashMap<Fingerprint, u32>,
    frequency_epoch: u64,
    pending_frequency_decays: u64,
    /// Unpublished deltas dropped to stay within capacity.
    shed: u64,
    corpus_shed: u64,
    /// Observations folded in from another instance.
    relayed: u64,
    /// Corpus documents adopted from another instance.
    relayed_corpus: u64,
    /// Relayed families discarded because they describe a different catalog
    /// generation than the one this instance observed.
    relayed_stale: u64,
    /// Latest semantic stamp seen for recently tracked families. This survives
    /// model eviction so stale relayed evidence cannot recreate an old model.
    semantic_stamps: HashMap<Fingerprint, u64>,
}

const MAX_PENDING_PROGRAMS: usize = 1024;

impl HotRegistry {
    pub fn new(capacity: usize) -> Self {
        Self {
            sketch: FrequencySketch::new(capacity * 4),
            models: HashMap::new(),
            capacity: capacity.max(16),
            absorbed: 0,
            evicted: 0,
            table_change_deltas: HashMap::new(),
            survey_changes: HashMap::new(),
            pending_programs: Vec::new(),
            synopses: HashMap::new(),
            synopses_dirty: false,
            pending: HashMap::new(),
            pending_frequency: HashMap::new(),
            frequency_epoch: 0,
            pending_frequency_decays: 0,
            shed: 0,
            corpus_shed: 0,
            relayed: 0,
            relayed_corpus: 0,
            relayed_stale: 0,
            semantic_stamps: HashMap::new(),
        }
    }

    /// Count one appearance, both in the sketch this instance plans from and
    /// in the counts awaiting publication.
    fn count_appearance(&mut self, family: &Fingerprint) {
        self.sketch.record(family);
        self.add_pending_frequency(*family, 1);
    }

    fn add_pending_frequency(&mut self, family: Fingerprint, count: u32) {
        if let Some(pending) = self.pending_frequency.get_mut(&family) {
            *pending = pending.saturating_add(count);
        } else if self.pending_frequency.len() < self.capacity * 4 {
            self.pending_frequency.insert(family, count);
        }
    }

    /// Fold evidence gathered by another instance into this one's, and into
    /// the delta awaiting publication so it reaches the store too.
    ///
    /// `age` is how long before the batch was sent each family was last
    /// observed. The writer subtracts it from its wall time so clock-offset
    /// differences do not become persisted model age.
    ///
    /// A family whose semantic stamp differs from the resident model's is
    /// dropped rather than merged, and this is the asymmetry that matters:
    /// observing a redefinition restarts a model, but a *remote* instance
    /// lagging a catalog change must never restart evidence gathered here.
    pub fn merge_relayed(
        &mut self,
        family: Fingerprint,
        incoming: &QueryModel,
        age: Duration,
        now: Duration,
    ) {
        if self
            .semantic_stamps
            .get(&family)
            .is_some_and(|semantic| *semantic != incoming.stamp.semantic)
        {
            self.relayed_stale += 1;
            return;
        }
        let mut rebased = incoming.clone();
        rebased.last_seen = now.saturating_sub(age);
        self.relayed += incoming.executions;
        self.record_semantic_stamp(family, incoming.stamp.semantic);
        let pair = self.admit(family, incoming.kind);
        pair.cumulative.merge(&rebased);
        pair.delta.merge(&rebased);
    }

    /// Fold appearance counts observed elsewhere into the frequency sketch,
    /// and into the counts awaiting publication so they travel onwards.
    pub fn merge_relayed_frequency(&mut self, family: Fingerprint, count: u32) {
        self.sketch.record_many(&family, count);
        self.add_pending_frequency(family, count);
    }

    /// Adopt previously stored models as the base this process extends, so a
    /// restart plans from accumulated history instead of from nothing.
    ///
    /// Only the cumulative models are seeded. Nothing enters the pending
    /// deltas, or a flush would send history that already reached the store
    /// and count it twice.
    pub fn hydrate(&mut self, models: Vec<QueryModel>) {
        for model in models {
            let id = ModelId::of(&model);
            if !self.models.contains_key(&id) && self.models.len() >= self.capacity {
                break;
            }
            let family = model.family;
            self.record_semantic_stamp(family, model.stamp.semantic);
            if let Some(local) = self.models.get_mut(&id) {
                let mut merged = model;
                merged.merge(local);
                *local = merged;
            } else {
                self.models.insert(id, model);
            }
        }
    }

    pub fn hydrate_frequency(&mut self, frequencies: Vec<(Fingerprint, u32)>) {
        for (family, count) in frequencies {
            self.sketch.record_many(&family, count);
        }
    }

    pub fn set_synopses(&mut self, models: Vec<SynopsisModel>) {
        self.synopses = models
            .into_iter()
            .map(|model| (model.table, model))
            .collect();
        self.synopses_dirty = true;
    }

    fn hydrate_synopses(&mut self, mut models: Vec<SynopsisModel>) {
        for model in &mut models {
            model.changes_since_collection = model
                .changes_since_collection
                .saturating_add(self.survey_changes.get(&model.table).copied().unwrap_or(0));
        }
        self.synopses = models
            .into_iter()
            .map(|model| (model.table, model))
            .collect();
        self.synopses_dirty = !self.survey_changes.is_empty();
    }

    fn set_synopsis(&mut self, mut model: SynopsisModel, covered_changes: u64) {
        let table = model.table;
        if let Some(changes) = self.survey_changes.get_mut(&table) {
            *changes = changes.saturating_sub(covered_changes);
            model.changes_since_collection = *changes;
            if *changes == 0 {
                self.survey_changes.remove(&table);
            }
        }
        self.synopses.insert(table, model);
        self.synopses_dirty = true;
    }

    fn survey_request(&self, config: &StatisticsConfig, now: Duration) -> SurveyRequest {
        SurveyRequest {
            changed: self.survey_changes.clone(),
            known: self
                .synopses
                .iter()
                .map(|(table, model)| (*table, model.collected_at_unix_micros))
                .collect(),
            change_threshold: config.survey_change_threshold,
            max_age_micros: config.survey_max_age.as_micros().min(u128::from(u64::MAX)) as u64,
            now_micros: now.as_micros().min(u128::from(u64::MAX)) as u64,
            row_budget: config.survey_row_budget,
            byte_budget: config.survey_byte_budget,
        }
    }

    /// Drain the accumulated per-table modification deltas for flushing.
    pub fn take_table_change_deltas(
        &mut self,
    ) -> HashMap<crate::engine::catalog::identity::SchemaId, u64> {
        std::mem::take(&mut self.table_change_deltas)
    }

    /// Adopt a document another instance captured.
    ///
    /// The timestamp is that instance's wall clock, which orders the execution
    /// log. Wall clocks across a cluster agree to within skew rather than
    /// exactly, so the log's order is approximate between instances and exact
    /// within one.
    pub fn absorb_relayed_program(&mut self, record: ProgramRecord) {
        self.relayed_corpus += 1;
        self.absorb_program(record);
    }

    pub fn absorb_program(&mut self, record: ProgramRecord) {
        if self.pending_programs.len() >= MAX_PENDING_PROGRAMS {
            self.pending_programs.remove(0);
            self.corpus_shed += 1;
        }
        self.pending_programs.push(record);
    }

    pub fn absorb(&mut self, observation: &StatementObservation, at: Duration) {
        self.absorbed += 1;
        if let Some(mutated) = observation.mutated
            && observation.affected > 0
        {
            *self.table_change_deltas.entry(mutated).or_insert(0) += observation.affected;
            *self.survey_changes.entry(mutated).or_insert(0) += observation.affected;
            if let Some(synopsis) = self.synopses.get_mut(&mutated) {
                synopsis.changes_since_collection = synopsis
                    .changes_since_collection
                    .saturating_add(observation.affected);
                self.synopses_dirty = true;
            }
        }
        for subtree in &observation.query.subtrees {
            self.count_appearance(&subtree.family);
        }
        if !observation
            .query
            .subtrees
            .iter()
            .any(|subtree| subtree.family == observation.query.family)
        {
            self.count_appearance(&observation.query.family);
        }

        for relation in &observation.relations {
            self.absorb_relation(relation, observation.stamp, at);
        }

        let family = observation.query.family;
        self.record_semantic_stamp(family, observation.stamp.semantic);
        self.admit(family, ObservationModelKind::Statement)
            .record_statement(observation, at);
    }

    /// Fold one relation's actual cardinality into the model for its own
    /// family, so a relation measured inside one statement informs estimates
    /// for every other statement containing it.
    fn absorb_relation(
        &mut self,
        relation: &crate::engine::exec::observe::RelationObservation,
        stamp: crate::engine::planner::models::DependencyStamp,
        at: Duration,
    ) {
        let family = relation.family;
        self.record_semantic_stamp(family, stamp.semantic);
        self.admit(family, ObservationModelKind::Relation)
            .record_relation(relation, stamp, at);
    }

    /// Make room for a family and return both of the models an observation
    /// updates: the cumulative one this instance plans from, and the delta
    /// awaiting publication.
    fn admit(&mut self, family: Fingerprint, kind: ObservationModelKind) -> ObservedPair<'_> {
        let id = ModelId::new(kind, family);
        if !self.models.contains_key(&id) && self.models.len() >= self.capacity {
            self.evict_coldest();
        }
        if !self.pending.contains_key(&id) && self.pending.len() >= self.capacity {
            self.shed_coldest_pending();
        }
        let cumulative = self
            .models
            .entry(id)
            .or_insert_with(|| QueryModel::new(family, kind));
        let delta = self
            .pending
            .entry(id)
            .or_insert_with(|| QueryModel::new(family, kind));
        ObservedPair { cumulative, delta }
    }

    fn record_semantic_stamp(&mut self, family: Fingerprint, semantic: u64) {
        let limit = self.capacity.saturating_mul(4);
        if !self.semantic_stamps.contains_key(&family) && self.semantic_stamps.len() >= limit {
            let coldest = self
                .semantic_stamps
                .keys()
                .min_by_key(|known| (self.sketch.estimate(known), **known))
                .copied();
            if let Some(coldest) = coldest {
                self.semantic_stamps.remove(&coldest);
            }
        }
        self.semantic_stamps.insert(family, semantic);
    }

    /// An unpublished delta is bounded like the models are. Dropping the
    /// coldest loses the least valuable evidence, and the store keeps whatever
    /// earlier deltas already reached it.
    fn shed_coldest_pending(&mut self) {
        let coldest = self
            .pending
            .iter()
            .min_by_key(|(id, model)| (self.sketch.estimate(&id.family), model.executions, **id))
            .map(|(id, _)| *id);
        if let Some(id) = coldest {
            self.pending.remove(&id);
            self.shed += 1;
        }
    }

    /// Evicting discards a family's accumulated distributions. If it is
    /// observed again it starts over, and the next flush replaces what was
    /// stored for it. That is the price of a bounded hot set: evidence is kept
    /// for the working set, not for every family ever seen.
    fn evict_coldest(&mut self) {
        let coldest = self
            .models
            .iter()
            .min_by_key(|(id, model)| (self.sketch.estimate(&id.family), model.executions, **id))
            .map(|(id, _)| *id);
        if let Some(id) = coldest {
            self.models.remove(&id);
            self.evicted += 1;
        }
    }

    pub fn frequency(&self, fingerprint: &Fingerprint) -> u32 {
        self.sketch.estimate(fingerprint)
    }

    pub fn model(&self, kind: ObservationModelKind, family: &Fingerprint) -> Option<&QueryModel> {
        self.models.get(&ModelId::new(kind, *family))
    }

    pub fn models(&self) -> impl Iterator<Item = &QueryModel> {
        self.models.values()
    }

    pub fn len(&self) -> usize {
        self.models.len()
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    pub fn absorbed(&self) -> u64 {
        self.absorbed
    }

    pub fn evicted(&self) -> u64 {
        self.evicted
    }

    pub fn decay(&mut self) {
        self.sketch.decay();
        self.pending_frequency
            .values_mut()
            .for_each(|count| *count >>= 1);
        self.pending_frequency.retain(|_, count| *count > 0);
        self.frequency_epoch = self.frequency_epoch.saturating_add(1);
        self.pending_frequency_decays = self.pending_frequency_decays.saturating_add(1);
    }
}

fn feedback_from_model(model: &QueryModel) -> FeedbackModel {
    FeedbackModel {
        family: model.family,
        retained_executions: model.executions,
        exact_variants: model.exact_variants.estimate(),
        last_seen: model.last_seen,
        rows_p50_upper_bound: model.rows.quantile_upper_bound(0.5),
        rows_p95_upper_bound: model.rows.quantile_upper_bound(0.95),
        rows_max: model.rows.maximum(),
        execute_micros_p50_upper_bound: model.execute_micros.quantile_upper_bound(0.5),
        execute_micros_p95_upper_bound: model.execute_micros.quantile_upper_bound(0.95),
        duration_ewma_micros: model.duration_ewma_micros,
        plans: model.plans.clone(),
        plan_profiles: model.plan_profiles.clone(),
        resources: model.resources.clone(),
        stamp: model.stamp,
        executions_with_estimate: model.estimated_executions,
        q_error_p50_upper_bound_x100: model.q_errors.quantile_upper_bound(0.5),
        q_error_p95_upper_bound_x100: model.q_errors.quantile_upper_bound(0.95),
        q_error_max_x100: model.q_error_max_x100,
    }
}

fn distill_with_persisted(
    registry: &HotRegistry,
    persisted: &[QueryModel],
    persisted_frequency: &[(Fingerprint, u32)],
    physical_cost: Option<crate::engine::planner::models::PhysicalCostModel>,
    collector: CollectorReport,
    corpus_maintenance: Option<CorpusMaintenanceStats>,
    published_at: Duration,
) -> PlannerStats {
    let mut stats = registry.distill(collector, published_at);
    stats.physical_cost = physical_cost;
    stats.corpus.maintenance = corpus_maintenance;
    for model in persisted {
        let models = match model.kind {
            ObservationModelKind::Relation => &mut stats.feedback_models,
            ObservationModelKind::Statement => &mut stats.statement_models,
        };
        models.insert(model.family, feedback_from_model(model));
    }
    for (family, count) in persisted_frequency {
        stats.workload_frequency.record_at_least(family, *count);
    }
    stats.snapshot_identity = planner_statistics_identity(&stats);
    stats
}

fn distill_persisted_only(
    persisted: &PersistedStatisticsSnapshot,
    published_at: Duration,
) -> PlannerStats {
    let mut stats = PlannerStats::empty();
    stats.snapshot_identity = persisted.identity.clone();
    stats.scope = crate::engine::planner::models::StatisticsScope::Persisted;
    stats.published_at = published_at;
    stats.synopsis_models = persisted
        .synopses
        .iter()
        .cloned()
        .map(|model| (model.table, model))
        .collect();
    for model in &persisted.models {
        let models = match model.kind {
            ObservationModelKind::Relation => &mut stats.feedback_models,
            ObservationModelKind::Statement => &mut stats.statement_models,
        };
        models.insert(model.family, feedback_from_model(model));
    }
    for (family, count) in &persisted.frequencies {
        stats.workload_frequency.record_at_least(family, *count);
    }
    stats
}

#[allow(clippy::too_many_arguments)]
fn runner_snapshots(
    registry: &HotRegistry,
    persisted: &PersistedStatisticsSnapshot,
    owns_stored_models: bool,
    physical_cost: Option<crate::engine::planner::models::PhysicalCostModel>,
    collector: CollectorReport,
    corpus_maintenance: Option<CorpusMaintenanceStats>,
    published_at: Duration,
) -> (Arc<PlannerStats>, Arc<PlannerStats>) {
    let mut diagnostic = distill_with_persisted(
        registry,
        &persisted.models,
        &persisted.frequencies,
        physical_cost,
        collector,
        corpus_maintenance,
        published_at,
    );
    if owns_stored_models {
        diagnostic.scope = crate::engine::planner::models::StatisticsScope::WriterLive;
        let planning = Arc::new(diagnostic.clone());
        (planning, Arc::new(diagnostic))
    } else {
        diagnostic.scope = crate::engine::planner::models::StatisticsScope::ReaderDiagnostic;
        (
            Arc::new(distill_persisted_only(persisted, published_at)),
            Arc::new(diagnostic),
        )
    }
}

impl HotRegistry {
    fn distill(&self, collector: CollectorReport, published_at: Duration) -> PlannerStats {
        let mut stats = PlannerStats {
            snapshot_identity: String::new(),
            scope: crate::engine::planner::models::StatisticsScope::WriterLive,
            feedback_models: self
                .models
                .iter()
                .filter(|(_, model)| model.kind == ObservationModelKind::Relation)
                .map(|(id, model)| (id.family, feedback_from_model(model)))
                .collect(),
            statement_models: self
                .models
                .iter()
                .filter(|(_, model)| model.kind == ObservationModelKind::Statement)
                .map(|(id, model)| (id.family, feedback_from_model(model)))
                .collect(),
            synopsis_models: self.synopses.clone(),
            workload_frequency: self.sketch.clone(),
            corpus: CorpusCaptureStats {
                enabled: collector.corpus_enabled,
                captured: collector.corpus_captured,
                skipped_oversize: collector.corpus_skipped_oversize,
                dropped_queue: collector.corpus_dropped_queue,
                shed_pending: self.corpus_shed,
                maintenance: None,
            },
            physical_cost: None,
            absorbed: self.absorbed,
            dropped: collector.dropped,
            shed: self.shed,
            evicted: self.evicted,
            relayed: self.relayed,
            relayed_corpus: self.relayed_corpus,
            relayed_stale: self.relayed_stale,
            published_at,
        };
        stats.snapshot_identity = planner_statistics_identity(&stats);
        stats
    }
}

pub struct StatisticsConfig {
    pub channel_capacity: usize,
    pub registry_capacity: usize,
    pub publish_interval: Duration,
    /// Sketch counters halve after this many publish intervals so ancient
    /// traffic cannot permanently outrank the current workload.
    pub decay_every: u32,
    /// Models and table-change deltas persist after this many publish
    /// intervals. Flushing runs its own background transactions, never the
    /// foreground commit path, and a failed flush retries next interval.
    pub flush_every: u32,
    /// Check for one eligible table after this many publish intervals.
    pub survey_every: u32,
    pub survey_change_threshold: u64,
    pub survey_max_age: Duration,
    pub survey_row_budget: u64,
    pub survey_byte_budget: u64,
    pub survey_time_budget: Duration,
    /// Published models are reloaded after this many publish intervals. A
    /// reader instance depends on this: it gathers no evidence of its own that
    /// anyone else can see, so refreshing is the only way its planner learns
    /// what the writer has established.
    pub refresh_every: u32,
    /// Record the canonical program behind each execution, for offline replay
    /// against a candidate estimator.
    pub capture_programs: bool,
}

impl Default for StatisticsConfig {
    fn default() -> Self {
        Self {
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
            registry_capacity: DEFAULT_REGISTRY_CAPACITY,
            publish_interval: Duration::from_secs(1),
            decay_every: 600,
            // Each flush commit forces object-store writes that contend
            // with foreground traffic. A crash loses at most this window,
            // which the advisory contract permits.
            flush_every: 60,
            survey_every: 30,
            survey_change_threshold: 10_000,
            survey_max_age: Duration::from_secs(24 * 60 * 60),
            survey_row_budget: DEFAULT_SURVEY_ROW_BUDGET,
            survey_byte_budget: DEFAULT_SURVEY_BYTE_BUDGET,
            survey_time_budget: Duration::from_secs(30),
            refresh_every: 60,
            capture_programs: false,
        }
    }
}

/// One flush worth of locally gathered statistics, detached from the registry
/// that produced it so the writing side never sees collection state.
#[derive(Clone, Default)]
pub struct StatisticsBatch {
    pub table_changes: HashMap<crate::engine::catalog::identity::SchemaId, u64>,
    pub models: Vec<(Fingerprint, QueryModel)>,
    /// Appearances per family since the last publication, normalized to the
    /// batch frequency epoch.
    pub frequency: Vec<(Fingerprint, u32)>,
    /// Process-local epoch used to age a returned batch before it is merged.
    pub frequency_epoch: u64,
    /// Decay steps the durable frequency epoch must advance on publication.
    pub frequency_decays: u64,
    pub synopses: Vec<SynopsisModel>,
    pub programs: Vec<ProgramRecord>,
}

impl StatisticsBatch {
    fn is_empty(&self) -> bool {
        self.table_changes.is_empty()
            && self.models.is_empty()
            && self.frequency.is_empty()
            && self.frequency_decays == 0
            && self.synopses.is_empty()
            && self.programs.is_empty()
    }

    /// True when nothing in this batch can travel to another instance. The
    /// store keeps things a relay does not carry, so an empty batch for one is
    /// not an empty batch for the other.
    pub fn carries_no_evidence(&self) -> bool {
        self.models.is_empty() && self.frequency.is_empty() && self.programs.is_empty()
    }
}

/// Reads published statistics. Every Rad instance can do this: a Rad instance
/// holds no state of its own, the models live in the shared store, and reading
/// them needs no write access. A reader instance refreshes from here so its
/// planner estimates from the same evidence the writer gathered.
#[async_trait::async_trait]
pub trait StatisticsSource: Send + Sync {
    async fn load_models(&self, limit: usize) -> Vec<QueryModel>;
    async fn load_synopses(&self) -> Vec<SynopsisModel>;

    async fn load_snapshot(&self, limit: usize) -> PersistedStatisticsSnapshot {
        PersistedStatisticsSnapshot::new(
            self.load_models(limit).await,
            self.load_synopses().await,
            self.load_frequencies(limit).await,
        )
    }

    fn physical_telemetry(&self) -> Option<crate::engine::kv::telemetry::SharedPhysicalTelemetry> {
        None
    }

    async fn load_frequencies(&self, _limit: usize) -> Vec<(Fingerprint, u32)> {
        Vec::new()
    }

    async fn load_corpus(&self, _limit: usize) -> Result<Vec<CorpusExecution>, CorpusReplayError> {
        Err(CorpusReplayError::Unavailable)
    }
}

#[derive(Clone, Debug)]
pub struct PersistedStatisticsSnapshot {
    pub models: Vec<QueryModel>,
    pub synopses: Vec<SynopsisModel>,
    pub frequencies: Vec<(Fingerprint, u32)>,
    pub identity: String,
}

impl Default for PersistedStatisticsSnapshot {
    fn default() -> Self {
        Self::new(Vec::new(), Vec::new(), Vec::new())
    }
}

impl PersistedStatisticsSnapshot {
    fn new(
        mut models: Vec<QueryModel>,
        mut synopses: Vec<SynopsisModel>,
        mut frequencies: Vec<(Fingerprint, u32)>,
    ) -> Self {
        for model in &mut models {
            canonicalize_query_model(model);
        }
        for synopsis in &mut synopses {
            canonicalize_synopsis_model(synopsis);
        }
        models.sort_by_key(|model| (model.kind, model.family));
        synopses.sort_by_key(|model| model.table);
        frequencies.sort_by_key(|(family, _)| *family);
        let identity = persisted_statistics_identity(&models, &synopses, &frequencies);
        Self {
            models,
            synopses,
            frequencies,
            identity,
        }
    }
}

fn canonicalize_query_model(model: &mut QueryModel) {
    model.plans.sort_by_key(|(plan, _)| *plan);
    model
        .plan_profiles
        .sort_by_key(|profile| (profile.plan, profile.access_stamp));
}

fn canonicalize_feedback_model(model: &mut FeedbackModel) {
    model.plans.sort_by_key(|(plan, _)| *plan);
    model
        .plan_profiles
        .sort_by_key(|profile| (profile.plan, profile.access_stamp));
}

fn canonicalize_synopsis_model(model: &mut SynopsisModel) {
    for column in &mut model.columns {
        column.most_common_values.sort_by_cached_key(|value| {
            serde_json::to_vec(value).expect("most common value serializes")
        });
        if let Some(sequence) = &mut column.degree_sequence {
            sequence.segments.sort_by_key(|segment| {
                (
                    segment.rank_start,
                    segment.rank_end,
                    segment.frequency_upper,
                )
            });
        }
    }
    model.columns.sort_by_key(|column| column.column);
    for group in &mut model.column_groups {
        group.most_common_values.sort_by_cached_key(|value| {
            serde_json::to_vec(value).expect("most common column group serializes")
        });
        if let Some(sequence) = &mut group.degree_sequence {
            sequence.segments.sort_by_key(|segment| {
                (
                    segment.rank_start,
                    segment.rank_end,
                    segment.frequency_upper,
                )
            });
        }
    }
    model
        .column_groups
        .sort_by(|left, right| left.columns.cmp(&right.columns));
    for conditioned in &mut model.predicate_conditioned_degrees {
        conditioned.values.sort_by_cached_key(|value| {
            serde_json::to_vec(&value.predicate_value).expect("conditioned value serializes")
        });
    }
    model.predicate_conditioned_degrees.sort_by(|left, right| {
        (&left.join_columns, left.predicate_column)
            .cmp(&(&right.join_columns, right.predicate_column))
    });
}

fn planner_statistics_identity(stats: &PlannerStats) -> String {
    use sha2::{Digest as _, Sha256};

    let mut hash = Sha256::new();
    hash.update(b"rad-planner-statistics-v1");
    hash.update(STATISTICS_MODEL_FORMAT.to_be_bytes());
    hash.update(STATISTICS_SYNOPSIS_FORMAT.to_be_bytes());
    hash.update(STATISTICS_FREQUENCY_FORMAT.to_be_bytes());
    let mut models = stats
        .feedback_models
        .values()
        .cloned()
        .map(|model| (ObservationModelKind::Relation, model))
        .chain(
            stats
                .statement_models
                .values()
                .cloned()
                .map(|model| (ObservationModelKind::Statement, model)),
        )
        .collect::<Vec<_>>();
    models.sort_by_key(|(kind, model)| (*kind, model.family));
    for (kind, mut model) in models {
        canonicalize_feedback_model(&mut model);
        hash_statistics_record(
            &mut hash,
            &serde_json::to_vec(&(kind, model)).expect("planner model serializes"),
        );
    }
    let mut synopses = stats.synopsis_models.values().cloned().collect::<Vec<_>>();
    for synopsis in &mut synopses {
        canonicalize_synopsis_model(synopsis);
    }
    synopses.sort_by_key(|model| model.table);
    for synopsis in synopses {
        hash_statistics_record(
            &mut hash,
            &serde_json::to_vec(&synopsis).expect("synopsis model serializes"),
        );
    }
    hash_statistics_record(
        &mut hash,
        &serde_json::to_vec(&stats.workload_frequency).expect("frequency sketch serializes"),
    );
    statistics_hash_identity(hash)
}

fn persisted_statistics_identity(
    models: &[QueryModel],
    synopses: &[SynopsisModel],
    frequencies: &[(Fingerprint, u32)],
) -> String {
    use sha2::{Digest as _, Sha256};

    let mut hash = Sha256::new();
    hash.update(b"rad-statistics-snapshot-v1");
    hash.update(STATISTICS_MODEL_FORMAT.to_be_bytes());
    hash.update(STATISTICS_SYNOPSIS_FORMAT.to_be_bytes());
    hash.update(STATISTICS_FREQUENCY_FORMAT.to_be_bytes());
    for model in models {
        hash_statistics_record(
            &mut hash,
            &serde_json::to_vec(model).expect("query model serializes"),
        );
    }
    for synopsis in synopses {
        hash_statistics_record(
            &mut hash,
            &serde_json::to_vec(synopsis).expect("synopsis model serializes"),
        );
    }
    for (family, frequency) in frequencies {
        let mut record = family.to_bytes().to_vec();
        record.extend_from_slice(&frequency.to_be_bytes());
        hash_statistics_record(&mut hash, &record);
    }
    statistics_hash_identity(hash)
}

fn statistics_hash_identity(hash: sha2::Sha256) -> String {
    use sha2::Digest as _;

    hash.finalize()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn hash_statistics_record(hash: &mut sha2::Sha256, record: &[u8]) {
    use sha2::Digest as _;

    hash.update((record.len() as u64).to_be_bytes());
    hash.update(record);
}

/// Publishes locally gathered statistics. Writing to the shared store is one
/// implementation and requires write access, which is why a reader instance
/// currently keeps what it observes to itself. Publishing to a channel that
/// the writer reconciles would be another, and is what reader-gathered
/// evidence needs to reach the store at all.
#[async_trait::async_trait]
pub trait StatisticsSink: Send + Sync {
    /// Returns the batch on failure so the caller can retry it.
    async fn publish(&self, batch: StatisticsBatch) -> Result<(), StatisticsBatch>;

    /// True when publishing writes to the same place the source reads, so
    /// this instance owns those models and continues them. False when the
    /// evidence goes somewhere else, as a relay's does: the instance is then
    /// still a consumer of what the owner publishes, and refreshes from it.
    fn extends_the_source(&self) -> bool {
        false
    }

    /// Publish intervals to accumulate before publishing, given the configured
    /// default.
    ///
    /// The default is set by what a storage commit costs. A sink that
    /// publishes more cheaply may answer sooner, and should: accumulating
    /// buys nothing it needs, while it delays the evidence and widens the
    /// window a crash loses.
    fn flush_every(&self, configured: u32) -> u32 {
        configured
    }

    /// Whether this sink may be given user values.
    ///
    /// A sink that writes to the database's own storage may: the values are
    /// already there. A relay may only when its transport is confidential.
    fn carries_user_values(&self) -> bool {
        false
    }

    /// How this sink's transport is faring, when it has one. A sink that
    /// writes to storage has nothing to report here; a relay does, and it is
    /// the only place the sending side of the channel is observable.
    fn relay_counters(&self) -> Option<super::relay::RelayCounters> {
        None
    }

    /// Whether the sink holds a publication that needs another attempt.
    fn has_pending_publication(&self) -> bool {
        false
    }

    async fn initialize_corpus_maintenance(&self) {}

    fn corpus_maintenance(&self) -> Option<CorpusMaintenanceStats> {
        None
    }

    async fn erase_corpus(&self) -> Result<u64, CorpusEraseError> {
        Err(CorpusEraseError::Unavailable)
    }
}

#[derive(Debug)]
pub enum CorpusEraseError {
    Unavailable,
    Storage(String),
}

impl std::fmt::Display for CorpusEraseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => {
                formatter.write_str("this process does not own the workload corpus")
            }
            Self::Storage(error) => write!(formatter, "cannot erase the workload corpus: {error}"),
        }
    }
}

impl std::error::Error for CorpusEraseError {}

#[derive(Debug)]
pub enum CorpusReplayError {
    Unavailable,
    EngineUnavailable,
    Engine(String),
    Storage(String),
}

impl std::fmt::Display for CorpusReplayError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("the workload corpus is unavailable"),
            Self::EngineUnavailable => {
                formatter.write_str("the statistics runner has no attached engine")
            }
            Self::Engine(error) => write!(formatter, "cannot prepare corpus replay: {error}"),
            Self::Storage(error) => write!(formatter, "cannot read the workload corpus: {error}"),
        }
    }
}

impl std::error::Error for CorpusReplayError {}

#[derive(Clone, Copy)]
struct CorpusPolicy {
    max_executions: usize,
    max_bytes: usize,
    max_age: Duration,
}

impl Default for CorpusPolicy {
    fn default() -> Self {
        Self {
            max_executions: CORPUS_MAX_EXECUTIONS,
            max_bytes: CORPUS_MAX_BYTES,
            max_age: CORPUS_MAX_AGE,
        }
    }
}

/// Source and sink backed by the database's own Slate store.
pub struct SlateStatistics {
    store: Arc<dyn crate::engine::kv::TransactionalKv>,
    model_capacity: usize,
    runtime: Arc<dyn crate::runtime::RuntimeEffects>,
    corpus_policy: CorpusPolicy,
    sequence: AtomicU64,
    writes: tokio::sync::Mutex<()>,
    stored_programs: std::sync::Mutex<HashSet<[u8; 16]>>,
    corpus_maintenance: std::sync::Mutex<Option<CorpusMaintenanceStats>>,
}

impl SlateStatistics {
    pub fn new(store: Arc<dyn crate::engine::kv::TransactionalKv>) -> Self {
        Self::with_options(
            store,
            DEFAULT_REGISTRY_CAPACITY,
            Arc::new(crate::runtime::SystemRuntime),
            CorpusPolicy::default(),
        )
    }

    #[cfg(test)]
    fn with_model_capacity(
        store: Arc<dyn crate::engine::kv::TransactionalKv>,
        model_capacity: usize,
    ) -> Self {
        Self::with_options(
            store,
            model_capacity,
            Arc::new(crate::runtime::SystemRuntime),
            CorpusPolicy::default(),
        )
    }

    fn with_options(
        store: Arc<dyn crate::engine::kv::TransactionalKv>,
        model_capacity: usize,
        runtime: Arc<dyn crate::runtime::RuntimeEffects>,
        corpus_policy: CorpusPolicy,
    ) -> Self {
        Self {
            store,
            model_capacity: model_capacity.max(16),
            runtime,
            corpus_policy,
            sequence: AtomicU64::new(0),
            writes: tokio::sync::Mutex::new(()),
            stored_programs: std::sync::Mutex::new(HashSet::new()),
            corpus_maintenance: std::sync::Mutex::new(None),
        }
    }

    fn record_corpus_retention(&self, retention: &CorpusRetention) {
        let mut report = self
            .corpus_maintenance
            .lock()
            .expect("corpus maintenance report");
        let report = report.get_or_insert_with(CorpusMaintenanceStats::default);
        report.expired_executions = report
            .expired_executions
            .saturating_add(retention.expired_executions);
        report.pruned_executions = report
            .pruned_executions
            .saturating_add(retention.pruned_executions);
        report.invalid_executions = report
            .invalid_executions
            .saturating_add(retention.invalid_executions);
        report.pruned_programs = report
            .pruned_programs
            .saturating_add(retention.pruned_programs);
        report.invalid_programs = report
            .invalid_programs
            .saturating_add(retention.invalid_programs);
        report.retained_executions = retention.retained_executions;
        report.retained_programs = retention.retained_programs;
        report.retained_program_bytes = retention.retained_program_bytes;
    }

    fn record_corpus_erasure(&self, erasure: CorpusErasure) {
        let mut report = self
            .corpus_maintenance
            .lock()
            .expect("corpus maintenance report");
        let report = report.get_or_insert_with(CorpusMaintenanceStats::default);
        report.erased_executions = report.erased_executions.saturating_add(erasure.executions);
        report.erased_programs = report.erased_programs.saturating_add(erasure.programs);
        report.retained_executions = 0;
        report.retained_programs = 0;
        report.retained_program_bytes = 0;
    }
}

#[async_trait::async_trait]
impl StatisticsSource for SlateStatistics {
    async fn load_models(&self, limit: usize) -> Vec<QueryModel> {
        load_persisted_models(&self.store, limit)
            .await
            .unwrap_or_default()
    }

    async fn load_synopses(&self) -> Vec<SynopsisModel> {
        load_persisted_synopses(&self.store)
            .await
            .unwrap_or_default()
    }

    async fn load_snapshot(&self, limit: usize) -> PersistedStatisticsSnapshot {
        load_persisted_snapshot(&self.store, limit)
            .await
            .unwrap_or_default()
    }

    fn physical_telemetry(&self) -> Option<crate::engine::kv::telemetry::SharedPhysicalTelemetry> {
        self.store.physical_telemetry()
    }

    async fn load_frequencies(&self, limit: usize) -> Vec<(Fingerprint, u32)> {
        load_persisted_frequencies(&self.store, limit)
            .await
            .unwrap_or_default()
    }

    async fn load_corpus(&self, limit: usize) -> Result<Vec<CorpusExecution>, CorpusReplayError> {
        load_corpus_executions(&self.store, limit)
            .await
            .map_err(|error| CorpusReplayError::Storage(error.to_string()))
    }
}

#[async_trait::async_trait]
impl StatisticsSink for SlateStatistics {
    async fn publish(&self, batch: StatisticsBatch) -> Result<(), StatisticsBatch> {
        match self.write(&batch).await {
            Ok(()) => Ok(()),
            Err(_) => Err(batch),
        }
    }

    fn extends_the_source(&self) -> bool {
        true
    }

    /// The values are already in this database's storage; recording the
    /// programs that produced them reveals nothing new to the store.
    fn carries_user_values(&self) -> bool {
        true
    }

    async fn initialize_corpus_maintenance(&self) {
        let _write = self.writes.lock().await;
        let Ok(transaction) = self
            .store
            .begin(crate::engine::kv::IsolationLevel::Snapshot)
            .await
        else {
            return;
        };
        let Ok(retention) = retain_corpus(
            transaction.as_ref(),
            self.runtime.unix_time().as_micros() as u64,
            self.corpus_policy,
        )
        .await
        else {
            transaction.rollback();
            return;
        };
        if transaction.commit().await.is_err() {
            return;
        }
        *self.stored_programs.lock().expect("corpus set") = retention.stored_programs.clone();
        self.record_corpus_retention(&retention);
    }

    fn corpus_maintenance(&self) -> Option<CorpusMaintenanceStats> {
        *self
            .corpus_maintenance
            .lock()
            .expect("corpus maintenance report")
    }

    async fn erase_corpus(&self) -> Result<u64, CorpusEraseError> {
        let _write = self.writes.lock().await;
        let erased = erase_corpus(self.store.as_ref())
            .await
            .map_err(|error| CorpusEraseError::Storage(error.to_string()))?;
        self.stored_programs.lock().expect("corpus set").clear();
        self.record_corpus_erasure(erased);
        Ok(erased.total())
    }
}

impl HotRegistry {
    /// Detach everything awaiting publication. The registry keeps its models
    /// so the planner keeps estimating from them; only the unpublished deltas
    /// and records leave.
    pub(super) fn take_batch(&mut self) -> StatisticsBatch {
        let mut models: Vec<(Fingerprint, QueryModel)> = std::mem::take(&mut self.pending)
            .into_iter()
            .map(|(id, model)| (id.family, model))
            .collect();
        for (family, delta) in &mut models {
            // A mean over one interval's arrivals is not the model's mean, so
            // the delta carries the cumulative value rather than its own.
            if let Some(cumulative) = self.models.get(&ModelId::new(delta.kind, *family)) {
                delta.duration_ewma_micros = cumulative.duration_ewma_micros;
            }
        }
        StatisticsBatch {
            table_changes: std::mem::take(&mut self.table_change_deltas),
            models,
            frequency: std::mem::take(&mut self.pending_frequency)
                .into_iter()
                .collect(),
            frequency_epoch: self.frequency_epoch,
            frequency_decays: std::mem::take(&mut self.pending_frequency_decays),
            synopses: if self.synopses_dirty {
                self.synopses.values().cloned().collect()
            } else {
                Vec::new()
            },
            programs: std::mem::take(&mut self.pending_programs),
        }
    }

    /// Record what a successful publication covered, so a hot program's bytes
    /// upload once. Model deltas need nothing: draining them was the record.
    fn batch_published(&mut self, batch: &StatisticsBatch) {
        if !batch.synopses.is_empty()
            && batch.synopses.iter().all(|published| {
                self.synopses
                    .get(&published.table)
                    .is_some_and(|current| current == published)
            })
        {
            self.synopses_dirty = false;
        }
    }

    /// Put an unpublished batch back so the next attempt carries it. Returned
    /// deltas merge forward into whatever has been observed since, so a failed
    /// publication costs latency rather than evidence.
    pub(super) fn batch_returned(&mut self, batch: StatisticsBatch) {
        for (schema, delta) in batch.table_changes {
            *self.table_change_deltas.entry(schema).or_insert(0) += delta;
        }
        let age = self.frequency_epoch.saturating_sub(batch.frequency_epoch);
        for (family, count) in batch.frequency {
            let count = decay_frequency(count, age);
            if count == 0 {
                continue;
            }
            self.add_pending_frequency(family, count);
        }
        self.pending_frequency_decays = self
            .pending_frequency_decays
            .saturating_add(batch.frequency_decays);
        for (family, delta) in batch.models {
            let id = ModelId::new(delta.kind, family);
            let room = self.pending.len() < self.capacity;
            if let Some(pending) = self.pending.get_mut(&id) {
                // The returned delta is the earlier of the two.
                let mut merged = delta;
                merged.merge(pending);
                *pending = merged;
            } else if room {
                self.pending.insert(id, delta);
            } else {
                self.shed += 1;
            }
        }
        for record in batch.programs.into_iter().rev() {
            self.pending_programs.insert(0, record);
            if self.pending_programs.len() > MAX_PENDING_PROGRAMS {
                self.pending_programs.remove(0);
                self.corpus_shed += 1;
            }
        }
        if !batch.synopses.is_empty() {
            self.synopses_dirty = true;
        }
    }
}

async fn scan_retained_persisted_models<V>(
    view: &V,
    limit: usize,
) -> crate::engine::kv::Result<Vec<QueryModel>>
where
    V: crate::engine::kv::KvView + ?Sized,
{
    use crate::engine::kv::{KeyRange, KvView, keys};

    let mut models = HashMap::<ModelId, QueryModel>::new();
    let mut retained = BinaryHeap::<Reverse<(u64, ModelId)>>::new();
    for kind in [
        ObservationModelKind::Relation,
        ObservationModelKind::Statement,
    ] {
        let prefix = match kind {
            ObservationModelKind::Relation => keys::statistics_relation_model_prefix(),
            ObservationModelKind::Statement => keys::statistics_model_prefix(),
        };
        let range = match crate::engine::kv::key_encoding::prefix_end(&prefix) {
            Some(end) => KeyRange::new(prefix.clone(), end),
            None => KeyRange::from_start(prefix.clone()),
        };
        let mut iterator = KvView::scan(view, range).await?;
        while let Some(entry) = iterator.next().await? {
            let family = match kind {
                ObservationModelKind::Relation => {
                    keys::decode_statistics_relation_model_key(&entry.key)
                        .and_then(|parts| Fingerprint::from_bytes(&parts.family))
                }
                ObservationModelKind::Statement => keys::decode_statistics_model_key(&entry.key)
                    .and_then(|parts| Fingerprint::from_bytes(&parts.family)),
            };
            let Some(family) = family else {
                continue;
            };
            let Some(model) = decode_stored_model(&entry.value, family, kind) else {
                continue;
            };
            if limit == 0 {
                continue;
            }
            let id = ModelId::new(kind, family);
            let rank = (model.executions, id);
            if models.len() < limit {
                retained.push(Reverse(rank));
                models.insert(id, model);
            } else if retained.peek().is_some_and(|known| rank > known.0) {
                if let Some(Reverse((_, removed))) = retained.pop() {
                    models.remove(&removed);
                }
                retained.push(Reverse(rank));
                models.insert(id, model);
            }
        }
    }
    Ok(models.into_values().collect())
}

async fn delete_unretained_models(
    transaction: &dyn crate::engine::kv::Transaction,
    retained: &HashSet<ModelId>,
) -> crate::engine::kv::Result<()> {
    use crate::engine::kv::{KeyRange, keys};

    for kind in [
        ObservationModelKind::Relation,
        ObservationModelKind::Statement,
    ] {
        let prefix = match kind {
            ObservationModelKind::Relation => keys::statistics_relation_model_prefix(),
            ObservationModelKind::Statement => keys::statistics_model_prefix(),
        };
        let range = match crate::engine::kv::key_encoding::prefix_end(&prefix) {
            Some(end) => KeyRange::new(prefix.clone(), end),
            None => KeyRange::from_start(prefix.clone()),
        };
        let mut iterator = transaction.scan(range).await?;
        while let Some(entry) = iterator.next().await? {
            let family = match kind {
                ObservationModelKind::Relation => {
                    keys::decode_statistics_relation_model_key(&entry.key)
                        .and_then(|parts| Fingerprint::from_bytes(&parts.family))
                }
                ObservationModelKind::Statement => keys::decode_statistics_model_key(&entry.key)
                    .and_then(|parts| Fingerprint::from_bytes(&parts.family)),
            };
            if family.is_some_and(|family| !retained.contains(&ModelId::new(kind, family))) {
                transaction.delete(&entry.key)?;
            }
        }
    }
    Ok(())
}

async fn delete_unretained_frequencies(
    transaction: &dyn crate::engine::kv::Transaction,
    retained: &HashSet<Fingerprint>,
) -> crate::engine::kv::Result<()> {
    use crate::engine::kv::{KeyRange, keys};

    let prefix = keys::statistics_frequency_prefix();
    let range = match crate::engine::kv::key_encoding::prefix_end(&prefix) {
        Some(end) => KeyRange::new(prefix.clone(), end),
        None => KeyRange::from_start(prefix.clone()),
    };
    let mut iterator = transaction.scan(range).await?;
    while let Some(entry) = iterator.next().await? {
        let family = keys::decode_statistics_frequency_key(&entry.key)
            .and_then(|parts| Fingerprint::from_bytes(&parts.family));
        if family.is_none_or(|family| !retained.contains(&family)) {
            transaction.delete(&entry.key)?;
        }
    }
    Ok(())
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredCorpusExecution {
    program: String,
    canonical_version: u64,
    statements: u32,
    #[serde(default)]
    outcomes: Vec<crate::engine::exec::observe::ProgramStatementOutcome>,
}

#[derive(Clone, Debug)]
pub struct CorpusExecution {
    pub canonical: Vec<u8>,
    pub content_hash: [u8; 16],
    pub at_unix_micros: u64,
    pub statements: u32,
    pub outcomes: Vec<crate::engine::exec::observe::ProgramStatementOutcome>,
}

fn corpus_range(prefix: Vec<u8>) -> crate::engine::kv::KeyRange {
    match crate::engine::kv::key_encoding::prefix_end(&prefix) {
        Some(end) => crate::engine::kv::KeyRange::new(prefix, end),
        None => crate::engine::kv::KeyRange::from_start(prefix),
    }
}

fn decode_hash(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).ok())
        .collect()
}

#[derive(Default)]
struct CorpusRetention {
    stored_programs: HashSet<[u8; 16]>,
    expired_executions: u64,
    pruned_executions: u64,
    invalid_executions: u64,
    pruned_programs: u64,
    invalid_programs: u64,
    retained_executions: u64,
    retained_programs: u64,
    retained_program_bytes: u64,
}

#[derive(Clone, Copy, Default)]
struct CorpusErasure {
    executions: u64,
    programs: u64,
}

impl CorpusErasure {
    fn total(self) -> u64 {
        self.executions.saturating_add(self.programs)
    }
}

enum CorpusExecutionRetention {
    Candidate((u64, Vec<u8>)),
    Expired,
    Invalid,
}

async fn retain_corpus(
    transaction: &dyn crate::engine::kv::Transaction,
    now_micros: u64,
    policy: CorpusPolicy,
) -> crate::engine::kv::Result<CorpusRetention> {
    use crate::engine::kv::keys;

    let mut report = CorpusRetention::default();
    let mut programs = Vec::new();
    let mut program_sizes = HashMap::new();
    {
        let mut iterator = transaction
            .scan(corpus_range(keys::corpus_program_prefix()))
            .await?;
        while let Some(entry) = iterator.next().await? {
            let key = entry.key.to_vec();
            let identity = keys::decode_corpus_program_key(&key)
                .map(|parts| (parts.canonical_version, parts.content_hash));
            if let Some(identity) = &identity {
                program_sizes.insert(identity.clone(), entry.value.len());
            }
            programs.push((key, identity));
        }
    }

    let cutoff = now_micros.saturating_sub(policy.max_age.as_micros() as u64);
    let mut executions = Vec::new();
    {
        let mut iterator = transaction
            .scan(corpus_range(keys::corpus_execution_prefix()))
            .await?;
        while let Some(entry) = iterator.next().await? {
            let key = entry.key.to_vec();
            let at = keys::decode_corpus_execution_key(&key).map(|parts| parts.at);
            let stored = serde_json::from_slice::<StoredCorpusExecution>(&entry.value).ok();
            let retention = match (at, stored) {
                (Some(at), _) if at < cutoff => CorpusExecutionRetention::Expired,
                (Some(_), Some(stored)) => decode_hash(&stored.program)
                    .map(|hash| (stored.canonical_version, hash))
                    .filter(|identity| program_sizes.contains_key(identity))
                    .map(CorpusExecutionRetention::Candidate)
                    .unwrap_or(CorpusExecutionRetention::Invalid),
                _ => CorpusExecutionRetention::Invalid,
            };
            executions.push((key, retention));
        }
    }

    let valid = executions
        .iter()
        .filter(|(_, retention)| matches!(retention, CorpusExecutionRetention::Candidate(_)))
        .count();
    let mut remove_for_capacity = valid.saturating_sub(policy.max_executions);
    let mut candidates = Vec::new();
    for (key, retention) in executions {
        match retention {
            CorpusExecutionRetention::Candidate(program) if remove_for_capacity == 0 => {
                candidates.push((key, program));
            }
            CorpusExecutionRetention::Candidate(_) => {
                remove_for_capacity -= 1;
                transaction.delete(&key)?;
                report.pruned_executions = report.pruned_executions.saturating_add(1);
            }
            CorpusExecutionRetention::Expired => {
                transaction.delete(&key)?;
                report.expired_executions = report.expired_executions.saturating_add(1);
            }
            CorpusExecutionRetention::Invalid => {
                transaction.delete(&key)?;
                report.invalid_executions = report.invalid_executions.saturating_add(1);
            }
        }
    }

    let mut retained = HashSet::new();
    let mut retained_bytes = 0usize;
    for (key, identity) in candidates.into_iter().rev() {
        if retained.contains(&identity) {
            report.retained_executions = report.retained_executions.saturating_add(1);
            continue;
        }
        let bytes = program_sizes[&identity];
        if retained_bytes.saturating_add(bytes) > policy.max_bytes {
            transaction.delete(&key)?;
            report.pruned_executions = report.pruned_executions.saturating_add(1);
            continue;
        }
        retained_bytes += bytes;
        retained.insert(identity);
        report.retained_executions = report.retained_executions.saturating_add(1);
    }

    for (key, identity) in programs {
        let Some(identity) = identity else {
            transaction.delete(&key)?;
            report.invalid_programs = report.invalid_programs.saturating_add(1);
            continue;
        };
        if !retained.contains(&identity) {
            transaction.delete(&key)?;
            report.pruned_programs = report.pruned_programs.saturating_add(1);
            continue;
        }
        report.retained_programs = report.retained_programs.saturating_add(1);
        report.retained_program_bytes = report
            .retained_program_bytes
            .saturating_add(program_sizes[&identity] as u64);
        if identity.0 == CANONICAL_FORMAT_VERSION
            && let Ok(hash) = <[u8; 16]>::try_from(identity.1)
        {
            report.stored_programs.insert(hash);
        }
    }
    Ok(report)
}

async fn erase_corpus(
    store: &dyn crate::engine::kv::TransactionalKv,
) -> crate::engine::kv::Result<CorpusErasure> {
    use crate::engine::kv::keys;

    let transaction = store
        .begin(crate::engine::kv::IsolationLevel::Snapshot)
        .await?;
    let mut erased = CorpusErasure::default();
    for (prefix, count) in [
        (keys::corpus_execution_prefix(), &mut erased.executions),
        (keys::corpus_program_prefix(), &mut erased.programs),
    ] {
        let mut keys_to_delete = Vec::new();
        {
            let mut iterator = transaction.scan(corpus_range(prefix)).await?;
            while let Some(entry) = iterator.next().await? {
                keys_to_delete.push(entry.key);
            }
        }
        for key in keys_to_delete {
            transaction.delete(&key)?;
            *count = count.saturating_add(1);
        }
    }
    transaction.commit().await?;
    Ok(erased)
}

async fn load_corpus_executions(
    store: &Arc<dyn crate::engine::kv::TransactionalKv>,
    limit: usize,
) -> crate::engine::kv::Result<Vec<CorpusExecution>> {
    use std::collections::VecDeque;

    use crate::engine::kv::{IsolationLevel, KvView, TransactionView, keys};

    if limit == 0 {
        return Ok(Vec::new());
    }
    let transaction = store.begin(IsolationLevel::Snapshot).await?;
    let result = async {
        let view = TransactionView(transaction.as_ref());
        let mut retained = VecDeque::with_capacity(limit);
        let mut iterator = view
            .scan(corpus_range(keys::corpus_execution_prefix()))
            .await?;
        while let Some(entry) = iterator.next().await? {
            let Some(parts) = keys::decode_corpus_execution_key(&entry.key) else {
                continue;
            };
            let Ok(stored) = serde_json::from_slice::<StoredCorpusExecution>(&entry.value) else {
                continue;
            };
            if retained.len() == limit {
                retained.pop_front();
            }
            retained.push_back((parts.at, stored));
        }

        let mut executions = Vec::with_capacity(retained.len());
        for (at_unix_micros, stored) in retained {
            if stored.canonical_version != CANONICAL_FORMAT_VERSION {
                continue;
            }
            let Some(hash) =
                decode_hash(&stored.program).and_then(|hash| <[u8; 16]>::try_from(hash).ok())
            else {
                continue;
            };
            let key = keys::corpus_program_key(stored.canonical_version, &hash);
            let Some(canonical) = view.get(&key).await? else {
                continue;
            };
            executions.push(CorpusExecution {
                canonical: canonical.to_vec(),
                content_hash: hash,
                at_unix_micros,
                statements: stored.statements,
                outcomes: stored.outcomes,
            });
        }
        Ok(executions)
    }
    .await;
    transaction.rollback();
    result
}

impl SlateStatistics {
    async fn write(&self, batch: &StatisticsBatch) -> crate::engine::kv::Result<()> {
        use crate::engine::kv::keys;

        if batch.is_empty() {
            return Ok(());
        }
        let _write = self.writes.lock().await;
        let transaction = self
            .store
            .begin(crate::engine::kv::IsolationLevel::Snapshot)
            .await?;
        for (schema, delta) in &batch.table_changes {
            let key = keys::statistics_table_changes_key(schema.get());
            let current = transaction
                .get(&key)
                .await?
                .and_then(|value| String::from_utf8(value.to_vec()).ok())
                .and_then(|text| text.parse::<u64>().ok())
                .unwrap_or(0);
            let next = current.saturating_add(*delta);
            transaction.put(key.into(), next.to_string().into_bytes().into())?;
        }
        if !batch.models.is_empty() {
            let catalog_key = keys::statistics_model_catalog_key();
            let catalog = match transaction.get(&catalog_key).await? {
                Some(value) => decode_model_catalog(&value),
                None => None,
            };
            let (catalog, repair_catalog) = match catalog {
                Some(catalog) => (catalog, false),
                None => {
                    let view = crate::engine::kv::TransactionView(transaction.as_ref());
                    (
                        ModelCatalog {
                            format: STATISTICS_MODEL_CATALOG_FORMAT,
                            entries: scan_retained_persisted_models(&view, self.model_capacity)
                                .await?
                                .iter()
                                .map(ModelCatalogEntry::of)
                                .collect(),
                        },
                        true,
                    )
                }
            };
            let mut entries: HashMap<ModelId, ModelCatalogEntry> = catalog
                .entries
                .into_iter()
                .map(|entry| (entry.id(), entry))
                .collect();
            let previous_families: HashSet<Fingerprint> =
                entries.keys().map(|id| id.family).collect();
            let mut updates = HashMap::<ModelId, QueryModel>::new();
            for (_, delta) in &batch.models {
                let id = ModelId::of(delta);
                if let Some(model) = updates.get_mut(&id) {
                    model.merge(delta);
                    continue;
                }
                let key = model_storage_key(id);
                let mut model = match transaction.get(&key).await? {
                    Some(value) => decode_stored_model(&value, id.family, id.kind)
                        .unwrap_or_else(|| QueryModel::new(id.family, id.kind)),
                    None => QueryModel::new(id.family, id.kind),
                };
                model.merge(delta);
                updates.insert(id, model);
            }
            for model in updates.values() {
                entries.insert(ModelId::of(model), ModelCatalogEntry::of(model));
            }

            let newest = entries
                .values()
                .map(|entry| entry.last_seen_unix_micros)
                .max()
                .unwrap_or(0);
            let mut ranked: Vec<ModelId> = entries.keys().copied().collect();
            ranked.sort_by_key(|id| model_retention_rank(&entries[id], newest));
            ranked.reverse();
            let retained: HashSet<ModelId> = ranked.into_iter().take(self.model_capacity).collect();
            let retained_families: HashSet<Fingerprint> =
                retained.iter().map(|id| id.family).collect();
            if repair_catalog {
                delete_unretained_models(transaction.as_ref(), &retained).await?;
                delete_unretained_frequencies(transaction.as_ref(), &retained_families).await?;
            }
            for id in entries.keys().filter(|id| !retained.contains(id)) {
                transaction.delete(&model_storage_key(*id))?;
            }
            for family in previous_families.difference(&retained_families) {
                transaction.delete(&keys::statistics_frequency_key(&family.to_bytes()))?;
            }
            entries.retain(|id, _| retained.contains(id));
            for (id, model) in updates {
                if !retained.contains(&id) {
                    continue;
                }
                let value = serde_json::to_vec(&StoredModel {
                    format: STATISTICS_MODEL_FORMAT,
                    model,
                })
                .expect("query model serializes");
                transaction.put(model_storage_key(id).into(), value.into())?;
            }
            let mut catalog_entries: Vec<ModelCatalogEntry> = entries.into_values().collect();
            catalog_entries.sort_by_key(ModelCatalogEntry::id);
            let value = serde_json::to_vec(&ModelCatalog {
                format: STATISTICS_MODEL_CATALOG_FORMAT,
                entries: catalog_entries,
            })
            .expect("statistics model catalog serializes");
            transaction.put(catalog_key.into(), value.into())?;
        }
        if !batch.frequency.is_empty() || batch.frequency_decays > 0 {
            let epoch_key = keys::statistics_frequency_epoch_key();
            let current_epoch = transaction
                .get(&epoch_key)
                .await?
                .and_then(|value| String::from_utf8(value.to_vec()).ok())
                .and_then(|text| text.parse::<u64>().ok())
                .unwrap_or(0);
            let next_epoch = current_epoch.saturating_add(batch.frequency_decays);
            if next_epoch != current_epoch {
                transaction.put(epoch_key.into(), next_epoch.to_string().into_bytes().into())?;
            }
            let retained_families: HashSet<Fingerprint> = transaction
                .get(&keys::statistics_model_catalog_key())
                .await?
                .and_then(|value| decode_model_catalog(&value))
                .into_iter()
                .flat_map(|catalog| catalog.entries)
                .map(|entry| entry.family)
                .collect();
            for (family, delta) in &batch.frequency {
                if *delta == 0 || !retained_families.contains(family) {
                    continue;
                }
                let key = keys::statistics_frequency_key(&family.to_bytes());
                let current = transaction
                    .get(&key)
                    .await?
                    .and_then(|value| decode_stored_frequency(&value, next_epoch))
                    .unwrap_or(0);
                let value = serde_json::to_vec(&StoredFrequency {
                    format: STATISTICS_FREQUENCY_FORMAT,
                    record: StoredFrequencyRecord {
                        count: current.saturating_add(*delta),
                        epoch: next_epoch,
                    },
                })
                .expect("statistics frequency serializes");
                transaction.put(key.into(), value.into())?;
            }
        }
        for model in &batch.synopses {
            let key = keys::statistics_synopsis_key(model.table.get());
            let value = serde_json::to_vec(model).expect("synopsis model serializes");
            transaction.put(key.into(), value.into())?;
        }
        let mut newly_stored = Vec::new();
        for record in &batch.programs {
            let unseen = {
                let stored = self.stored_programs.lock().expect("corpus set");
                !stored.contains(&record.content_hash)
                    && !newly_stored.contains(&record.content_hash)
            };
            if unseen {
                let key = keys::corpus_program_key(CANONICAL_FORMAT_VERSION, &record.content_hash);
                transaction.put(key.into(), record.canonical.clone().into())?;
                newly_stored.push(record.content_hash);
            }
            let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
            let hash_hex: String = record
                .content_hash
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let entry = StoredCorpusExecution {
                program: hash_hex,
                canonical_version: CANONICAL_FORMAT_VERSION,
                statements: record.statements,
                outcomes: record.outcomes.clone(),
            };
            transaction.put(
                keys::corpus_execution_key(record.at_unix_micros, sequence).into(),
                serde_json::to_vec(&entry)
                    .expect("execution log entry serializes")
                    .into(),
            )?;
        }
        let retention = retain_corpus(
            transaction.as_ref(),
            self.runtime.unix_time().as_micros() as u64,
            self.corpus_policy,
        )
        .await?;
        transaction.commit().await?;
        *self.stored_programs.lock().expect("corpus set") = retention.stored_programs.clone();
        self.record_corpus_retention(&retention);
        Ok(())
    }
}

async fn load_persisted_synopses(
    store: &Arc<dyn crate::engine::kv::TransactionalKv>,
) -> crate::engine::kv::Result<Vec<SynopsisModel>> {
    let store_view: &dyn crate::engine::kv::Kv = store.as_ref();
    load_persisted_synopses_from(store_view).await
}

async fn load_persisted_synopses_from<V>(view: &V) -> crate::engine::kv::Result<Vec<SynopsisModel>>
where
    V: crate::engine::kv::KvView + ?Sized,
{
    use crate::engine::kv::{KeyRange, KvView, keys};

    let prefix = keys::statistics_synopsis_prefix();
    let end = crate::engine::kv::key_encoding::prefix_end(&prefix);
    let range = match end {
        Some(end) => KeyRange::new(prefix.clone(), end),
        None => KeyRange::from_start(prefix.clone()),
    };
    let mut models = Vec::new();
    let mut iterator = KvView::scan(view, range).await?;
    while let Some(entry) = iterator.next().await? {
        if let Ok(model) = serde_json::from_slice::<SynopsisModel>(&entry.value) {
            models.push(model);
        }
    }
    Ok(models)
}

async fn load_persisted_models(
    store: &Arc<dyn crate::engine::kv::TransactionalKv>,
    limit: usize,
) -> crate::engine::kv::Result<Vec<QueryModel>> {
    let store_view: &dyn crate::engine::kv::Kv = store.as_ref();
    load_persisted_models_from(store_view, limit).await
}

async fn load_persisted_models_from<V>(
    view: &V,
    limit: usize,
) -> crate::engine::kv::Result<Vec<QueryModel>>
where
    V: crate::engine::kv::KvView + ?Sized,
{
    use crate::engine::kv::{KvView, keys};

    if limit == 0 {
        return Ok(Vec::new());
    }
    let catalog = KvView::get(view, &keys::statistics_model_catalog_key())
        .await?
        .and_then(|value| decode_model_catalog(&value));
    if let Some(mut catalog) = catalog {
        let newest = catalog
            .entries
            .iter()
            .map(|entry| entry.last_seen_unix_micros)
            .max()
            .unwrap_or(0);
        catalog
            .entries
            .sort_by_key(|entry| model_retention_rank(entry, newest));
        let mut models = Vec::with_capacity(limit.min(catalog.entries.len()));
        let mut seen = HashSet::new();
        for entry in catalog.entries.into_iter().rev().take(limit) {
            let id = entry.id();
            if !seen.insert(id) {
                continue;
            }
            if let Some(value) = KvView::get(view, &model_storage_key(id)).await?
                && let Some(model) = decode_stored_model(&value, id.family, id.kind)
            {
                models.push(model);
            }
        }
        return Ok(models);
    }
    scan_retained_persisted_models(view, limit).await
}

async fn load_persisted_frequencies(
    store: &Arc<dyn crate::engine::kv::TransactionalKv>,
    limit: usize,
) -> crate::engine::kv::Result<Vec<(Fingerprint, u32)>> {
    let store_view: &dyn crate::engine::kv::Kv = store.as_ref();
    load_persisted_frequencies_from(store_view, limit).await
}

async fn load_persisted_frequencies_from<V>(
    view: &V,
    limit: usize,
) -> crate::engine::kv::Result<Vec<(Fingerprint, u32)>>
where
    V: crate::engine::kv::KvView + ?Sized,
{
    use crate::engine::kv::{KvView, keys};

    if limit == 0 {
        return Ok(Vec::new());
    }
    let epoch = KvView::get(view, &keys::statistics_frequency_epoch_key())
        .await?
        .and_then(|value| String::from_utf8(value.to_vec()).ok())
        .and_then(|text| text.parse::<u64>().ok())
        .unwrap_or(0);
    let catalog = KvView::get(view, &keys::statistics_model_catalog_key())
        .await?
        .and_then(|value| decode_model_catalog(&value));
    let mut families = Vec::new();
    let mut seen = HashSet::new();
    if let Some(mut catalog) = catalog {
        let newest = catalog
            .entries
            .iter()
            .map(|entry| entry.last_seen_unix_micros)
            .max()
            .unwrap_or(0);
        catalog
            .entries
            .sort_by_key(|entry| model_retention_rank(entry, newest));
        for entry in catalog.entries.into_iter().rev() {
            if seen.insert(entry.family) {
                families.push(entry.family);
                if families.len() == limit {
                    break;
                }
            }
        }
    } else {
        for model in scan_retained_persisted_models(view, limit).await? {
            if seen.insert(model.family) {
                families.push(model.family);
            }
        }
    }
    let mut frequencies = Vec::with_capacity(families.len());
    for family in families {
        let key = keys::statistics_frequency_key(&family.to_bytes());
        if let Some(value) = KvView::get(view, &key).await?
            && let Some(count) = decode_stored_frequency(&value, epoch)
            && count > 0
        {
            frequencies.push((family, count));
        }
    }
    Ok(frequencies)
}

async fn load_persisted_snapshot(
    store: &Arc<dyn crate::engine::kv::TransactionalKv>,
    limit: usize,
) -> crate::engine::kv::Result<PersistedStatisticsSnapshot> {
    use crate::engine::kv::{IsolationLevel, TransactionView};

    let transaction = store.begin(IsolationLevel::Snapshot).await?;
    let result = async {
        let view = TransactionView(transaction.as_ref());
        let models = load_persisted_models_from(&view, limit).await?;
        let synopses = load_persisted_synopses_from(&view).await?;
        let frequencies = load_persisted_frequencies_from(&view, limit).await?;
        Ok(PersistedStatisticsSnapshot::new(
            models,
            synopses,
            frequencies,
        ))
    }
    .await;
    transaction.rollback();
    result
}

type SurveyFuture = std::pin::Pin<Box<dyn Future<Output = Option<SurveyResult>> + Send>>;
type FlushFuture =
    std::pin::Pin<Box<dyn Future<Output = (StatisticsBatch, Result<(), StatisticsBatch>)> + Send>>;
type RefreshFuture = std::pin::Pin<Box<dyn Future<Output = PersistedStatisticsSnapshot> + Send>>;
type CorpusMaintenanceFuture = std::pin::Pin<Box<dyn Future<Output = ()> + Send>>;

fn complete_flush(
    registry: &mut HotRegistry,
    covered: StatisticsBatch,
    result: Result<(), StatisticsBatch>,
) {
    match result {
        Ok(()) => registry.batch_published(&covered),
        Err(batch) => registry.batch_returned(batch),
    }
}

async fn finish_publication(sink: &dyn StatisticsSink, mut batch: StatisticsBatch) {
    while !batch.is_empty() || sink.has_pending_publication() {
        batch = match sink.publish(batch).await {
            Ok(()) => StatisticsBatch::default(),
            Err(returned) if sink.has_pending_publication() => returned,
            Err(_) => break,
        };
    }
}

/// Background statistics runner: drains the collector channel into the hot
/// registry and publishes the distilled [`PlannerStats`] snapshot.
/// What the relay is doing, from this instance's side of it.
#[derive(Default)]
pub struct RelayReport {
    /// Absent when this instance publishes to storage rather than to a peer.
    pub sending: Option<super::relay::RelayCounters>,
    pub receiving: super::relay::IngestCounters,
}

pub struct StatisticsRunner {
    collector: Arc<StatisticsCollector>,
    ingest: Arc<super::relay::RelayIngest>,
    source: Option<Arc<dyn StatisticsSource>>,
    /// Kept so the sending side of the relay stays observable; the runner's
    /// own use of the sink is inside its task.
    sink: Option<Arc<dyn StatisticsSink>>,
    planning_snapshot: Arc<std::sync::RwLock<Arc<PlannerStats>>>,
    diagnostic_snapshot: Arc<std::sync::RwLock<Arc<PlannerStats>>>,
    engine: Arc<std::sync::Mutex<Option<Arc<crate::engine::exec::Engine>>>>,
    stop: Arc<tokio::sync::Notify>,
    task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl StatisticsRunner {
    pub fn start(
        runtime: Arc<dyn crate::runtime::RuntimeEffects>,
        config: StatisticsConfig,
        source: Option<Arc<dyn StatisticsSource>>,
        sink: Option<Arc<dyn StatisticsSink>>,
    ) -> Arc<Self> {
        let (collector, mut receiver) = StatisticsCollector::channel_with_programs(
            config.channel_capacity,
            config.capture_programs,
        );
        // Relayed batches are large and rare where observations are small and
        // frequent, so they queue separately and cannot crowd each other out.
        let (ingest, mut relayed) = super::relay::RelayIngest::channel_with_corpus(
            RELAY_QUEUE_CAPACITY,
            config.capture_programs,
        );
        let planning_snapshot = Arc::new(std::sync::RwLock::new(Arc::new(PlannerStats::empty())));
        let diagnostic_snapshot = Arc::new(std::sync::RwLock::new(Arc::new(PlannerStats::empty())));
        let stop = Arc::new(tokio::sync::Notify::new());
        let engine = Arc::new(std::sync::Mutex::new(
            None::<Arc<crate::engine::exec::Engine>>,
        ));

        let task_planning_snapshot = planning_snapshot.clone();
        let task_diagnostic_snapshot = diagnostic_snapshot.clone();
        let task_stop = stop.clone();
        let task_collector = collector.clone();
        let task_engine = engine.clone();
        let reported_sink = sink.clone();
        let reported_source = source.clone();
        let physical_telemetry = source
            .as_ref()
            .and_then(|source| source.physical_telemetry());
        let task = tokio::spawn(async move {
            let mut registry = HotRegistry::new(config.registry_capacity);
            let mut physical_cost = PhysicalCostRegistry::default();
            let mut persisted = PersistedStatisticsSnapshot::default();
            // An instance that owns the stored models adopts them as the base
            // it extends. One that does not keeps them separate: its own
            // observations describe the traffic it serves, and it refreshes,
            // so adopting would add the same history again on every pass.
            let owns_stored_models = sink.as_ref().is_some_and(|sink| sink.extends_the_source());
            let mut stored_models_loaded = source.is_none();
            let mut ticker = tokio::time::interval(config.publish_interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut ticks_since_decay = 0u32;
            let mut ticks_since_flush = 0u32;
            let mut ticks_since_survey = 0u32;
            let mut ticks_since_refresh = 0u32;
            let mut published_absorbed = u64::MAX;
            let mut published_collector = CollectorReport::default();
            let mut published_corpus_maintenance = None;
            let mut force_publish = false;
            let mut survey_future: Option<SurveyFuture> = None;
            let mut flush_future: Option<FlushFuture> = None;
            let mut refresh_future: Option<RefreshFuture> = source.as_ref().map(|source| {
                let source = source.clone();
                Box::pin(async move { source.load_snapshot(config.registry_capacity).await })
                    as RefreshFuture
            });
            let mut corpus_maintenance_future: Option<CorpusMaintenanceFuture> =
                sink.as_ref().map(|sink| {
                    let sink = sink.clone();
                    Box::pin(async move { sink.initialize_corpus_maintenance().await })
                        as CorpusMaintenanceFuture
                });
            loop {
                tokio::select! {
                    _ = task_stop.notified() => break,
                    event = receiver.recv() => match event {
                        Some(StatisticsEvent::Statement(observation)) => {
                            registry.absorb(&observation, runtime.unix_time());
                        }
                        Some(StatisticsEvent::Program(record)) => {
                            registry.absorb_program(record);
                        }
                        None => break,
                    },
                    batch = relayed.recv() => match batch {
                        Some(batch) => {
                            let now = runtime.unix_time();
                            for model in &batch.families {
                                registry.merge_relayed(
                                    model.family,
                                    model,
                                    batch.age_of(model),
                                    now,
                                );
                            }
                            for (family, count) in batch.frequency {
                                registry.merge_relayed_frequency(family, count);
                            }
                            // Relayed documents join the local corpus queue,
                            // so the writer's ordinary flush stores them and
                            // content addressing makes a duplicate harmless.
                            for record in batch.corpus {
                                registry.absorb_relayed_program(record);
                            }
                            force_publish = true;
                        }
                        None => break,
                    },
                    result = async {
                        survey_future.as_mut().expect("survey future").await
                    }, if survey_future.is_some() => {
                        survey_future = None;
                        if let Some(result) = result {
                            registry.set_synopsis(result.model, result.covered_changes);
                            force_publish = true;
                        }
                    },
                    (covered, result) = async {
                        flush_future.as_mut().expect("flush future").await
                    }, if flush_future.is_some() => {
                        flush_future = None;
                        complete_flush(&mut registry, covered, result);
                    },
                    refreshed = async {
                        refresh_future.as_mut().expect("refresh future").await
                    }, if refresh_future.is_some() => {
                        refresh_future = None;
                        if owns_stored_models {
                            registry.hydrate(refreshed.models);
                            registry.hydrate_frequency(refreshed.frequencies);
                            stored_models_loaded = true;
                        } else {
                            persisted = refreshed.clone();
                        }
                        if !refreshed.synopses.is_empty() {
                            registry.hydrate_synopses(refreshed.synopses);
                        }
                        force_publish = true;
                    },
                    _ = async {
                        corpus_maintenance_future
                            .as_mut()
                            .expect("corpus maintenance future")
                            .await
                    }, if corpus_maintenance_future.is_some() => {
                        corpus_maintenance_future = None;
                        force_publish = true;
                    },
                    _ = ticker.tick() => {
                        if let Some(telemetry) = &physical_telemetry
                            && physical_cost.observe(telemetry.snapshot())
                        {
                            force_publish = true;
                        }
                        ticks_since_decay += 1;
                        if ticks_since_decay >= config.decay_every {
                            registry.decay();
                            ticks_since_decay = 0;
                        }
                        ticks_since_survey += 1;
                        if ticks_since_survey >= config.survey_every
                            && survey_future.is_none()
                        {
                            ticks_since_survey = 0;
                            let engine = task_engine
                                .lock()
                                .expect("statistics engine lock")
                                .clone();
                            // Only a writer publishes survey results. Read
                            // instances take the writer's synopses through the
                            // ordinary refresh instead.
                            if let Some(engine) = engine
                                && !engine.is_read_only()
                            {
                                let request = registry.survey_request(
                                    &config,
                                    runtime.unix_time(),
                                );
                                let time_budget = config.survey_time_budget;
                                survey_future = Some(Box::pin(async move {
                                    tokio::time::timeout(
                                        time_budget,
                                        engine.survey_next(request),
                                    )
                                    .await
                                    .ok()
                                    .and_then(Result::ok)
                                    .flatten()
                                }));
                            }
                        }
                        if let Some(sink) = &sink
                            && (!owns_stored_models || stored_models_loaded)
                        {
                            ticks_since_flush += 1;
                            if ticks_since_flush >= sink.flush_every(config.flush_every)
                                && flush_future.is_none()
                            {
                                ticks_since_flush = 0;
                                let batch = registry.take_batch();
                                let covered = StatisticsBatch {
                                    models: batch.models.clone(),
                                    synopses: batch.synopses.clone(),
                                    ..StatisticsBatch::default()
                                };
                                if !batch.is_empty() || sink.has_pending_publication() {
                                    let sink = sink.clone();
                                    flush_future = Some(Box::pin(async move {
                                        let result = sink.publish(batch).await;
                                        (covered, result)
                                    }));
                                }
                            }
                        }
                        // Only an instance that does not own the stored models
                        // has anything to learn from them: an owner's registry
                        // is already what it last wrote there.
                        if let Some(source) = &source
                            && !owns_stored_models
                        {
                            ticks_since_refresh += 1;
                            if ticks_since_refresh >= config.refresh_every
                                && refresh_future.is_none()
                            {
                                ticks_since_refresh = 0;
                                    let source = source.clone();
                                    refresh_future = Some(Box::pin(async move {
                                        source.load_snapshot(config.registry_capacity).await
                                    }));
                            }
                        }
                        let collector_report = task_collector.report();
                        let corpus_maintenance = sink
                            .as_ref()
                            .and_then(|sink| sink.corpus_maintenance());
                        if registry.absorbed() != published_absorbed
                            || collector_report != published_collector
                            || corpus_maintenance != published_corpus_maintenance
                            || force_publish
                        {
                            published_absorbed = registry.absorbed();
                            published_collector = collector_report;
                            published_corpus_maintenance = corpus_maintenance;
                            force_publish = false;
                            let (planning, diagnostic) = runner_snapshots(
                                &registry,
                                &persisted,
                                owns_stored_models,
                                physical_cost.model(),
                                collector_report,
                                corpus_maintenance,
                                runtime.unix_time(),
                            );
                            *task_planning_snapshot
                                .write()
                                .expect("planning statistics snapshot lock") = planning;
                            *task_diagnostic_snapshot
                                .write()
                                .expect("diagnostic statistics snapshot lock") = diagnostic;
                        }
                    }
                }
            }
            if let Some(future) = flush_future {
                let (covered, result) = future.await;
                complete_flush(&mut registry, covered, result);
            }
            if let Some(sink) = &sink {
                finish_publication(sink.as_ref(), registry.take_batch()).await;
            }
            let (planning, diagnostic) = runner_snapshots(
                &registry,
                &persisted,
                owns_stored_models,
                physical_cost.model(),
                task_collector.report(),
                sink.as_ref().and_then(|sink| sink.corpus_maintenance()),
                runtime.unix_time(),
            );
            *task_planning_snapshot
                .write()
                .expect("planning statistics snapshot lock") = planning;
            *task_diagnostic_snapshot
                .write()
                .expect("diagnostic statistics snapshot lock") = diagnostic;
        });

        Arc::new(Self {
            collector,
            ingest,
            source: reported_source,
            sink: reported_sink,
            planning_snapshot,
            diagnostic_snapshot,
            engine,
            stop,
            task: std::sync::Mutex::new(Some(task)),
        })
    }

    /// Attach the engine surveys read through. Separate from construction
    /// because the engine is built after the runner (it takes the runner's
    /// collector as its observer).
    pub fn attach_engine(&self, engine: Arc<crate::engine::exec::Engine>) {
        *self.engine.lock().expect("statistics engine lock") = Some(engine);
    }

    /// The observer to install on the engine.
    pub fn collector(&self) -> Arc<StatisticsCollector> {
        self.collector.clone()
    }

    /// Where another instance's evidence enters. Every instance can receive;
    /// whether anything can reach it is a question of what listens.
    pub fn ingest(&self) -> Arc<super::relay::RelayIngest> {
        self.ingest.clone()
    }

    /// Both directions of the relay, as far as this instance can see them.
    ///
    /// An instance normally has one: a reader sends, a writer receives. Both
    /// are reported because an instance cannot know which it is without being
    /// told, and a value of zero is itself the answer.
    pub fn relay(&self) -> RelayReport {
        RelayReport {
            sending: self.sink.as_ref().and_then(|sink| sink.relay_counters()),
            receiving: self.ingest.counters(),
        }
    }

    /// The current distilled snapshot. Cheap: one lock, one `Arc` clone.
    pub fn stats(&self) -> Arc<PlannerStats> {
        self.diagnostic_snapshot
            .read()
            .expect("diagnostic statistics snapshot lock")
            .clone()
    }

    pub fn planning_stats(&self) -> Arc<PlannerStats> {
        self.planning_snapshot
            .read()
            .expect("planning statistics snapshot lock")
            .clone()
    }

    pub async fn erase_corpus(&self) -> Result<u64, CorpusEraseError> {
        let Some(sink) = &self.sink else {
            return Err(CorpusEraseError::Unavailable);
        };
        sink.erase_corpus().await
    }

    pub async fn replay_corpus(
        &self,
        limit: usize,
    ) -> Result<super::replay::CorpusReplayReport, CorpusReplayError> {
        let source = self.source.as_ref().ok_or(CorpusReplayError::Unavailable)?;
        let engine = self
            .engine
            .lock()
            .expect("statistics engine lock")
            .clone()
            .ok_or(CorpusReplayError::EngineUnavailable)?;
        let executions = source.load_corpus(limit).await?;
        let revision = engine
            .catalog_revision()
            .await
            .map_err(|error| CorpusReplayError::Engine(error.to_string()))?;
        Ok(super::replay::replay(
            engine.as_ref(),
            self.planning_stats(),
            executions,
            &super::replay::FamilyFeedbackCandidate,
            revision.version.get(),
            revision.hash,
        )
        .await)
    }

    pub async fn shutdown(&self) {
        self.stop.notify_one();
        let task = self.task.lock().expect("statistics task lock").take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

impl Drop for StatisticsRunner {
    fn drop(&mut self) {
        self.stop.notify_one();
    }
}

impl crate::engine::planner::estimator::StatisticsProvider for StatisticsRunner {
    fn planning_stats(&self) -> Arc<PlannerStats> {
        Self::planning_stats(self)
    }

    fn diagnostic_stats(&self) -> Arc<PlannerStats> {
        Self::stats(self)
    }

    fn relay(&self) -> RelayReport {
        Self::relay(self)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use crate::engine::exec::observe::{KvWork, PhaseTimings};
    use crate::engine::lir::fingerprint::{
        CANONICALIZATION_VERSION, HASH_SHA256_128, QueryFingerprints, RelationFingerprints,
    };

    use super::*;

    struct TestRuntime(std::sync::atomic::AtomicU64);

    impl crate::runtime::RuntimeEffects for TestRuntime {
        fn now(&self) -> chrono::DateTime<chrono::Utc> {
            chrono::Utc::now()
        }

        fn new_uuid(&self) -> uuid::Uuid {
            uuid::Uuid::nil()
        }

        fn unix_time(&self) -> Duration {
            Duration::from_micros(self.0.load(Ordering::Relaxed))
        }
    }

    #[test]
    fn persisted_snapshot_identity_is_canonical_and_content_sensitive() {
        let mut first = QueryModel::new(fingerprint(1), ObservationModelKind::Statement);
        first.executions = 3;
        let mut second = QueryModel::new(fingerprint(2), ObservationModelKind::Relation);
        second.executions = 5;
        let canonical = PersistedStatisticsSnapshot::new(
            vec![first.clone(), second.clone()],
            Vec::new(),
            vec![(fingerprint(1), 7), (fingerprint(2), 11)],
        );
        let reordered = PersistedStatisticsSnapshot::new(
            vec![second.clone(), first.clone()],
            Vec::new(),
            vec![(fingerprint(2), 11), (fingerprint(1), 7)],
        );
        let changed = PersistedStatisticsSnapshot::new(
            vec![first, second],
            Vec::new(),
            vec![(fingerprint(1), 8), (fingerprint(2), 11)],
        );

        assert_eq!(canonical.identity, reordered.identity);
        assert_ne!(canonical.identity, changed.identity);
    }

    #[test]
    fn live_snapshot_identity_changes_with_planner_content() {
        let mut registry = HotRegistry::new(16);
        let empty = registry.distill(CollectorReport::default(), Duration::ZERO);
        registry.absorb(&observation(1, 1, 10), Duration::ZERO);
        let observed = registry.distill(CollectorReport::default(), Duration::ZERO);

        assert_eq!(empty.snapshot_identity.len(), 32);
        assert_eq!(observed.snapshot_identity.len(), 32);
        assert_ne!(empty.snapshot_identity, observed.snapshot_identity);
    }

    async fn prefix_count(store: &dyn crate::engine::kv::Kv, prefix: Vec<u8>) -> usize {
        let mut iterator = store.scan(corpus_range(prefix)).await.unwrap();
        let mut count = 0;
        while iterator.next().await.unwrap().is_some() {
            count += 1;
        }
        count
    }

    fn fingerprint(seed: u8) -> Fingerprint {
        let mut digest = [0u8; 16];
        for (index, byte) in digest.iter_mut().enumerate() {
            *byte = seed.wrapping_mul(31).wrapping_add(index as u8);
        }
        Fingerprint {
            canonicalization_version: CANONICALIZATION_VERSION,
            hash_algorithm: HASH_SHA256_128,
            digest,
        }
    }

    pub(crate) fn observation(family_seed: u8, exact_seed: u8, rows: u64) -> StatementObservation {
        let family = fingerprint(family_seed);
        let exact = fingerprint(exact_seed);
        StatementObservation {
            query: QueryFingerprints {
                exact,
                family,
                root: RelationFingerprints { exact, family },
                subtrees: vec![RelationFingerprints { exact, family }],
                tables: Default::default(),
                bindings: Vec::new(),
            },
            plan: Some(fingerprint(family_seed.wrapping_add(100))),
            phase: PhaseTimings {
                bind: Duration::from_micros(50),
                execute: Duration::from_micros(400),
            },
            rows,
            estimate: None,
            stamp: Default::default(),
            relations: Vec::new(),
            affected: rows,
            mutated: None,
            kv: Default::default(),
            join_operators: Vec::new(),
            failure: None,
        }
    }

    fn physical_snapshot(
        backend: &str,
        requests: u64,
        errors: u64,
        buckets: Vec<u64>,
        maximum_micros: u64,
        cache: (u64, u64),
    ) -> crate::engine::kv::telemetry::PhysicalTelemetrySnapshot {
        crate::engine::kv::telemetry::PhysicalTelemetrySnapshot {
            identity: crate::engine::kv::telemetry::PhysicalTelemetryIdentity {
                backend: backend.to_owned(),
                format: crate::engine::kv::telemetry::PHYSICAL_TELEMETRY_FORMAT,
            },
            capabilities: crate::engine::kv::telemetry::PhysicalTelemetryCapabilities {
                request_latency: true,
                cache_tiers: true,
                ..Default::default()
            },
            requests: vec![crate::engine::kv::telemetry::PhysicalRequestSnapshot {
                class: crate::engine::kv::telemetry::PhysicalRequestClass::RangeRead,
                condition: crate::engine::kv::telemetry::PhysicalRequestCondition::default(),
                requests,
                errors,
                latency_micros: Some(crate::engine::kv::telemetry::CumulativeHistogram {
                    boundaries: vec![1_000, 5_000],
                    bucket_counts: buckets,
                    count: requests,
                    maximum: maximum_micros,
                }),
                bytes: None,
            }],
            caches: vec![crate::engine::kv::telemetry::PhysicalCacheSnapshot {
                tier: crate::engine::kv::telemetry::PhysicalCacheTier::Memory,
                accesses: cache.0,
                hits: cache.1,
            }],
        }
    }

    #[test]
    fn physical_cost_calibration_uses_only_new_cumulative_samples() {
        let mut registry = PhysicalCostRegistry::default();
        assert!(!registry.observe(physical_snapshot(
            "test",
            4,
            0,
            vec![1, 3, 0],
            4_000,
            (10, 4),
        )));
        assert!(registry.model().is_none());

        assert!(registry.observe(physical_snapshot(
            "test",
            7,
            1,
            vec![1, 5, 1],
            8_000,
            (15, 7),
        )));
        let model = registry.model().expect("physical cost model");
        assert_eq!(model.backend, "test");
        assert_eq!(model.requests.len(), 1);
        assert_eq!(model.requests[0].observed_requests, 3);
        assert_eq!(model.requests[0].errors, 1);
        let latency = model.requests[0].latency_micros.expect("latency");
        assert!(latency.p50_upper_bound >= 5_000);
        assert_eq!(latency.maximum_upper_bound, 8_000);
        assert_eq!(model.caches[0].accesses, 5);
        assert_eq!(model.caches[0].hits, 3);
        assert_eq!(model.caches[0].hit_rate_ppm, 600_000);
        assert!(model.capabilities.request_latency);
        assert!(!model.capabilities.request_bytes);
    }

    #[test]
    fn physical_cost_calibration_resets_for_a_new_backend_identity() {
        let mut registry = PhysicalCostRegistry::default();
        registry.observe(physical_snapshot("first", 1, 0, vec![1, 0, 0], 500, (1, 1)));
        registry.observe(physical_snapshot("first", 2, 0, vec![2, 0, 0], 500, (2, 2)));
        assert!(registry.model().is_some());

        assert!(!registry.observe(physical_snapshot(
            "second",
            100,
            20,
            vec![50, 30, 20],
            20_000,
            (100, 50),
        )));
        assert!(registry.model().is_none());
    }

    #[test]
    fn physical_cost_calibration_keeps_request_conditions_separate() {
        let mut registry = PhysicalCostRegistry::default();
        let mut initial = physical_snapshot("test", 0, 0, vec![0, 0, 0], 0, (0, 0));
        initial.requests[0].condition.size_upper_bound = Some(4_096);
        let mut larger = initial.requests[0].clone();
        larger.condition.size_upper_bound = Some(1_048_576);
        initial.requests.push(larger);

        let mut current = initial.clone();
        current.requests[0].requests = 2;
        current.requests[0]
            .latency_micros
            .as_mut()
            .expect("small latency")
            .bucket_counts = vec![0, 2, 0];
        current.requests[0]
            .latency_micros
            .as_mut()
            .expect("small latency")
            .count = 2;
        current.requests[1].requests = 1;
        current.requests[1]
            .latency_micros
            .as_mut()
            .expect("large latency")
            .bucket_counts = vec![0, 0, 1];
        current.requests[1]
            .latency_micros
            .as_mut()
            .expect("large latency")
            .count = 1;
        current.requests[1]
            .latency_micros
            .as_mut()
            .expect("large latency")
            .maximum = 20_000;

        assert!(!registry.observe(initial));
        assert!(registry.observe(current));
        let model = registry.model().expect("physical cost model");
        assert_eq!(model.requests.len(), 2);
        assert_eq!(model.requests[0].size_upper_bound, Some(4_096));
        assert_eq!(model.requests[0].observed_requests, 2);
        assert_eq!(model.requests[1].size_upper_bound, Some(1_048_576));
        assert_eq!(model.requests[1].observed_requests, 1);
    }

    #[test]
    fn malformed_physical_histograms_do_not_create_latency_evidence() {
        let mut registry = PhysicalCostRegistry::default();
        registry.observe(physical_snapshot("test", 1, 0, vec![1, 0, 0], 500, (0, 0)));
        registry.observe(physical_snapshot("test", 2, 0, vec![2], 800, (0, 0)));
        let model = registry.model().expect("request count model");
        assert!(model.requests[0].latency_micros.is_none());

        registry.observe(physical_snapshot("test", 3, 0, vec![3, 0, 0], 900, (0, 0)));
        let calibration = registry.requests.values().next().expect("request model");
        assert_eq!(calibration.observed_requests, 2);
        assert_eq!(calibration.latency_micros.count(), 2);
    }

    #[test]
    fn registry_builds_models_and_tracks_frequency() {
        let mut registry = HotRegistry::new(16);
        for index in 0..10 {
            registry.absorb(
                &observation(1, index, 100 + u64::from(index)),
                Duration::ZERO,
            );
        }
        registry.absorb(&observation(2, 200, 5), Duration::from_secs(1));

        let hot = registry
            .model(ObservationModelKind::Statement, &fingerprint(1))
            .unwrap();
        assert_eq!(hot.executions, 10);
        assert!(hot.rows.quantile_upper_bound(0.5) >= 100);
        assert!(hot.exact_variants.estimate() >= 9);
        assert_eq!(hot.plans.len(), 1);
        assert_eq!(hot.plans[0].1, 10);
        assert!(registry.frequency(&fingerprint(1)) >= 10);
        assert!(registry.frequency(&fingerprint(2)) >= 1);
        assert_eq!(registry.frequency(&fingerprint(3)), 0);
    }

    #[test]
    fn plan_profiles_partition_access_generations() {
        let mut registry = HotRegistry::new(16);
        let mut first = observation(1, 1, 10);
        first.stamp.access = 7;
        registry.absorb(&first, Duration::ZERO);
        registry.absorb(&first, Duration::ZERO);
        let mut second = first.clone();
        second.stamp.access = 8;
        registry.absorb(&second, Duration::ZERO);

        let model = registry
            .model(ObservationModelKind::Statement, &fingerprint(1))
            .expect("statement model");
        assert_eq!(model.plan_profiles.len(), 2);
        assert_eq!(
            model
                .plan_profiles
                .iter()
                .find(|profile| profile.access_stamp == 7)
                .expect("first access generation")
                .executions,
            2
        );
        assert_eq!(
            model
                .plan_profiles
                .iter()
                .find(|profile| profile.access_stamp == 8)
                .expect("second access generation")
                .executions,
            1
        );
    }

    #[test]
    fn measured_relations_do_not_increment_subtree_frequency_twice() {
        let mut registry = HotRegistry::new(16);
        let mut observed = observation(1, 1, 10);
        let relation_family = fingerprint(2);
        observed.query.subtrees = vec![RelationFingerprints {
            exact: fingerprint(3),
            family: relation_family,
        }];
        observed
            .relations
            .push(crate::engine::exec::observe::RelationObservation {
                family: relation_family,
                rows: 5,
                estimate: None,
            });

        registry.absorb(&observed, Duration::ZERO);

        assert_eq!(registry.frequency(&relation_family), 1);
        assert_eq!(registry.frequency(&observed.query.family), 1);
        let stats = registry.distill(CollectorReport::default(), Duration::ZERO);
        assert!(stats.feedback_models.contains_key(&relation_family));
        assert!(stats.statement_models.contains_key(&observed.query.family));
        assert!(!stats.feedback_models.contains_key(&observed.query.family));
    }

    #[test]
    fn a_root_relation_and_its_statement_keep_separate_models() {
        let mut registry = HotRegistry::new(16);
        let mut observed = observation(1, 1, 10);
        observed
            .relations
            .push(crate::engine::exec::observe::RelationObservation {
                family: observed.query.family,
                rows: 4,
                estimate: None,
            });

        registry.absorb(&observed, Duration::from_secs(1));

        let relation = registry
            .model(ObservationModelKind::Relation, &observed.query.family)
            .expect("relation model");
        let statement = registry
            .model(ObservationModelKind::Statement, &observed.query.family)
            .expect("statement model");
        assert_eq!(relation.rows.maximum(), 4);
        assert_eq!(relation.execute_micros.count(), 0);
        assert_eq!(statement.rows.maximum(), 10);
        assert_eq!(statement.execute_micros.count(), 1);
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn persisted_writer_models_replace_reader_local_models() {
        let family = fingerprint(1);
        let mut registry = HotRegistry::new(16);
        for index in 0..3 {
            registry.absorb(&observation(1, index, 10), Duration::ZERO);
        }

        let mut writer_model = QueryModel::new(family, ObservationModelKind::Statement);
        writer_model.executions = 20;
        writer_model.rows.record(900);
        let persisted = vec![writer_model];

        let stats = distill_with_persisted(
            &registry,
            &persisted,
            &[(family, 10)],
            None,
            CollectorReport::default(),
            None,
            Duration::ZERO,
        );
        let model = stats.statement_models.get(&family).expect("writer model");
        assert_eq!(model.retained_executions, 20);
        assert_eq!(model.rows_max, 900);
        assert_eq!(stats.frequency(&family), 10);
    }

    #[test]
    fn synopsis_drift_survives_a_survey_and_an_overlapping_flush() {
        use crate::engine::catalog::identity::SchemaId;
        use crate::engine::planner::models::{SynopsisCoverage, SynopsisModel};

        let table = SchemaId::new(7).unwrap();
        let synopsis = |collected_at_unix_micros| SynopsisModel {
            table,
            observed_rows: 100,
            coverage: SynopsisCoverage::Complete,
            sample_size: 100,
            changes_since_collection: 0,
            table_existence_generation: 0,
            collected_at_unix_micros,
            catalog_version: 1,
            columns: Vec::new(),
            column_groups: Vec::new(),
            predicate_conditioned_degrees: Vec::new(),
        };
        let mutation = |affected| {
            let mut observation = observation(1, 1, affected);
            observation.mutated = Some(table);
            observation
        };
        let mut registry = HotRegistry::new(16);
        registry.set_synopses(vec![synopsis(1)]);
        registry.synopses_dirty = false;

        registry.absorb(&mutation(5), Duration::ZERO);
        let request = registry.survey_request(&StatisticsConfig::default(), Duration::ZERO);
        assert_eq!(request.changed.get(&table), Some(&5));
        registry.absorb(&mutation(2), Duration::ZERO);
        registry.set_synopsis(synopsis(2), 5);
        assert_eq!(registry.synopses[&table].changes_since_collection, 2);

        let publishing = registry.take_batch();
        assert_eq!(publishing.synopses[0].changes_since_collection, 2);
        registry.absorb(&mutation(3), Duration::ZERO);
        registry.batch_published(&publishing);
        let next = registry.take_batch();
        assert_eq!(next.synopses[0].changes_since_collection, 5);
    }

    #[test]
    fn synopsis_loading_includes_writes_observed_during_startup() {
        use crate::engine::catalog::identity::SchemaId;
        use crate::engine::planner::models::{SynopsisCoverage, SynopsisModel};

        let table = SchemaId::new(7).unwrap();
        let mut mutation = observation(1, 1, 4);
        mutation.mutated = Some(table);
        let mut registry = HotRegistry::new(16);
        registry.absorb(&mutation, Duration::ZERO);
        registry.hydrate_synopses(vec![SynopsisModel {
            table,
            observed_rows: 100,
            coverage: SynopsisCoverage::Complete,
            sample_size: 100,
            changes_since_collection: 6,
            table_existence_generation: 0,
            collected_at_unix_micros: 1,
            catalog_version: 1,
            columns: Vec::new(),
            column_groups: Vec::new(),
            predicate_conditioned_degrees: Vec::new(),
        }]);

        assert_eq!(registry.synopses[&table].changes_since_collection, 10);
        assert_eq!(registry.take_batch().synopses.len(), 1);
    }

    #[test]
    fn a_moved_semantic_stamp_restarts_a_model_rather_than_blending_it() {
        use crate::engine::planner::models::DependencyStamp;

        let mut registry = HotRegistry::new(16);
        for _ in 0..10 {
            let mut observed = observation(1, 1, 100);
            observed.stamp = DependencyStamp {
                semantic: 7,
                access: 7,
            };
            registry.absorb(&observed, Duration::ZERO);
        }
        assert_eq!(
            registry
                .model(ObservationModelKind::Statement, &fingerprint(1))
                .unwrap()
                .executions,
            10
        );

        let mut replaced = observation(1, 1, 3);
        replaced.stamp = DependencyStamp {
            semantic: 9,
            access: 7,
        };
        registry.absorb(&replaced, Duration::ZERO);

        let model = registry
            .model(ObservationModelKind::Statement, &fingerprint(1))
            .unwrap();
        assert_eq!(model.executions, 1);
        assert_eq!(model.stamp.semantic, 9);
        assert!(
            model.rows.quantile_upper_bound(0.5) < 100,
            "the pre-change distribution must not survive"
        );
    }

    #[test]
    fn an_access_path_change_alone_preserves_accumulated_evidence() {
        use crate::engine::planner::models::DependencyStamp;

        let mut registry = HotRegistry::new(16);
        for index in 0..10 {
            let mut observed = observation(1, 1, 100);
            observed.stamp = DependencyStamp {
                semantic: 7,
                access: index,
            };
            registry.absorb(&observed, Duration::ZERO);
        }
        assert_eq!(
            registry
                .model(ObservationModelKind::Statement, &fingerprint(1))
                .unwrap()
                .executions,
            10
        );
    }

    #[test]
    fn registry_stays_bounded_and_keeps_the_hot_set() {
        let mut registry = HotRegistry::new(16);
        for _ in 0..50 {
            registry.absorb(&observation(1, 1, 10), Duration::ZERO);
        }
        for seed in 10..200 {
            registry.absorb(&observation(seed, seed, 1), Duration::ZERO);
        }
        assert!(registry.len() <= 16);
        assert!(registry.evicted() > 0);
        assert!(
            registry
                .model(ObservationModelKind::Statement, &fingerprint(1))
                .is_some(),
            "the hot family must survive eviction pressure"
        );
    }

    #[test]
    fn sketch_estimates_never_undercount_and_decay_halves() {
        let mut sketch = FrequencySketch::new(1024);
        let hot = fingerprint(7);
        for _ in 0..100 {
            sketch.record(&hot);
        }
        assert!(sketch.estimate(&hot) >= 100);
        sketch.decay();
        assert!(sketch.estimate(&hot) >= 50);
        assert!(sketch.estimate(&hot) < 100);
    }

    #[test]
    fn a_returned_frequency_batch_ages_before_retry() {
        let family = fingerprint(7);
        let mut registry = HotRegistry::new(16);
        registry.merge_relayed_frequency(family, 80);
        let rejected = registry.take_batch();

        registry.decay();
        registry.decay();
        registry.batch_returned(rejected);

        let retried = registry.take_batch();
        assert_eq!(retried.frequency, vec![(family, 20)]);
        assert_eq!(retried.frequency_decays, 2);
    }

    #[test]
    fn a_known_frequency_family_keeps_counting_at_the_family_limit() {
        let mut registry = HotRegistry::new(16);
        for seed in 0..64 {
            registry.merge_relayed_frequency(fingerprint(seed), 1);
        }
        let known = fingerprint(0);
        registry.merge_relayed_frequency(known, 4);

        let batch = registry.take_batch();
        let count = batch
            .frequency
            .iter()
            .find_map(|(family, count)| (*family == known).then_some(*count));
        assert_eq!(count, Some(5));
    }

    #[test]
    fn histogram_quantiles_bracket_recorded_values() {
        let mut histogram = Log2Histogram::default();
        for value in [1u64, 2, 3, 100, 100, 100, 100, 5000] {
            histogram.record(value);
        }
        assert_eq!(histogram.count(), 8);
        assert!(histogram.quantile_upper_bound(0.5) >= 100);
        assert!(histogram.quantile_upper_bound(0.5) <= 127);
        assert!(histogram.quantile_upper_bound(1.0) >= 5000);
    }

    #[test]
    fn model_merge_keeps_the_latest_wall_time_in_any_order() {
        let family = fingerprint(1);
        let mut early = QueryModel::new(family, ObservationModelKind::Statement);
        early.last_seen = Duration::from_secs(100);
        early.duration_ewma_micros = 100.0;
        let mut late = QueryModel::new(family, ObservationModelKind::Statement);
        late.last_seen = Duration::from_secs(200);
        late.duration_ewma_micros = 50.0;

        let mut forward = early.clone();
        forward.merge(&late);
        let mut backward = late;
        backward.merge(&early);

        assert_eq!(forward.last_seen, Duration::from_secs(200));
        assert_eq!(backward.last_seen, Duration::from_secs(200));
        assert_eq!(forward.duration_ewma_micros, 50.0);
        assert_eq!(backward.duration_ewma_micros, 50.0);
    }

    #[tokio::test]
    async fn runner_drains_the_collector_and_publishes_snapshots() {
        let runner = StatisticsRunner::start(
            Arc::new(crate::runtime::SystemRuntime),
            StatisticsConfig {
                publish_interval: Duration::from_millis(10),
                ..StatisticsConfig::default()
            },
            None,
            None,
        );
        let collector = runner.collector();
        use crate::engine::exec::observe::ExecutionObserver as _;
        for index in 0..20 {
            collector.statement(observation(1, index, 50));
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let stats = runner.stats();
            if stats.absorbed == 20 {
                let model = stats.statement_models.get(&fingerprint(1)).unwrap();
                assert_eq!(model.retained_executions, 20);
                assert!(model.rows_p50_upper_bound >= 50);
                assert!(stats.frequency(&fingerprint(1)) >= 20);
                assert_eq!(stats.dropped, 0);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "snapshot never published: absorbed={}",
                stats.absorbed
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        runner.shutdown().await;
    }

    struct TestPhysicalTelemetry {
        snapshot: std::sync::Mutex<crate::engine::kv::telemetry::PhysicalTelemetrySnapshot>,
    }

    impl crate::engine::kv::telemetry::PhysicalTelemetry for TestPhysicalTelemetry {
        fn snapshot(&self) -> crate::engine::kv::telemetry::PhysicalTelemetrySnapshot {
            self.snapshot.lock().expect("telemetry snapshot").clone()
        }
    }

    struct TelemetrySource {
        telemetry: Arc<TestPhysicalTelemetry>,
    }

    #[async_trait::async_trait]
    impl StatisticsSource for TelemetrySource {
        async fn load_models(&self, _limit: usize) -> Vec<QueryModel> {
            Vec::new()
        }

        async fn load_synopses(&self) -> Vec<SynopsisModel> {
            Vec::new()
        }

        fn physical_telemetry(
            &self,
        ) -> Option<crate::engine::kv::telemetry::SharedPhysicalTelemetry> {
            Some(self.telemetry.clone())
        }
    }

    #[tokio::test]
    async fn runner_publishes_instance_local_physical_cost_calibration() {
        let telemetry = Arc::new(TestPhysicalTelemetry {
            snapshot: std::sync::Mutex::new(physical_snapshot(
                "test",
                0,
                0,
                vec![0, 0, 0],
                0,
                (0, 0),
            )),
        });
        let runner = StatisticsRunner::start(
            Arc::new(crate::runtime::SystemRuntime),
            StatisticsConfig {
                publish_interval: Duration::from_millis(10),
                ..StatisticsConfig::default()
            },
            Some(Arc::new(TelemetrySource {
                telemetry: telemetry.clone(),
            })),
            None,
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
        *telemetry.snapshot.lock().expect("telemetry snapshot") =
            physical_snapshot("test", 2, 0, vec![0, 2, 0], 4_000, (2, 1));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let stats = runner.stats();
            if let Some(cost) = &stats.physical_cost {
                assert_eq!(cost.backend, "test");
                assert_eq!(cost.requests[0].observed_requests, 2);
                assert_eq!(cost.caches[0].hit_rate_ppm, 500_000);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "physical cost calibration was not published"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        runner.shutdown().await;
    }

    struct BlockingSink {
        started: tokio::sync::Notify,
        release: tokio::sync::Semaphore,
    }

    struct RejectingSink {
        attempts: std::sync::atomic::AtomicUsize,
    }

    struct BlockingSource {
        started: tokio::sync::Notify,
        release: tokio::sync::Semaphore,
    }

    #[async_trait::async_trait]
    impl StatisticsSource for BlockingSource {
        async fn load_models(&self, _limit: usize) -> Vec<QueryModel> {
            self.started.notify_one();
            self.release
                .acquire()
                .await
                .expect("release permit")
                .forget();
            Vec::new()
        }

        async fn load_synopses(&self) -> Vec<SynopsisModel> {
            Vec::new()
        }
    }

    #[async_trait::async_trait]
    impl StatisticsSink for BlockingSink {
        async fn publish(&self, _batch: StatisticsBatch) -> Result<(), StatisticsBatch> {
            self.started.notify_one();
            self.release
                .acquire()
                .await
                .expect("release permit")
                .forget();
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl StatisticsSink for RejectingSink {
        async fn publish(&self, batch: StatisticsBatch) -> Result<(), StatisticsBatch> {
            self.attempts.fetch_add(1, Ordering::Relaxed);
            Err(batch)
        }
    }

    #[tokio::test]
    async fn shutdown_publication_stops_when_the_sink_returns_the_batch() {
        let sink = RejectingSink {
            attempts: std::sync::atomic::AtomicUsize::new(0),
        };
        let batch = StatisticsBatch {
            frequency: vec![(fingerprint(1), 1)],
            ..StatisticsBatch::default()
        };

        tokio::time::timeout(Duration::from_secs(1), finish_publication(&sink, batch))
            .await
            .expect("shutdown publication did not stop");

        assert_eq!(sink.attempts.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn a_slow_sink_does_not_stop_observation_drain() {
        use crate::engine::exec::observe::ExecutionObserver as _;

        let sink = Arc::new(BlockingSink {
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        });
        let runner = StatisticsRunner::start(
            Arc::new(crate::runtime::SystemRuntime),
            StatisticsConfig {
                publish_interval: Duration::from_millis(10),
                flush_every: 1,
                ..StatisticsConfig::default()
            },
            None,
            Some(sink.clone()),
        );
        let collector = runner.collector();
        collector.statement(observation(1, 1, 10));
        tokio::time::timeout(Duration::from_secs(2), sink.started.notified())
            .await
            .expect("sink did not start");

        for index in 0..20 {
            collector.statement(observation(1, index, 20));
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while runner.stats().absorbed != 21 {
            assert!(
                std::time::Instant::now() < deadline,
                "observation drain waited for the sink"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        sink.release.add_permits(4);
        runner.shutdown().await;
    }

    #[tokio::test]
    async fn initial_model_loading_does_not_stop_observation_drain() {
        use crate::engine::exec::observe::ExecutionObserver as _;

        let source = Arc::new(BlockingSource {
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        });
        let runner = StatisticsRunner::start(
            Arc::new(crate::runtime::SystemRuntime),
            StatisticsConfig {
                publish_interval: Duration::from_millis(10),
                ..StatisticsConfig::default()
            },
            Some(source.clone()),
            None,
        );
        tokio::time::timeout(Duration::from_secs(2), source.started.notified())
            .await
            .expect("source did not start");
        let collector = runner.collector();
        for index in 0..20 {
            collector.statement(observation(1, index, 20));
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while runner.stats().absorbed != 20 {
            assert!(
                std::time::Instant::now() < deadline,
                "observation drain waited for initial model loading"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        source.release.add_permits(1);
        runner.shutdown().await;
    }

    #[tokio::test]
    async fn models_and_table_changes_survive_a_runner_restart() {
        use crate::engine::kv::TransactionalKv;

        let store: Arc<dyn TransactionalKv> = Arc::new(
            crate::engine::kv::slatedb::Store::memory("stats-flush")
                .await
                .unwrap(),
        );
        let slate = Arc::new(SlateStatistics::new(store.clone()));
        let config = || StatisticsConfig {
            publish_interval: Duration::from_millis(10),
            flush_every: 1,
            ..StatisticsConfig::default()
        };
        let first = StatisticsRunner::start(
            Arc::new(crate::runtime::SystemRuntime),
            config(),
            Some(slate.clone()),
            Some(slate.clone()),
        );
        let collector = first.collector();
        use crate::engine::exec::observe::ExecutionObserver as _;
        let mutated = crate::engine::catalog::identity::SchemaId::new(9).unwrap();
        for index in 0..5 {
            let mut observed = observation(1, index, 40);
            observed.mutated = Some(mutated);
            observed.kv = KvWork {
                gets: u64::from(index) + 1,
                scans: 1,
                iterated: 40,
                bytes_read: 400,
                bytes_written: 100,
                ..KvWork::default()
            };
            collector.statement(observed);
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while first.stats().absorbed != 5 {
            assert!(std::time::Instant::now() < deadline, "never absorbed");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        first.shutdown().await;

        let raw = crate::engine::kv::Kv::get(
            store.as_ref(),
            &crate::engine::kv::keys::statistics_table_changes_key(9),
        )
        .await
        .unwrap()
        .expect("table change counter persisted");
        assert_eq!(String::from_utf8(raw.to_vec()).unwrap(), "200");

        let second = StatisticsRunner::start(
            Arc::new(crate::runtime::SystemRuntime),
            config(),
            Some(slate.clone()),
            Some(slate.clone()),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let stats = second.stats();
            if let Some(model) = stats.statement_models.get(&fingerprint(1)) {
                assert_eq!(model.retained_executions, 5);
                assert!(model.rows_p50_upper_bound >= 40);
                assert_eq!(stats.frequency(&fingerprint(1)), 5);
                assert_eq!(model.plan_profiles.len(), 1);
                assert_eq!(model.plan_profiles[0].executions, 5);
                assert_eq!(model.plan_profiles[0].execute_micros_p50_upper_bound(), 400);
                let cost = model.resources.cost().expect("resource cost");
                assert_eq!(cost.observed_executions, 5);
                assert_eq!(cost.gets.maximum, 5);
                assert_eq!(cost.scans.maximum, 1);
                assert_eq!(cost.iterated.maximum, 40);
                assert_eq!(cost.bytes_read.maximum, 400);
                assert_eq!(cost.bytes_written.maximum, 100);
                assert_eq!(
                    model.plan_profiles[0]
                        .resources
                        .cost()
                        .expect("plan resource cost")
                        .gets
                        .maximum,
                    5
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "persisted model never loaded"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        second.shutdown().await;
    }

    #[tokio::test]
    async fn synopsis_drift_survives_persistence() {
        use crate::engine::catalog::identity::SchemaId;
        use crate::engine::kv::TransactionalKv;
        use crate::engine::planner::models::{
            ColumnGroupSynopsis, ColumnSynopsis, DEGREE_SEQUENCE_FORMAT_VERSION,
            DegreeSequenceNorms, DegreeSequenceSegment, DegreeSequenceSynopsis,
            MostCommonColumnGroup, MostCommonValue, PREDICATE_CONDITIONED_DEGREE_FORMAT_VERSION,
            PredicateConditionedDegreeSynopsis, PredicateConditionedDegreeValue,
            RANGE_DISTRIBUTION_FORMAT_VERSION, RangeDistribution, RangeDistributionBucket,
            SynopsisCountBounds, SynopsisCoverage, SynopsisModel, SynopsisValue,
        };

        let store: Arc<dyn TransactionalKv> = Arc::new(
            crate::engine::kv::slatedb::Store::memory("stats-synopsis-drift")
                .await
                .unwrap(),
        );
        let slate = SlateStatistics::new(store);
        let table = SchemaId::new(7).unwrap();
        assert!(
            slate
                .publish(StatisticsBatch {
                    synopses: vec![SynopsisModel {
                        table,
                        observed_rows: 100,
                        coverage: SynopsisCoverage::Complete,
                        sample_size: 100,
                        changes_since_collection: 12,
                        table_existence_generation: 3,
                        collected_at_unix_micros: 9,
                        catalog_version: 4,
                        columns: vec![ColumnSynopsis {
                            column: SchemaId::new(8).unwrap(),
                            value_generation: 5,
                            null_fraction: 0.1,
                            null_count: 10,
                            distinct: 3,
                            distinct_is_exact: true,
                            average_width: 4,
                            maximum_width: Some(4),
                            minimum: Some("\"cold\"".into()),
                            maximum: Some("\"hot\"".into()),
                            most_common_values: vec![MostCommonValue {
                                value: SynopsisValue::Text("hot".into()),
                                frequency: 70,
                                maximum_error: 2,
                            }],
                            range_distribution: Some(RangeDistribution {
                                format_version: RANGE_DISTRIBUTION_FORMAT_VERSION,
                                coverage: SynopsisCoverage::Complete,
                                sample_size: 100,
                                value_generation: 5,
                                collected_row_count: 100,
                                buckets: vec![RangeDistributionBucket {
                                    lower: SynopsisValue::Text("cold".into()),
                                    upper: SynopsisValue::Text("hot".into()),
                                    rows: SynopsisCountBounds::exact(90),
                                    cumulative_rows: SynopsisCountBounds::exact(90),
                                    lower_endpoint_rows: SynopsisCountBounds::exact(20),
                                    upper_endpoint_rows: SynopsisCountBounds::exact(70),
                                }],
                            }),
                            degree_sequence: Some(DegreeSequenceSynopsis {
                                format_version: DEGREE_SEQUENCE_FORMAT_VERSION,
                                coverage: SynopsisCoverage::Complete,
                                sample_size: 100,
                                value_generations: vec![5],
                                collected_row_count: 100,
                                non_null_rows: 90,
                                distinct_values: 3,
                                distinct_is_exact: true,
                                norms: DegreeSequenceNorms {
                                    l1: 90,
                                    l2_upper: 73,
                                    l_infinity: 70,
                                    exact: true,
                                },
                                segments: vec![DegreeSequenceSegment {
                                    rank_start: 0,
                                    rank_end: 3,
                                    frequency_upper: 70,
                                }],
                            }),
                        }],
                        column_groups: vec![ColumnGroupSynopsis {
                            columns: vec![SchemaId::new(8).unwrap(), SchemaId::new(9).unwrap()],
                            value_generations: vec![5, 6],
                            null_count: 4,
                            distinct: 8,
                            distinct_is_exact: true,
                            most_common_values: vec![MostCommonColumnGroup {
                                values: vec![
                                    SynopsisValue::Text("hot".into()),
                                    SynopsisValue::Bool(true),
                                ],
                                frequency: 60,
                                maximum_error: 3,
                            }],
                            degree_sequence: Some(DegreeSequenceSynopsis {
                                format_version: DEGREE_SEQUENCE_FORMAT_VERSION,
                                coverage: SynopsisCoverage::Complete,
                                sample_size: 100,
                                value_generations: vec![5, 6],
                                collected_row_count: 100,
                                non_null_rows: 96,
                                distinct_values: 8,
                                distinct_is_exact: true,
                                norms: DegreeSequenceNorms {
                                    l1: 96,
                                    l2_upper: 62,
                                    l_infinity: 60,
                                    exact: true,
                                },
                                segments: vec![DegreeSequenceSegment {
                                    rank_start: 0,
                                    rank_end: 8,
                                    frequency_upper: 60,
                                }],
                            }),
                        }],
                        predicate_conditioned_degrees: vec![PredicateConditionedDegreeSynopsis {
                            format_version: PREDICATE_CONDITIONED_DEGREE_FORMAT_VERSION,
                            coverage: SynopsisCoverage::Complete,
                            sample_size: 100,
                            join_columns: vec![SchemaId::new(8).unwrap()],
                            join_value_generations: vec![5],
                            predicate_column: SchemaId::new(9).unwrap(),
                            predicate_value_generation: 6,
                            values: vec![PredicateConditionedDegreeValue {
                                predicate_value: SynopsisValue::Bool(true),
                                matching_rows: 60,
                                non_null_join_rows: 58,
                                distinct_join_values: 3,
                                norms: DegreeSequenceNorms {
                                    l1: 58,
                                    l2_upper: 50,
                                    l_infinity: 48,
                                    exact: true,
                                },
                            }],
                        },],
                    }],
                    ..StatisticsBatch::default()
                })
                .await
                .is_ok()
        );

        let loaded = slate.load_synopses().await;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].changes_since_collection, 12);
        assert_eq!(loaded[0].table_existence_generation, 3);
        assert_eq!(loaded[0].columns[0].value_generation, 5);
        assert_eq!(loaded[0].columns[0].null_count, 10);
        assert_eq!(loaded[0].columns[0].maximum_width, Some(4));
        assert_eq!(loaded[0].columns[0].most_common_values[0].frequency, 70);
        assert_eq!(loaded[0].columns[0].most_common_values[0].maximum_error, 2);
        assert_eq!(
            loaded[0].columns[0]
                .degree_sequence
                .as_ref()
                .unwrap()
                .norms
                .l2_upper,
            73
        );
        let distribution = loaded[0].columns[0]
            .range_distribution
            .as_ref()
            .expect("persisted range distribution");
        assert_eq!(distribution.value_generation, 5);
        assert_eq!(distribution.buckets[0].rows.lower_bound, 90);
        assert_eq!(
            distribution.buckets[0].cumulative_rows,
            SynopsisCountBounds::exact(90)
        );
        assert_eq!(
            distribution.buckets[0].upper_endpoint_rows,
            SynopsisCountBounds::exact(70)
        );
        assert_eq!(loaded[0].column_groups[0].value_generations, [5, 6]);
        assert_eq!(loaded[0].column_groups[0].null_count, 4);
        assert_eq!(
            loaded[0].column_groups[0]
                .degree_sequence
                .as_ref()
                .unwrap()
                .value_generations,
            [5, 6]
        );
        assert_eq!(
            loaded[0].column_groups[0].most_common_values[0].frequency,
            60
        );
        assert_eq!(
            loaded[0].column_groups[0].most_common_values[0].maximum_error,
            3
        );
        assert_eq!(
            loaded[0].predicate_conditioned_degrees[0].join_value_generations,
            [5]
        );
        assert_eq!(
            loaded[0].predicate_conditioned_degrees[0].values[0]
                .norms
                .l_infinity,
            48
        );
    }

    #[tokio::test]
    async fn persisted_loading_keeps_only_the_most_observed_models() {
        use crate::engine::kv::TransactionalKv;

        let store: Arc<dyn TransactionalKv> = Arc::new(
            crate::engine::kv::slatedb::Store::memory("stats-load-bound")
                .await
                .unwrap(),
        );
        let slate = SlateStatistics::new(store);
        let mut models = Vec::new();
        for seed in 0..20u8 {
            let family = fingerprint(seed);
            let mut model = QueryModel::new(family, ObservationModelKind::Statement);
            model.executions = u64::from(seed) + 1;
            models.push((family, model));
        }
        assert!(
            slate
                .publish(StatisticsBatch {
                    models,
                    ..StatisticsBatch::default()
                })
                .await
                .is_ok()
        );

        let loaded = slate.load_models(4).await;
        assert_eq!(loaded.len(), 4);
        assert!(loaded.iter().all(|model| model.executions >= 17));
    }

    #[tokio::test]
    async fn persisted_model_catalog_bounds_durable_models() {
        use crate::engine::kv::TransactionalKv;

        let store: Arc<dyn TransactionalKv> = Arc::new(
            crate::engine::kv::slatedb::Store::memory("stats-catalog-bound")
                .await
                .unwrap(),
        );
        let slate = SlateStatistics::with_model_capacity(store.clone(), 16);
        let mut models = Vec::new();
        let mut frequency = Vec::new();
        for seed in 0..40u8 {
            let family = fingerprint(seed);
            let mut model = QueryModel::new(family, ObservationModelKind::Statement);
            model.executions = u64::from(seed) + 1;
            models.push((family, model));
            frequency.push((family, u32::from(seed) + 1));
        }
        assert!(
            slate
                .publish(StatisticsBatch {
                    models,
                    frequency,
                    ..StatisticsBatch::default()
                })
                .await
                .is_ok()
        );

        let loaded = slate.load_models(100).await;
        assert_eq!(loaded.len(), 16);
        assert!(loaded.iter().all(|model| model.executions >= 25));
        assert_eq!(slate.load_frequencies(100).await.len(), 16);
        assert_eq!(
            prefix_count(
                store.as_ref(),
                crate::engine::kv::keys::statistics_frequency_prefix()
            )
            .await,
            16
        );
        assert!(
            crate::engine::kv::Kv::get(
                store.as_ref(),
                &crate::engine::kv::keys::statistics_model_catalog_key(),
            )
            .await
            .unwrap()
            .is_some()
        );
    }

    #[tokio::test]
    async fn persisted_frequency_uses_lazy_decay_and_requires_a_model() {
        use crate::engine::kv::TransactionalKv;

        let store: Arc<dyn TransactionalKv> = Arc::new(
            crate::engine::kv::slatedb::Store::memory("stats-frequency-decay")
                .await
                .unwrap(),
        );
        let slate = SlateStatistics::new(store.clone());
        let family = fingerprint(1);
        let absent = fingerprint(2);
        let mut model = QueryModel::new(family, ObservationModelKind::Statement);
        model.executions = 80;
        assert!(
            slate
                .publish(StatisticsBatch {
                    models: vec![(family, model)],
                    frequency: vec![(family, 80), (absent, 90)],
                    ..StatisticsBatch::default()
                })
                .await
                .is_ok()
        );
        assert_eq!(slate.load_frequencies(16).await, vec![(family, 80)]);
        assert!(
            crate::engine::kv::Kv::get(
                store.as_ref(),
                &crate::engine::kv::keys::statistics_frequency_key(&absent.to_bytes())
            )
            .await
            .unwrap()
            .is_none()
        );
        assert!(
            slate
                .publish(StatisticsBatch {
                    frequency_decays: 2,
                    ..StatisticsBatch::default()
                })
                .await
                .is_ok()
        );
        assert_eq!(slate.load_frequencies(16).await, vec![(family, 20)]);
    }

    #[tokio::test]
    async fn persistence_keeps_relation_and_statement_models_for_one_family() {
        use crate::engine::kv::TransactionalKv;

        let store: Arc<dyn TransactionalKv> = Arc::new(
            crate::engine::kv::slatedb::Store::memory("stats-model-kinds")
                .await
                .unwrap(),
        );
        let slate = SlateStatistics::new(store);
        let family = fingerprint(1);
        let mut relation = QueryModel::new(family, ObservationModelKind::Relation);
        relation.executions = 3;
        relation.rows.record(4);
        let mut statement = QueryModel::new(family, ObservationModelKind::Statement);
        statement.executions = 5;
        statement.rows.record(10);
        assert!(
            slate
                .publish(StatisticsBatch {
                    models: vec![(family, relation), (family, statement)],
                    ..StatisticsBatch::default()
                })
                .await
                .is_ok()
        );

        let loaded = slate.load_models(16).await;
        assert!(loaded.iter().any(|model| {
            model.family == family
                && model.kind == ObservationModelKind::Relation
                && model.rows.maximum() == 4
        }));
        assert!(loaded.iter().any(|model| {
            model.family == family
                && model.kind == ObservationModelKind::Statement
                && model.rows.maximum() == 10
        }));
    }

    #[test]
    fn stored_models_without_resource_profiles_remain_readable() {
        let family = fingerprint(1);
        let mut model = QueryModel::new(family, ObservationModelKind::Statement);
        model.executions = 1;
        let mut profile = PlanProfile::new(fingerprint(2), 3);
        profile.record(4, 5);
        model.plan_profiles.push(profile);
        let mut stored = serde_json::to_value(StoredModel {
            format: STATISTICS_MODEL_FORMAT,
            model,
        })
        .unwrap();
        stored["model"].as_object_mut().unwrap().remove("resources");
        stored["model"]["plan_profiles"][0]
            .as_object_mut()
            .unwrap()
            .remove("resources");

        let bytes = serde_json::to_vec(&stored).unwrap();
        let decoded = decode_stored_model(&bytes, family, ObservationModelKind::Statement)
            .expect("stored model");
        assert!(decoded.resources.cost().is_none());
        assert!(decoded.plan_profiles[0].resources.cost().is_none());
    }

    /// A publisher overwrites what it stored, so it must continue those models
    /// rather than start over. Without that, every restart replaces the whole
    /// accumulated history with whatever the new process has seen since boot.
    #[tokio::test]
    async fn stored_evidence_accumulates_across_restarts_rather_than_resetting() {
        use crate::engine::exec::observe::ExecutionObserver as _;
        use crate::engine::kv::TransactionalKv;

        let store: Arc<dyn TransactionalKv> = Arc::new(
            crate::engine::kv::slatedb::Store::memory("stats-accumulate")
                .await
                .unwrap(),
        );
        let slate = Arc::new(SlateStatistics::new(store.clone()));
        let serve = async |executions: u64| {
            let runner = StatisticsRunner::start(
                Arc::new(crate::runtime::SystemRuntime),
                StatisticsConfig {
                    publish_interval: Duration::from_millis(10),
                    flush_every: 1,
                    ..StatisticsConfig::default()
                },
                Some(slate.clone()),
                Some(slate.clone()),
            );
            let collector = runner.collector();
            for index in 0..executions {
                collector.statement(observation(1, index as u8, 40));
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while runner.stats().absorbed != executions {
                assert!(std::time::Instant::now() < deadline, "never absorbed");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let retained = runner
                .stats()
                .statement_models
                .get(&fingerprint(1))
                .expect("the family this process observed")
                .retained_executions;
            runner.shutdown().await;
            retained
        };

        assert_eq!(serve(5).await, 5);
        assert_eq!(
            serve(3).await,
            8,
            "a restart discarded the evidence gathered before it"
        );
        assert_eq!(serve(2).await, 10);
    }

    #[tokio::test]
    async fn a_source_only_instance_refreshes_published_models() {
        use crate::engine::kv::TransactionalKv;

        let store: Arc<dyn TransactionalKv> = Arc::new(
            crate::engine::kv::slatedb::Store::memory("stats-reader-refresh")
                .await
                .unwrap(),
        );
        let slate = Arc::new(SlateStatistics::new(store.clone()));

        // A writer gathers and publishes.
        let writer = StatisticsRunner::start(
            Arc::new(crate::runtime::SystemRuntime),
            StatisticsConfig {
                publish_interval: Duration::from_millis(10),
                flush_every: 1,
                ..StatisticsConfig::default()
            },
            Some(slate.clone()),
            Some(slate.clone()),
        );
        use crate::engine::exec::observe::ExecutionObserver as _;
        let collector = writer.collector();
        for index in 0..12 {
            collector.statement(observation(1, index, 70));
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while writer.stats().absorbed != 12 {
            assert!(
                std::time::Instant::now() < deadline,
                "writer never absorbed"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        writer.shutdown().await;

        // A reader instance holds no sink: it cannot publish, but it must be
        // able to plan from what the writer established.
        let reader = StatisticsRunner::start(
            Arc::new(crate::runtime::SystemRuntime),
            StatisticsConfig {
                publish_interval: Duration::from_millis(10),
                refresh_every: 1,
                ..StatisticsConfig::default()
            },
            Some(slate.clone()),
            None,
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let planning_identity = loop {
            let stats = reader.stats();
            if let Some(model) = stats.statement_models.get(&fingerprint(1)) {
                assert_eq!(model.retained_executions, 12);
                assert!(model.rows_p50_upper_bound >= 70);
                let planning = reader.planning_stats();
                assert_eq!(
                    planning.scope,
                    crate::engine::planner::models::StatisticsScope::Persisted
                );
                assert_eq!(planning.snapshot_identity.len(), 32);
                assert_eq!(
                    planning
                        .statement_models
                        .get(&fingerprint(1))
                        .expect("persisted planning model")
                        .retained_executions,
                    12
                );
                break planning.snapshot_identity.clone();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the reader never picked up the published model"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        };

        let peer = StatisticsRunner::start(
            Arc::new(crate::runtime::SystemRuntime),
            StatisticsConfig {
                publish_interval: Duration::from_millis(10),
                refresh_every: 1,
                ..StatisticsConfig::default()
            },
            Some(slate.clone()),
            None,
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let planning = peer.planning_stats();
            if planning.snapshot_identity == planning_identity
                && planning.statement_models.contains_key(&fingerprint(1))
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the peer reader never loaded the same snapshot"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let reader_collector = reader.collector();
        reader_collector.statement(observation(2, 1, 9));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while reader.stats().absorbed == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "reader never absorbed its local observation"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            reader
                .stats()
                .statement_models
                .contains_key(&fingerprint(2))
        );
        assert!(
            !reader
                .planning_stats()
                .statement_models
                .contains_key(&fingerprint(2))
        );
        assert_eq!(reader.planning_stats().snapshot_identity, planning_identity);
        assert_eq!(peer.planning_stats().snapshot_identity, planning_identity);
        peer.shutdown().await;
        reader.shutdown().await;
    }

    #[tokio::test]
    async fn a_reader_adopts_relayed_evidence_only_after_writer_publication() {
        use crate::engine::kv::TransactionalKv;

        let store: Arc<dyn TransactionalKv> = Arc::new(
            crate::engine::kv::slatedb::Store::memory("stats-relay-publication")
                .await
                .unwrap(),
        );
        let slate = Arc::new(SlateStatistics::new(store));
        let writer = StatisticsRunner::start(
            Arc::new(crate::runtime::SystemRuntime),
            StatisticsConfig {
                publish_interval: Duration::from_millis(5),
                flush_every: 1_000_000,
                ..StatisticsConfig::default()
            },
            Some(slate.clone()),
            Some(slate.clone()),
        );
        let reader = StatisticsRunner::start(
            Arc::new(crate::runtime::SystemRuntime),
            StatisticsConfig {
                publish_interval: Duration::from_millis(5),
                refresh_every: 1,
                ..StatisticsConfig::default()
            },
            Some(slate.clone()),
            None,
        );
        let family = fingerprint(9);
        let mut relayed = QueryModel::new(family, ObservationModelKind::Statement);
        relayed.executions = 6;
        assert_eq!(
            writer
                .ingest()
                .submit(super::super::relay::ObservationBatch {
                    format: super::super::relay::RELAY_FORMAT,
                    instance: "reader-1".into(),
                    boot: "boot-1".into(),
                    sequence: 1,
                    sent_at_micros: 0,
                    families: vec![relayed],
                    frequency: vec![(family, 6)],
                    corpus: Vec::new(),
                }),
            super::super::relay::IngestOutcome::Accepted
        );

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !writer
            .planning_stats()
            .statement_models
            .contains_key(&family)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the writer did not merge relayed evidence"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            !reader
                .planning_stats()
                .statement_models
                .contains_key(&family)
        );

        writer.shutdown().await;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !reader
            .planning_stats()
            .statement_models
            .contains_key(&family)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the reader did not refresh the writer publication"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        reader.shutdown().await;
    }

    #[tokio::test]
    async fn corpus_stores_programs_once_and_logs_every_execution() {
        use crate::engine::kv::TransactionalKv;
        use crate::runtime::RuntimeEffects as _;

        let store: Arc<dyn TransactionalKv> = Arc::new(
            crate::engine::kv::slatedb::Store::memory("stats-corpus")
                .await
                .unwrap(),
        );
        let slate = Arc::new(SlateStatistics::new(store.clone()));
        let runner = StatisticsRunner::start(
            Arc::new(crate::runtime::SystemRuntime),
            StatisticsConfig {
                publish_interval: Duration::from_millis(10),
                flush_every: 1,
                capture_programs: true,
                ..StatisticsConfig::default()
            },
            Some(slate.clone()),
            Some(slate.clone()),
        );
        let collector = runner.collector();
        use crate::engine::exec::observe::ExecutionObserver as _;
        let now = crate::runtime::SystemRuntime.unix_time().as_micros() as u64;
        let record = ProgramRecord {
            canonical: br#"{"statements":[]}"#.to_vec(),
            content_hash: [7u8; 16],
            at_unix_micros: now,
            statements: 1,
            outcomes: vec![crate::engine::exec::observe::ProgramStatementOutcome {
                name: "read".into(),
                rows: 7,
            }],
        };
        collector.program(record.clone());
        collector.program(ProgramRecord {
            at_unix_micros: now + 1,
            ..record.clone()
        });

        let program_key =
            crate::engine::kv::keys::corpus_program_key(CANONICAL_FORMAT_VERSION, &[7u8; 16]);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let stored = crate::engine::kv::Kv::get(store.as_ref(), &program_key)
                .await
                .unwrap();
            if let Some(stored) = stored {
                assert_eq!(stored.to_vec(), record.canonical);
                break;
            }
            assert!(std::time::Instant::now() < deadline, "corpus never flushed");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        runner.shutdown().await;

        let mut executions = 0;
        let prefix = crate::engine::kv::keys::corpus_execution_prefix();
        let end = crate::engine::kv::key_encoding::prefix_end(&prefix).unwrap();
        let mut iterator = crate::engine::kv::Kv::scan(
            store.as_ref(),
            crate::engine::kv::KeyRange::new(prefix, end),
        )
        .await
        .unwrap();
        while let Some(entry) = iterator.next().await.unwrap() {
            let value: serde_json::Value = serde_json::from_slice(&entry.value).unwrap();
            assert_eq!(value["program"], "07".repeat(16));
            executions += 1;
        }
        assert_eq!(executions, 2);
        let replayed = slate.load_corpus(10).await.unwrap();
        assert_eq!(replayed.len(), 2);
        assert_eq!(replayed[0].outcomes, record.outcomes);
    }

    #[test]
    fn corpus_execution_without_actuals_remains_readable() {
        let stored: StoredCorpusExecution = serde_json::from_str(
            r#"{"program":"00000000000000000000000000000000","canonicalVersion":1,"statements":1}"#,
        )
        .unwrap();
        assert!(stored.outcomes.is_empty());
    }

    #[tokio::test]
    async fn corpus_retention_bounds_storage_and_erasure_clears_the_cache() {
        use crate::engine::kv::TransactionalKv;

        let store: Arc<dyn TransactionalKv> = Arc::new(
            crate::engine::kv::slatedb::Store::memory("stats-corpus-retention")
                .await
                .unwrap(),
        );
        let runtime = Arc::new(TestRuntime(std::sync::atomic::AtomicU64::new(100)));
        let slate = SlateStatistics::with_options(
            store.clone(),
            DEFAULT_REGISTRY_CAPACITY,
            runtime.clone(),
            CorpusPolicy {
                max_executions: 2,
                max_bytes: 2,
                max_age: Duration::from_micros(50),
            },
        );
        let program = |seed: u8, at_unix_micros: u64| ProgramRecord {
            canonical: vec![seed],
            content_hash: [seed; 16],
            at_unix_micros,
            statements: 1,
            outcomes: Vec::new(),
        };

        assert!(
            slate
                .publish(StatisticsBatch {
                    programs: vec![program(1, 80), program(2, 90), program(3, 100)],
                    ..StatisticsBatch::default()
                })
                .await
                .is_ok()
        );
        assert_eq!(
            prefix_count(
                store.as_ref(),
                crate::engine::kv::keys::corpus_execution_prefix()
            )
            .await,
            2
        );
        assert_eq!(
            prefix_count(
                store.as_ref(),
                crate::engine::kv::keys::corpus_program_prefix()
            )
            .await,
            2
        );
        assert!(
            crate::engine::kv::Kv::get(
                store.as_ref(),
                &crate::engine::kv::keys::corpus_program_key(CANONICAL_FORMAT_VERSION, &[1; 16])
            )
            .await
            .unwrap()
            .is_none()
        );
        assert_eq!(
            slate.corpus_maintenance(),
            Some(CorpusMaintenanceStats {
                pruned_executions: 1,
                pruned_programs: 1,
                retained_executions: 2,
                retained_programs: 2,
                retained_program_bytes: 2,
                ..CorpusMaintenanceStats::default()
            })
        );

        let mut oversized = program(5, 101);
        oversized.canonical = vec![5; 3];
        assert!(
            slate
                .publish(StatisticsBatch {
                    programs: vec![oversized],
                    ..StatisticsBatch::default()
                })
                .await
                .is_ok()
        );
        assert_eq!(
            prefix_count(
                store.as_ref(),
                crate::engine::kv::keys::corpus_execution_prefix()
            )
            .await,
            1
        );
        assert!(
            crate::engine::kv::Kv::get(
                store.as_ref(),
                &crate::engine::kv::keys::corpus_program_key(CANONICAL_FORMAT_VERSION, &[5; 16])
            )
            .await
            .unwrap()
            .is_none()
        );
        assert_eq!(
            slate.corpus_maintenance(),
            Some(CorpusMaintenanceStats {
                pruned_executions: 3,
                pruned_programs: 3,
                retained_executions: 1,
                retained_programs: 1,
                retained_program_bytes: 1,
                ..CorpusMaintenanceStats::default()
            })
        );

        runtime.0.store(200, Ordering::Relaxed);
        assert!(
            slate
                .publish(StatisticsBatch {
                    programs: vec![program(4, 200)],
                    ..StatisticsBatch::default()
                })
                .await
                .is_ok()
        );
        assert_eq!(
            prefix_count(
                store.as_ref(),
                crate::engine::kv::keys::corpus_execution_prefix()
            )
            .await,
            1
        );
        assert_eq!(
            prefix_count(
                store.as_ref(),
                crate::engine::kv::keys::corpus_program_prefix()
            )
            .await,
            1
        );
        assert_eq!(
            slate.corpus_maintenance(),
            Some(CorpusMaintenanceStats {
                expired_executions: 1,
                pruned_executions: 3,
                pruned_programs: 4,
                retained_executions: 1,
                retained_programs: 1,
                retained_program_bytes: 1,
                ..CorpusMaintenanceStats::default()
            })
        );

        assert_eq!(slate.erase_corpus().await.unwrap(), 2);
        assert_eq!(
            prefix_count(
                store.as_ref(),
                crate::engine::kv::keys::corpus_execution_prefix()
            )
            .await,
            0
        );
        assert_eq!(
            slate.corpus_maintenance(),
            Some(CorpusMaintenanceStats {
                expired_executions: 1,
                pruned_executions: 3,
                pruned_programs: 4,
                erased_executions: 1,
                erased_programs: 1,
                ..CorpusMaintenanceStats::default()
            })
        );
        assert_eq!(
            prefix_count(
                store.as_ref(),
                crate::engine::kv::keys::corpus_program_prefix()
            )
            .await,
            0
        );

        runtime.0.store(201, Ordering::Relaxed);
        assert!(
            slate
                .publish(StatisticsBatch {
                    programs: vec![program(4, 201)],
                    ..StatisticsBatch::default()
                })
                .await
                .is_ok()
        );
        assert_eq!(
            prefix_count(
                store.as_ref(),
                crate::engine::kv::keys::corpus_program_prefix()
            )
            .await,
            1
        );
        assert_eq!(
            slate.corpus_maintenance(),
            Some(CorpusMaintenanceStats {
                expired_executions: 1,
                pruned_executions: 3,
                pruned_programs: 4,
                erased_executions: 1,
                erased_programs: 1,
                retained_executions: 1,
                retained_programs: 1,
                retained_program_bytes: 1,
                ..CorpusMaintenanceStats::default()
            })
        );
    }

    #[tokio::test]
    async fn corpus_startup_maintenance_removes_invalid_entries() {
        use crate::engine::kv::{IsolationLevel, TransactionalKv as _};

        let store = Arc::new(
            crate::engine::kv::slatedb::Store::memory("stats-corpus-startup")
                .await
                .unwrap(),
        );
        let transaction = store.begin(IsolationLevel::Snapshot).await.unwrap();
        transaction
            .put(
                crate::engine::kv::keys::corpus_execution_prefix().into(),
                b"not-json".as_slice().into(),
            )
            .unwrap();
        transaction
            .put(
                crate::engine::kv::keys::corpus_program_prefix().into(),
                b"program".as_slice().into(),
            )
            .unwrap();
        transaction.commit().await.unwrap();

        let slate = SlateStatistics::new(store.clone());
        assert_eq!(slate.corpus_maintenance(), None);
        slate.initialize_corpus_maintenance().await;

        assert_eq!(
            slate.corpus_maintenance(),
            Some(CorpusMaintenanceStats {
                invalid_executions: 1,
                invalid_programs: 1,
                ..CorpusMaintenanceStats::default()
            })
        );
        assert_eq!(
            prefix_count(
                store.as_ref(),
                crate::engine::kv::keys::corpus_execution_prefix()
            )
            .await,
            0
        );
        assert_eq!(
            prefix_count(
                store.as_ref(),
                crate::engine::kv::keys::corpus_program_prefix()
            )
            .await,
            0
        );
    }

    /// The guarantee the relay rests on: evidence gathered in pieces and
    /// merged must equal evidence gathered in one place. Whatever a reader
    /// relays, the writer must end up with the model it would have had if it
    /// had served that traffic itself.
    ///
    /// Two fields are excluded by design, both documented on
    /// [`QueryModel::merge`]: the duration mean cannot be combined by
    /// addition, and the exact-variant count overstates once summed.
    #[test]
    fn merged_deltas_equal_evidence_gathered_in_one_place() {
        let together_at = Duration::from_millis(500);
        let mut together = HotRegistry::new(64);
        for index in 0..24u8 {
            together.absorb(&observation(1, index, 10 * u64::from(index)), together_at);
        }
        let whole = together
            .model(ObservationModelKind::Statement, &fingerprint(1))
            .expect("observed family");

        // The same observations, gathered by an instance that publishes a
        // delta every few executions, folded by a receiver that has never
        // seen the family.
        let mut apart = HotRegistry::new(64);
        let mut received = QueryModel::new(fingerprint(1), ObservationModelKind::Statement);
        for (index, observed) in (0..24u8).enumerate() {
            apart.absorb(
                &observation(1, observed, 10 * u64::from(observed)),
                together_at,
            );
            if index % 5 == 4 {
                for (_, delta) in apart.take_batch().models {
                    received.merge(&delta);
                }
            }
        }
        for (_, delta) in apart.take_batch().models {
            received.merge(&delta);
        }

        assert_eq!(received.executions, whole.executions);
        assert_eq!(received.estimated_executions, whole.estimated_executions);
        assert_eq!(received.q_error_max_x100, whole.q_error_max_x100);
        assert_eq!(received.stamp, whole.stamp);
        assert_eq!(received.plans, whole.plans);
        for (merged, gathered) in [
            (&received.rows, &whole.rows),
            (&received.execute_micros, &whole.execute_micros),
            (&received.bind_micros, &whole.bind_micros),
            (&received.q_errors, &whole.q_errors),
        ] {
            assert_eq!(merged.count(), gathered.count());
            assert_eq!(merged.maximum(), gathered.maximum());
            for quantile in [0.5, 0.95, 1.0] {
                assert_eq!(
                    merged.quantile_upper_bound(quantile),
                    gathered.quantile_upper_bound(quantile),
                    "quantile {quantile} differs between merged and whole evidence"
                );
            }
        }
        // The excluded field is the last value, not a blend of nothing.
        assert_eq!(received.duration_ewma_micros, whole.duration_ewma_micros);
    }

    /// The registry is a bounded hot set, so a family leaves it under
    /// pressure. What was already published must survive that: the store
    /// accumulates deltas, so a family that returns extends its stored history
    /// instead of replacing it.
    #[tokio::test]
    async fn a_family_evicted_from_the_hot_set_keeps_its_stored_history() {
        use crate::engine::kv::TransactionalKv;

        let store: Arc<dyn TransactionalKv> = Arc::new(
            crate::engine::kv::slatedb::Store::memory("stats-eviction")
                .await
                .unwrap(),
        );
        let slate = SlateStatistics::new(store.clone());
        let at = Duration::from_millis(1);
        let mut registry = HotRegistry::new(16);

        for index in 0..3u8 {
            registry.absorb(&observation(1, index, 40), at);
        }
        assert!(slate.publish(registry.take_batch()).await.is_ok());

        // Crowd the working set with families that are hotter than this one,
        // until the policy drops it.
        for family in 2..40u8 {
            for _ in 0..5 {
                registry.absorb(&observation(family, family, 7), at);
            }
        }
        assert!(
            registry
                .model(ObservationModelKind::Statement, &fingerprint(1))
                .is_none(),
            "the family was never evicted, so this proves nothing about eviction"
        );
        assert!(slate.publish(registry.take_batch()).await.is_ok());

        registry.absorb(&observation(1, 9, 40), at);
        assert!(slate.publish(registry.take_batch()).await.is_ok());

        let stored = slate.load_models(DEFAULT_REGISTRY_CAPACITY).await;
        let model = stored
            .iter()
            .find(|model| {
                model.kind == ObservationModelKind::Statement && model.family == fingerprint(1)
            })
            .expect("the evicted family is still stored");
        assert_eq!(
            model.executions, 4,
            "returning to the hot set replaced the stored history instead of extending it"
        );
        assert_eq!(model.rows.count(), 4);
    }

    /// A delta is evidence in flight. Losing the publication must cost time,
    /// not evidence, so a returned batch merges forward into what has been
    /// observed since and the next attempt carries both.
    #[test]
    fn a_returned_batch_merges_forward_instead_of_being_lost() {
        let at = Duration::from_millis(10);
        let mut registry = HotRegistry::new(64);
        for index in 0..3u8 {
            registry.absorb(&observation(1, index, 40), at);
        }
        let rejected = registry.take_batch();
        assert_eq!(rejected.models.len(), 1);
        registry.batch_returned(rejected);

        for index in 3..7u8 {
            registry.absorb(&observation(1, index, 40), at);
        }
        let retried = registry.take_batch();
        let (_, delta) = retried
            .models
            .iter()
            .find(|(family, _)| *family == fingerprint(1))
            .expect("the family observed across both attempts");
        assert_eq!(
            delta.executions, 7,
            "the returned delta did not survive into the next attempt"
        );
        assert_eq!(delta.rows.count(), 7);

        // Draining twice must not report the same evidence twice.
        assert!(registry.take_batch().models.is_empty());
    }

    /// Only the writer publishes survey results. A reader running one would
    /// consume storage bandwidth to build a synopsis nothing can read.
    #[tokio::test]
    async fn only_a_write_instance_surveys() {
        use crate::engine::catalog::identity::SchemaId;
        use crate::engine::catalog::model::{ColumnDraft, ScalarType, TableDraft};
        use crate::engine::kv::TransactionalKv;

        let store: Arc<dyn TransactionalKv> = Arc::new(
            crate::engine::kv::slatedb::Store::memory("stats-survey-role")
                .await
                .unwrap(),
        );
        crate::engine::catalog::Catalog::new(store.clone())
            .create_table(TableDraft {
                id: Some(SchemaId::new(1).unwrap()),
                name: "items".into(),
                columns: vec![ColumnDraft {
                    id: Some(SchemaId::new(1).unwrap()),
                    name: "id".into(),
                    scalar_type: ScalarType::Text,
                    nullable: false,
                    format: String::new(),
                    default: None,
                }],
                primary_key: vec!["id".into()],
                indexes: Vec::new(),
                foreign_keys: Vec::new(),
            })
            .await
            .unwrap();

        let config = || StatisticsConfig {
            publish_interval: Duration::from_millis(10),
            survey_every: 1,
            ..StatisticsConfig::default()
        };
        let surveyed = async |engine: crate::engine::exec::Engine| {
            let runner = StatisticsRunner::start(
                Arc::new(crate::runtime::SystemRuntime),
                config(),
                None,
                None,
            );
            runner.attach_engine(Arc::new(engine));
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while std::time::Instant::now() < deadline {
                if !runner.stats().synopsis_models.is_empty() {
                    runner.shutdown().await;
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            runner.shutdown().await;
            false
        };

        assert!(
            !surveyed(crate::engine::exec::Engine::read_only(store.clone())).await,
            "a read instance surveyed the whole database for a synopsis it cannot publish"
        );
        assert!(
            surveyed(crate::engine::exec::Engine::new(store.clone())).await,
            "the write instance did not survey, so the negative case proves nothing"
        );
    }

    #[tokio::test]
    async fn collector_drops_on_full_channel_and_counts_drops() {
        let (collector, mut receiver) = StatisticsCollector::channel(2);
        for index in 0..5 {
            collector.statement(observation(index, index, 1));
        }
        assert_eq!(collector.dropped(), 3);
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn program_capture_is_disabled_by_default() {
        use crate::engine::exec::observe::ExecutionObserver as _;

        assert!(!StatisticsConfig::default().capture_programs);
        let (collector, mut receiver) = StatisticsCollector::channel(2);
        collector.program(ProgramRecord {
            canonical: br#"{"statements":[]}"#.to_vec(),
            content_hash: [7; 16],
            at_unix_micros: 1,
            statements: 0,
            outcomes: Vec::new(),
        });

        assert!(!collector.captures_programs());
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn corpus_loss_reasons_are_counted_separately() {
        use crate::engine::exec::observe::ExecutionObserver as _;

        let record = |size| ProgramRecord {
            canonical: vec![1; size],
            content_hash: [1; 16],
            at_unix_micros: 1,
            statements: 1,
            outcomes: Vec::new(),
        };
        let (collector, mut receiver) = StatisticsCollector::channel_with_programs(1, true);
        collector.program(record(CORPUS_MAX_PROGRAM_BYTES + 1));
        collector.program(record(1));
        collector.program(record(1));

        let report = collector.report();
        assert_eq!(report.corpus_captured, 1);
        assert_eq!(report.corpus_skipped_oversize, 1);
        assert_eq!(report.corpus_dropped_queue, 1);
        assert!(receiver.try_recv().is_ok());

        let mut registry = HotRegistry::new(16);
        for _ in 0..=MAX_PENDING_PROGRAMS {
            registry.absorb_program(record(1));
        }
        let stats = registry.distill(report, Duration::ZERO);
        assert_eq!(stats.corpus.shed_pending, 1);
    }
}
