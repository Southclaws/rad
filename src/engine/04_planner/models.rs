//! Planner-facing statistical models: evidence with provenance.
//!
//! `PlannerStats` is the model repository the estimator reads — never a
//! pre-arbitrated answer. The scheduler-side collection pipeline builds and
//! publishes these values; the planner and estimator only consume them.

use std::collections::HashMap;
use std::time::Duration;

use crate::engine::catalog::identity::SchemaId;
use crate::engine::lir::fingerprint::Fingerprint;

/// Four-row count-min sketch over fingerprint digests. Width is a power of
/// two. Estimates overcount, never undercount, which is the safe direction
/// for both eviction and cache-admission consumers.
#[derive(Clone, serde::Serialize)]
pub struct FrequencySketch {
    rows: [Vec<u32>; 4],
    mask: usize,
}

impl FrequencySketch {
    pub fn new(width: usize) -> Self {
        let width = width.next_power_of_two().max(64);
        Self {
            rows: std::array::from_fn(|_| vec![0; width]),
            mask: width - 1,
        }
    }

    fn indexes(&self, fingerprint: &Fingerprint) -> [usize; 4] {
        // The digest is already uniform hash output: four independent
        // 32-bit lanes index the four rows.
        std::array::from_fn(|row| {
            let lane: [u8; 4] = fingerprint.digest[row * 4..row * 4 + 4]
                .try_into()
                .expect("digest lane");
            u32::from_be_bytes(lane) as usize & self.mask
        })
    }

    pub fn record(&mut self, fingerprint: &Fingerprint) {
        self.record_many(fingerprint, 1);
    }

    /// Record several appearances at once, so counts observed elsewhere fold
    /// in without replaying every appearance.
    pub fn record_many(&mut self, fingerprint: &Fingerprint, count: u32) {
        for (row, index) in self.indexes(fingerprint).into_iter().enumerate() {
            self.rows[row][index] = self.rows[row][index].saturating_add(count);
        }
    }

    pub fn record_at_least(&mut self, fingerprint: &Fingerprint, count: u32) {
        for (row, index) in self.indexes(fingerprint).into_iter().enumerate() {
            self.rows[row][index] = self.rows[row][index].max(count);
        }
    }

    pub fn estimate(&self, fingerprint: &Fingerprint) -> u32 {
        self.indexes(fingerprint)
            .into_iter()
            .enumerate()
            .map(|(row, index)| self.rows[row][index])
            .min()
            .unwrap_or(0)
    }

    /// Halve every counter: a periodic aging pass so ancient traffic cannot
    /// permanently outrank the current workload.
    pub fn decay(&mut self) {
        for row in &mut self.rows {
            for counter in row {
                *counter >>= 1;
            }
        }
    }
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct FingerprintCardinalitySketch {
    registers: [u8; 32],
}

impl FingerprintCardinalitySketch {
    pub fn record(&mut self, fingerprint: &Fingerprint) {
        let hash = u64::from_be_bytes(
            fingerprint.digest[..8]
                .try_into()
                .expect("fingerprint prefix"),
        );
        let index_bits = self.registers.len().ilog2();
        let index = hash as usize & (self.registers.len() - 1);
        let remainder = hash >> index_bits;
        let rank = remainder
            .leading_zeros()
            .saturating_sub(index_bits)
            .saturating_add(1) as u8;
        self.registers[index] = self.registers[index].max(rank);
    }

    pub fn merge(&mut self, other: &Self) {
        for (register, incoming) in self.registers.iter_mut().zip(other.registers) {
            *register = (*register).max(incoming);
        }
    }

    pub fn estimate(&self) -> u64 {
        let register_count = self.registers.len() as f64;
        let harmonic_sum: f64 = self
            .registers
            .iter()
            .map(|rank| 2f64.powi(-i32::from(*rank)))
            .sum();
        let raw = 0.7213 / (1.0 + 1.079 / register_count) * register_count.powi(2) / harmonic_sum;
        let empty = self
            .registers
            .iter()
            .filter(|register| **register == 0)
            .count();
        let estimate = if raw <= 2.5 * register_count && empty > 0 {
            register_count * (register_count / empty as f64).ln()
        } else {
            raw
        };
        estimate.round() as u64
    }
}

/// Power-of-two bucketed distribution: bucket k holds values whose bit length
/// is k, coarse by design. Quantiles are bucket upper bounds, so
/// they overstate; the exact maximum is tracked alongside them and bounds are
/// clamped to it, which keeps a reported quantile from ever exceeding a value
/// that was actually observed.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(from = "HistogramWire", into = "HistogramWire")]
pub struct Log2Histogram {
    buckets: [u64; 64],
    count: u64,
    maximum: u64,
}

/// Stored form: only occupied buckets, since most of the 64 are empty. The
/// count is the sum of the buckets, so it is recovered rather than stored.
#[derive(serde::Deserialize, serde::Serialize)]
struct HistogramWire {
    buckets: Vec<(u8, u64)>,
    maximum: u64,
}

impl From<Log2Histogram> for HistogramWire {
    fn from(histogram: Log2Histogram) -> Self {
        Self {
            buckets: histogram
                .buckets
                .iter()
                .enumerate()
                .filter(|(_, count)| **count > 0)
                .map(|(bucket, count)| (bucket as u8, *count))
                .collect(),
            maximum: histogram.maximum,
        }
    }
}

impl From<HistogramWire> for Log2Histogram {
    fn from(wire: HistogramWire) -> Self {
        let mut histogram = Self::default();
        for (bucket, count) in wire.buckets {
            let index = usize::from(bucket).min(63);
            histogram.buckets[index] = histogram.buckets[index].saturating_add(count);
            histogram.count = histogram.count.saturating_add(count);
        }
        histogram.maximum = wire.maximum;
        histogram
    }
}

impl Default for Log2Histogram {
    fn default() -> Self {
        Self {
            buckets: [0; 64],
            count: 0,
            maximum: 0,
        }
    }
}

impl Log2Histogram {
    pub fn record(&mut self, value: u64) {
        self.record_many(value, 1);
    }

    pub fn record_many(&mut self, value: u64, count: u64) {
        let bucket = (u64::BITS - value.leading_zeros()) as usize;
        self.buckets[bucket.min(63)] = self.buckets[bucket.min(63)].saturating_add(count);
        self.count = self.count.saturating_add(count);
        self.maximum = self.maximum.max(value);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    /// Largest value recorded, exactly.
    pub fn maximum(&self) -> u64 {
        self.maximum
    }

    /// Fold another distribution over the same quantity into this one.
    ///
    /// Bucket counts add and the maximum is the larger, so the result is the
    /// distribution of both sets of samples. This is why the stored and
    /// relayed form is a histogram rather than the quantiles distilled from
    /// one: quantiles of two sample sets cannot be combined into quantiles of
    /// their union.
    pub fn merge(&mut self, other: &Self) {
        for (bucket, count) in self.buckets.iter_mut().zip(other.buckets) {
            *bucket = bucket.saturating_add(count);
        }
        self.count = self.count.saturating_add(other.count);
        self.maximum = self.maximum.max(other.maximum);
    }

    /// An upper bound on the q-quantile: the bound of the bucket holding it,
    /// never above the largest value actually recorded.
    pub fn quantile_upper_bound(&self, q: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let rank = ((self.count as f64) * q.clamp(0.0, 1.0)).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (bucket, &count) in self.buckets.iter().enumerate() {
            seen += count;
            if seen >= rank {
                let bound = match bucket {
                    0 => 0,
                    63 => u64::MAX,
                    _ => (1u64 << bucket) - 1,
                };
                return bound.min(self.maximum);
            }
        }
        self.maximum
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KvResourceSample {
    pub gets: u64,
    pub puts: u64,
    pub deletes: u64,
    pub scans: u64,
    pub iterated: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct KvResourceDistribution {
    gets: ResourceHistogram,
    puts: ResourceHistogram,
    deletes: ResourceHistogram,
    scans: ResourceHistogram,
    iterated: ResourceHistogram,
    bytes_read: ResourceHistogram,
    bytes_written: ResourceHistogram,
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(from = "ResourceHistogramWire", into = "ResourceHistogramWire")]
struct ResourceHistogram {
    buckets: Vec<(u8, u64)>,
    count: u64,
    maximum: u64,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct ResourceHistogramWire {
    buckets: Vec<(u8, u64)>,
    maximum: u64,
}

impl From<ResourceHistogram> for ResourceHistogramWire {
    fn from(histogram: ResourceHistogram) -> Self {
        Self {
            buckets: histogram.buckets,
            maximum: histogram.maximum,
        }
    }
}

impl From<ResourceHistogramWire> for ResourceHistogram {
    fn from(wire: ResourceHistogramWire) -> Self {
        let mut histogram = Self::default();
        for (bucket, count) in wire.buckets {
            histogram.record_bucket(bucket.min(63), count);
        }
        histogram.maximum = wire.maximum;
        histogram
    }
}

impl ResourceHistogram {
    fn record(&mut self, value: u64) {
        let bucket = (u64::BITS - value.leading_zeros()).min(63) as u8;
        self.record_bucket(bucket, 1);
        self.maximum = self.maximum.max(value);
    }

    fn record_bucket(&mut self, bucket: u8, count: u64) {
        match self
            .buckets
            .binary_search_by_key(&bucket, |(bucket, _)| *bucket)
        {
            Ok(index) => {
                self.buckets[index].1 = self.buckets[index].1.saturating_add(count);
            }
            Err(index) => self.buckets.insert(index, (bucket, count)),
        }
        self.count = self.count.saturating_add(count);
    }

    fn merge(&mut self, incoming: &Self) {
        for &(bucket, count) in &incoming.buckets {
            self.record_bucket(bucket, count);
        }
        self.maximum = self.maximum.max(incoming.maximum);
    }

    fn quantile_upper_bound(&self, q: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let rank = ((self.count as f64) * q.clamp(0.0, 1.0)).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for &(bucket, count) in &self.buckets {
            seen = seen.saturating_add(count);
            if seen >= rank {
                let bound = match bucket {
                    0 => 0,
                    63 => u64::MAX,
                    _ => (1u64 << bucket) - 1,
                };
                return bound.min(self.maximum);
            }
        }
        self.maximum
    }

    fn count(&self) -> u64 {
        self.count
    }

    fn maximum(&self) -> u64 {
        self.maximum
    }
}

impl KvResourceDistribution {
    pub fn is_empty(&self) -> bool {
        self.gets.count() == 0
    }

    pub fn record(&mut self, sample: KvResourceSample) {
        self.gets.record(sample.gets);
        self.puts.record(sample.puts);
        self.deletes.record(sample.deletes);
        self.scans.record(sample.scans);
        self.iterated.record(sample.iterated);
        self.bytes_read.record(sample.bytes_read);
        self.bytes_written.record(sample.bytes_written);
    }

    pub fn merge(&mut self, incoming: &Self) {
        self.gets.merge(&incoming.gets);
        self.puts.merge(&incoming.puts);
        self.deletes.merge(&incoming.deletes);
        self.scans.merge(&incoming.scans);
        self.iterated.merge(&incoming.iterated);
        self.bytes_read.merge(&incoming.bytes_read);
        self.bytes_written.merge(&incoming.bytes_written);
    }

    pub fn cost(&self) -> Option<KvResourceCost> {
        let observed_executions = self.gets.count();
        (observed_executions > 0).then(|| KvResourceCost {
            basis: "logical_kv_work",
            observed_executions,
            gets: ResourceMetric::of(&self.gets),
            puts: ResourceMetric::of(&self.puts),
            deletes: ResourceMetric::of(&self.deletes),
            scans: ResourceMetric::of(&self.scans),
            iterated: ResourceMetric::of(&self.iterated),
            bytes_read: ResourceMetric::of(&self.bytes_read),
            bytes_written: ResourceMetric::of(&self.bytes_written),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceMetric {
    pub p50_upper_bound: u64,
    pub p95_upper_bound: u64,
    pub maximum: u64,
}

impl ResourceMetric {
    fn of(histogram: &ResourceHistogram) -> Self {
        Self {
            p50_upper_bound: histogram.quantile_upper_bound(0.5),
            p95_upper_bound: histogram.quantile_upper_bound(0.95),
            maximum: histogram.maximum(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KvResourceCost {
    pub basis: &'static str,
    pub observed_executions: u64,
    pub gets: ResourceMetric,
    pub puts: ResourceMetric,
    pub deletes: ResourceMetric,
    pub scans: ResourceMetric,
    pub iterated: ResourceMetric,
    pub bytes_read: ResourceMetric,
    pub bytes_written: ResourceMetric,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhysicalCostMetric {
    pub p50_upper_bound: u64,
    pub p95_upper_bound: u64,
    pub maximum_upper_bound: u64,
}

impl PhysicalCostMetric {
    pub(crate) fn of(histogram: &Log2Histogram) -> Option<Self> {
        (histogram.count() > 0).then(|| Self {
            p50_upper_bound: histogram.quantile_upper_bound(0.5),
            p95_upper_bound: histogram.quantile_upper_bound(0.95),
            maximum_upper_bound: histogram.maximum(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhysicalRequestCost {
    pub class: crate::engine::kv::telemetry::PhysicalRequestClass,
    pub size_upper_bound: Option<u64>,
    pub concurrency_upper_bound: Option<u32>,
    pub service_tier: Option<crate::engine::kv::telemetry::PhysicalServiceTier>,
    pub observed_requests: u64,
    pub errors: u64,
    pub latency_micros: Option<PhysicalCostMetric>,
    pub bytes: Option<PhysicalCostMetric>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhysicalCacheCost {
    pub tier: crate::engine::kv::telemetry::PhysicalCacheTier,
    pub accesses: u64,
    pub hits: u64,
    pub hit_rate_ppm: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhysicalCostModel {
    pub basis: &'static str,
    pub backend: String,
    pub telemetry_format: u32,
    pub capabilities: crate::engine::kv::telemetry::PhysicalTelemetryCapabilities,
    pub requests: Vec<PhysicalRequestCost>,
    pub caches: Vec<PhysicalCacheCost>,
}

/// Digest of the catalog generations a statement was planned against.
///
/// The two halves have different consequences for accumulated evidence.
/// `semantic` covers table existence and column value representations: a
/// change there alters what the same relational shape means, so feedback
/// from before it is untrusted rather than merely old. `access` covers
/// index access paths: a change there can alter which plan runs without
/// altering what the relation denotes, so cardinality feedback survives it
/// while plan histories do not.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct DependencyStamp {
    pub semantic: u64,
    pub access: u64,
}

impl DependencyStamp {
    pub fn of(dependencies: &crate::engine::catalog::model::CatalogDependencies) -> Self {
        let mut semantic = Vec::new();
        for table in &dependencies.table_existence {
            framed(&mut semantic, table.table_id.as_str().as_bytes());
            semantic.extend_from_slice(&table.generation.get().to_be_bytes());
            semantic.extend_from_slice(&table.storage_generation.get().to_be_bytes());
        }
        for column in &dependencies.column_values {
            framed(&mut semantic, column.table_id.as_str().as_bytes());
            framed(&mut semantic, column.column_id.as_str().as_bytes());
            semantic.extend_from_slice(&column.generation.get().to_be_bytes());
        }
        let mut access = Vec::new();
        for index in &dependencies.index_access {
            framed(&mut access, index.table_id.as_str().as_bytes());
            framed(&mut access, index.index_id.as_str().as_bytes());
            access.extend_from_slice(&index.generation.get().to_be_bytes());
        }
        Self {
            semantic: digest_u64(b"semantic", &semantic),
            access: digest_u64(b"access", &access),
        }
    }
}

fn framed(payload: &mut Vec<u8>, value: &[u8]) {
    payload.extend_from_slice(&(value.len() as u64).to_be_bytes());
    payload.extend_from_slice(value);
}

fn digest_u64(domain: &[u8], payload: &[u8]) -> u64 {
    let basis = crate::fnv::fnv1a(domain);
    crate::fnv::fnv1a_from(basis, payload)
}

/// Symmetric multiplicative estimate error, scaled by 100 so it survives
/// integer histogram buckets: 100 is a perfect estimate, 1000 is wrong by
/// a factor of ten in either direction. Row counts are clamped to one, so
/// an empty result is not infinitely wrong.
pub fn q_error_x100(estimated: u64, actual: u64) -> u64 {
    let estimated = estimated.max(1);
    let actual = actual.max(1);
    let (larger, smaller) = if estimated >= actual {
        (estimated, actual)
    } else {
        (actual, estimated)
    };
    larger.saturating_mul(100) / smaller
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct PlanProfile {
    pub plan: Fingerprint,
    pub access_stamp: u64,
    pub executions: u64,
    pub rows: Log2Histogram,
    pub execute_micros: Log2Histogram,
    #[serde(default, skip_serializing_if = "KvResourceDistribution::is_empty")]
    pub resources: KvResourceDistribution,
}

impl PlanProfile {
    pub fn new(plan: Fingerprint, access_stamp: u64) -> Self {
        Self {
            plan,
            access_stamp,
            executions: 0,
            rows: Log2Histogram::default(),
            execute_micros: Log2Histogram::default(),
            resources: KvResourceDistribution::default(),
        }
    }

    pub fn record(&mut self, rows: u64, execute_micros: u64) {
        self.record_with_resources(rows, execute_micros, KvResourceSample::default());
    }

    pub fn record_with_resources(
        &mut self,
        rows: u64,
        execute_micros: u64,
        resources: KvResourceSample,
    ) {
        self.executions = self.executions.saturating_add(1);
        self.rows.record(rows);
        self.execute_micros.record(execute_micros);
        self.resources.record(resources);
    }

    pub fn merge(&mut self, incoming: &Self) {
        if self.plan != incoming.plan || self.access_stamp != incoming.access_stamp {
            return;
        }
        self.executions = self.executions.saturating_add(incoming.executions);
        self.rows.merge(&incoming.rows);
        self.execute_micros.merge(&incoming.execute_micros);
        self.resources.merge(&incoming.resources);
    }

    pub fn rows_p50_upper_bound(&self) -> u64 {
        self.rows.quantile_upper_bound(0.5)
    }

    pub fn execute_micros_p50_upper_bound(&self) -> u64 {
        self.execute_micros.quantile_upper_bound(0.5)
    }

    pub fn execute_micros_p95_upper_bound(&self) -> u64 {
        self.execute_micros.quantile_upper_bound(0.95)
    }
}

pub const PLANNING_VALUE_MINIMUM_EXECUTIONS: u64 = 3;
const PARTS_PER_MILLION: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanningValue {
    pub score: u64,
    pub frequency: u32,
    pub uncertainty_ppm: u64,
    pub observed_plan_variation_ppm: u64,
    pub cost_difference_micros: u64,
    pub comparable_executions: u64,
    pub row_count_class_upper_bound: u64,
    pub minimum_executions: u64,
    pub basis: &'static str,
}

/// Distilled execution feedback for one relation family.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct FeedbackModel {
    pub family: Fingerprint,
    /// Times this family was itself the measured relation or statement, as
    /// distinct from how often it appears anywhere in a query tree.
    pub retained_executions: u64,
    pub exact_variants: u64,
    /// Wall time since the Unix epoch.
    pub last_seen: Duration,
    pub rows_p50_upper_bound: u64,
    pub rows_p95_upper_bound: u64,
    pub rows_max: u64,
    pub execute_micros_p50_upper_bound: u64,
    pub execute_micros_p95_upper_bound: u64,
    pub duration_ewma_micros: f64,
    pub plans: Vec<(Fingerprint, u64)>,
    #[serde(default)]
    pub plan_profiles: Vec<PlanProfile>,
    #[serde(default, skip_serializing_if = "KvResourceDistribution::is_empty")]
    pub resources: KvResourceDistribution,
    /// The catalog generations this evidence was gathered under. Feedback
    /// is only offered for a plan whose semantic stamp matches.
    pub stamp: DependencyStamp,
    /// Executions that carried an estimate, and the resulting error
    /// distribution. Zero estimates means nothing has scored this family.
    pub executions_with_estimate: u64,
    pub q_error_p50_upper_bound_x100: u64,
    pub q_error_p95_upper_bound_x100: u64,
    pub q_error_max_x100: u64,
}

impl FeedbackModel {
    pub fn planning_value(&self, frequency: u32) -> Option<PlanningValue> {
        if self.executions_with_estimate < PLANNING_VALUE_MINIMUM_EXECUTIONS {
            return None;
        }
        let uncertainty_ppm = normalized_uncertainty_ppm(self.q_error_p95_upper_bound_x100);
        let mut groups = std::collections::BTreeMap::<u64, Vec<&PlanProfile>>::new();
        for profile in &self.plan_profiles {
            if profile.access_stamp == self.stamp.access
                && profile.executions >= PLANNING_VALUE_MINIMUM_EXECUTIONS
                && profile.execute_micros_p50_upper_bound() > 0
            {
                groups
                    .entry(profile.rows_p50_upper_bound())
                    .or_default()
                    .push(profile);
            }
        }
        groups
            .into_iter()
            .filter_map(|(row_count_class_upper_bound, profiles)| {
                if profiles
                    .iter()
                    .map(|profile| profile.plan)
                    .collect::<std::collections::HashSet<_>>()
                    .len()
                    < 2
                {
                    return None;
                }
                let comparable_executions = profiles.iter().fold(0u64, |total, profile| {
                    total.saturating_add(profile.executions)
                });
                let dominant_executions =
                    profiles.iter().map(|profile| profile.executions).max()?;
                let observed_plan_variation_ppm = scaled_fraction(
                    comparable_executions.saturating_sub(dominant_executions),
                    comparable_executions,
                );
                let fastest = profiles
                    .iter()
                    .map(|profile| profile.execute_micros_p50_upper_bound())
                    .min()?;
                let slowest = profiles
                    .iter()
                    .map(|profile| profile.execute_micros_p50_upper_bound())
                    .max()?;
                let cost_difference_micros = slowest.saturating_sub(fastest);
                Some(PlanningValue {
                    score: planning_value_score(
                        frequency,
                        uncertainty_ppm,
                        observed_plan_variation_ppm,
                        cost_difference_micros,
                    ),
                    frequency,
                    uncertainty_ppm,
                    observed_plan_variation_ppm,
                    cost_difference_micros,
                    comparable_executions,
                    row_count_class_upper_bound,
                    minimum_executions: PLANNING_VALUE_MINIMUM_EXECUTIONS,
                    basis: "observed_plan_variation_for_same_access_generation_and_row_count_class",
                })
            })
            .max_by_key(|value| {
                (
                    value.score,
                    value.comparable_executions,
                    std::cmp::Reverse(value.row_count_class_upper_bound),
                )
            })
    }
}

fn normalized_uncertainty_ppm(q_error_x100: u64) -> u64 {
    if q_error_x100 <= 100 {
        return 0;
    }
    let numerator = u128::from(q_error_x100 - 100) * u128::from(PARTS_PER_MILLION);
    u64::try_from(numerator / u128::from(q_error_x100)).unwrap_or(PARTS_PER_MILLION)
}

fn scaled_fraction(numerator: u64, denominator: u64) -> u64 {
    if denominator == 0 {
        return 0;
    }
    let scaled = u128::from(numerator) * u128::from(PARTS_PER_MILLION);
    u64::try_from(scaled / u128::from(denominator)).unwrap_or(PARTS_PER_MILLION)
}

fn planning_value_score(
    frequency: u32,
    uncertainty_ppm: u64,
    observed_plan_variation_ppm: u64,
    cost_difference_micros: u64,
) -> u64 {
    let numerator = u128::from(frequency)
        .saturating_mul(u128::from(uncertainty_ppm))
        .saturating_mul(u128::from(observed_plan_variation_ppm))
        .saturating_mul(u128::from(cost_difference_micros));
    let denominator = u128::from(PARTS_PER_MILLION).pow(2);
    u64::try_from(numerator / denominator).unwrap_or(u64::MAX)
}

#[derive(
    Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ObservationModelKind {
    Relation,
    Statement,
}

/// Synopsis of one table's population, produced by a survey.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SynopsisModel {
    pub table: SchemaId,
    /// Rows observed by the survey. This is the table row count only when
    /// `coverage` is `complete`.
    pub observed_rows: u64,
    pub coverage: SynopsisCoverage,
    pub sample_size: u64,
    #[serde(default)]
    pub changes_since_collection: u64,
    #[serde(default)]
    pub table_existence_generation: u64,
    pub collected_at_unix_micros: u64,
    pub catalog_version: u64,
    pub columns: Vec<ColumnSynopsis>,
    #[serde(default)]
    pub column_groups: Vec<ColumnGroupSynopsis>,
    #[serde(default)]
    pub predicate_conditioned_degrees: Vec<PredicateConditionedDegreeSynopsis>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SynopsisCoverage {
    Complete,
    PrefixLimit,
}

pub const COLUMN_GROUP_MAX_COLUMNS: usize = 4;

pub(crate) fn declares_column_group(
    table: &crate::engine::catalog::model::Table,
    columns: &[SchemaId],
) -> bool {
    prefix_contains_column_group(
        table,
        table.primary_key.iter().map(String::as_str).collect(),
        columns,
    ) || table
        .indexes
        .iter()
        .filter(|index| index.is_ready())
        .any(|index| prefix_contains_column_group(table, table.index_column_names(index), columns))
        || table.foreign_keys.iter().any(|foreign_key| {
            exact_column_group(
                table,
                foreign_key.columns.iter().map(String::as_str).collect(),
                columns,
            )
        })
}

fn prefix_contains_column_group(
    table: &crate::engine::catalog::model::Table,
    names: Vec<&str>,
    columns: &[SchemaId],
) -> bool {
    (2..=names.len().min(COLUMN_GROUP_MAX_COLUMNS)).any(|length| {
        let mut candidate: Vec<_> = names[..length]
            .iter()
            .filter_map(|name| table.column(name).map(|column| column.schema_id))
            .collect();
        candidate.sort_unstable();
        candidate == columns
    })
}

fn exact_column_group(
    table: &crate::engine::catalog::model::Table,
    names: Vec<&str>,
    columns: &[SchemaId],
) -> bool {
    if !(2..=COLUMN_GROUP_MAX_COLUMNS).contains(&names.len()) {
        return false;
    }
    let mut candidate: Vec<_> = names
        .iter()
        .filter_map(|name| table.column(name).map(|column| column.schema_id))
        .collect();
    candidate.sort_unstable();
    candidate == columns
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ColumnSynopsis {
    pub column: SchemaId,
    #[serde(default)]
    pub value_generation: u64,
    pub null_fraction: f64,
    #[serde(default)]
    pub null_count: u64,
    pub distinct: u64,
    pub distinct_is_exact: bool,
    pub average_width: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum_width: Option<u64>,
    /// Rendered minimum/maximum for diagnostics; absent for all-null columns.
    pub minimum: Option<String>,
    pub maximum: Option<String>,
    #[serde(default)]
    pub most_common_values: Vec<MostCommonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range_distribution: Option<RangeDistribution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degree_sequence: Option<DegreeSequenceSynopsis>,
}

pub const RANGE_DISTRIBUTION_FORMAT_VERSION: u32 = 1;
pub const DEGREE_SEQUENCE_FORMAT_VERSION: u32 = 1;
pub const PREDICATE_CONDITIONED_DEGREE_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DegreeSequenceSynopsis {
    pub format_version: u32,
    pub coverage: SynopsisCoverage,
    pub sample_size: u64,
    pub value_generations: Vec<u64>,
    pub collected_row_count: u64,
    pub non_null_rows: u64,
    pub distinct_values: u64,
    pub distinct_is_exact: bool,
    pub norms: DegreeSequenceNorms,
    pub segments: Vec<DegreeSequenceSegment>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DegreeSequenceNorms {
    pub l1: u64,
    /// This integer is the mathematical ceiling of the l_2 norm.
    pub l2_upper: u64,
    pub l_infinity: u64,
    pub exact: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DegreeSequenceSegment {
    pub rank_start: u64,
    pub rank_end: u64,
    /// This value is an entry-wise upper bound for each rank in the segment.
    pub frequency_upper: u64,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PredicateConditionedDegreeSynopsis {
    pub format_version: u32,
    pub coverage: SynopsisCoverage,
    pub sample_size: u64,
    pub join_columns: Vec<SchemaId>,
    pub join_value_generations: Vec<u64>,
    pub predicate_column: SchemaId,
    pub predicate_value_generation: u64,
    pub values: Vec<PredicateConditionedDegreeValue>,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PredicateConditionedDegreeValue {
    pub predicate_value: SynopsisValue,
    pub matching_rows: u64,
    pub non_null_join_rows: u64,
    pub distinct_join_values: u64,
    pub norms: DegreeSequenceNorms,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RangeDistribution {
    pub format_version: u32,
    pub coverage: SynopsisCoverage,
    pub sample_size: u64,
    pub value_generation: u64,
    pub collected_row_count: u64,
    pub buckets: Vec<RangeDistributionBucket>,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RangeDistributionBucket {
    pub lower: SynopsisValue,
    pub upper: SynopsisValue,
    pub rows: SynopsisCountBounds,
    pub cumulative_rows: SynopsisCountBounds,
    pub lower_endpoint_rows: SynopsisCountBounds,
    pub upper_endpoint_rows: SynopsisCountBounds,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SynopsisCountBounds {
    pub lower_bound: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upper_bound: Option<u64>,
}

impl SynopsisCountBounds {
    pub const fn exact(value: u64) -> Self {
        Self {
            lower_bound: value,
            upper_bound: Some(value),
        }
    }

    pub const fn lower(value: u64) -> Self {
        Self {
            lower_bound: value,
            upper_bound: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MostCommonValue {
    pub value: SynopsisValue,
    pub frequency: u64,
    pub maximum_error: u64,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnGroupSynopsis {
    pub columns: Vec<SchemaId>,
    #[serde(default)]
    pub value_generations: Vec<u64>,
    #[serde(default)]
    pub null_count: u64,
    pub distinct: u64,
    pub distinct_is_exact: bool,
    #[serde(default)]
    pub most_common_values: Vec<MostCommonColumnGroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degree_sequence: Option<DegreeSequenceSynopsis>,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MostCommonColumnGroup {
    pub values: Vec<SynopsisValue>,
    pub frequency: u64,
    pub maximum_error: u64,
}

impl MostCommonColumnGroup {
    pub fn lower_frequency(&self) -> u64 {
        self.frequency.saturating_sub(self.maximum_error)
    }
}

impl MostCommonValue {
    pub fn lower_frequency(&self) -> u64 {
        self.frequency.saturating_sub(self.maximum_error)
    }
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum SynopsisValue {
    Text(String),
    Int64(i64),
    Float64(f64),
    Bool(bool),
    Bytes(Vec<u8>),
}

impl SynopsisValue {
    pub fn of(value: &crate::engine::lir::Value) -> Option<Self> {
        match value {
            crate::engine::lir::Value::Text(value) => Some(Self::Text(value.clone())),
            crate::engine::lir::Value::Int64(value) => Some(Self::Int64(*value)),
            crate::engine::lir::Value::Float64(value) => Some(Self::Float64(*value)),
            crate::engine::lir::Value::Bool(value) => Some(Self::Bool(*value)),
            crate::engine::lir::Value::Bytes(value) => Some(Self::Bytes(value.as_slice().to_vec())),
            crate::engine::lir::Value::Null(_) => None,
        }
    }

    pub fn storage_eq(&self, value: &crate::engine::lir::Value) -> bool {
        match (self, value) {
            (Self::Text(left), crate::engine::lir::Value::Text(right)) => left == right,
            (Self::Int64(left), crate::engine::lir::Value::Int64(right)) => left == right,
            (Self::Float64(left), crate::engine::lir::Value::Float64(right)) => left == right,
            (Self::Bool(left), crate::engine::lir::Value::Bool(right)) => left == right,
            (Self::Bytes(left), crate::engine::lir::Value::Bytes(right)) => {
                left.as_slice() == right.as_slice()
            }
            _ => false,
        }
    }

    pub fn storage_compare(&self, value: &crate::engine::lir::Value) -> Option<std::cmp::Ordering> {
        self.to_value().compare(value).ok()
    }

    pub fn compare(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.to_value().compare(&other.to_value()).ok()
    }

    fn to_value(&self) -> crate::engine::lir::Value {
        match self {
            Self::Text(value) => crate::engine::lir::Value::Text(value.clone()),
            Self::Int64(value) => crate::engine::lir::Value::Int64(*value),
            Self::Float64(value) => crate::engine::lir::Value::Float64(*value),
            Self::Bool(value) => crate::engine::lir::Value::Bool(*value),
            Self::Bytes(value) => {
                crate::engine::lir::Value::Bytes(crate::engine::lir::BytesValue::raw(value.clone()))
            }
        }
    }
}

impl std::fmt::Display for SynopsisValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(value) => write!(formatter, "{value:?}"),
            Self::Int64(value) => write!(formatter, "{value}"),
            Self::Float64(value) => write!(formatter, "{value}"),
            Self::Bool(value) => write!(formatter, "{value}"),
            Self::Bytes(value) => write!(
                formatter,
                "b64{:?}",
                crate::identifiers::encode_base64(value)
            ),
        }
    }
}

/// The model repository the planner reads: evidence with provenance, never a
/// pre-arbitrated answer. Swapped atomically by the collection runner;
/// consumers pin an `Arc` at bind time.
#[derive(Clone)]
pub struct PlannerStats {
    pub snapshot_identity: String,
    pub scope: StatisticsScope,
    /// Actual row distributions for logical relations.
    pub feedback_models: HashMap<Fingerprint, FeedbackModel>,
    /// Outcome and plan distributions for complete statements.
    pub statement_models: HashMap<Fingerprint, FeedbackModel>,
    /// Per-table synopsis models produced by surveys.
    pub synopsis_models: HashMap<SchemaId, SynopsisModel>,
    /// Workload frequency over every observed subtree family fingerprint.
    pub workload_frequency: FrequencySketch,
    pub corpus: CorpusCaptureStats,
    pub physical_cost: Option<PhysicalCostModel>,
    pub absorbed: u64,
    pub dropped: u64,
    pub evicted: u64,
    pub shed: u64,
    /// Observations folded in from another instance, and families refused
    /// because they described a different catalog generation.
    pub relayed: u64,
    /// Corpus documents adopted from another instance.
    pub relayed_corpus: u64,
    pub relayed_stale: u64,
    /// Wall time since the Unix epoch.
    pub published_at: Duration,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StatisticsScope {
    #[default]
    Empty,
    Persisted,
    ReaderDiagnostic,
    WriterLive,
}

impl PlannerStats {
    pub fn empty() -> Self {
        Self {
            snapshot_identity: "empty".into(),
            scope: StatisticsScope::Empty,
            feedback_models: HashMap::new(),
            statement_models: HashMap::new(),
            synopsis_models: HashMap::new(),
            workload_frequency: FrequencySketch::new(64),
            corpus: CorpusCaptureStats::default(),
            physical_cost: None,
            absorbed: 0,
            dropped: 0,
            evicted: 0,
            shed: 0,
            relayed: 0,
            relayed_corpus: 0,
            relayed_stale: 0,
            published_at: Duration::ZERO,
        }
    }

    pub fn frequency(&self, fingerprint: &Fingerprint) -> u32 {
        self.workload_frequency.estimate(fingerprint)
    }
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CorpusCaptureStats {
    pub enabled: bool,
    pub captured: u64,
    pub skipped_oversize: u64,
    pub dropped_queue: u64,
    pub shed_pending: u64,
    pub maintenance: Option<CorpusMaintenanceStats>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CorpusMaintenanceStats {
    pub expired_executions: u64,
    pub pruned_executions: u64,
    pub invalid_executions: u64,
    pub pruned_programs: u64,
    pub invalid_programs: u64,
    pub erased_executions: u64,
    pub erased_programs: u64,
    pub retained_executions: u64,
    pub retained_programs: u64,
    pub retained_program_bytes: u64,
}

#[cfg(test)]
mod tests {
    use sha2::Digest as _;

    use super::*;

    fn fingerprint(seed: u64) -> Fingerprint {
        let digest = sha2::Sha256::digest(seed.to_be_bytes());
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&digest[..16]);
        Fingerprint {
            canonicalization_version: 1,
            hash_algorithm: 1,
            digest: bytes,
        }
    }

    #[test]
    fn fingerprint_cardinality_is_mergeable_and_bounded() {
        let mut left = FingerprintCardinalitySketch::default();
        let mut right = FingerprintCardinalitySketch::default();
        for seed in 0..100 {
            if seed < 50 {
                left.record(&fingerprint(seed));
            } else {
                right.record(&fingerprint(seed));
            }
        }
        left.merge(&right);
        assert!((70..=140).contains(&left.estimate()));
        assert_eq!(std::mem::size_of::<FingerprintCardinalitySketch>(), 32);
    }

    #[test]
    fn plan_profiles_merge_only_the_same_plan_and_access_stamp() {
        let plan = fingerprint(1);
        let mut profile = PlanProfile::new(plan, 7);
        profile.record(10, 40);
        let mut matching = PlanProfile::new(plan, 7);
        matching.record(10, 20);
        profile.merge(&matching);
        assert_eq!(profile.executions, 2);
        assert_eq!(profile.rows_p50_upper_bound(), 10);
        assert_eq!(profile.execute_micros_p95_upper_bound(), 40);

        let mut other_access = PlanProfile::new(plan, 8);
        other_access.record(10, 1);
        profile.merge(&other_access);
        assert_eq!(profile.executions, 2);
        assert_eq!(profile.execute_micros_p50_upper_bound(), 31);
    }

    #[test]
    fn kv_resource_distributions_merge_samples_before_distillation() {
        assert!(
            std::mem::size_of::<KvResourceDistribution>()
                < std::mem::size_of::<Log2Histogram>() * 7
        );
        let mut first = KvResourceDistribution::default();
        first.record(KvResourceSample {
            gets: 1,
            puts: 2,
            deletes: 1,
            scans: 1,
            iterated: 4,
            bytes_read: 100,
            bytes_written: 50,
        });
        first.record(KvResourceSample {
            gets: 4,
            puts: 4,
            deletes: 2,
            scans: 1,
            iterated: 8,
            bytes_read: 400,
            bytes_written: 200,
        });
        let mut second = KvResourceDistribution::default();
        second.record(KvResourceSample {
            gets: 2,
            puts: 3,
            deletes: 1,
            scans: 2,
            iterated: 6,
            bytes_read: 200,
            bytes_written: 100,
        });

        first.merge(&second);
        let cost = first.cost().expect("resource cost");
        assert_eq!(cost.observed_executions, 3);
        assert_eq!(cost.gets.p50_upper_bound, 3);
        assert_eq!(cost.gets.p95_upper_bound, 4);
        assert_eq!(cost.gets.maximum, 4);
        assert_eq!(cost.puts.maximum, 4);
        assert_eq!(cost.deletes.maximum, 2);
        assert_eq!(cost.bytes_read.maximum, 400);
        assert_eq!(cost.bytes_written.maximum, 200);
    }

    #[test]
    fn largest_unsigned_samples_keep_an_upper_bound() {
        let mut histogram = Log2Histogram::default();
        histogram.record(u64::MAX);
        assert_eq!(histogram.quantile_upper_bound(0.5), u64::MAX);

        let mut resources = KvResourceDistribution::default();
        resources.record(KvResourceSample {
            bytes_read: u64::MAX,
            ..KvResourceSample::default()
        });
        assert_eq!(
            resources
                .cost()
                .expect("resource cost")
                .bytes_read
                .p50_upper_bound,
            u64::MAX
        );
    }

    #[test]
    fn planning_value_uses_only_comparable_plan_evidence() {
        let profile = |seed, access_stamp, rows, execute_micros, executions| {
            let mut profile = PlanProfile::new(fingerprint(seed), access_stamp);
            for _ in 0..executions {
                profile.record(rows, execute_micros);
            }
            profile
        };
        let family = fingerprint(1);
        let mut model = FeedbackModel {
            family,
            retained_executions: 8,
            exact_variants: 1,
            last_seen: Duration::ZERO,
            rows_p50_upper_bound: 10,
            rows_p95_upper_bound: 10,
            rows_max: 10,
            execute_micros_p50_upper_bound: 40,
            execute_micros_p95_upper_bound: 40,
            duration_ewma_micros: 25.0,
            plans: Vec::new(),
            plan_profiles: vec![
                profile(2, 7, 10, 10, 4),
                profile(3, 7, 10, 40, 4),
                profile(4, 8, 10, 100, 100),
                profile(5, 7, 20, 100, 100),
            ],
            resources: Default::default(),
            stamp: DependencyStamp {
                semantic: 1,
                access: 7,
            },
            executions_with_estimate: 8,
            q_error_p50_upper_bound_x100: 200,
            q_error_p95_upper_bound_x100: 400,
            q_error_max_x100: 400,
        };

        let value = model.planning_value(100).expect("planning value");
        assert_eq!(value.score, 1125);
        assert_eq!(value.uncertainty_ppm, 750_000);
        assert_eq!(value.observed_plan_variation_ppm, 500_000);
        assert_eq!(value.cost_difference_micros, 30);
        assert_eq!(value.comparable_executions, 8);
        assert_eq!(value.row_count_class_upper_bound, 10);
        assert_eq!(model.planning_value(200).unwrap().score, 2250);

        model.executions_with_estimate = 2;
        assert_eq!(model.planning_value(100), None);
        model.executions_with_estimate = 8;
        model.plan_profiles.truncate(1);
        assert_eq!(model.planning_value(100), None);
    }

    #[test]
    fn stored_synopses_without_drift_fields_remain_readable() {
        let model: SynopsisModel = serde_json::from_value(serde_json::json!({
            "table": 7,
            "observed_rows": 100,
            "coverage": "complete",
            "sample_size": 100,
            "collected_at_unix_micros": 1,
            "catalog_version": 2,
            "columns": [{
                "column": 8,
                "null_fraction": 0.25,
                "distinct": 2,
                "distinct_is_exact": true,
                "average_width": 4,
                "minimum": "a",
                "maximum": "z"
            }]
        }))
        .unwrap();

        assert_eq!(model.changes_since_collection, 0);
        assert_eq!(model.table_existence_generation, 0);
        assert_eq!(model.columns[0].value_generation, 0);
        assert_eq!(model.columns[0].null_count, 0);
        assert!(model.columns[0].most_common_values.is_empty());
        assert!(model.columns[0].degree_sequence.is_none());
        assert!(model.column_groups.is_empty());
    }

    #[test]
    fn a_reported_quantile_never_exceeds_the_observed_maximum() {
        // The contradiction this guards against: bucket bounds overstate,
        // so an unclamped p50 could exceed a max that was tracked exactly.
        let mut histogram = Log2Histogram::default();
        histogram.record(666);
        assert_eq!(histogram.maximum(), 666);
        for quantile in [0.5, 0.95, 1.0] {
            assert!(
                histogram.quantile_upper_bound(quantile) <= histogram.maximum(),
                "quantile {quantile} exceeded the observed maximum"
            );
        }
        assert_eq!(histogram.quantile_upper_bound(0.5), 666);
    }

    #[test]
    fn quantile_bounds_are_monotonic_and_bracket_the_samples() {
        let mut histogram = Log2Histogram::default();
        for value in [1u64, 4, 9, 40, 90, 400, 900, 4000, 9000] {
            histogram.record(value);
        }
        let p50 = histogram.quantile_upper_bound(0.5);
        let p95 = histogram.quantile_upper_bound(0.95);
        assert!(p50 <= p95, "p50 {p50} above p95 {p95}");
        assert!(p95 <= histogram.maximum());
        assert_eq!(histogram.maximum(), 9000);
        assert_eq!(histogram.count(), 9);
    }

    #[test]
    fn an_empty_distribution_reports_zero_rather_than_a_bucket_bound() {
        let histogram = Log2Histogram::default();
        assert_eq!(histogram.count(), 0);
        assert_eq!(histogram.maximum(), 0);
        assert_eq!(histogram.quantile_upper_bound(0.95), 0);
    }

    fn histogram(values: &[u64]) -> Log2Histogram {
        let mut histogram = Log2Histogram::default();
        for value in values {
            histogram.record(*value);
        }
        histogram
    }

    /// Merging must be the distribution of both sample sets, however the
    /// samples were grouped. Everything the relay and the store do rests on
    /// this: evidence gathered separately has to combine into evidence of the
    /// whole.
    #[test]
    fn merging_distributions_is_independent_of_how_samples_were_grouped() {
        let samples: Vec<u64> = (0..40).map(|index| index * index + 1).collect();
        let whole = histogram(&samples);

        for split in [1usize, 7, 20, 39] {
            let (left, right) = samples.split_at(split);
            let mut merged = histogram(left);
            merged.merge(&histogram(right));
            assert_eq!(merged.count(), whole.count(), "split at {split}");
            assert_eq!(merged.maximum(), whole.maximum(), "split at {split}");
            for quantile in [0.0, 0.5, 0.95, 1.0] {
                assert_eq!(
                    merged.quantile_upper_bound(quantile),
                    whole.quantile_upper_bound(quantile),
                    "split at {split}, quantile {quantile}"
                );
            }
        }

        // Order cannot matter, and merging in an empty distribution changes
        // nothing.
        let mut forward = histogram(&samples[..12]);
        forward.merge(&histogram(&samples[12..]));
        let mut backward = histogram(&samples[12..]);
        backward.merge(&histogram(&samples[..12]));
        assert_eq!(forward.count(), backward.count());
        assert_eq!(forward.maximum(), backward.maximum());
        assert_eq!(
            forward.quantile_upper_bound(0.5),
            backward.quantile_upper_bound(0.5)
        );

        let mut identity = histogram(&samples);
        identity.merge(&Log2Histogram::default());
        assert_eq!(identity.count(), whole.count());
        assert_eq!(identity.maximum(), whole.maximum());

        // Associativity, so a chain of relayed deltas lands the same way
        // regardless of how the writer folds them.
        let mut left_first = histogram(&samples[..10]);
        left_first.merge(&histogram(&samples[10..20]));
        left_first.merge(&histogram(&samples[20..]));
        let mut right_first = histogram(&samples[10..20]);
        right_first.merge(&histogram(&samples[20..]));
        let mut chained = histogram(&samples[..10]);
        chained.merge(&right_first);
        assert_eq!(left_first.count(), chained.count());
        assert_eq!(left_first.maximum(), chained.maximum());
        assert_eq!(
            left_first.quantile_upper_bound(0.95),
            chained.quantile_upper_bound(0.95)
        );
    }

    #[test]
    fn q_error_is_symmetric_and_clamps_empty_results() {
        assert_eq!(q_error_x100(100, 100), 100);
        assert_eq!(q_error_x100(1000, 100), 1000);
        assert_eq!(q_error_x100(100, 1000), 1000);
        assert_eq!(q_error_x100(0, 0), 100);
        assert_eq!(q_error_x100(50, 0), 5000);
        assert_eq!(q_error_x100(0, 50), 5000);
    }

    #[test]
    fn log2_histogram_quantiles_bracket_their_samples() {
        let mut histogram = Log2Histogram::default();
        for _ in 0..99 {
            histogram.record(100);
        }
        histogram.record(1_000_000);
        assert!(histogram.quantile_upper_bound(0.5) >= 100);
        assert!(histogram.quantile_upper_bound(0.5) < 1000);
        assert!(histogram.quantile_upper_bound(1.0) >= 1_000_000);
    }
}
