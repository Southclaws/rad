use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use rad::engine::catalog;
use rad::engine::catalog::identity::SchemaId;
use rad::engine::catalog::model::{ColumnDef, ScalarType, Table, TableDef};
use rad::engine::exec::observe::{
    ExecutionObserver, KvWork, StatementObservation, StatementSource,
};
use rad::engine::exec::{
    CatalogPolicy, Engine, EngineEvent, EngineEventHook, Limits, Program,
    RelationCacheCohortStatistics, RelationCacheConfig, RelationCacheDomainStatistics,
    RelationCacheDomains, RelationCacheLimits, RelationCacheLookupResult,
    RelationCacheMaterialization, RelationCachePolicyConfig, RelationCachePolicyCounters,
    RelationCachePolicyMode, RelationCachePrior, RelationCacheReuseAdmission,
    RelationCacheStatistics, Statement,
};
use rad::engine::kv::slatedb::Store;
use rad::engine::kv::{IsolationLevel, Transaction, TransactionalKv};
use rad::engine::lir::{
    self, BinaryOp, Expr, Field, Kind, Literal, RawScalar, Relation, RootCardinality, Row, RowType,
    SlotId, Type, Value,
};
use rad::engine::planner::estimator::StatisticsProvider;
use rad::engine::planner::models::{ColumnSynopsis, PlannerStats, SynopsisCoverage, SynopsisModel};
use rad::runtime::RuntimeEffects;
use rad::service::result_json;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

mod support;

#[path = "benchmarks/relation_cache/falsification.rs"]
mod falsification;

use support::s3::{RustFs, object_store, object_store_at};
use support::toxiproxy::ToxiProxy;

const MANIFEST: &str = "tests/benchmarks/relation_cache/benchmark.yaml";
const FORMAT: &str = "rad-relation-cache-benchmark-v2";
const RESULT_FORMAT: &str = "rad-relation-cache-benchmark-result-v3";
const RUN_FORMAT: &str = "rad-relation-cache-benchmark-run-v1";

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
type ResultKey = (String, String, String, String, String, usize);

#[derive(Debug, Deserialize)]
struct Manifest {
    format: String,
    name: String,
    description: String,
    scenarios: Vec<Scenario>,
}

#[derive(Clone, Debug, Deserialize)]
struct Scenario {
    name: String,
    expected: ExpectedValue,
    #[serde(default)]
    suite: WorkloadSuite,
    #[serde(default)]
    shape: WorkloadShape,
    #[serde(default = "default_dataset_rows")]
    dataset_rows: usize,
    #[serde(default = "default_dimension_rows")]
    dimension_rows: usize,
    #[serde(default = "default_fact_rows")]
    fact_rows: usize,
    #[serde(default = "default_payload_bytes")]
    seed_payload_bytes: usize,
    #[serde(default)]
    cache_capacity_bytes: Option<usize>,
    #[serde(default)]
    cache_entry_limit: Option<usize>,
    #[serde(default)]
    result_limit_bytes: Option<usize>,
    #[serde(default)]
    falsification: Option<falsification::FalsificationFactors>,
    steps: Vec<Step>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WorkloadSuite {
    #[default]
    Core,
    Extended,
    Falsification,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum MutationPattern {
    #[default]
    None,
    Append,
    UniformPoint,
    ZipfPoint,
    HotSet,
    Burst,
    PeriodicBatch,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ReadPattern {
    #[default]
    ExactRepeat,
    Uniform,
    Zipf,
    HotSet,
    RangeTail,
    HistoricalRange,
    Aggregate,
    Join,
    Mixed,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum LiteralDistribution {
    #[default]
    Constant,
    Uniform,
    Zipf,
    Monotonic,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
struct WorkloadShape {
    mutation_pattern: MutationPattern,
    read_pattern: ReadPattern,
    literal_distribution: LiteralDistribution,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ExpectedValue {
    Useful,
    Wasteful,
    Mixed,
    Safe,
    Rejected,
}

#[derive(Clone, Debug, Deserialize)]
struct Step {
    #[serde(rename = "type")]
    kind: StepKind,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    table: Option<String>,
    #[serde(default)]
    count: Option<usize>,
    #[serde(default)]
    payload_bytes: Option<usize>,
    #[serde(default)]
    milliseconds: Option<u64>,
    #[serde(default)]
    interval_milliseconds: Option<u64>,
    #[serde(default)]
    literal_distribution: Option<LiteralDistribution>,
    #[serde(default)]
    literal_cardinality: Option<usize>,
    #[serde(default)]
    literal_offset: Option<usize>,
    #[serde(default)]
    repeat_each: Option<usize>,
    #[serde(default)]
    mutation_pattern: Option<MutationPattern>,
    #[serde(default)]
    hot_set_size: Option<usize>,
    #[serde(default)]
    gate_fill: bool,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    steps: Vec<Step>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum StepKind {
    Query,
    QuerySeries,
    ConcurrentQuery,
    Mutation,
    Commit,
    TimeAdvance,
    Barrier,
    PinSnapshot,
    PinnedQuery,
    ReleaseSnapshot,
    Checkpoint,
    Repeat,
}

#[derive(Clone)]
enum Operation {
    Query {
        query: String,
        count: usize,
        interval: Duration,
    },
    QuerySeries {
        query: String,
        count: usize,
        distribution: LiteralDistribution,
        cardinality: usize,
        offset: usize,
        repeat_each: usize,
        interval: Duration,
    },
    ConcurrentQuery {
        query: String,
        count: usize,
        gate_fill: bool,
    },
    Mutation {
        table: String,
        count: usize,
        payload_bytes: usize,
        pattern: MutationPattern,
        hot_set_size: usize,
    },
    Commit,
    TimeAdvance(Duration),
    Barrier,
    PinSnapshot,
    PinnedQuery {
        query: String,
        count: usize,
    },
    ReleaseSnapshot,
    Checkpoint(String),
}

struct QuerySeriesExecution {
    query: String,
    count: usize,
    distribution: LiteralDistribution,
    cardinality: usize,
    offset: usize,
    repeat_each: usize,
    interval: Duration,
}

const fn default_dataset_rows() -> usize {
    20
}

const fn default_dimension_rows() -> usize {
    5
}

const fn default_fact_rows() -> usize {
    5
}

const fn default_payload_bytes() -> usize {
    32
}

#[derive(Default)]
struct BenchmarkObserver {
    observations: Mutex<Vec<(StatementSource, KvWork)>>,
}

impl ExecutionObserver for BenchmarkObserver {
    fn statement(&self, observation: StatementObservation) {
        self.observations
            .lock()
            .expect("benchmark observation lock poisoned")
            .push((observation.source, observation.kv));
    }
}

impl BenchmarkObserver {
    fn take(&self) -> ObservedExecution {
        let observations = std::mem::take(
            &mut *self
                .observations
                .lock()
                .expect("benchmark observation lock poisoned"),
        );
        let mut result = ObservedExecution::default();
        for (source, work) in observations {
            if source == StatementSource::Executed {
                result.executions = result.executions.saturating_add(1);
            }
            result.work.add(work);
        }
        result
    }
}

#[derive(Default)]
struct BenchmarkFillGate {
    armed: AtomicBool,
    blocked: AtomicBool,
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl BenchmarkFillGate {
    fn arm(&self) {
        self.blocked.store(false, Ordering::Release);
        self.armed.store(true, Ordering::Release);
    }

    async fn wait_until_reached(&self) -> bool {
        tokio::time::timeout(Duration::from_secs(10), self.reached.notified())
            .await
            .is_ok()
    }

    fn release(&self) {
        self.armed.store(false, Ordering::Release);
        self.release.notify_one();
    }
}

#[async_trait]
impl EngineEventHook for BenchmarkFillGate {
    async fn reach(&self, event: EngineEvent) {
        let hash_build_is_ready = matches!(
            event,
            EngineEvent::SubrelationCacheFillReady {
                materialization: RelationCacheMaterialization::HashJoinBuild,
                ..
            } | EngineEvent::SubrelationCacheLookupCompleted {
                materialization: RelationCacheMaterialization::HashJoinBuild,
                result: RelationCacheLookupResult::Reused,
                ..
            }
        );
        let must_block = self.armed.load(Ordering::Acquire)
            && hash_build_is_ready
            && !self.blocked.swap(true, Ordering::AcqRel);
        if must_block {
            self.reached.notify_one();
            self.release.notified().await;
        }
    }
}

#[derive(Clone, Copy, Default)]
struct ObservedExecution {
    executions: u64,
    work: WorkScore,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
struct WorkScore {
    work_units: u64,
    bytes_read: u64,
    gets: u64,
    scans: u64,
    seeks: u64,
    rows: u64,
}

impl WorkScore {
    fn add(&mut self, work: KvWork) {
        self.bytes_read = self.bytes_read.saturating_add(work.bytes_read);
        self.gets = self.gets.saturating_add(work.gets);
        self.scans = self.scans.saturating_add(work.scans);
        self.seeks = self.seeks.saturating_add(work.forward_seeks);
        self.rows = self.rows.saturating_add(work.iterated);
        self.work_units = self.work_units.saturating_add(deterministic_work(work));
    }

    fn add_score(&mut self, other: Self) {
        self.work_units = self.work_units.saturating_add(other.work_units);
        self.bytes_read = self.bytes_read.saturating_add(other.bytes_read);
        self.gets = self.gets.saturating_add(other.gets);
        self.scans = self.scans.saturating_add(other.scans);
        self.seeks = self.seeks.saturating_add(other.seeks);
        self.rows = self.rows.saturating_add(other.rows);
    }

    fn repeated(self, count: usize) -> Self {
        let count = u64::try_from(count).unwrap_or(u64::MAX);
        Self {
            work_units: self.work_units.saturating_mul(count),
            bytes_read: self.bytes_read.saturating_mul(count),
            gets: self.gets.saturating_mul(count),
            scans: self.scans.saturating_mul(count),
            seeks: self.seeks.saturating_mul(count),
            rows: self.rows.saturating_mul(count),
        }
    }

    fn saved_by(self, actual: Self) -> Self {
        Self {
            work_units: self.work_units.saturating_sub(actual.work_units),
            bytes_read: self.bytes_read.saturating_sub(actual.bytes_read),
            gets: self.gets.saturating_sub(actual.gets),
            scans: self.scans.saturating_sub(actual.scans),
            seeks: self.seeks.saturating_sub(actual.seeks),
            rows: self.rows.saturating_sub(actual.rows),
        }
    }
}

fn deterministic_work(work: KvWork) -> u64 {
    work.bytes_read
        .saturating_add(work.gets.saturating_mul(256))
        .saturating_add(work.scans.saturating_mul(4 * 1024))
        .saturating_add(work.forward_seeks.saturating_mul(256))
        .saturating_add(work.iterated.saturating_mul(64))
}

#[derive(Default)]
struct Scorecard {
    requests: u64,
    executions: u64,
    actual_work: WorkScore,
    baseline_work: WorkScore,
    peak_resident_bytes: u64,
    request_latency_micros: Vec<u64>,
    fill_latency_micros: Vec<u64>,
}

#[derive(Default)]
struct PendingMutations {
    appends: HashMap<String, Vec<Row>>,
    updates: HashMap<String, Vec<Row>>,
}

impl PendingMutations {
    fn is_empty(&self) -> bool {
        self.appends.is_empty() && self.updates.is_empty()
    }
}

#[derive(Default)]
struct WorkloadState {
    literal_sequence: u64,
    mutation_sequence: u64,
    append_sequence: u64,
}

impl WorkloadState {
    fn literal(
        &mut self,
        distribution: LiteralDistribution,
        cardinality: usize,
        offset: usize,
    ) -> usize {
        let sequence = self.literal_sequence;
        self.literal_sequence = self.literal_sequence.saturating_add(1);
        let index = match distribution {
            LiteralDistribution::Constant => 0,
            LiteralDistribution::Uniform => {
                usize::try_from(splitmix64(sequence) % cardinality as u64).unwrap_or_default()
            }
            LiteralDistribution::Zipf => zipf_index(sequence, cardinality),
            LiteralDistribution::Monotonic => {
                usize::try_from(sequence % cardinality as u64).unwrap_or_default()
            }
        };
        offset.saturating_add(index)
    }

    fn mutation_target(
        &mut self,
        pattern: MutationPattern,
        rows: usize,
        hot_set_size: usize,
    ) -> usize {
        let sequence = self.mutation_sequence;
        self.mutation_sequence = self.mutation_sequence.saturating_add(1);
        match pattern {
            MutationPattern::ZipfPoint => zipf_index(sequence, rows),
            MutationPattern::HotSet => {
                usize::try_from(sequence % hot_set_size.min(rows) as u64).unwrap_or_default()
            }
            MutationPattern::UniformPoint
            | MutationPattern::Burst
            | MutationPattern::PeriodicBatch => {
                usize::try_from(sequence % rows as u64).unwrap_or_default()
            }
            MutationPattern::None | MutationPattern::Append => 0,
        }
    }
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn zipf_index(sequence: u64, cardinality: usize) -> usize {
    let total_weight = (1..=cardinality)
        .map(|rank| 1_000_000u64 / rank as u64)
        .sum::<u64>();
    let mut sample = splitmix64(sequence) % total_weight.max(1);
    for index in 0..cardinality {
        let weight = 1_000_000u64 / (index as u64 + 1);
        if sample < weight {
            return index;
        }
        sample -= weight;
    }
    cardinality.saturating_sub(1)
}

impl Scorecard {
    fn observe_request(
        &mut self,
        measured: ObservedExecution,
        baseline: WorkScore,
        elapsed: Duration,
        fills: u64,
    ) {
        self.requests = self.requests.saturating_add(1);
        self.executions = self.executions.saturating_add(measured.executions);
        self.actual_work.add_score(measured.work);
        self.baseline_work.add_score(baseline);
        self.request_latency_micros.push(duration_micros(elapsed));
        for _ in 0..fills {
            self.fill_latency_micros.push(duration_micros(elapsed));
        }
    }

    fn observe_residency(&mut self, statistics: &RelationCacheStatistics) {
        self.peak_resident_bytes = self.peak_resident_bytes.max(statistics.retained_bytes);
    }

    fn observe_batch(
        &mut self,
        requests: usize,
        measured: ObservedExecution,
        baseline: WorkScore,
        latencies: impl IntoIterator<Item = u64>,
        fill_latency_micros: u64,
        fills: u64,
    ) {
        self.requests = self
            .requests
            .saturating_add(u64::try_from(requests).unwrap_or(u64::MAX));
        self.executions = self.executions.saturating_add(measured.executions);
        self.actual_work.add_score(measured.work);
        self.baseline_work.add_score(baseline);
        self.request_latency_micros.extend(latencies);
        for _ in 0..fills {
            self.fill_latency_micros.push(fill_latency_micros);
        }
    }
}

fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
struct LatencyQuantiles {
    p50_micros: u64,
    p95_micros: u64,
    p99_micros: u64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
struct CohortQuantiles {
    samples: u64,
    lifetime_p50_micros: u64,
    lifetime_p95_micros: u64,
    requests_p50: u64,
    requests_p95: u64,
    reuse_p50: u64,
    reuse_p95: u64,
}

impl From<RelationCacheCohortStatistics> for CohortQuantiles {
    fn from(value: RelationCacheCohortStatistics) -> Self {
        Self {
            samples: value.samples,
            lifetime_p50_micros: value.observed_lifetime_micros.p50,
            lifetime_p95_micros: value.observed_lifetime_micros.p95,
            requests_p50: value.successful_requests.p50,
            requests_p95: value.successful_requests.p95,
            reuse_p50: value.reuse_opportunities.p50,
            reuse_p95: value.reuse_opportunities.p95,
        }
    }
}

impl LatencyQuantiles {
    fn from_samples(mut samples: Vec<u64>) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        samples.sort_unstable();
        Self {
            p50_micros: percentile(&samples, 50),
            p95_micros: percentile(&samples, 95),
            p99_micros: percentile(&samples, 99),
        }
    }
}

fn percentile(samples: &[u64], percent: usize) -> u64 {
    let rank = samples
        .len()
        .saturating_mul(percent)
        .saturating_add(99)
        .saturating_div(100)
        .saturating_sub(1);
    samples[rank.min(samples.len() - 1)]
}

#[derive(Default)]
struct BenchmarkRuntime {
    monotonic_micros: AtomicU64,
    uuid_sequence: AtomicU64,
}

impl BenchmarkRuntime {
    fn advance(&self, duration: Duration) {
        self.monotonic_micros.fetch_add(
            u64::try_from(duration.as_micros()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }
}

impl RuntimeEffects for BenchmarkRuntime {
    fn now(&self) -> DateTime<Utc> {
        Utc.timestamp_micros(self.monotonic_micros.load(Ordering::Relaxed) as i64)
            .single()
            .expect("benchmark time is valid")
    }

    fn new_uuid(&self) -> Uuid {
        Uuid::from_u128(u128::from(
            self.uuid_sequence.fetch_add(1, Ordering::Relaxed) + 1,
        ))
    }

    fn monotonic(&self) -> Duration {
        Duration::from_micros(self.monotonic_micros.load(Ordering::Relaxed))
    }
}

struct FixedPlannerStats(Arc<PlannerStats>);

impl StatisticsProvider for FixedPlannerStats {
    fn planning_stats(&self) -> Arc<PlannerStats> {
        self.0.clone()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct BenchmarkResult {
    format: String,
    benchmark: String,
    backend: String,
    scenario: String,
    expected: ExpectedValue,
    #[serde(default)]
    shape: WorkloadShape,
    #[serde(default)]
    dataset_rows: usize,
    #[serde(default)]
    falsification: Option<falsification::FalsificationFactors>,
    policy_mode: String,
    prior: String,
    #[serde(default = "default_reuse_admission")]
    reuse_admission: String,
    #[serde(default = "default_family_minimum_observations")]
    family_minimum_observations: usize,
    correctness_hash: String,
    #[serde(default)]
    requests: u64,
    #[serde(default)]
    executions: u64,
    executed_fills: u64,
    admissions: u64,
    policy_rejections: u64,
    foyer_rejections: u64,
    hard_limit_rejections: u64,
    resident_hits: u64,
    coalesced_reuses: u64,
    work_performed: u64,
    work_avoided: u64,
    #[serde(default)]
    work: WorkScore,
    #[serde(default)]
    work_avoided_score: WorkScore,
    retained_bytes: u64,
    #[serde(default)]
    peak_resident_bytes: u64,
    completed_cohorts: u64,
    zero_reuse_cohorts: u64,
    false_admissions: u64,
    false_rejections: u64,
    #[serde(default)]
    admissions_followed_by_reuse: u64,
    #[serde(default)]
    admissions_without_future_reuse: u64,
    #[serde(default)]
    rejections_followed_by_reuse: u64,
    #[serde(default)]
    avoidable_work_after_rejection: u64,
    #[serde(default)]
    retained_bytes_without_future_reuse: u64,
    #[serde(default)]
    family_entries: usize,
    #[serde(default)]
    family_second_touch_observations: u64,
    #[serde(default)]
    family_third_touch_conversions: u64,
    oracle_value: i128,
    actual_residency_value: i128,
    coalescing_value: i128,
    policy_regret: i128,
    #[serde(default)]
    zero_reuse_cohort_percent: u8,
    #[serde(default)]
    cohort_quantiles: CohortQuantiles,
    #[serde(default)]
    request_latency: LatencyQuantiles,
    #[serde(default)]
    fill_latency: LatencyQuantiles,
    decisions: DecisionBreakdown,
    domains: BTreeMap<String, DomainResult>,
    checkpoints: Vec<BenchmarkCheckpoint>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
struct DecisionBreakdown {
    second_touch: u64,
    #[serde(default)]
    third_touch: u64,
    #[serde(default)]
    value_density: u64,
    #[serde(default)]
    family_conversion: u64,
    learned_value: u64,
    generation_rate: u64,
    #[serde(default)]
    recovery_probe: u64,
    expensive_probation: u64,
    #[serde(default)]
    recent_no_reuse: u64,
    no_reuse_history: u64,
    insufficient_value: u64,
    too_large: u64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
struct DomainResult {
    executed_fills: u64,
    admissions: u64,
    policy_rejections: u64,
    foyer_rejections: u64,
    hard_limit_rejections: u64,
    resident_hits: u64,
    coalesced_reuses: u64,
    work_performed: u64,
    work_avoided: u64,
    retained_bytes: u64,
    completed_cohorts: u64,
    zero_reuse_cohorts: u64,
    false_admissions: u64,
    false_rejections: u64,
    #[serde(default)]
    admissions_followed_by_reuse: u64,
    #[serde(default)]
    admissions_without_future_reuse: u64,
    #[serde(default)]
    rejections_followed_by_reuse: u64,
    #[serde(default)]
    avoidable_work_after_rejection: u64,
    #[serde(default)]
    retained_bytes_without_future_reuse: u64,
    oracle_value: i128,
    actual_residency_value: i128,
    coalescing_value: i128,
    policy_regret: i128,
    decisions: DecisionBreakdown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct BenchmarkCheckpoint {
    name: String,
    executed_fills: u64,
    admissions: u64,
    policy_rejections: u64,
    resident_hits: u64,
    oracle_value: i128,
    actual_residency_value: i128,
    policy_regret: i128,
}

#[derive(Debug, Deserialize, Serialize)]
struct BenchmarkRun {
    format: String,
    phase: String,
    benchmark_format: String,
    benchmark: String,
    manifest_hash: String,
    source_revision: String,
    source_hash: String,
    results: Vec<BenchmarkResult>,
}

#[test]
fn relation_cache_benchmark_manifest_is_valid() {
    let manifest = load_manifest().expect("load relation cache benchmark manifest");
    assert_eq!(manifest.format, FORMAT);
    assert!(!manifest.name.is_empty());
    assert!(!manifest.description.is_empty());
    assert!(manifest.scenarios.len() >= 18);
    let names = manifest
        .scenarios
        .iter()
        .map(|scenario| scenario.name.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(names.len(), manifest.scenarios.len());
    let operations = manifest
        .scenarios
        .iter()
        .flat_map(|scenario| expand(&scenario.steps).expect("expand benchmark scenario"))
        .collect::<Vec<_>>();
    assert!(operations.iter().any(|step| matches!(
        step,
        Operation::Query { .. } | Operation::QuerySeries { .. }
    )));
    assert!(
        operations
            .iter()
            .any(|step| matches!(step, Operation::ConcurrentQuery { .. }))
    );
    assert!(
        operations
            .iter()
            .any(|step| matches!(step, Operation::Mutation { .. }))
    );
    assert!(
        operations
            .iter()
            .any(|step| matches!(step, Operation::Commit))
    );
    assert!(
        operations
            .iter()
            .any(|step| matches!(step, Operation::TimeAdvance(_)))
    );
    assert!(
        operations
            .iter()
            .any(|step| matches!(step, Operation::Barrier))
    );
    assert!(
        operations
            .iter()
            .any(|step| matches!(step, Operation::PinSnapshot))
    );
    assert!(
        operations
            .iter()
            .any(|step| matches!(step, Operation::PinnedQuery { .. }))
    );
    assert!(
        operations
            .iter()
            .any(|step| matches!(step, Operation::Checkpoint(_)))
    );
}

#[test]
fn literal_distributions_are_bounded_and_replayable() {
    let sequence = (0..1_000)
        .map(|value| zipf_index(value, 100))
        .collect::<Vec<_>>();
    let replay = (0..1_000)
        .map(|value| zipf_index(value, 100))
        .collect::<Vec<_>>();
    assert_eq!(sequence, replay);
    assert!(sequence.iter().all(|value| *value < 100));
    assert!(
        sequence.iter().filter(|value| **value < 10).count()
            > sequence.iter().filter(|value| **value >= 90).count()
    );
}

#[test]
fn workload_suite_contains_the_policy_axes() {
    let manifest = load_manifest().expect("load relation cache benchmark manifest");
    for name in [
        "hot_point_lookup",
        "relevant_point_updates",
        "unrelated_point_updates",
        "uniform_point_lookup",
        "zipfian_point_lookup",
        "periodic_batch_rebuild",
        "write_once_read_many",
        "tiny_result_high_work",
        "stable_hash_build",
        "reuse_pressure_r100_g1",
        "reuse_pressure_r1_g10",
        "literals_monotonic",
        "cache_pollution",
        "false_second_hit",
        "family_all_convert",
        "family_false_second",
        "family_hot_to_cold",
        "family_cold_to_hot",
        "family_single_hot_exact",
        "thundering_herd",
    ] {
        assert!(
            manifest
                .scenarios
                .iter()
                .any(|scenario| scenario.name == name),
            "missing benchmark scenario {name:?}"
        );
    }
}

#[tokio::test]
#[ignore = "emits deterministic policy measurements rather than timing thresholds"]
async fn relation_cache_policy_matrix_records_structured_results() -> TestResult {
    let manifest = load_manifest()?;
    let mut results = Vec::new();
    for config in benchmark_configurations() {
        for scenario in manifest
            .scenarios
            .iter()
            .filter(|scenario| selected(scenario))
        {
            let result = run_scenario(&manifest, scenario, config).await?;
            println!("{}", serde_json::to_string(&result)?);
            results.push(result);
        }
    }
    if let Ok(output) = std::env::var("RAD_RELATION_CACHE_BENCHMARK_OUTPUT") {
        let phase = std::env::var("RAD_RELATION_CACHE_BENCHMARK_PHASE")
            .map_err(|_| "RAD_RELATION_CACHE_BENCHMARK_PHASE is required with benchmark output")?;
        write_benchmark_run(Path::new(&output), &phase, &manifest, results)?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "runs deterministic primary-key invalidation workloads"]
async fn relation_cache_primary_key_invalidation_meets_targets() -> TestResult {
    let manifest = load_manifest()?;
    let unrelated = manifest
        .scenarios
        .iter()
        .find(|scenario| scenario.name == "unrelated_point_updates")
        .ok_or("unrelated point update scenario is absent")?;
    let relevant = manifest
        .scenarios
        .iter()
        .find(|scenario| scenario.name == "relevant_point_updates")
        .ok_or("relevant point update scenario is absent")?;

    let foyer = policy_config(RelationCachePolicyMode::Foyer, RelationCachePrior::None);
    let foyer_unrelated = run_scenario(&manifest, unrelated, foyer).await?;
    let foyer_relevant = run_scenario(&manifest, relevant, foyer).await?;
    assert_eq!(foyer_unrelated.executed_fills, 1);
    assert_eq!(foyer_unrelated.resident_hits, 99);
    assert_eq!(foyer_relevant.executed_fills, 20);
    assert_eq!(foyer_relevant.resident_hits, 80);

    let enforced_unrelated =
        run_scenario(&manifest, unrelated, RelationCacheConfig::default()).await?;
    assert!(enforced_unrelated.executed_fills <= 2);
    assert!(enforced_unrelated.resident_hits >= 98);
    assert!(enforced_unrelated.policy_rejections <= 1);
    Ok(())
}

#[tokio::test]
#[ignore = "emits deterministic reuse admission measurements"]
async fn relation_cache_reuse_admission_experiment_records_structured_results() -> TestResult {
    let manifest = load_manifest()?;
    let mut results = Vec::new();
    for reuse_admission in [
        RelationCacheReuseAdmission::SecondTouch,
        RelationCacheReuseAdmission::ThirdTouch,
        RelationCacheReuseAdmission::ValueDensity,
        RelationCacheReuseAdmission::FamilyConversion,
    ] {
        let mut config = policy_config(RelationCachePolicyMode::Enforced, RelationCachePrior::None);
        config.policy.reuse_admission = reuse_admission;
        config.policy.probation_minimum_work_units = u64::MAX;
        for scenario in manifest.scenarios.iter().filter(|scenario| {
            matches!(
                scenario.name.as_str(),
                "stable_dimension" | "useful_hot_feed" | "false_second_hit"
            ) || scenario.name.starts_with("reuse_pressure_")
                || scenario.name.starts_with("family_")
        }) {
            let result = run_scenario(&manifest, scenario, config).await?;
            println!("{}", serde_json::to_string(&result)?);
            results.push(result);
        }
    }
    if let Ok(output) = std::env::var("RAD_RELATION_CACHE_BENCHMARK_OUTPUT") {
        let phase = std::env::var("RAD_RELATION_CACHE_BENCHMARK_PHASE")
            .map_err(|_| "RAD_RELATION_CACHE_BENCHMARK_PHASE is required with benchmark output")?;
        write_benchmark_run(Path::new(&output), &phase, &manifest, results)?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "emits deterministic family conversion measurements"]
async fn relation_cache_family_conversion_experiment_records_structured_results() -> TestResult {
    let manifest = load_manifest()?;
    let mut results = Vec::new();
    for reuse_admission in [
        RelationCacheReuseAdmission::SecondTouch,
        RelationCacheReuseAdmission::ThirdTouch,
    ] {
        let mut config = policy_config(RelationCachePolicyMode::Enforced, RelationCachePrior::None);
        config.policy.reuse_admission = reuse_admission;
        config.policy.probation_minimum_work_units = u64::MAX;
        for scenario in manifest
            .scenarios
            .iter()
            .filter(|scenario| scenario.name.starts_with("family_"))
        {
            let result = run_scenario(&manifest, scenario, config).await?;
            println!("{}", serde_json::to_string(&result)?);
            results.push(result);
        }
    }
    for minimum_observations in [1, 3, 5, 10] {
        let mut config = policy_config(RelationCachePolicyMode::Enforced, RelationCachePrior::None);
        config.policy.reuse_admission = RelationCacheReuseAdmission::FamilyConversion;
        config.policy.family_minimum_observations = minimum_observations;
        config.policy.probation_minimum_work_units = u64::MAX;
        for scenario in manifest
            .scenarios
            .iter()
            .filter(|scenario| scenario.name.starts_with("family_"))
        {
            let result = run_scenario(&manifest, scenario, config).await?;
            println!("{}", serde_json::to_string(&result)?);
            results.push(result);
        }
    }
    if let Ok(output) = std::env::var("RAD_RELATION_CACHE_BENCHMARK_OUTPUT") {
        let phase = std::env::var("RAD_RELATION_CACHE_BENCHMARK_PHASE")
            .map_err(|_| "RAD_RELATION_CACHE_BENCHMARK_PHASE is required with benchmark output")?;
        write_benchmark_run(Path::new(&output), &phase, &manifest, results)?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "emits deterministic family conversion candidate measurements"]
async fn relation_cache_family_conversion_candidate_records_structured_results() -> TestResult {
    let manifest = load_manifest()?;
    let mut config = policy_config(RelationCachePolicyMode::Enforced, RelationCachePrior::None);
    config.policy.reuse_admission = RelationCacheReuseAdmission::FamilyConversion;
    config.policy.family_minimum_observations = 1;
    let mut results = Vec::new();
    for scenario in manifest
        .scenarios
        .iter()
        .filter(|scenario| selected(scenario))
    {
        let result = run_scenario(&manifest, scenario, config).await?;
        println!("{}", serde_json::to_string(&result)?);
        results.push(result);
    }
    if let Ok(output) = std::env::var("RAD_RELATION_CACHE_BENCHMARK_OUTPUT") {
        let phase = std::env::var("RAD_RELATION_CACHE_BENCHMARK_PHASE")
            .map_err(|_| "RAD_RELATION_CACHE_BENCHMARK_PHASE is required with benchmark output")?;
        write_benchmark_run(Path::new(&output), &phase, &manifest, results)?;
    }
    Ok(())
}

fn selected(scenario: &Scenario) -> bool {
    if let Ok(names) = std::env::var("RAD_RELATION_CACHE_BENCHMARK_SCENARIOS") {
        return names
            .split(',')
            .map(str::trim)
            .any(|name| name == scenario.name);
    }
    if let Ok(name) = std::env::var("RAD_RELATION_CACHE_BENCHMARK_SCENARIO") {
        return scenario.name == name;
    }
    match std::env::var("RAD_RELATION_CACHE_BENCHMARK_SUITE").as_deref() {
        Ok("all") => true,
        Ok("extended") => scenario.suite == WorkloadSuite::Extended,
        Ok("falsification") => scenario.suite == WorkloadSuite::Falsification,
        Ok("core") | Err(_) => scenario.suite == WorkloadSuite::Core,
        Ok(value) => panic!("unknown relation cache benchmark suite {value:?}"),
    }
}

fn benchmark_configurations() -> [RelationCacheConfig; 6] {
    let mut disabled = policy_config(RelationCachePolicyMode::Foyer, RelationCachePrior::None);
    disabled.domains = RelationCacheDomains::none();
    [
        disabled,
        policy_config(RelationCachePolicyMode::Foyer, RelationCachePrior::None),
        policy_config(RelationCachePolicyMode::Shadow, RelationCachePrior::None),
        policy_config(
            RelationCachePolicyMode::Shadow,
            RelationCachePrior::GenerationRate,
        ),
        policy_config(RelationCachePolicyMode::Enforced, RelationCachePrior::None),
        policy_config(
            RelationCachePolicyMode::Enforced,
            RelationCachePrior::GenerationRate,
        ),
    ]
}

#[test]
#[ignore = "compares two recorded deterministic benchmark runs"]
fn relation_cache_benchmark_runs_compare() -> TestResult {
    let baseline_path = std::env::var("RAD_RELATION_CACHE_BENCHMARK_BASELINE")
        .map_err(|_| "RAD_RELATION_CACHE_BENCHMARK_BASELINE is required")?;
    let candidate_path = std::env::var("RAD_RELATION_CACHE_BENCHMARK_CANDIDATE")
        .map_err(|_| "RAD_RELATION_CACHE_BENCHMARK_CANDIDATE is required")?;
    let baseline: BenchmarkRun = serde_json::from_slice(&std::fs::read(&baseline_path)?)?;
    let candidate: BenchmarkRun = serde_json::from_slice(&std::fs::read(&candidate_path)?)?;
    let report = compare_runs(&baseline, &candidate)?;
    print!("{report}");
    if let Ok(output) = std::env::var("RAD_RELATION_CACHE_BENCHMARK_COMPARISON_OUTPUT") {
        write_file(Path::new(&output), report.as_bytes())?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "emits deterministic one-variable policy sweeps"]
async fn relation_cache_policy_threshold_sweep_records_structured_results() -> TestResult {
    let manifest = load_manifest()?;
    let scenarios = ["stable_dimension", "wasteful_hot_feed"];
    for scenario_name in scenarios {
        let scenario = manifest
            .scenarios
            .iter()
            .find(|scenario| scenario.name == scenario_name)
            .expect("sweep scenario is present");
        for value in [1usize, 3, 5] {
            let mut config =
                policy_config(RelationCachePolicyMode::Enforced, RelationCachePrior::None);
            config.policy.minimum_completed_cohorts = value;
            config.policy.cohorts_per_exact_relation = 8;
            let result = run_scenario(&manifest, scenario, config).await?;
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "format": RESULT_FORMAT,
                    "sweepParameter": "minimum_completed_cohorts",
                    "sweepValue": value,
                    "result": result,
                }))?
            );
        }
        for value in [50u8, 75, 100] {
            let mut config =
                policy_config(RelationCachePolicyMode::Enforced, RelationCachePrior::None);
            config.policy.zero_reuse_percent = value;
            let result = run_scenario(&manifest, scenario, config).await?;
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "format": RESULT_FORMAT,
                    "sweepParameter": "zero_reuse_percent",
                    "sweepValue": value,
                    "result": result,
                }))?
            );
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires a Docker daemon; emits deterministic policy measurements"]
async fn relation_cache_representative_scenarios_run_against_rustfs() -> TestResult {
    let manifest = load_manifest()?;
    let rustfs = RustFs::start_or_external().await?;
    let objects = object_store(&rustfs.config)?;
    for scenario_name in [
        "stable_dimension",
        "useful_hot_feed",
        "wasteful_hot_feed",
        "domain_split",
        "tiny_result_high_work",
        "stable_hash_build",
    ] {
        let scenario = manifest
            .scenarios
            .iter()
            .find(|scenario| scenario.name == scenario_name)
            .expect("RustFS scenario is present");
        let path = format!(
            "relation-cache-{}-{}",
            scenario.name,
            Uuid::new_v4().simple()
        );
        let store = Arc::new(Store::open(path, objects.clone()).await?);
        let result = run_scenario_with_store(
            &manifest,
            scenario,
            policy_config(
                RelationCachePolicyMode::Enforced,
                RelationCachePrior::GenerationRate,
            ),
            store,
            "s3-rustfs",
        )
        .await?;
        println!("{}", serde_json::to_string(&result)?);
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker; emits storage-latency measurements"]
async fn relation_cache_workloads_run_across_storage_latency_profiles() -> TestResult {
    let manifest = load_manifest()?;
    let scenario = manifest
        .scenarios
        .iter()
        .find(|scenario| scenario.name == "tiny_result_high_work")
        .expect("storage latency scenario is present");
    let rustfs = RustFs::start_or_external().await?;
    let proxy = ToxiProxy::start(&rustfs.config.endpoint, "relation-cache-latency").await?;
    for (profile, latency_ms) in [
        ("object-fast", 5),
        ("object-normal", 15),
        ("object-adverse", 50),
    ] {
        proxy.set_latency(latency_ms, 0).await?;
        let path = format!(
            "relation-cache-latency-{latency_ms}-{}",
            Uuid::new_v4().simple()
        );
        let seed_store = Arc::new(Store::open(path.clone(), object_store(&rustfs.config)?).await?);
        seed_scenario_store(scenario, seed_store).await?;
        let measured_store =
            Arc::new(Store::open(path, object_store_at(&rustfs.config, &proxy.endpoint)?).await?);
        let result = run_preseeded_scenario_with_store(
            &manifest,
            scenario,
            policy_config(
                RelationCachePolicyMode::Enforced,
                RelationCachePrior::GenerationRate,
            ),
            measured_store,
            profile,
        )
        .await?;
        println!("{}", serde_json::to_string(&result)?);
    }
    Ok(())
}

fn load_manifest() -> TestResult<Manifest> {
    let source = std::fs::read(MANIFEST)?;
    let manifest: Manifest = serde_yaml::from_slice(&source)?;
    if manifest.format != FORMAT {
        return Err(format!(
            "unknown relation cache benchmark format {:?}",
            manifest.format
        )
        .into());
    }
    for scenario in &manifest.scenarios {
        if scenario.steps.is_empty() {
            return Err(format!("benchmark scenario {:?} has no steps", scenario.name).into());
        }
        if scenario.dataset_rows == 0 || scenario.dimension_rows == 0 || scenario.fact_rows == 0 {
            return Err(format!(
                "benchmark scenario {:?} has an empty dataset",
                scenario.name
            )
            .into());
        }
        expand(&scenario.steps)?;
    }
    Ok(manifest)
}

fn expand(steps: &[Step]) -> TestResult<Vec<Operation>> {
    let mut operations = Vec::new();
    for step in steps {
        let count = step.count.unwrap_or(1);
        match step.kind {
            StepKind::Query => operations.push(Operation::Query {
                query: required(&step.query, "query", step.kind)?.clone(),
                count: positive(count, step.kind)?,
                interval: Duration::from_millis(step.interval_milliseconds.unwrap_or(0)),
            }),
            StepKind::QuerySeries => operations.push(Operation::QuerySeries {
                query: required(&step.query, "query", step.kind)?.clone(),
                count: positive(count, step.kind)?,
                distribution: step.literal_distribution.unwrap_or_default(),
                cardinality: positive(step.literal_cardinality.unwrap_or(1), step.kind)?,
                offset: step.literal_offset.unwrap_or(0),
                repeat_each: positive(step.repeat_each.unwrap_or(1), step.kind)?,
                interval: Duration::from_millis(step.interval_milliseconds.unwrap_or(0)),
            }),
            StepKind::ConcurrentQuery => operations.push(Operation::ConcurrentQuery {
                query: required(&step.query, "query", step.kind)?.clone(),
                count: positive(count, step.kind)?,
                gate_fill: step.gate_fill,
            }),
            StepKind::Mutation => operations.push(Operation::Mutation {
                table: required(&step.table, "table", step.kind)?.clone(),
                count: positive(count, step.kind)?,
                payload_bytes: step.payload_bytes.unwrap_or(16),
                pattern: step.mutation_pattern.unwrap_or(MutationPattern::Append),
                hot_set_size: positive(step.hot_set_size.unwrap_or(1), step.kind)?,
            }),
            StepKind::Commit => operations.push(Operation::Commit),
            StepKind::TimeAdvance => {
                operations.push(Operation::TimeAdvance(Duration::from_millis(
                    step.milliseconds
                        .ok_or("time_advance requires milliseconds")?,
                )))
            }
            StepKind::Barrier => operations.push(Operation::Barrier),
            StepKind::PinSnapshot => operations.push(Operation::PinSnapshot),
            StepKind::PinnedQuery => operations.push(Operation::PinnedQuery {
                query: required(&step.query, "query", step.kind)?.clone(),
                count: positive(count, step.kind)?,
            }),
            StepKind::ReleaseSnapshot => operations.push(Operation::ReleaseSnapshot),
            StepKind::Checkpoint => operations.push(Operation::Checkpoint(
                required(&step.name, "name", step.kind)?.clone(),
            )),
            StepKind::Repeat => {
                let repeated = expand(&step.steps)?;
                for _ in 0..positive(count, step.kind)? {
                    operations.extend(repeated.iter().cloned());
                }
            }
        }
    }
    Ok(operations)
}

fn required<'a, T>(value: &'a Option<T>, field: &str, kind: StepKind) -> TestResult<&'a T> {
    value
        .as_ref()
        .ok_or_else(|| format!("{kind:?} requires {field}").into())
}

fn positive(value: usize, kind: StepKind) -> TestResult<usize> {
    (value > 0)
        .then_some(value)
        .ok_or_else(|| format!("{kind:?} count must be positive").into())
}

fn policy_config(mode: RelationCachePolicyMode, prior: RelationCachePrior) -> RelationCacheConfig {
    RelationCacheConfig {
        policy: RelationCachePolicyConfig {
            mode,
            prior,
            cohorts_per_exact_relation: 8,
            ..RelationCachePolicyConfig::default()
        },
        ..RelationCacheConfig::default()
    }
}

fn default_reuse_admission() -> String {
    RelationCachePolicyConfig::default()
        .reuse_admission
        .as_str()
        .to_owned()
}

fn default_family_minimum_observations() -> usize {
    RelationCachePolicyConfig::default().family_minimum_observations
}

struct ScenarioExecution<'a> {
    scenario: &'a Scenario,
    engine: &'a Arc<Engine>,
    store: &'a Arc<Store>,
    runtime: &'a Arc<BenchmarkRuntime>,
    observer: &'a Arc<BenchmarkObserver>,
    fill_gate: Option<&'a BenchmarkFillGate>,
    cache_enabled: bool,
    pending: PendingMutations,
    workload: WorkloadState,
    scorecard: Scorecard,
    hash: Sha256,
    pinned: Option<Box<dyn Transaction>>,
    checkpoints: Vec<BenchmarkCheckpoint>,
}

struct ScenarioOutcome {
    scorecard: Scorecard,
    correctness_hash: String,
    checkpoints: Vec<BenchmarkCheckpoint>,
}

impl ScenarioExecution<'_> {
    async fn run(mut self, operations: Vec<Operation>) -> TestResult<ScenarioOutcome> {
        for operation in operations {
            self.execute_operation(operation).await?;
            self.scorecard
                .observe_residency(&self.engine.relation_cache_statistics());
        }
        if !self.pending.is_empty() {
            return Err(format!(
                "scenario {:?} has uncommitted mutations",
                self.scenario.name
            )
            .into());
        }
        if self.pinned.is_some() {
            return Err(format!("scenario {:?} has a pinned snapshot", self.scenario.name).into());
        }
        Ok(ScenarioOutcome {
            scorecard: self.scorecard,
            correctness_hash: format!("{:x}", self.hash.finalize()),
            checkpoints: self.checkpoints,
        })
    }

    async fn execute_operation(&mut self, operation: Operation) -> TestResult {
        match operation {
            Operation::Query {
                query,
                count,
                interval,
            } => self.execute_queries(query, count, interval).await?,
            Operation::QuerySeries {
                query,
                count,
                distribution,
                cardinality,
                offset,
                repeat_each,
                interval,
            } => {
                self.execute_query_series(QuerySeriesExecution {
                    query,
                    count,
                    distribution,
                    cardinality,
                    offset,
                    repeat_each,
                    interval,
                })
                .await?
            }
            Operation::ConcurrentQuery {
                query,
                count,
                gate_fill,
            } => {
                self.execute_concurrent_query(query, count, gate_fill)
                    .await?
            }
            Operation::Mutation {
                table,
                count,
                payload_bytes,
                pattern,
                hot_set_size,
            } => self.stage_mutations(table, count, payload_bytes, pattern, hot_set_size),
            Operation::Commit => self.commit_mutations().await?,
            Operation::TimeAdvance(duration) => self.runtime.advance(duration),
            Operation::Barrier => tokio::task::yield_now().await,
            Operation::PinSnapshot => self.pin_snapshot().await?,
            Operation::PinnedQuery { query, count } => {
                self.execute_pinned_queries(query, count).await?
            }
            Operation::ReleaseSnapshot => self.release_snapshot()?,
            Operation::Checkpoint(name) => self
                .checkpoints
                .push(checkpoint(name, &self.engine.relation_cache_statistics())),
        }
        Ok(())
    }

    async fn execute_queries(
        &mut self,
        query: String,
        count: usize,
        interval: Duration,
    ) -> TestResult {
        for _ in 0..count {
            let query = resolve_query(&query, &mut self.workload.literal_sequence);
            execute_and_verify(
                self.engine,
                self.observer,
                query,
                &mut self.hash,
                &mut self.scorecard,
            )
            .await?;
            self.runtime.advance(interval);
        }
        Ok(())
    }

    async fn execute_query_series(&mut self, series: QuerySeriesExecution) -> TestResult {
        for _ in 0..series.count {
            let literal =
                self.workload
                    .literal(series.distribution, series.cardinality, series.offset);
            let query = query_for_literal(&series.query, literal);
            for _ in 0..series.repeat_each {
                execute_and_verify(
                    self.engine,
                    self.observer,
                    query.clone(),
                    &mut self.hash,
                    &mut self.scorecard,
                )
                .await?;
                self.runtime.advance(series.interval);
            }
        }
        Ok(())
    }

    async fn execute_concurrent_query(
        &mut self,
        query: String,
        count: usize,
        gate_fill: bool,
    ) -> TestResult {
        let query = resolve_query(&query, &mut self.workload.literal_sequence);
        self.observer.take();
        let before_fills = total_fills(&self.engine.relation_cache_statistics());
        let active_fill_gate = self.fill_gate.filter(|_| gate_fill && self.cache_enabled);
        if let Some(fill_gate) = active_fill_gate {
            fill_gate.arm();
        }
        let requests = spawn_concurrent_queries(self.engine, &query, count);
        if let Some(fill_gate) = active_fill_gate {
            release_fill_gate(fill_gate, count).await?;
        }
        let (values, latencies) = collect_concurrent_results(requests).await?;
        let measured = self.observer.take();
        let uncached = execute_query_uncached(self.engine, query).await?;
        let baseline = self.observer.take().work.repeated(count);
        for value in values {
            if value != uncached {
                return Err("concurrent cached result differs from uncached execution".into());
            }
            self.hash
                .update(serde_json::to_vec(&result_json::datum(&value)?)?);
        }
        let fills =
            total_fills(&self.engine.relation_cache_statistics()).saturating_sub(before_fills);
        let fill_latency = latencies.iter().copied().max().unwrap_or_default();
        self.scorecard.observe_batch(
            count,
            measured,
            baseline,
            latencies.iter().copied(),
            fill_latency,
            fills,
        );
        Ok(())
    }

    fn stage_mutations(
        &mut self,
        table: String,
        count: usize,
        payload_bytes: usize,
        pattern: MutationPattern,
        hot_set_size: usize,
    ) {
        for _ in 0..count {
            if matches!(pattern, MutationPattern::Append) {
                self.stage_append(&table, payload_bytes);
            } else {
                self.stage_update(&table, payload_bytes, pattern, hot_set_size);
            }
        }
    }

    fn stage_append(&mut self, table: &str, payload_bytes: usize) {
        self.pending
            .appends
            .entry(table.to_owned())
            .or_default()
            .push(row(table, self.workload.append_sequence, payload_bytes));
        self.workload.append_sequence = self.workload.append_sequence.saturating_add(1);
    }

    fn stage_update(
        &mut self,
        table: &str,
        payload_bytes: usize,
        pattern: MutationPattern,
        hot_set_size: usize,
    ) {
        let rows = scenario_rows(self.scenario, table);
        let target = self.workload.mutation_target(pattern, rows, hot_set_size);
        let sequence = self.workload.mutation_sequence;
        self.pending
            .updates
            .entry(table.to_owned())
            .or_default()
            .push(update_row(table, target, sequence, payload_bytes));
    }

    async fn commit_mutations(&mut self) -> TestResult {
        for (table, rows) in self.pending.appends.drain() {
            self.engine.create_many(&table, rows).await?;
        }
        for (table, rows) in self.pending.updates.drain() {
            self.engine
                .update_many(&table, text_input_type(&["id", "payload"]), rows)
                .await?;
        }
        self.observer.take();
        Ok(())
    }

    async fn pin_snapshot(&mut self) -> TestResult {
        if self.pinned.is_some() {
            return Err("benchmark snapshot is already pinned".into());
        }
        self.pinned = Some(self.store.begin(IsolationLevel::Snapshot).await?);
        Ok(())
    }

    async fn execute_pinned_queries(&mut self, query: String, count: usize) -> TestResult {
        let transaction = self
            .pinned
            .as_mut()
            .ok_or("pinned_query requires a pinned snapshot")?;
        for _ in 0..count {
            let query = resolve_query(&query, &mut self.workload.literal_sequence);
            self.observer.take();
            let before_fills = total_fills(&self.engine.relation_cache_statistics());
            let started = Instant::now();
            let cached = self
                .engine
                .execute_cached_in(transaction.as_mut(), query.clone())
                .await?;
            let elapsed = started.elapsed();
            let mut measured = self.observer.take();
            if measured.executions == 0 {
                measured.executions = 1;
            }
            let uncached = self.engine.execute_in(transaction.as_mut(), query).await?;
            let baseline = self.observer.take().work;
            if cached != uncached {
                return Err("cached pinned result differs from uncached pinned execution".into());
            }
            self.hash
                .update(serde_json::to_vec(&result_json::datum(&cached)?)?);
            let fills =
                total_fills(&self.engine.relation_cache_statistics()).saturating_sub(before_fills);
            self.scorecard
                .observe_request(measured, baseline, elapsed, fills);
        }
        Ok(())
    }

    fn release_snapshot(&mut self) -> TestResult {
        self.pinned
            .take()
            .ok_or("release_snapshot requires a pinned snapshot")?
            .rollback();
        Ok(())
    }
}

type ConcurrentRequest = tokio::task::JoinHandle<(TestResult<lir::Datum>, u64)>;

fn spawn_concurrent_queries(
    engine: &Arc<Engine>,
    query: &lir::Query,
    count: usize,
) -> Vec<ConcurrentRequest> {
    let barrier = Arc::new(tokio::sync::Barrier::new(count));
    (0..count)
        .map(|_| {
            let engine = engine.clone();
            let query = query.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                let started = Instant::now();
                let value = execute_query(&engine, query).await;
                (value, duration_micros(started.elapsed()))
            })
        })
        .collect()
}

async fn release_fill_gate(fill_gate: &BenchmarkFillGate, request_count: usize) -> TestResult {
    if !fill_gate.wait_until_reached().await {
        fill_gate.release();
        return Err("concurrent query did not reach the fill gate".into());
    }
    for _ in 0..request_count {
        tokio::task::yield_now().await;
    }
    fill_gate.release();
    Ok(())
}

async fn collect_concurrent_results(
    requests: Vec<ConcurrentRequest>,
) -> TestResult<(Vec<lir::Datum>, Vec<u64>)> {
    let mut values = Vec::with_capacity(requests.len());
    let mut latencies = Vec::with_capacity(requests.len());
    for request in requests {
        let (value, latency) = request.await?;
        values.push(value?);
        latencies.push(latency);
    }
    Ok((values, latencies))
}

async fn run_scenario(
    manifest: &Manifest,
    scenario: &Scenario,
    config: RelationCacheConfig,
) -> TestResult<BenchmarkResult> {
    let store_name = format!(
        "relation-cache-benchmark-{}-{}-{}",
        scenario.name,
        config.policy.mode.as_str(),
        config.policy.prior.as_str(),
    );
    let store = Arc::new(Store::memory(&store_name).await?);
    run_scenario_with_store(manifest, scenario, config, store, "memory").await
}

async fn run_scenario_with_store(
    manifest: &Manifest,
    scenario: &Scenario,
    config: RelationCacheConfig,
    store: Arc<Store>,
    backend: &str,
) -> TestResult<BenchmarkResult> {
    run_scenario_with_store_state(manifest, scenario, config, store, backend, true).await
}

async fn run_preseeded_scenario_with_store(
    manifest: &Manifest,
    scenario: &Scenario,
    config: RelationCacheConfig,
    store: Arc<Store>,
    backend: &str,
) -> TestResult<BenchmarkResult> {
    run_scenario_with_store_state(manifest, scenario, config, store, backend, false).await
}

async fn run_scenario_with_store_state(
    manifest: &Manifest,
    scenario: &Scenario,
    config: RelationCacheConfig,
    store: Arc<Store>,
    backend: &str,
    seed_store: bool,
) -> TestResult<BenchmarkResult> {
    let config = configure_scenario_limits(config, scenario);
    let cache_enabled = config.domains != RelationCacheDomains::none();
    let operations = expand(&scenario.steps)?;
    let fill_gate = operations
        .iter()
        .any(operation_requires_fill_gate)
        .then(|| Arc::new(BenchmarkFillGate::default()));
    let runtime = Arc::new(BenchmarkRuntime::default());
    let observer = Arc::new(BenchmarkObserver::default());
    let engine = create_benchmark_engine(
        &store,
        scenario,
        config,
        &runtime,
        &observer,
        fill_gate.as_ref(),
        seed_store,
    )
    .await?;
    if seed_store {
        seed(&engine, scenario).await?;
    }
    observer.take();

    let execution = ScenarioExecution {
        scenario,
        engine: &engine,
        store: &store,
        runtime: &runtime,
        observer: &observer,
        fill_gate: fill_gate.as_deref(),
        cache_enabled,
        pending: PendingMutations::default(),
        workload: WorkloadState {
            append_sequence: 10_000,
            ..Default::default()
        },
        scorecard: Scorecard::default(),
        hash: Sha256::new(),
        pinned: None,
        checkpoints: Vec::new(),
    };
    let outcome = execution.run(operations).await?;
    let statistics = engine.relation_cache_statistics();
    let result = benchmark_result(
        manifest,
        scenario,
        config,
        backend,
        cache_enabled,
        outcome,
        statistics,
    );
    drop(engine);
    store.close().await?;
    Ok(result)
}

async fn seed_scenario_store(scenario: &Scenario, store: Arc<Store>) -> TestResult {
    let runtime = Arc::new(BenchmarkRuntime::default());
    let observer = Arc::new(BenchmarkObserver::default());
    let mut config = policy_config(RelationCachePolicyMode::Foyer, RelationCachePrior::None);
    config.domains = RelationCacheDomains::none();
    let engine =
        create_benchmark_engine(&store, scenario, config, &runtime, &observer, None, true).await?;
    seed(&engine, scenario).await?;
    drop(engine);
    store.close().await?;
    Ok(())
}

fn configure_scenario_limits(
    mut config: RelationCacheConfig,
    scenario: &Scenario,
) -> RelationCacheConfig {
    config.limits = RelationCacheLimits {
        byte_limit: scenario
            .cache_capacity_bytes
            .unwrap_or(config.limits.byte_limit),
        entry_limit: scenario
            .cache_entry_limit
            .unwrap_or(config.limits.entry_limit),
        result_byte_limit: scenario
            .result_limit_bytes
            .unwrap_or(config.limits.result_byte_limit),
    };
    config
}

fn operation_requires_fill_gate(operation: &Operation) -> bool {
    matches!(
        operation,
        Operation::ConcurrentQuery {
            gate_fill: true,
            ..
        }
    )
}

async fn create_benchmark_engine(
    store: &Arc<Store>,
    scenario: &Scenario,
    config: RelationCacheConfig,
    runtime: &Arc<BenchmarkRuntime>,
    observer: &Arc<BenchmarkObserver>,
    fill_gate: Option<&Arc<BenchmarkFillGate>>,
    create_tables: bool,
) -> TestResult<Arc<Engine>> {
    let catalog = catalog::Catalog::new(store.clone());
    let items = benchmark_table(&catalog, 1, "items", create_tables).await?;
    let dimensions = benchmark_table(&catalog, 2, "dimensions", create_tables).await?;
    let facts = benchmark_table(&catalog, 3, "facts", create_tables).await?;
    let mut statistics = complete_stats(&items, scenario.dataset_rows as u64);
    statistics
        .synopsis_models
        .extend(complete_stats(&dimensions, scenario.dimension_rows as u64).synopsis_models);
    statistics
        .synopsis_models
        .extend(complete_stats(&facts, scenario.fact_rows.max(5_000) as u64).synopsis_models);
    let mut engine =
        Engine::with_limits_and_runtime(store.clone(), Limits::default(), runtime.clone())
            .with_relation_cache_config(config)
            .with_observer(observer.clone())
            .with_statistics_provider(Arc::new(FixedPlannerStats(Arc::new(statistics))));
    if let Some(fill_gate) = fill_gate {
        engine = engine.with_event_hook(fill_gate.clone());
    }
    Ok(Arc::new(engine))
}

async fn benchmark_table(
    catalog: &catalog::Catalog,
    schema_id: u32,
    name: &str,
    create: bool,
) -> TestResult<Table> {
    if create {
        return Ok(catalog.create_table(table(schema_id, name)).await?);
    }
    catalog
        .get_table(name)
        .await?
        .ok_or_else(|| format!("benchmark table {name:?} is missing").into())
}

fn benchmark_result(
    manifest: &Manifest,
    scenario: &Scenario,
    config: RelationCacheConfig,
    backend: &str,
    cache_enabled: bool,
    outcome: ScenarioOutcome,
    statistics: RelationCacheStatistics,
) -> BenchmarkResult {
    let ScenarioOutcome {
        scorecard,
        correctness_hash,
        checkpoints,
    } = outcome;
    let admissions = statistics
        .admissions
        .saturating_add(statistics.subrelation_admissions);
    let policy_rejections = statistics
        .rad_policy_rejections
        .saturating_add(statistics.subrelation_rad_policy_rejections);
    let foyer_rejections = statistics
        .foyer_rejections
        .saturating_add(statistics.subrelation_foyer_rejections);
    let hard_limit_rejections = statistics
        .rejected_too_large
        .saturating_add(statistics.subrelation_rejected_too_large);
    let resident_hits = statistics.hits.saturating_add(statistics.subrelation_hits);
    let coalesced_reuses = statistics
        .coalesced
        .saturating_add(statistics.subrelation_coalesced);
    let executed_fills = statistics
        .fills
        .saturating_add(statistics.subrelation_fills);
    let work_avoided_score = if cache_enabled {
        scorecard.baseline_work.saved_by(scorecard.actual_work)
    } else {
        WorkScore::default()
    };
    let work_performed = scorecard.actual_work.work_units;
    let work_avoided = work_avoided_score.work_units;
    let policy = statistics.policy.total;
    let domains = BTreeMap::from([
        (
            "query".to_owned(),
            domain_result(statistics.domains.query, statistics.policy.query),
        ),
        (
            "hash_build".to_owned(),
            domain_result(statistics.domains.hash_build, statistics.policy.hash_build),
        ),
        (
            "grouped_dimension".to_owned(),
            domain_result(
                statistics.domains.grouped_dimension,
                statistics.policy.grouped_dimension,
            ),
        ),
    ]);
    BenchmarkResult {
        format: RESULT_FORMAT.to_owned(),
        benchmark: manifest.name.clone(),
        backend: backend.to_owned(),
        scenario: scenario.name.clone(),
        expected: scenario.expected,
        shape: scenario.shape,
        dataset_rows: scenario.dataset_rows,
        falsification: scenario.falsification,
        policy_mode: if cache_enabled {
            config.policy.mode.as_str()
        } else {
            "off"
        }
        .to_owned(),
        prior: config.policy.prior.as_str().to_owned(),
        reuse_admission: config.policy.reuse_admission.as_str().to_owned(),
        family_minimum_observations: config.policy.family_minimum_observations,
        correctness_hash,
        requests: scorecard.requests,
        executions: scorecard.executions,
        executed_fills,
        admissions,
        policy_rejections,
        foyer_rejections,
        hard_limit_rejections,
        resident_hits,
        coalesced_reuses,
        work_performed,
        work_avoided,
        work: scorecard.actual_work,
        work_avoided_score,
        retained_bytes: statistics.retained_bytes,
        peak_resident_bytes: scorecard.peak_resident_bytes,
        completed_cohorts: policy.completed_cohorts,
        zero_reuse_cohorts: policy.zero_reuse_cohorts,
        false_admissions: policy.false_admissions,
        false_rejections: policy.false_rejections,
        admissions_followed_by_reuse: policy.admissions_followed_by_reuse,
        admissions_without_future_reuse: policy.admissions_without_future_reuse,
        rejections_followed_by_reuse: policy.rejections_followed_by_reuse,
        avoidable_work_after_rejection: policy.avoidable_work_after_rejection,
        retained_bytes_without_future_reuse: policy.retained_bytes_without_future_reuse,
        family_entries: policy.family_entries,
        family_second_touch_observations: policy.family_second_touch_observations,
        family_third_touch_conversions: policy.family_third_touch_conversions,
        oracle_value: policy.oracle_value,
        actual_residency_value: policy.actual_residency_value,
        coalescing_value: policy.coalescing_value,
        policy_regret: policy.policy_regret,
        zero_reuse_cohort_percent: percent_of(policy.zero_reuse_cohorts, policy.completed_cohorts),
        cohort_quantiles: statistics.policy.total_cohorts.into(),
        request_latency: LatencyQuantiles::from_samples(scorecard.request_latency_micros),
        fill_latency: LatencyQuantiles::from_samples(scorecard.fill_latency_micros),
        decisions: decisions(policy),
        domains,
        checkpoints,
    }
}

fn total_fills(statistics: &RelationCacheStatistics) -> u64 {
    statistics
        .fills
        .saturating_add(statistics.subrelation_fills)
}

fn percent_of(part: u64, total: u64) -> u8 {
    if total == 0 {
        return 0;
    }
    part.saturating_mul(100).saturating_div(total).min(100) as u8
}

fn scenario_rows(scenario: &Scenario, table: &str) -> usize {
    match table {
        "items" => scenario.dataset_rows,
        "dimensions" => scenario.dimension_rows,
        "facts" => scenario.fact_rows,
        _ => scenario.dataset_rows,
    }
}

fn decisions(policy: RelationCachePolicyCounters) -> DecisionBreakdown {
    DecisionBreakdown {
        second_touch: policy.admit_second_touch,
        third_touch: policy.admit_third_touch,
        value_density: policy.admit_value_density,
        family_conversion: policy.admit_family_conversion,
        learned_value: policy.admit_learned_value,
        generation_rate: policy.admit_generation_rate,
        recovery_probe: policy.admit_recovery_probe,
        expensive_probation: policy.admit_expensive_probation,
        recent_no_reuse: policy.reject_recent_no_reuse,
        no_reuse_history: policy.reject_no_reuse_history,
        insufficient_value: policy.reject_insufficient_value,
        too_large: policy.reject_too_large,
    }
}

fn domain_result(
    activity: RelationCacheDomainStatistics,
    policy: RelationCachePolicyCounters,
) -> DomainResult {
    DomainResult {
        executed_fills: activity.fills,
        admissions: activity.admissions,
        policy_rejections: activity.rad_policy_rejections,
        foyer_rejections: activity.foyer_rejections,
        hard_limit_rejections: activity.rejected_too_large,
        resident_hits: activity.hits,
        coalesced_reuses: activity.coalesced,
        work_performed: activity.fill_work_units,
        work_avoided: activity.avoided_work_units,
        retained_bytes: activity.retained_bytes,
        completed_cohorts: policy.completed_cohorts,
        zero_reuse_cohorts: policy.zero_reuse_cohorts,
        false_admissions: policy.false_admissions,
        false_rejections: policy.false_rejections,
        admissions_followed_by_reuse: policy.admissions_followed_by_reuse,
        admissions_without_future_reuse: policy.admissions_without_future_reuse,
        rejections_followed_by_reuse: policy.rejections_followed_by_reuse,
        avoidable_work_after_rejection: policy.avoidable_work_after_rejection,
        retained_bytes_without_future_reuse: policy.retained_bytes_without_future_reuse,
        oracle_value: policy.oracle_value,
        actual_residency_value: policy.actual_residency_value,
        coalescing_value: policy.coalescing_value,
        policy_regret: policy.policy_regret,
        decisions: decisions(policy),
    }
}

fn checkpoint(name: String, statistics: &RelationCacheStatistics) -> BenchmarkCheckpoint {
    let policy = statistics.policy.total;
    BenchmarkCheckpoint {
        name,
        executed_fills: statistics
            .fills
            .saturating_add(statistics.subrelation_fills),
        admissions: statistics
            .admissions
            .saturating_add(statistics.subrelation_admissions),
        policy_rejections: statistics
            .rad_policy_rejections
            .saturating_add(statistics.subrelation_rad_policy_rejections),
        resident_hits: statistics.hits.saturating_add(statistics.subrelation_hits),
        oracle_value: policy.oracle_value,
        actual_residency_value: policy.actual_residency_value,
        policy_regret: policy.policy_regret,
    }
}

fn write_benchmark_run(
    path: &Path,
    phase: &str,
    manifest: &Manifest,
    results: Vec<BenchmarkResult>,
) -> TestResult {
    if phase.is_empty() {
        return Err("benchmark phase must not be empty".into());
    }
    let run = BenchmarkRun {
        format: RUN_FORMAT.to_owned(),
        phase: phase.to_owned(),
        benchmark_format: manifest.format.clone(),
        benchmark: manifest.name.clone(),
        manifest_hash: file_hash(Path::new(MANIFEST))?,
        source_revision: source_revision(),
        source_hash: source_hash()?,
        results,
    };
    let mut output = serde_json::to_vec_pretty(&run)?;
    output.push(b'\n');
    write_file(path, &output)
}

fn write_file(path: &Path, contents: &[u8]) -> TestResult {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    Ok(())
}

fn file_hash(path: &Path) -> TestResult<String> {
    let mut hash = Sha256::new();
    hash.update(std::fs::read(path)?);
    Ok(format!("{:x}", hash.finalize()))
}

fn source_hash() -> TestResult<String> {
    let mut hash = Sha256::new();
    for path in [
        "src/engine/04_planner/plan.rs",
        "src/engine/05_exec/engine.rs",
        "src/engine/05_exec/relation_cache.rs",
        "src/engine/05_exec/relation_cache/policy.rs",
        "tests/relation_cache_policy_benchmark.rs",
        MANIFEST,
        "tests/benchmarks/relation_cache/falsification.rs",
        "tests/benchmarks/relation_cache/falsification.yaml",
    ] {
        hash.update((path.len() as u64).to_be_bytes());
        hash.update(path.as_bytes());
        hash.update(std::fs::read(path)?);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn source_revision() -> String {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|revision| revision.trim().to_owned())
        .filter(|revision| !revision.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

#[derive(Default)]
struct ComparisonTotal {
    requests: u64,
    executions: u64,
    fills: u64,
    admissions: u64,
    policy_rejections: u64,
    resident_hits: u64,
    work_executed: u64,
    work_avoided: u64,
    bytes_avoided: u64,
    oracle_value: i128,
    actual_residency_value: i128,
    policy_regret: i128,
}

impl ComparisonTotal {
    fn observe(&mut self, result: &BenchmarkResult) {
        self.requests = self.requests.saturating_add(result.requests);
        self.executions = self.executions.saturating_add(result.executions);
        self.fills = self.fills.saturating_add(result.executed_fills);
        self.admissions = self.admissions.saturating_add(result.admissions);
        self.policy_rejections = self
            .policy_rejections
            .saturating_add(result.policy_rejections);
        self.resident_hits = self.resident_hits.saturating_add(result.resident_hits);
        self.work_executed = self.work_executed.saturating_add(result.work_performed);
        self.work_avoided = self.work_avoided.saturating_add(result.work_avoided);
        self.bytes_avoided = self
            .bytes_avoided
            .saturating_add(result.work_avoided_score.bytes_read);
        self.oracle_value = self.oracle_value.saturating_add(result.oracle_value);
        self.actual_residency_value = self
            .actual_residency_value
            .saturating_add(result.actual_residency_value);
        self.policy_regret = self.policy_regret.saturating_add(result.policy_regret);
    }
}

fn compare_runs(baseline: &BenchmarkRun, candidate: &BenchmarkRun) -> TestResult<String> {
    if baseline.format != RUN_FORMAT || candidate.format != RUN_FORMAT {
        return Err("benchmark run format is not supported".into());
    }
    let baseline_results = baseline
        .results
        .iter()
        .map(|result| (result_key(result), result))
        .collect::<BTreeMap<_, _>>();
    let candidate_results = candidate
        .results
        .iter()
        .map(|result| (result_key(result), result))
        .collect::<BTreeMap<_, _>>();
    let shared_keys = baseline_results
        .keys()
        .filter(|key| candidate_results.contains_key(*key))
        .cloned()
        .collect::<Vec<_>>();
    if shared_keys.is_empty() {
        return Err("benchmark runs have no common results".into());
    }
    let common_keys = shared_keys
        .iter()
        .filter(|key| {
            baseline_results
                .get(*key)
                .zip(candidate_results.get(*key))
                .is_some_and(|(baseline, candidate)| {
                    baseline.correctness_hash == candidate.correctness_hash
                })
        })
        .cloned()
        .collect::<Vec<_>>();
    if common_keys.is_empty() {
        return Err("benchmark runs have no comparable results".into());
    }
    let scorecards_comparable = common_keys.iter().all(|key| {
        baseline_results
            .get(key)
            .is_some_and(|result| result.format == RESULT_FORMAT)
            && candidate_results
                .get(key)
                .is_some_and(|result| result.format == RESULT_FORMAT)
    });

    let mut baseline_totals = BTreeMap::<(String, String), ComparisonTotal>::new();
    let mut candidate_totals = BTreeMap::<(String, String), ComparisonTotal>::new();
    for key in &common_keys {
        let result = baseline_results
            .get(key)
            .expect("common baseline result is present");
        baseline_totals
            .entry((result.policy_mode.clone(), result.prior.clone()))
            .or_default()
            .observe(result);
        let result = candidate_results
            .get(key)
            .expect("common candidate result is present");
        candidate_totals
            .entry((result.policy_mode.clone(), result.prior.clone()))
            .or_default()
            .observe(result);
    }

    let mut report = String::new();
    writeln!(report, "# Relation cache benchmark comparison")?;
    writeln!(report)?;
    writeln!(report, "- Baseline: `{}`", baseline.phase)?;
    writeln!(report, "- Candidate: `{}`", candidate.phase)?;
    writeln!(report, "- Baseline source: `{}`", baseline.source_hash)?;
    writeln!(report, "- Candidate source: `{}`", candidate.source_hash)?;
    writeln!(
        report,
        "- Manifest match: `{}`",
        baseline.manifest_hash == candidate.manifest_hash
    )?;
    writeln!(report, "- Common results: `{}`", common_keys.len())?;
    writeln!(
        report,
        "- Changed results: `{}`",
        shared_keys.len().saturating_sub(common_keys.len())
    )?;
    writeln!(
        report,
        "- Added results: `{}`",
        candidate_results.len().saturating_sub(shared_keys.len())
    )?;
    writeln!(
        report,
        "- Removed results: `{}`",
        baseline_results.len().saturating_sub(shared_keys.len())
    )?;
    if !scorecards_comparable {
        writeln!(
            report,
            "- Scorecard deltas: `not comparable across result formats`"
        )?;
    }
    writeln!(report)?;
    writeln!(
        report,
        "| Policy | Prior | Request delta | Execution delta | Fill delta | Admission delta | Rejection delta | Hit delta | Executed work delta | Avoided work delta | Avoided bytes delta | Actual value delta | Regret delta |"
    )?;
    writeln!(
        report,
        "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    )?;
    for (key, baseline_total) in &baseline_totals {
        let candidate_total = candidate_totals
            .get(key)
            .expect("matching benchmark total is present");
        writeln!(
            report,
            "| {} | {} | {} | {} | {:+} | {:+} | {:+} | {:+} | {} | {} | {} | {:+} | {:+} |",
            key.0,
            key.1,
            scorecard_delta(
                candidate_total.requests,
                baseline_total.requests,
                scorecards_comparable,
            ),
            scorecard_delta(
                candidate_total.executions,
                baseline_total.executions,
                scorecards_comparable,
            ),
            delta_u64(candidate_total.fills, baseline_total.fills),
            delta_u64(candidate_total.admissions, baseline_total.admissions),
            delta_u64(
                candidate_total.policy_rejections,
                baseline_total.policy_rejections,
            ),
            delta_u64(candidate_total.resident_hits, baseline_total.resident_hits),
            scorecard_delta(
                candidate_total.work_executed,
                baseline_total.work_executed,
                scorecards_comparable,
            ),
            scorecard_delta(
                candidate_total.work_avoided,
                baseline_total.work_avoided,
                scorecards_comparable,
            ),
            scorecard_delta(
                candidate_total.bytes_avoided,
                baseline_total.bytes_avoided,
                scorecards_comparable,
            ),
            candidate_total
                .actual_residency_value
                .saturating_sub(baseline_total.actual_residency_value),
            candidate_total
                .policy_regret
                .saturating_sub(baseline_total.policy_regret),
        )?;
    }
    writeln!(report)?;
    writeln!(report, "## Enforced generation-rate scenario deltas")?;
    writeln!(report)?;
    writeln!(
        report,
        "| Scenario | Fill delta | Admission delta | Hit delta | Actual value delta | Regret delta |"
    )?;
    writeln!(report, "| --- | ---: | ---: | ---: | ---: | ---: |")?;
    for key in &common_keys {
        let baseline_result = baseline_results
            .get(key)
            .expect("common baseline result is present");
        if key.1 != "enforced" || key.2 != "generation_rate" {
            continue;
        }
        let candidate_result = candidate_results
            .get(key)
            .expect("matching benchmark result is present");
        writeln!(
            report,
            "| {} | {:+} | {:+} | {:+} | {:+} | {:+} |",
            key.0,
            delta_u64(
                candidate_result.executed_fills,
                baseline_result.executed_fills
            ),
            delta_u64(candidate_result.admissions, baseline_result.admissions),
            delta_u64(
                candidate_result.resident_hits,
                baseline_result.resident_hits
            ),
            candidate_result
                .actual_residency_value
                .saturating_sub(baseline_result.actual_residency_value),
            candidate_result
                .policy_regret
                .saturating_sub(baseline_result.policy_regret),
        )?;
    }
    write_transition_targets(&mut report, &baseline_results, &candidate_results)?;
    Ok(report)
}

fn scorecard_delta(candidate: u64, baseline: u64, comparable: bool) -> String {
    if comparable {
        format!("{:+}", delta_u64(candidate, baseline))
    } else {
        "n/a".to_owned()
    }
}

fn write_transition_targets(
    report: &mut String,
    baseline: &BTreeMap<ResultKey, &BenchmarkResult>,
    candidate: &BTreeMap<ResultKey, &BenchmarkResult>,
) -> std::fmt::Result {
    let required = [
        ("wasteful_to_useful", "wasteful_phase"),
        ("useful_to_wasteful", "useful_phase"),
    ];
    let complete = [baseline, candidate].into_iter().all(|results| {
        ["none", "generation_rate"].into_iter().all(|prior| {
            required.iter().all(|(scenario, checkpoint)| {
                has_transition_value(results, scenario, prior, checkpoint)
            })
        })
    });
    if !complete {
        return Ok(());
    }
    writeln!(report)?;
    writeln!(report, "## Phase transition targets")?;
    writeln!(report)?;
    writeln!(
        report,
        "| Prior | Wasteful-to-useful baseline capture | Candidate capture | Capture delta | Useful-to-wasteful baseline value | Candidate value | Value delta |"
    )?;
    writeln!(report, "| --- | ---: | ---: | ---: | ---: | ---: | ---: |")?;
    for prior in ["none", "generation_rate"] {
        let baseline_recovery =
            transition_value(baseline, "wasteful_to_useful", prior, "wasteful_phase");
        let candidate_recovery =
            transition_value(candidate, "wasteful_to_useful", prior, "wasteful_phase");
        let baseline_cooling =
            transition_value(baseline, "useful_to_wasteful", prior, "useful_phase");
        let candidate_cooling =
            transition_value(candidate, "useful_to_wasteful", prior, "useful_phase");
        let baseline_capture = value_capture_basis_points(baseline_recovery);
        let candidate_capture = value_capture_basis_points(candidate_recovery);
        writeln!(
            report,
            "| {prior} | {} | {} | {} | {:+} | {:+} | {:+} |",
            format_basis_points(baseline_capture),
            format_basis_points(candidate_capture),
            format_basis_points(candidate_capture.saturating_sub(baseline_capture)),
            baseline_cooling.1,
            candidate_cooling.1,
            candidate_cooling.1.saturating_sub(baseline_cooling.1),
        )?;
    }
    Ok(())
}

fn has_transition_value(
    results: &BTreeMap<ResultKey, &BenchmarkResult>,
    scenario: &str,
    prior: &str,
    checkpoint: &str,
) -> bool {
    results
        .get(&(
            scenario.to_owned(),
            "enforced".to_owned(),
            prior.to_owned(),
            "memory".to_owned(),
            RelationCacheReuseAdmission::SecondTouch.as_str().to_owned(),
            RelationCachePolicyConfig::default().family_minimum_observations,
        ))
        .is_some_and(|result| {
            result
                .checkpoints
                .iter()
                .any(|value| value.name == checkpoint)
        })
}

fn transition_value(
    results: &BTreeMap<ResultKey, &BenchmarkResult>,
    scenario: &str,
    prior: &str,
    checkpoint: &str,
) -> (i128, i128) {
    let result = results
        .get(&(
            scenario.to_owned(),
            "enforced".to_owned(),
            prior.to_owned(),
            "memory".to_owned(),
            RelationCacheReuseAdmission::SecondTouch.as_str().to_owned(),
            RelationCachePolicyConfig::default().family_minimum_observations,
        ))
        .expect("phase transition benchmark result is present");
    let checkpoint = result
        .checkpoints
        .iter()
        .find(|value| value.name == checkpoint)
        .expect("phase transition checkpoint is present");
    (
        result.oracle_value.saturating_sub(checkpoint.oracle_value),
        result
            .actual_residency_value
            .saturating_sub(checkpoint.actual_residency_value),
    )
}

fn value_capture_basis_points((oracle, actual): (i128, i128)) -> i128 {
    if oracle <= 0 {
        return 0;
    }
    actual.saturating_mul(10_000).saturating_div(oracle)
}

fn format_basis_points(value: i128) -> String {
    let sign = if value < 0 { "-" } else { "" };
    let magnitude = value.saturating_abs();
    format!("{sign}{}.{:02}%", magnitude / 100, magnitude % 100)
}

fn result_key(result: &BenchmarkResult) -> ResultKey {
    (
        result.scenario.clone(),
        result.policy_mode.clone(),
        result.prior.clone(),
        result.backend.clone(),
        result.reuse_admission.clone(),
        result.family_minimum_observations,
    )
}

fn delta_u64(candidate: u64, baseline: u64) -> i128 {
    i128::from(candidate).saturating_sub(i128::from(baseline))
}

async fn execute_and_verify(
    engine: &Engine,
    observer: &BenchmarkObserver,
    query: lir::Query,
    hash: &mut Sha256,
    scorecard: &mut Scorecard,
) -> TestResult {
    observer.take();
    let before_fills = total_fills(&engine.relation_cache_statistics());
    let started = Instant::now();
    let cached = execute_query(engine, query.clone()).await?;
    let elapsed = started.elapsed();
    let measured = observer.take();
    let uncached = execute_query_uncached(engine, query).await?;
    let baseline = observer.take().work;
    if cached != uncached {
        return Err("cached result differs from uncached execution".into());
    }
    hash.update(serde_json::to_vec(&result_json::datum(&cached)?)?);
    let after_fills = total_fills(&engine.relation_cache_statistics());
    scorecard.observe_request(
        measured,
        baseline,
        elapsed,
        after_fills.saturating_sub(before_fills),
    );
    Ok(())
}

async fn execute_query(engine: &Engine, query: lir::Query) -> TestResult<lir::Datum> {
    Ok(engine
        .execute_program(query_program(query), CatalogPolicy::Forbidden)
        .await?
        .result)
}

async fn execute_query_uncached(engine: &Engine, query: lir::Query) -> TestResult<lir::Datum> {
    Ok(engine
        .execute_program_uncached(query_program(query), CatalogPolicy::Forbidden)
        .await?
        .result)
}

fn query_program(query: lir::Query) -> Program {
    Program {
        statements: vec![Statement::Query {
            name: "read".into(),
            relation: query,
        }],
        result: Some("read".into()),
    }
}

fn table(id: u32, name: &str) -> TableDef {
    TableDef {
        id: SchemaId::new(id).expect("table schema ID is valid"),
        name: name.into(),
        columns: vec![
            ColumnDef {
                id: SchemaId::new(1).expect("column schema ID is valid"),
                name: "id".into(),
                scalar_type: ScalarType::Text,
                nullable: false,
                format: String::new(),
                default: None,
            },
            ColumnDef {
                id: SchemaId::new(2).expect("column schema ID is valid"),
                name: "join_key".into(),
                scalar_type: ScalarType::Text,
                nullable: false,
                format: String::new(),
                default: None,
            },
            ColumnDef {
                id: SchemaId::new(3).expect("column schema ID is valid"),
                name: "payload".into(),
                scalar_type: ScalarType::Text,
                nullable: false,
                format: String::new(),
                default: None,
            },
        ],
        primary_key: vec!["id".into()],
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
    }
}

async fn seed(engine: &Engine, scenario: &Scenario) -> TestResult {
    engine
        .create_many(
            "items",
            (0..scenario.dataset_rows)
                .map(|index| row("items", index as u64, scenario.seed_payload_bytes))
                .collect(),
        )
        .await?;
    engine
        .create_many(
            "dimensions",
            (0..scenario.dimension_rows)
                .map(|index| row("dimensions", index as u64, scenario.seed_payload_bytes))
                .collect(),
        )
        .await?;
    engine
        .create_many(
            "facts",
            (0..scenario.fact_rows)
                .map(|index| row("facts", index as u64, scenario.seed_payload_bytes))
                .collect(),
        )
        .await?;
    Ok(())
}

fn row(table: &str, sequence: u64, payload_bytes: usize) -> Row {
    let prefix = match table {
        "items" => "item",
        "dimensions" => "dimension",
        "facts" => "fact",
        _ => "row",
    };
    Row::from([
        ("id".into(), Value::Text(format!("{prefix}-{sequence:04}"))),
        (
            "join_key".into(),
            Value::Text(if table == "items" && sequence >= 10_000 {
                "tail".into()
            } else if sequence.is_multiple_of(2) {
                "closed".into()
            } else {
                "tail".into()
            }),
        ),
        ("payload".into(), Value::Text("x".repeat(payload_bytes))),
    ])
}

fn update_row(table: &str, target: usize, sequence: u64, payload_bytes: usize) -> Row {
    let prefix = match table {
        "items" => "item",
        "dimensions" => "dimension",
        "facts" => "fact",
        _ => "row",
    };
    Row::from([
        ("id".into(), Value::Text(format!("{prefix}-{target:04}"))),
        (
            "payload".into(),
            Value::Text(format!(
                "{:0width$}",
                sequence,
                width = payload_bytes.max(1)
            )),
        ),
    ])
}

fn text_input_type(names: &[&str]) -> RowType {
    RowType {
        fields: names
            .iter()
            .enumerate()
            .map(|(index, name)| Field {
                name: (*name).into(),
                slot: SlotId(index),
                value_type: Type::scalar(Kind::Text, false),
            })
            .collect(),
    }
}

fn resolve_query(query: &str, literal_sequence: &mut u64) -> lir::Query {
    if query == "next-point" {
        let resolved = format!("point-{:04}", *literal_sequence);
        *literal_sequence = (*literal_sequence).saturating_add(1);
        return query_for(&resolved);
    }
    query_for(query)
}

fn query_for_literal(query: &str, literal: usize) -> lir::Query {
    match query {
        "point" => query_for(&format!("point-{literal:04}")),
        "range" => query_for(&format!("range-{literal:04}")),
        "aggregate-range" => aggregate_range_query(literal, 10),
        _ => query_for(query),
    }
}

fn query_for(query: &str) -> lir::Query {
    match query {
        "stable" => ordered_scan("items", "item", false),
        "aggregate" => aggregate_query(),
        "tail" => lir::Query {
            root: Relation::Slice {
                input: Box::new(ordered_relation("items", "item", true)),
                offset: 0,
                limit: Some(5),
            },
            cardinality: RootCardinality::Many,
            bindings: HashMap::new(),
        },
        "closed-range" => filtered_items("join_key", "closed", RootCardinality::Many),
        "join" => join_query(),
        range if range.starts_with("range-") => range_query(
            range[6..]
                .parse::<usize>()
                .expect("range literal is numeric"),
            10,
        ),
        point if point.starts_with("point-") => filtered_items(
            "id",
            &format!("item-{}", &point[6..]),
            RootCardinality::First,
        ),
        _ => panic!("unknown benchmark query {query:?}"),
    }
}

fn aggregate_query() -> lir::Query {
    lir::Query {
        root: Relation::Aggregate {
            input: Box::new(Relation::Scan {
                table: "items".into(),
                scope: "item".into(),
            }),
            scope: Some("summary".into()),
            groups: Vec::new(),
            terms: vec![lir::AggregateTerm {
                function: lir::AggregateFunction::Count,
                argument: Some(Expr::Column {
                    scope: "item".into(),
                    name: "id".into(),
                }),
                name: "item_count".into(),
            }],
        },
        cardinality: RootCardinality::ExactlyOne,
        bindings: HashMap::new(),
    }
}

fn aggregate_range_query(start: usize, width: usize) -> lir::Query {
    lir::Query {
        root: Relation::Aggregate {
            input: Box::new(range_query(start, width).root),
            scope: Some("summary".into()),
            groups: Vec::new(),
            terms: vec![lir::AggregateTerm {
                function: lir::AggregateFunction::Count,
                argument: Some(Expr::Column {
                    scope: "item".into(),
                    name: "id".into(),
                }),
                name: "item_count".into(),
            }],
        },
        cardinality: RootCardinality::ExactlyOne,
        bindings: HashMap::new(),
    }
}

fn range_query(start: usize, width: usize) -> lir::Query {
    let end = start.saturating_add(width);
    let column = || Expr::Column {
        scope: "item".into(),
        name: "id".into(),
    };
    let literal = |value: usize| {
        Expr::Literal(Literal {
            raw: RawScalar::Text(format!("item-{value:04}")),
            kind: None,
        })
    };
    lir::Query {
        root: Relation::Filter {
            input: Box::new(ordered_relation("items", "item", false)),
            predicate: Expr::Binary {
                op: BinaryOp::And,
                left: Box::new(Expr::Binary {
                    op: BinaryOp::Gte,
                    left: Box::new(column()),
                    right: Box::new(literal(start)),
                }),
                right: Box::new(Expr::Binary {
                    op: BinaryOp::Lt,
                    left: Box::new(column()),
                    right: Box::new(literal(end)),
                }),
            },
        },
        cardinality: RootCardinality::Many,
        bindings: HashMap::new(),
    }
}

fn ordered_scan(table: &str, scope: &str, descending: bool) -> lir::Query {
    lir::Query {
        root: ordered_relation(table, scope, descending),
        cardinality: RootCardinality::Many,
        bindings: HashMap::new(),
    }
}

fn ordered_relation(table: &str, scope: &str, descending: bool) -> Relation {
    Relation::Order {
        input: Box::new(Relation::Scan {
            table: table.into(),
            scope: scope.into(),
        }),
        terms: vec![lir::OrderTerm {
            expression: Expr::Column {
                scope: scope.into(),
                name: "id".into(),
            },
            descending,
        }],
    }
}

fn filtered_items(column: &str, value: &str, cardinality: RootCardinality) -> lir::Query {
    let input = if column == "id" && cardinality == RootCardinality::First {
        Relation::Scan {
            table: "items".into(),
            scope: "item".into(),
        }
    } else {
        ordered_relation("items", "item", false)
    };
    lir::Query {
        root: Relation::Filter {
            input: Box::new(input),
            predicate: Expr::Binary {
                op: BinaryOp::Eq,
                left: Box::new(Expr::Column {
                    scope: "item".into(),
                    name: column.into(),
                }),
                right: Box::new(Expr::Literal(Literal {
                    raw: RawScalar::Text(value.into()),
                    kind: None,
                })),
            },
        },
        cardinality,
        bindings: HashMap::new(),
    }
}

fn join_query() -> lir::Query {
    let joined = Relation::Join {
        left: Box::new(Relation::Scan {
            table: "dimensions".into(),
            scope: "dimension".into(),
        }),
        right: Box::new(Relation::Scan {
            table: "facts".into(),
            scope: "fact".into(),
        }),
        kind: lir::JoinKind::Inner,
        on: Expr::Binary {
            op: BinaryOp::Eq,
            left: Box::new(Expr::Column {
                scope: "dimension".into(),
                name: "join_key".into(),
            }),
            right: Box::new(Expr::Column {
                scope: "fact".into(),
                name: "join_key".into(),
            }),
        },
    };
    let projected = Relation::Project {
        input: Box::new(joined),
        scope: Some("result".into()),
        spread: Vec::new(),
        fields: vec![
            lir::ProjectField {
                name: "dimension_id".into(),
                expression: Expr::Column {
                    scope: "dimension".into(),
                    name: "id".into(),
                },
            },
            lir::ProjectField {
                name: "fact_id".into(),
                expression: Expr::Column {
                    scope: "fact".into(),
                    name: "id".into(),
                },
            },
        ],
    };
    lir::Query {
        root: Relation::Order {
            input: Box::new(projected),
            terms: vec![lir::OrderTerm {
                expression: Expr::Column {
                    scope: "result".into(),
                    name: "fact_id".into(),
                },
                descending: false,
            }],
        },
        cardinality: RootCardinality::Many,
        bindings: HashMap::new(),
    }
}

fn complete_stats(table: &Table, rows: u64) -> PlannerStats {
    let mut statistics = PlannerStats::empty();
    statistics.synopsis_models.insert(
        table.schema_id,
        SynopsisModel {
            table: table.schema_id,
            observed_rows: rows,
            coverage: SynopsisCoverage::Complete,
            sample_size: rows,
            changes_since_collection: 0,
            table_existence_generation: table.existence_generation.get(),
            collected_at_unix_micros: 0,
            catalog_version: 1,
            columns: table
                .columns
                .iter()
                .map(|column| ColumnSynopsis {
                    column: column.schema_id,
                    value_generation: column.value_generation.get(),
                    null_fraction: 0.0,
                    null_count: 0,
                    distinct: rows.max(1),
                    distinct_is_exact: false,
                    average_width: 32,
                    maximum_width: Some(4096),
                    minimum: None,
                    maximum: None,
                    most_common_values: Vec::new(),
                    range_distribution: None,
                    degree_sequence: None,
                })
                .collect(),
            column_groups: Vec::new(),
            predicate_conditioned_degrees: Vec::new(),
        },
    );
    statistics
}
