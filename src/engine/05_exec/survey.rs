//! Table surveys: enumerate a table's population inside one snapshot and
//! produce the synopsis models the estimator consumes.
//!
//! A survey reads through a snapshot transaction that always rolls back. A
//! table within the row limit has complete coverage. A larger table reports
//! prefix coverage and an observed-row lower bound.
//!
//! Join-key frequency sequences follow the degree-sequence statistics in
//! Deeds et al., "SafeBound: A Practical System for Generating Cardinality
//! Bounds," SIGMOD 2023: <https://doi.org/10.1145/3588907>. Stored l_p norms
//! follow Zhang et al., "LpBound: Pessimistic Cardinality Estimation using
//! l_p-Norms of Degree Sequences," SIGMOD 2025:
//! <https://doi.org/10.1145/3725321>. Rad stores an entry-wise rank-segment
//! upper envelope for diagnostics and exact l_1, rounded-up l_2, and l_infinity
//! norms for online bounds. Predicate-conditioned norms use the literal-
//! specific MCV method from both papers. A complete small domain replaces an
//! MCV default so one literal cannot use statistics from another literal.

use std::collections::{HashMap, HashSet};

use sha2::{Digest as _, Sha256};

use crate::engine::catalog;
use crate::engine::catalog::model::Table;
use crate::engine::kv::{IsolationLevel, TransactionView};
use crate::engine::lir::Value;
use crate::engine::planner::models::{
    COLUMN_GROUP_MAX_COLUMNS, ColumnGroupSynopsis, ColumnSynopsis, DEGREE_SEQUENCE_FORMAT_VERSION,
    DegreeSequenceNorms, DegreeSequenceSegment, DegreeSequenceSynopsis, MostCommonColumnGroup,
    MostCommonValue, PREDICATE_CONDITIONED_DEGREE_FORMAT_VERSION,
    PredicateConditionedDegreeSynopsis, PredicateConditionedDegreeValue,
    RANGE_DISTRIBUTION_FORMAT_VERSION, RangeDistribution, RangeDistributionBucket,
    SynopsisCountBounds, SynopsisCoverage, SynopsisModel, SynopsisValue,
};

use super::{Engine, Result, row_store};

pub const SURVEY_ROW_CAP: u64 = 1_000_000;

pub struct SurveyRequest {
    pub changed: HashMap<catalog::identity::SchemaId, u64>,
    pub known: HashMap<catalog::identity::SchemaId, u64>,
    pub change_threshold: u64,
    pub max_age_micros: u64,
    pub now_micros: u64,
    pub row_budget: u64,
    pub byte_budget: u64,
}

pub struct SurveyResult {
    pub model: SynopsisModel,
    pub covered_changes: u64,
}

struct ColumnAccumulator {
    nulls: u64,
    distinct: DistinctAccumulator,
    most_common: MostCommonAccumulator,
    width: u64,
    maximum_width: u64,
    minimum: Option<Value>,
    maximum: Option<Value>,
    range_distribution: Option<RangeDistributionAccumulator>,
    degree_sequence: Option<DegreeSequenceAccumulator>,
}

impl ColumnAccumulator {
    fn new(collect_range_distribution: bool, collect_degree_sequence: bool) -> Self {
        Self {
            nulls: 0,
            distinct: DistinctAccumulator::new(),
            most_common: MostCommonAccumulator::new(),
            width: 0,
            maximum_width: 0,
            minimum: None,
            maximum: None,
            range_distribution: collect_range_distribution.then(RangeDistributionAccumulator::new),
            degree_sequence: collect_degree_sequence.then(DegreeSequenceAccumulator::new),
        }
    }

    fn observe(&mut self, value: &Value) {
        if value.is_null() {
            self.nulls += 1;
            return;
        }
        let (digest, width) = value_digest(value);
        if let Some(degree_sequence) = &mut self.degree_sequence {
            degree_sequence.observe(degree_value_key(value));
        }
        self.width = self.width.saturating_add(width);
        self.maximum_width = self.maximum_width.max(width);
        self.distinct.observe(u64::from_be_bytes(
            digest[..8].try_into().expect("SHA-256 prefix"),
        ));
        self.most_common.observe(value, digest);
        if self
            .range_distribution
            .as_mut()
            .is_some_and(|distribution| !distribution.observe(value))
        {
            self.range_distribution = None;
        }
        let replace_minimum = match &self.minimum {
            Some(minimum) => matches!(value.compare(minimum), Ok(std::cmp::Ordering::Less)),
            None => true,
        };
        if replace_minimum {
            self.minimum = Some(value.clone());
        }
        let replace_maximum = match &self.maximum {
            Some(maximum) => matches!(value.compare(maximum), Ok(std::cmp::Ordering::Greater)),
            None => true,
        };
        if replace_maximum {
            self.maximum = Some(value.clone());
        }
    }

    fn synopsis(
        self,
        column: &catalog::model::Column,
        rows: u64,
        coverage: SynopsisCoverage,
    ) -> ColumnSynopsis {
        let observed = rows.saturating_sub(self.nulls);
        let (distinct, distinct_is_exact) = self.distinct.result();
        ColumnSynopsis {
            column: column.schema_id,
            value_generation: column.value_generation.get(),
            null_fraction: if rows == 0 {
                0.0
            } else {
                self.nulls as f64 / rows as f64
            },
            null_count: self.nulls,
            distinct,
            distinct_is_exact,
            average_width: self.width.checked_div(observed).unwrap_or(0),
            maximum_width: Some(self.maximum_width),
            minimum: self.minimum.map(|value| value.to_string()),
            maximum: self.maximum.map(|value| value.to_string()),
            most_common_values: self.most_common.result(),
            range_distribution: self
                .range_distribution
                .map(|distribution| distribution.result(column, rows, coverage)),
            degree_sequence: self.degree_sequence.map(|sequence| {
                sequence.result(vec![column.value_generation.get()], rows, coverage)
            }),
        }
    }
}

const RANGE_DISTRIBUTION_BUCKET_CAPACITY: usize = 64;
const RANGE_DISTRIBUTION_MAX_TEXT_BYTES: usize = 256;

struct RangeDistributionAccumulator {
    buckets: Vec<RangeBucketAccumulator>,
}

struct RangeBucketAccumulator {
    lower: Value,
    upper: Value,
    rows: u64,
    lower_endpoint_rows: u64,
    upper_endpoint_rows: u64,
}

impl RangeDistributionAccumulator {
    fn new() -> Self {
        Self {
            buckets: Vec::with_capacity(RANGE_DISTRIBUTION_BUCKET_CAPACITY * 2),
        }
    }

    fn observe(&mut self, value: &Value) -> bool {
        if matches!(value, Value::Text(text) if text.len() > RANGE_DISTRIBUTION_MAX_TEXT_BYTES) {
            return false;
        }
        let mut start = 0;
        let mut end = self.buckets.len();
        while start < end {
            let middle = start + (end - start) / 2;
            let Some(ordering) = self.buckets[middle].upper.compare(value).ok() else {
                return false;
            };
            if ordering.is_lt() {
                start = middle + 1;
            } else {
                end = middle;
            }
        }
        let position = start;
        if let Some(bucket) = self.buckets.get_mut(position) {
            let Some(lower_ordering) = bucket.lower.compare(value).ok() else {
                return false;
            };
            if !lower_ordering.is_gt() {
                bucket.rows = bucket.rows.saturating_add(1);
                if lower_ordering.is_eq() {
                    bucket.lower_endpoint_rows = bucket.lower_endpoint_rows.saturating_add(1);
                }
                if bucket.upper.compare(value).is_ok_and(|order| order.is_eq()) {
                    bucket.upper_endpoint_rows = bucket.upper_endpoint_rows.saturating_add(1);
                }
                return true;
            }
        }
        self.buckets.insert(
            position,
            RangeBucketAccumulator {
                lower: value.clone(),
                upper: value.clone(),
                rows: 1,
                lower_endpoint_rows: 1,
                upper_endpoint_rows: 1,
            },
        );
        if self.buckets.len() >= RANGE_DISTRIBUTION_BUCKET_CAPACITY * 2 {
            self.compact();
        }
        true
    }

    fn compact(&mut self) {
        while self.buckets.len() > RANGE_DISTRIBUTION_BUCKET_CAPACITY {
            let merge_at = (0..self.buckets.len() - 1)
                .min_by_key(|index| {
                    (
                        self.buckets[*index]
                            .rows
                            .saturating_add(self.buckets[*index + 1].rows),
                        *index,
                    )
                })
                .expect("range distribution exceeds one bucket");
            let right = self.buckets.remove(merge_at + 1);
            let left = &mut self.buckets[merge_at];
            left.upper = right.upper;
            left.rows = left.rows.saturating_add(right.rows);
            left.upper_endpoint_rows = right.upper_endpoint_rows;
        }
    }

    fn result(
        mut self,
        column: &catalog::model::Column,
        rows: u64,
        coverage: SynopsisCoverage,
    ) -> RangeDistribution {
        if self.buckets.len() > RANGE_DISTRIBUTION_BUCKET_CAPACITY {
            self.compact();
        }
        let bounds = |value| match coverage {
            SynopsisCoverage::Complete => SynopsisCountBounds::exact(value),
            SynopsisCoverage::PrefixLimit => SynopsisCountBounds::lower(value),
        };
        let mut cumulative_rows = 0u64;
        RangeDistribution {
            format_version: RANGE_DISTRIBUTION_FORMAT_VERSION,
            coverage,
            sample_size: rows,
            value_generation: column.value_generation.get(),
            collected_row_count: rows,
            buckets: self
                .buckets
                .into_iter()
                .map(|bucket| {
                    cumulative_rows = cumulative_rows.saturating_add(bucket.rows);
                    RangeDistributionBucket {
                        lower: SynopsisValue::of(&bucket.lower)
                            .expect("range endpoint is not null"),
                        upper: SynopsisValue::of(&bucket.upper)
                            .expect("range endpoint is not null"),
                        rows: bounds(bucket.rows),
                        cumulative_rows: bounds(cumulative_rows),
                        lower_endpoint_rows: bounds(bucket.lower_endpoint_rows),
                        upper_endpoint_rows: bounds(bucket.upper_endpoint_rows),
                    }
                })
                .collect(),
        }
    }
}

fn value_digest(value: &Value) -> ([u8; 32], u64) {
    let mut hasher = Sha256::new();
    let width = match value {
        Value::Text(text) => {
            hasher.update([0]);
            hasher.update(text.as_bytes());
            text.len() as u64
        }
        Value::Int64(value) => {
            hasher.update([1]);
            hasher.update(value.to_be_bytes());
            8
        }
        Value::Float64(value) => {
            hasher.update([2]);
            let bits = if *value == 0.0 { 0 } else { value.to_bits() };
            hasher.update(bits.to_be_bytes());
            8
        }
        Value::Bool(value) => {
            hasher.update([3, u8::from(*value)]);
            1
        }
        Value::Null(_) => unreachable!("nulls counted before hashing"),
    };
    (hasher.finalize().into(), width)
}

const DEGREE_SEQUENCE_DISTINCT_CAPACITY: usize = 4_096;
const DEGREE_SEQUENCE_SEGMENT_CAPACITY: usize = 64;
const DEGREE_SEQUENCE_MAX_TEXT_BYTES: usize = 256;

struct DegreeSequenceAccumulator {
    non_null_rows: u64,
    frequencies: Option<HashMap<Vec<u8>, u64>>,
}

impl DegreeSequenceAccumulator {
    fn new() -> Self {
        Self {
            non_null_rows: 0,
            frequencies: Some(HashMap::new()),
        }
    }

    fn observe(&mut self, key: Option<Vec<u8>>) {
        self.non_null_rows = self.non_null_rows.saturating_add(1);
        let Some(frequencies) = &mut self.frequencies else {
            return;
        };
        let Some(key) = key else {
            self.frequencies = None;
            return;
        };
        if let Some(frequency) = frequencies.get_mut(&key) {
            *frequency = frequency.saturating_add(1);
            return;
        }
        if frequencies.len() >= DEGREE_SEQUENCE_DISTINCT_CAPACITY {
            self.frequencies = None;
            return;
        }
        frequencies.insert(key, 1);
    }

    fn result(
        self,
        value_generations: Vec<u64>,
        rows: u64,
        coverage: SynopsisCoverage,
    ) -> DegreeSequenceSynopsis {
        let exact = coverage == SynopsisCoverage::Complete && self.frequencies.is_some();
        let mut frequencies = self
            .frequencies
            .map(|frequencies| frequencies.into_values().collect::<Vec<_>>())
            .unwrap_or_default();
        frequencies.sort_unstable_by(|left, right| right.cmp(left));
        let distinct_values = frequencies.len() as u64;
        let norms = if exact {
            exact_degree_norms(&frequencies)
        } else {
            DegreeSequenceNorms {
                l1: self.non_null_rows,
                l2_upper: self.non_null_rows,
                l_infinity: self.non_null_rows,
                exact: false,
            }
        };
        let segments = if exact {
            compress_degree_sequence(&frequencies)
        } else if self.non_null_rows == 0 {
            Vec::new()
        } else {
            vec![DegreeSequenceSegment {
                rank_start: 0,
                rank_end: 1,
                frequency_upper: self.non_null_rows,
            }]
        };
        DegreeSequenceSynopsis {
            format_version: DEGREE_SEQUENCE_FORMAT_VERSION,
            coverage,
            sample_size: rows,
            value_generations,
            collected_row_count: rows,
            non_null_rows: self.non_null_rows,
            distinct_values,
            distinct_is_exact: exact,
            norms,
            segments,
        }
    }
}

fn exact_degree_norms(frequencies: &[u64]) -> DegreeSequenceNorms {
    let l1 = frequencies.iter().copied().fold(0u64, u64::saturating_add);
    let squares = frequencies.iter().fold(0u128, |sum, frequency| {
        sum.saturating_add(u128::from(*frequency).pow(2))
    });
    DegreeSequenceNorms {
        l1,
        l2_upper: ceil_sqrt(squares),
        l_infinity: frequencies.first().copied().unwrap_or(0),
        exact: true,
    }
}

fn ceil_sqrt(value: u128) -> u64 {
    if value == 0 {
        return 0;
    }
    let mut lower = 1u128;
    let mut upper = value.min(u128::from(u64::MAX));
    while lower < upper {
        let middle = lower + (upper - lower) / 2;
        if middle.saturating_mul(middle) >= value {
            upper = middle;
        } else {
            lower = middle + 1;
        }
    }
    lower as u64
}

fn compress_degree_sequence(frequencies: &[u64]) -> Vec<DegreeSequenceSegment> {
    if frequencies.is_empty() {
        return Vec::new();
    }
    let width = frequencies.len().div_ceil(DEGREE_SEQUENCE_SEGMENT_CAPACITY);
    frequencies
        .chunks(width)
        .enumerate()
        .map(|(index, frequencies)| {
            let rank_start = index.saturating_mul(width) as u64;
            DegreeSequenceSegment {
                rank_start,
                rank_end: rank_start.saturating_add(frequencies.len() as u64),
                frequency_upper: frequencies[0],
            }
        })
        .collect()
}

const PREDICATE_CONDITION_VALUE_CAPACITY: usize = 32;
const PREDICATE_CONDITION_PAIR_CAPACITY: usize = 4_096;
const PREDICATE_CONDITION_SYNOPSIS_CAPACITY: usize = 64;

struct PredicateConditionedDegreeAccumulator {
    join_column_indexes: Vec<usize>,
    predicate_column_index: usize,
    values: Option<HashMap<Vec<u8>, PredicateConditionedValueAccumulator>>,
    distinct_pairs: usize,
}

struct PredicateConditionedValueAccumulator {
    predicate_value: SynopsisValue,
    matching_rows: u64,
    non_null_join_rows: u64,
    join_frequencies: HashMap<Vec<u8>, u64>,
}

impl PredicateConditionedDegreeAccumulator {
    fn new(join_column_indexes: Vec<usize>, predicate_column_index: usize) -> Self {
        Self {
            join_column_indexes,
            predicate_column_index,
            values: Some(HashMap::new()),
            distinct_pairs: 0,
        }
    }

    fn observe(&mut self, row: &[Value]) {
        let Some(values) = &mut self.values else {
            return;
        };
        let predicate = &row[self.predicate_column_index];
        if predicate.is_null() {
            return;
        }
        let Some(predicate_key) = degree_value_key(predicate) else {
            self.values = None;
            return;
        };
        if !values.contains_key(&predicate_key)
            && values.len() >= PREDICATE_CONDITION_VALUE_CAPACITY
        {
            self.values = None;
            return;
        }
        let value =
            values
                .entry(predicate_key)
                .or_insert_with(|| PredicateConditionedValueAccumulator {
                    predicate_value: SynopsisValue::of(predicate)
                        .expect("predicate-conditioned values exclude null"),
                    matching_rows: 0,
                    non_null_join_rows: 0,
                    join_frequencies: HashMap::new(),
                });
        value.matching_rows = value.matching_rows.saturating_add(1);
        let join_values = self
            .join_column_indexes
            .iter()
            .map(|index| row[*index].clone())
            .collect::<Vec<_>>();
        if join_values.iter().any(Value::is_null) {
            return;
        }
        let Some(join_key) = degree_group_key(&join_values) else {
            self.values = None;
            return;
        };
        value.non_null_join_rows = value.non_null_join_rows.saturating_add(1);
        if let Some(frequency) = value.join_frequencies.get_mut(&join_key) {
            *frequency = frequency.saturating_add(1);
            return;
        }
        if self.distinct_pairs >= PREDICATE_CONDITION_PAIR_CAPACITY {
            self.values = None;
            return;
        }
        self.distinct_pairs += 1;
        value.join_frequencies.insert(join_key, 1);
    }

    fn synopsis(
        self,
        table: &Table,
        rows: u64,
        coverage: SynopsisCoverage,
    ) -> Option<PredicateConditionedDegreeSynopsis> {
        if coverage != SynopsisCoverage::Complete {
            return None;
        }
        let values = self.values?;
        let mut values = values
            .into_values()
            .map(|value| {
                let mut frequencies = value.join_frequencies.into_values().collect::<Vec<_>>();
                frequencies.sort_unstable_by(|left, right| right.cmp(left));
                PredicateConditionedDegreeValue {
                    predicate_value: value.predicate_value,
                    matching_rows: value.matching_rows,
                    non_null_join_rows: value.non_null_join_rows,
                    distinct_join_values: frequencies.len() as u64,
                    norms: exact_degree_norms(&frequencies),
                }
            })
            .collect::<Vec<_>>();
        values.sort_by_cached_key(|value| {
            serde_json::to_vec(&value.predicate_value).expect("synopsis value serializes")
        });
        Some(PredicateConditionedDegreeSynopsis {
            format_version: PREDICATE_CONDITIONED_DEGREE_FORMAT_VERSION,
            coverage,
            sample_size: rows,
            join_columns: self
                .join_column_indexes
                .iter()
                .map(|index| table.columns[*index].schema_id)
                .collect(),
            join_value_generations: self
                .join_column_indexes
                .iter()
                .map(|index| table.columns[*index].value_generation.get())
                .collect(),
            predicate_column: table.columns[self.predicate_column_index].schema_id,
            predicate_value_generation: table.columns[self.predicate_column_index]
                .value_generation
                .get(),
            values,
        })
    }
}

fn degree_value_key(value: &Value) -> Option<Vec<u8>> {
    let mut key = Vec::new();
    match value {
        Value::Text(value) if value.len() <= DEGREE_SEQUENCE_MAX_TEXT_BYTES => {
            key.push(0);
            key.extend_from_slice(&(value.len() as u64).to_be_bytes());
            key.extend_from_slice(value.as_bytes());
        }
        Value::Text(_) => return None,
        Value::Int64(value) => {
            key.push(1);
            key.extend_from_slice(&value.to_be_bytes());
        }
        Value::Float64(value) => {
            key.push(2);
            let bits = if *value == 0.0 { 0 } else { value.to_bits() };
            key.extend_from_slice(&bits.to_be_bytes());
        }
        Value::Bool(value) => key.extend_from_slice(&[3, u8::from(*value)]),
        Value::Null(_) => return None,
    }
    Some(key)
}

fn degree_group_key(values: &[Value]) -> Option<Vec<u8>> {
    let mut key = Vec::new();
    for value in values {
        let value = degree_value_key(value)?;
        key.extend_from_slice(&(value.len() as u64).to_be_bytes());
        key.extend_from_slice(&value);
    }
    Some(key)
}

const MCV_CAPACITY: usize = 32;
const MCV_MAX_TEXT_BYTES: usize = 256;
const COLUMN_GROUP_CAPACITY: usize = 32;

struct MostCommonAccumulator {
    entries: Vec<MostCommonEntry>,
}

struct MostCommonEntry {
    digest: [u8; 32],
    value: SynopsisValue,
    frequency: u64,
    maximum_error: u64,
}

impl MostCommonAccumulator {
    fn new() -> Self {
        Self {
            entries: Vec::with_capacity(MCV_CAPACITY),
        }
    }

    fn observe(&mut self, value: &Value, digest: [u8; 32]) {
        if matches!(value, Value::Text(text) if text.len() > MCV_MAX_TEXT_BYTES) {
            return;
        }
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.digest == digest && entry.value.storage_eq(value))
        {
            entry.frequency = entry.frequency.saturating_add(1);
            return;
        }
        let value = SynopsisValue::of(value).expect("nulls do not enter MCV collection");
        if self.entries.len() < MCV_CAPACITY {
            self.entries.push(MostCommonEntry {
                digest,
                value,
                frequency: 1,
                maximum_error: 0,
            });
            return;
        }
        let victim = self
            .entries
            .iter()
            .enumerate()
            .min_by_key(|(_, entry)| (entry.frequency, entry.digest))
            .map(|(index, _)| index)
            .expect("MCV capacity is positive");
        let frequency = self.entries[victim].frequency;
        self.entries[victim] = MostCommonEntry {
            digest,
            value,
            frequency: frequency.saturating_add(1),
            maximum_error: frequency,
        };
    }

    fn result(mut self) -> Vec<MostCommonValue> {
        self.entries
            .sort_by_key(|entry| (std::cmp::Reverse(entry.frequency), entry.digest));
        self.entries
            .into_iter()
            .map(|entry| MostCommonValue {
                value: entry.value,
                frequency: entry.frequency,
                maximum_error: entry.maximum_error,
            })
            .collect()
    }
}

struct ColumnGroupAccumulator {
    column_indexes: Vec<usize>,
    nulls: u64,
    distinct: DistinctAccumulator,
    most_common: MostCommonColumnGroupAccumulator,
    degree_sequence: DegreeSequenceAccumulator,
}

impl ColumnGroupAccumulator {
    fn new(column_indexes: Vec<usize>) -> Self {
        Self {
            column_indexes,
            nulls: 0,
            distinct: DistinctAccumulator::new(),
            most_common: MostCommonColumnGroupAccumulator::new(),
            degree_sequence: DegreeSequenceAccumulator::new(),
        }
    }

    fn observe(&mut self, values: &[Value]) {
        if values.iter().any(Value::is_null) {
            self.nulls = self.nulls.saturating_add(1);
            return;
        }
        let mut hasher = Sha256::new();
        for value in values {
            hasher.update(value_digest(value).0);
        }
        let digest: [u8; 32] = hasher.finalize().into();
        self.distinct.observe(u64::from_be_bytes(
            digest[..8].try_into().expect("SHA-256 prefix"),
        ));
        self.most_common.observe(values, digest);
        self.degree_sequence.observe(degree_group_key(values));
    }

    fn synopsis(self, table: &Table, rows: u64, coverage: SynopsisCoverage) -> ColumnGroupSynopsis {
        let (distinct, distinct_is_exact) = self.distinct.result();
        let value_generations = self
            .column_indexes
            .iter()
            .map(|index| table.columns[*index].value_generation.get())
            .collect();
        ColumnGroupSynopsis {
            columns: self
                .column_indexes
                .iter()
                .map(|index| table.columns[*index].schema_id)
                .collect(),
            value_generations: self
                .column_indexes
                .iter()
                .map(|index| table.columns[*index].value_generation.get())
                .collect(),
            null_count: self.nulls,
            distinct,
            distinct_is_exact,
            most_common_values: self.most_common.result(),
            degree_sequence: Some(
                self.degree_sequence
                    .result(value_generations, rows, coverage),
            ),
        }
    }
}

struct MostCommonColumnGroupAccumulator {
    entries: Vec<MostCommonColumnGroupEntry>,
}

struct MostCommonColumnGroupEntry {
    digest: [u8; 32],
    values: Vec<SynopsisValue>,
    frequency: u64,
    maximum_error: u64,
}

impl MostCommonColumnGroupAccumulator {
    fn new() -> Self {
        Self {
            entries: Vec::with_capacity(MCV_CAPACITY),
        }
    }

    fn observe(&mut self, values: &[Value], digest: [u8; 32]) {
        let text_bytes = values
            .iter()
            .filter_map(|value| match value {
                Value::Text(text) => Some(text.len()),
                _ => None,
            })
            .fold(0usize, usize::saturating_add);
        if text_bytes > MCV_MAX_TEXT_BYTES {
            return;
        }
        if let Some(entry) = self.entries.iter_mut().find(|entry| {
            entry.digest == digest
                && entry.values.len() == values.len()
                && entry
                    .values
                    .iter()
                    .zip(values)
                    .all(|(left, right)| left.storage_eq(right))
        }) {
            entry.frequency = entry.frequency.saturating_add(1);
            return;
        }
        let values = values
            .iter()
            .map(|value| SynopsisValue::of(value).expect("nulls do not enter group collection"))
            .collect();
        if self.entries.len() < MCV_CAPACITY {
            self.entries.push(MostCommonColumnGroupEntry {
                digest,
                values,
                frequency: 1,
                maximum_error: 0,
            });
            return;
        }
        let victim = self
            .entries
            .iter()
            .enumerate()
            .min_by_key(|(_, entry)| (entry.frequency, entry.digest))
            .map(|(index, _)| index)
            .expect("MCV capacity is positive");
        let frequency = self.entries[victim].frequency;
        self.entries[victim] = MostCommonColumnGroupEntry {
            digest,
            values,
            frequency: frequency.saturating_add(1),
            maximum_error: frequency,
        };
    }

    fn result(mut self) -> Vec<MostCommonColumnGroup> {
        self.entries
            .sort_by_key(|entry| (std::cmp::Reverse(entry.frequency), entry.digest));
        self.entries
            .into_iter()
            .map(|entry| MostCommonColumnGroup {
                values: entry.values,
                frequency: entry.frequency,
                maximum_error: entry.maximum_error,
            })
            .collect()
    }
}

fn column_group_accumulators(table: &Table) -> Vec<ColumnGroupAccumulator> {
    column_group_indexes(table)
        .into_iter()
        .map(ColumnGroupAccumulator::new)
        .collect()
}

fn column_group_indexes(table: &Table) -> Vec<Vec<usize>> {
    let mut groups: HashMap<Vec<usize>, u8> = HashMap::new();
    let mut add_prefixes = |names: Vec<&str>, priority: u8| {
        let Some(mut column_indexes) = names
            .into_iter()
            .map(|name| table.columns.iter().position(|column| column.name == name))
            .collect::<Option<Vec<_>>>()
        else {
            return;
        };
        column_indexes.truncate(COLUMN_GROUP_MAX_COLUMNS);
        for length in 2..=column_indexes.len() {
            let mut group = column_indexes[..length].to_vec();
            group.sort_by_key(|position| table.columns[*position].schema_id);
            groups
                .entry(group)
                .and_modify(|current| *current = (*current).min(priority))
                .or_insert(priority);
        }
    };
    add_prefixes(table.primary_key.iter().map(String::as_str).collect(), 0);
    for index in table.indexes.iter().filter(|index| index.is_ready()) {
        add_prefixes(table.index_column_names(index), 2);
    }
    for foreign_key in &table.foreign_keys {
        let Some(mut group) = foreign_key
            .columns
            .iter()
            .map(|name| table.columns.iter().position(|column| column.name == *name))
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        if !(2..=COLUMN_GROUP_MAX_COLUMNS).contains(&group.len()) {
            continue;
        }
        group.sort_by_key(|position| table.columns[*position].schema_id);
        groups
            .entry(group)
            .and_modify(|current| *current = (*current).min(1))
            .or_insert(1);
    }
    let mut groups: Vec<_> = groups.into_iter().collect();
    groups.sort_by_key(|(group, priority)| {
        (
            *priority,
            group
                .iter()
                .map(|position| table.columns[*position].schema_id)
                .collect::<Vec<_>>(),
        )
    });
    groups.truncate(COLUMN_GROUP_CAPACITY);
    groups.into_iter().map(|(group, _)| group).collect()
}

const EXACT_DISTINCT_LIMIT: usize = 4096;
const HLL_REGISTER_COUNT: usize = 256;

struct DistinctAccumulator {
    exact: Option<HashSet<u64>>,
    registers: [u8; HLL_REGISTER_COUNT],
}

impl DistinctAccumulator {
    fn new() -> Self {
        Self {
            exact: Some(HashSet::new()),
            registers: [0; HLL_REGISTER_COUNT],
        }
    }

    fn observe(&mut self, hash: u64) {
        if let Some(exact) = &mut self.exact {
            exact.insert(hash);
            if exact.len() > EXACT_DISTINCT_LIMIT {
                self.exact = None;
            }
        }

        let index = hash as usize & (HLL_REGISTER_COUNT - 1);
        let remainder = hash >> HLL_REGISTER_COUNT.ilog2();
        let rank = remainder
            .leading_zeros()
            .saturating_sub(HLL_REGISTER_COUNT.ilog2())
            .saturating_add(1) as u8;
        self.registers[index] = self.registers[index].max(rank);
    }

    fn result(self) -> (u64, bool) {
        if let Some(exact) = self.exact {
            return (exact.len() as u64, true);
        }

        let register_count = HLL_REGISTER_COUNT as f64;
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
        (estimate.round() as u64, false)
    }
}

impl Engine {
    /// Survey every live table under one snapshot. Read-only: the
    /// transaction always rolls back.
    pub async fn survey(&self) -> Result<Vec<SynopsisModel>> {
        let transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let mut view = TransactionView(&*transaction);
        let revision = catalog::store::current_revision(&mut view).await?;
        let tables = catalog::store::list_tables(&mut view).await?;
        let mut models = Vec::with_capacity(tables.len());
        let collected_at = self.now_unix_micros();
        for table in &tables {
            models.push(survey_table(&view, table, revision.version.get(), collected_at).await?);
        }
        transaction.rollback();
        Ok(models)
    }

    pub async fn survey_next(&self, request: SurveyRequest) -> Result<Option<SurveyResult>> {
        let transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let result = async {
            let mut view = TransactionView(&*transaction);
            let revision = catalog::store::current_revision(&mut view).await?;
            let tables = catalog::store::list_tables(&mut view).await?;
            let Some(table) = select_table(&tables, &request) else {
                return Ok(None);
            };
            let covered_changes = request.changed.get(&table.schema_id).copied().unwrap_or(0);
            let model = survey_table_with_budget(
                &view,
                table,
                revision.version.get(),
                request.now_micros,
                request.row_budget,
                request.byte_budget,
            )
            .await?;
            Ok(Some(SurveyResult {
                model,
                covered_changes,
            }))
        }
        .await;
        transaction.rollback();
        result
    }
}

fn select_table<'a>(tables: &'a [Table], request: &SurveyRequest) -> Option<&'a Table> {
    let mut ordered: Vec<&Table> = tables.iter().collect();
    ordered.sort_by_key(|table| table.schema_id);

    ordered
        .iter()
        .copied()
        .find(|table| !request.known.contains_key(&table.schema_id))
        .or_else(|| {
            ordered
                .iter()
                .copied()
                .filter_map(|table| {
                    let changes = request.changed.get(&table.schema_id).copied().unwrap_or(0);
                    (changes >= request.change_threshold).then_some((changes, table))
                })
                .max_by_key(|(changes, table)| (*changes, std::cmp::Reverse(table.schema_id)))
                .map(|(_, table)| table)
        })
        .or_else(|| {
            let cutoff = request.now_micros.saturating_sub(request.max_age_micros);
            ordered
                .into_iter()
                .filter_map(|table| {
                    request
                        .known
                        .get(&table.schema_id)
                        .copied()
                        .filter(|collected_at| *collected_at <= cutoff)
                        .map(|collected_at| (collected_at, table))
                })
                .min_by_key(|(collected_at, table)| (*collected_at, table.schema_id))
                .map(|(_, table)| table)
        })
}

pub(super) async fn survey_table(
    view: &TransactionView<'_>,
    table: &Table,
    catalog_version: u64,
    collected_at_unix_micros: u64,
) -> Result<SynopsisModel> {
    survey_table_with_limit(
        view,
        table,
        catalog_version,
        collected_at_unix_micros,
        SURVEY_ROW_CAP,
    )
    .await
}

async fn survey_table_with_limit(
    view: &TransactionView<'_>,
    table: &Table,
    catalog_version: u64,
    collected_at_unix_micros: u64,
    row_limit: u64,
) -> Result<SynopsisModel> {
    survey_table_with_budget(
        view,
        table,
        catalog_version,
        collected_at_unix_micros,
        row_limit,
        u64::MAX,
    )
    .await
}

async fn survey_table_with_budget(
    view: &TransactionView<'_>,
    table: &Table,
    catalog_version: u64,
    collected_at_unix_micros: u64,
    row_limit: u64,
    byte_limit: u64,
) -> Result<SynopsisModel> {
    let mut iterator = row_store::scan_table(view, table, &table.columns).await?;
    let range_columns: HashSet<_> = table
        .indexes
        .iter()
        .filter(|index| index.is_ready())
        .flat_map(|index| table.index_column_names(index))
        .collect();
    let degree_columns = degree_sequence_columns(table);
    let mut accumulators: Vec<ColumnAccumulator> = table
        .columns
        .iter()
        .map(|column| {
            ColumnAccumulator::new(
                range_columns.contains(column.name.as_str()),
                degree_columns.contains(column.name.as_str()),
            )
        })
        .collect();
    let mut group_accumulators = column_group_accumulators(table);
    let mut predicate_conditioned_accumulators =
        predicate_conditioned_degree_accumulators(table, &degree_columns);
    let mut rows = 0u64;
    let mut bytes = 0u64;
    let mut exact = true;
    while let Some(row) = iterator.next().await? {
        if rows >= row_limit {
            exact = false;
            break;
        }
        let row_values = table
            .columns
            .iter()
            .map(|column| {
                row.get(&column.name)
                    .cloned()
                    .unwrap_or(Value::Null(column.scalar_type))
            })
            .collect::<Vec<_>>();
        let row_bytes = row_values
            .iter()
            .map(value_width)
            .fold(0u64, u64::saturating_add);
        if rows > 0 && bytes.saturating_add(row_bytes) > byte_limit {
            exact = false;
            break;
        }
        rows += 1;
        bytes = bytes.saturating_add(row_bytes);
        for (value, accumulator) in row_values.iter().zip(&mut accumulators) {
            accumulator.observe(value);
        }
        for accumulator in &mut group_accumulators {
            let values = accumulator
                .column_indexes
                .iter()
                .map(|index| row_values[*index].clone())
                .collect::<Vec<_>>();
            accumulator.observe(&values);
        }
        for accumulator in &mut predicate_conditioned_accumulators {
            accumulator.observe(&row_values);
        }
    }
    let coverage = if exact {
        SynopsisCoverage::Complete
    } else {
        SynopsisCoverage::PrefixLimit
    };
    Ok(SynopsisModel {
        table: table.schema_id,
        observed_rows: rows,
        coverage,
        sample_size: rows,
        changes_since_collection: 0,
        table_existence_generation: table.existence_generation.get(),
        collected_at_unix_micros,
        catalog_version,
        columns: table
            .columns
            .iter()
            .zip(accumulators)
            .map(|(column, accumulator)| accumulator.synopsis(column, rows, coverage))
            .collect(),
        column_groups: group_accumulators
            .into_iter()
            .map(|accumulator| accumulator.synopsis(table, rows, coverage))
            .collect(),
        predicate_conditioned_degrees: predicate_conditioned_accumulators
            .into_iter()
            .filter_map(|accumulator| accumulator.synopsis(table, rows, coverage))
            .collect(),
    })
}

fn degree_sequence_columns(table: &Table) -> HashSet<&str> {
    table
        .primary_key
        .iter()
        .map(String::as_str)
        .chain(
            table
                .indexes
                .iter()
                .filter(|index| index.is_ready())
                .flat_map(|index| table.index_column_names(index)),
        )
        .chain(
            table
                .foreign_keys
                .iter()
                .flat_map(|foreign_key| foreign_key.columns.iter().map(String::as_str)),
        )
        .collect()
}

fn predicate_conditioned_degree_accumulators(
    table: &Table,
    degree_columns: &HashSet<&str>,
) -> Vec<PredicateConditionedDegreeAccumulator> {
    let mut join_groups = table
        .columns
        .iter()
        .enumerate()
        .filter(|(_, column)| degree_columns.contains(column.name.as_str()))
        .map(|(index, _)| vec![index])
        .collect::<Vec<_>>();
    join_groups.extend(column_group_indexes(table));
    join_groups
        .into_iter()
        .flat_map(|join_column_indexes| {
            (0..table.columns.len()).map(move |predicate_column_index| {
                PredicateConditionedDegreeAccumulator::new(
                    join_column_indexes.clone(),
                    predicate_column_index,
                )
            })
        })
        .take(PREDICATE_CONDITION_SYNOPSIS_CAPACITY)
        .collect()
}

fn value_width(value: &Value) -> u64 {
    match value {
        Value::Text(value) => value.len() as u64,
        Value::Int64(_) | Value::Float64(_) => 8,
        Value::Bool(_) => 1,
        Value::Null(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use crate::engine::catalog::identity::SchemaId;
    use crate::engine::catalog::model::{ColumnDraft, IndexDef, ScalarType, TableDraft};
    use crate::engine::exec::{CatalogPolicy, Program, Statement};
    use crate::engine::kv::slatedb::Store;
    use crate::engine::lir::{Kind, RawScalar, Relation, RootCardinality, RowsColumn};

    use super::*;

    #[tokio::test]
    async fn survey_produces_exact_synopses() {
        let store = Arc::new(Store::memory("survey-exact").await.unwrap());
        let engine = Engine::new(store.clone());
        let catalog = catalog::Catalog::new(store.clone());
        catalog
            .create_table(TableDraft {
                id: Some(SchemaId::new(1).unwrap()),
                name: "items".into(),
                columns: vec![
                    ColumnDraft {
                        id: Some(SchemaId::new(1).unwrap()),
                        name: "id".into(),
                        scalar_type: ScalarType::Text,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                    ColumnDraft {
                        id: Some(SchemaId::new(2).unwrap()),
                        name: "label".into(),
                        scalar_type: ScalarType::Text,
                        nullable: true,
                        format: String::new(),
                        default: None,
                    },
                ],
                primary_key: vec!["id".into()],
                indexes: vec![IndexDef {
                    name: "items_id_label_idx".into(),
                    columns: vec!["id".into(), "label".into()],
                    unique: false,
                }],
                foreign_keys: Vec::new(),
            })
            .await
            .unwrap();

        let rows: Vec<Vec<RawScalar>> = vec![
            vec![RawScalar::Text("a".into()), RawScalar::Text("x".into())],
            vec![RawScalar::Text("b".into()), RawScalar::Text("x".into())],
            vec![RawScalar::Text("c".into()), RawScalar::Null],
            vec![RawScalar::Text("d".into()), RawScalar::Text("y".into())],
        ];
        engine
            .execute_program(
                Program {
                    statements: vec![Statement::Create {
                        name: "seed".into(),
                        relation: crate::engine::lir::Query {
                            root: Relation::Rows {
                                scope: "input".into(),
                                columns: vec![
                                    RowsColumn {
                                        name: "id".into(),
                                        kind: Kind::Text,
                                        nullable: false,
                                    },
                                    RowsColumn {
                                        name: "label".into(),
                                        kind: Kind::Text,
                                        nullable: true,
                                    },
                                ],
                                values: rows,
                            },
                            cardinality: RootCardinality::Many,
                            bindings: HashMap::new(),
                        },
                        table: "items".into(),
                    }],
                    result: None,
                },
                CatalogPolicy::Forbidden,
            )
            .await
            .unwrap();

        let models = engine.survey().await.unwrap();
        assert_eq!(models.len(), 1);
        let model = &models[0];
        assert_eq!(model.observed_rows, 4);
        assert_eq!(model.coverage, SynopsisCoverage::Complete);
        assert_eq!(model.sample_size, 4);
        assert_eq!(model.columns.len(), 2);

        let id = &model.columns[0];
        assert_eq!(id.null_fraction, 0.0);
        assert_eq!(id.distinct, 4);
        assert!(id.distinct_is_exact);
        assert_eq!(id.maximum_width, Some(1));
        assert_eq!(id.minimum.as_deref(), Some("\"a\""));
        assert_eq!(id.maximum.as_deref(), Some("\"d\""));
        let id_distribution = id.range_distribution.as_ref().expect("id distribution");
        assert_eq!(id_distribution.format_version, 1);
        assert_eq!(id_distribution.coverage, SynopsisCoverage::Complete);
        assert_eq!(id_distribution.sample_size, 4);
        assert_eq!(id_distribution.collected_row_count, 4);
        assert_eq!(id_distribution.buckets.len(), 4);
        assert!(id_distribution.buckets.iter().all(|bucket| {
            bucket.rows == SynopsisCountBounds::exact(1)
                && bucket.lower_endpoint_rows == SynopsisCountBounds::exact(1)
                && bucket.upper_endpoint_rows == SynopsisCountBounds::exact(1)
        }));
        assert_eq!(
            id_distribution.buckets[3].cumulative_rows,
            SynopsisCountBounds::exact(4)
        );
        let id_degree = id.degree_sequence.as_ref().expect("id degree sequence");
        assert_eq!(id_degree.format_version, DEGREE_SEQUENCE_FORMAT_VERSION);
        assert_eq!(id_degree.value_generations, [id.value_generation]);
        assert_eq!(id_degree.distinct_values, 4);
        assert!(id_degree.distinct_is_exact);
        assert_eq!(
            id_degree.norms,
            DegreeSequenceNorms {
                l1: 4,
                l2_upper: 2,
                l_infinity: 1,
                exact: true,
            }
        );

        let label = &model.columns[1];
        assert_eq!(label.null_fraction, 0.25);
        assert_eq!(label.null_count, 1);
        assert_eq!(label.distinct, 2);
        assert_eq!(label.average_width, 1);
        let label_distribution = label
            .range_distribution
            .as_ref()
            .expect("label distribution");
        assert_eq!(label_distribution.buckets.len(), 2);
        assert_eq!(
            label_distribution.buckets[1].rows,
            SynopsisCountBounds::exact(1)
        );
        assert_eq!(label.most_common_values.len(), 2);
        assert_eq!(
            label.degree_sequence.as_ref().unwrap().norms,
            DegreeSequenceNorms {
                l1: 3,
                l2_upper: 3,
                l_infinity: 2,
                exact: true,
            }
        );
        assert_eq!(
            label.most_common_values[0],
            MostCommonValue {
                value: SynopsisValue::Text("x".into()),
                frequency: 2,
                maximum_error: 0,
            }
        );
        assert_eq!(model.column_groups.len(), 1);
        let group = &model.column_groups[0];
        assert_eq!(group.columns.len(), 2);
        assert_eq!(group.value_generations.len(), 2);
        assert_eq!(group.null_count, 1);
        assert_eq!(group.distinct, 3);
        assert!(group.distinct_is_exact);
        assert_eq!(group.most_common_values.len(), 3);
        assert_eq!(
            group.degree_sequence.as_ref().unwrap().norms,
            DegreeSequenceNorms {
                l1: 3,
                l2_upper: 2,
                l_infinity: 1,
                exact: true,
            }
        );
        assert!(group.most_common_values.iter().any(|common| {
            common.frequency == 1
                && common.maximum_error == 0
                && common.values
                    == [
                        SynopsisValue::Text("a".into()),
                        SynopsisValue::Text("x".into()),
                    ]
        }));
        let id_conditioned_on_label = model
            .predicate_conditioned_degrees
            .iter()
            .find(|conditioned| {
                conditioned.join_columns == [id.column]
                    && conditioned.predicate_column == label.column
            })
            .expect("id conditioned on label");
        assert_eq!(
            id_conditioned_on_label.values,
            vec![
                PredicateConditionedDegreeValue {
                    predicate_value: SynopsisValue::Text("x".into()),
                    matching_rows: 2,
                    non_null_join_rows: 2,
                    distinct_join_values: 2,
                    norms: DegreeSequenceNorms {
                        l1: 2,
                        l2_upper: 2,
                        l_infinity: 1,
                        exact: true,
                    },
                },
                PredicateConditionedDegreeValue {
                    predicate_value: SynopsisValue::Text("y".into()),
                    matching_rows: 1,
                    non_null_join_rows: 1,
                    distinct_join_values: 1,
                    norms: DegreeSequenceNorms {
                        l1: 1,
                        l2_upper: 1,
                        l_infinity: 1,
                        exact: true,
                    },
                },
            ]
        );

        let transaction = engine.store.begin(IsolationLevel::Snapshot).await.unwrap();
        let mut view = TransactionView(&*transaction);
        let table = catalog::store::list_tables(&mut view)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(
            model.columns[1].value_generation,
            table.columns[1].value_generation.get()
        );
        let capped = survey_table_with_limit(&view, &table, 1, 0, 2)
            .await
            .unwrap();
        let byte_capped = survey_table_with_budget(&view, &table, 1, 0, 10, 1)
            .await
            .unwrap();
        transaction.rollback();
        assert_eq!(capped.observed_rows, 2);
        assert_eq!(capped.coverage, SynopsisCoverage::PrefixLimit);
        let capped_distribution = capped.columns[0]
            .range_distribution
            .as_ref()
            .expect("prefix distribution");
        assert_eq!(capped_distribution.coverage, SynopsisCoverage::PrefixLimit);
        let capped_degree = capped.columns[0].degree_sequence.as_ref().unwrap();
        assert_eq!(capped_degree.coverage, SynopsisCoverage::PrefixLimit);
        assert!(!capped_degree.norms.exact);
        assert!(capped.predicate_conditioned_degrees.is_empty());
        assert!(
            capped_distribution
                .buckets
                .iter()
                .all(|bucket| bucket.rows.upper_bound.is_none())
        );
        assert_eq!(byte_capped.observed_rows, 1);
        assert_eq!(byte_capped.coverage, SynopsisCoverage::PrefixLimit);

        let request = |changed, known| SurveyRequest {
            changed,
            known,
            change_threshold: 10,
            max_age_micros: 50,
            now_micros: 120,
            row_budget: 10,
            byte_budget: 1024,
        };
        let first = engine
            .survey_next(request(HashMap::new(), HashMap::new()))
            .await
            .unwrap()
            .expect("missing synopsis");
        assert_eq!(first.model.table, table.schema_id);

        let known = HashMap::from([(table.schema_id, 100)]);
        assert!(
            engine
                .survey_next(request(HashMap::new(), known.clone()))
                .await
                .unwrap()
                .is_none()
        );
        let changed = HashMap::from([(table.schema_id, 10)]);
        let changed = engine
            .survey_next(request(changed, known))
            .await
            .unwrap()
            .expect("changed table");
        assert_eq!(changed.covered_changes, 10);
    }

    #[test]
    fn most_common_value_collection_is_bounded_and_tracks_skew() {
        let mut accumulator = MostCommonAccumulator::new();
        for index in 0..100 {
            let value = Value::Text(format!("value-{index}"));
            accumulator.observe(&value, value_digest(&value).0);
        }
        let hot = Value::Text("hot".into());
        for _ in 0..200 {
            accumulator.observe(&hot, value_digest(&hot).0);
        }

        let values = accumulator.result();
        assert_eq!(values.len(), MCV_CAPACITY);
        let hot = values
            .iter()
            .find(|entry| entry.value.storage_eq(&hot))
            .expect("hot value");
        assert_eq!(hot.lower_frequency(), 200);
        assert!(hot.frequency >= hot.lower_frequency());

        let mut oversized = MostCommonAccumulator::new();
        let value = Value::Text("x".repeat(MCV_MAX_TEXT_BYTES + 1));
        oversized.observe(&value, value_digest(&value).0);
        assert!(oversized.result().is_empty());
    }

    #[test]
    fn range_distribution_collection_is_bounded_and_keeps_exact_counts() {
        let mut distribution = RangeDistributionAccumulator::new();
        for value in 0..1_000 {
            assert!(distribution.observe(&Value::Int64(value)));
        }
        let column = crate::engine::planner::test_support::table().columns[0].clone();
        let distribution = distribution.result(&column, 1_000, SynopsisCoverage::Complete);
        assert!(!distribution.buckets.is_empty());
        assert!(distribution.buckets.len() <= RANGE_DISTRIBUTION_BUCKET_CAPACITY);
        assert_eq!(
            distribution
                .buckets
                .iter()
                .map(|bucket| bucket.rows.lower_bound)
                .sum::<u64>(),
            1_000
        );
        assert!(distribution.buckets.windows(2).all(|buckets| {
            buckets[0]
                .upper
                .compare(&buckets[1].lower)
                .is_some_and(|ordering| ordering.is_lt())
        }));
    }

    #[test]
    fn oversized_text_disables_one_column_distribution() {
        let mut accumulator = ColumnAccumulator::new(true, true);
        accumulator.observe(&Value::Text(
            "x".repeat(RANGE_DISTRIBUTION_MAX_TEXT_BYTES + 1),
        ));
        let column = crate::engine::planner::test_support::table().columns[0].clone();
        let synopsis = accumulator.synopsis(&column, 1, SynopsisCoverage::Complete);
        assert!(synopsis.range_distribution.is_none());
        assert!(!synopsis.degree_sequence.unwrap().norms.exact);
    }

    #[test]
    fn degree_sequence_collection_has_fixed_storage_and_safe_norms() {
        let mut accumulator = DegreeSequenceAccumulator::new();
        for value in 0..1_000i64 {
            accumulator.observe(degree_value_key(&Value::Int64(value % 100)));
        }
        let synopsis = accumulator.result(vec![7], 1_000, SynopsisCoverage::Complete);
        assert_eq!(synopsis.distinct_values, 100);
        assert!(synopsis.distinct_is_exact);
        assert!(synopsis.segments.len() <= DEGREE_SEQUENCE_SEGMENT_CAPACITY);
        assert_eq!(
            synopsis.norms,
            DegreeSequenceNorms {
                l1: 1_000,
                l2_upper: 100,
                l_infinity: 10,
                exact: true,
            }
        );
        assert!(synopsis.segments.iter().all(|segment| {
            segment.rank_start < segment.rank_end && segment.frequency_upper == 10
        }));

        let mut overflow = DegreeSequenceAccumulator::new();
        for value in 0..=DEGREE_SEQUENCE_DISTINCT_CAPACITY as i64 {
            overflow.observe(degree_value_key(&Value::Int64(value)));
        }
        let synopsis = overflow.result(
            vec![7],
            DEGREE_SEQUENCE_DISTINCT_CAPACITY as u64 + 1,
            SynopsisCoverage::Complete,
        );
        assert!(!synopsis.distinct_is_exact);
        assert!(!synopsis.norms.exact);
        assert_eq!(synopsis.segments.len(), 1);
    }

    #[test]
    fn predicate_conditioned_collection_has_fixed_domain_and_pair_limits() {
        let mut domain_overflow = PredicateConditionedDegreeAccumulator::new(vec![0], 1);
        for value in 0..=PREDICATE_CONDITION_VALUE_CAPACITY as i64 {
            domain_overflow.observe(&[Value::Int64(value), Value::Int64(value)]);
        }
        assert!(domain_overflow.values.is_none());

        let mut pair_overflow = PredicateConditionedDegreeAccumulator::new(vec![0], 1);
        for value in 0..=PREDICATE_CONDITION_PAIR_CAPACITY as i64 {
            pair_overflow.observe(&[Value::Int64(value), Value::Bool(true)]);
        }
        assert!(pair_overflow.values.is_none());
    }

    #[test]
    fn most_common_column_group_collection_is_bounded_and_tracks_skew() {
        let digest = |values: &[Value]| -> [u8; 32] {
            let mut hasher = Sha256::new();
            for value in values {
                hasher.update(value_digest(value).0);
            }
            hasher.finalize().into()
        };
        let mut accumulator = MostCommonColumnGroupAccumulator::new();
        for index in 0..100 {
            let values = [
                Value::Text(format!("board-{index}")),
                Value::Text("open".into()),
            ];
            accumulator.observe(&values, digest(&values));
        }
        let hot = [Value::Text("board-hot".into()), Value::Text("open".into())];
        for _ in 0..200 {
            accumulator.observe(&hot, digest(&hot));
        }

        let values = accumulator.result();
        assert_eq!(values.len(), MCV_CAPACITY);
        let hot = values
            .iter()
            .find(|entry| {
                entry.values.len() == hot.len()
                    && entry
                        .values
                        .iter()
                        .zip(&hot)
                        .all(|(left, right)| left.storage_eq(right))
            })
            .expect("hot column group");
        assert_eq!(hot.lower_frequency(), 200);

        let mut oversized = MostCommonColumnGroupAccumulator::new();
        let values = [
            Value::Text("x".repeat(MCV_MAX_TEXT_BYTES)),
            Value::Text("y".into()),
        ];
        oversized.observe(&values, digest(&values));
        assert!(oversized.result().is_empty());
    }

    #[test]
    fn composite_primary_keys_select_column_groups_without_a_secondary_index() {
        let mut table = crate::engine::planner::test_support::table();
        table.primary_key = vec!["board_id".into(), "status".into()];
        table.indexes.clear();

        let groups = column_group_accumulators(&table);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].column_indexes, [1, 2]);
    }

    #[test]
    fn composite_foreign_keys_select_column_groups_without_a_secondary_index() {
        let mut table = crate::engine::planner::test_support::table();
        table.indexes.clear();
        table
            .foreign_keys
            .push(crate::engine::catalog::model::ForeignKey {
                id: "board-status-fk".into(),
                name: "board_status_fk".into(),
                columns: vec!["board_id".into(), "status".into()],
                ref_table_id: "board-status-table".into(),
                ref_columns: vec!["board_id".into(), "status".into()],
            });

        let groups = column_group_accumulators(&table);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].column_indexes, [1, 2]);
    }

    #[test]
    fn signed_zero_has_one_statistical_identity() {
        let minus = Value::Float64(-0.0);
        let plus = Value::Float64(0.0);
        let minus_digest = value_digest(&minus).0;
        let plus_digest = value_digest(&plus).0;
        assert_eq!(minus_digest, plus_digest);

        let mut most_common = MostCommonAccumulator::new();
        most_common.observe(&minus, minus_digest);
        most_common.observe(&plus, plus_digest);
        let values = most_common.result();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].frequency, 2);
    }

    #[test]
    fn distinct_accumulator_has_bounded_exact_storage() {
        let mut accumulator = DistinctAccumulator::new();
        for value in 0..EXACT_DISTINCT_LIMIT as u64 + 1000 {
            accumulator.observe(value.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        }
        let (distinct, exact) = accumulator.result();
        assert!(!exact);
        assert!(distinct > EXACT_DISTINCT_LIMIT as u64);
    }
}
