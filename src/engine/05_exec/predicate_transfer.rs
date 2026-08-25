//! Predicate-transfer execution follows the RPT+ cascade and dynamic filter
//! policies from Qiao et al., "Robust Predicate Transfer with Dynamic
//! Execution," PVLDB 2026:
//! <https://people.iiis.tsinghua.edu.cn/~huanchen/publications/rpt%2B-vldb26.pdf>.
//! Min-max pruning operates on materialized execution blocks. A range-only
//! transfer schedule also narrows direct primary-key and index-prefix scans
//! before Slate reads their rows.

use std::sync::Arc;

use crate::engine::lir::eval::Env;
use crate::engine::planner::physical::{
    PredicateTransferPass, PredicateTransferRuntimePolicy, PredicateTransferSchedule,
};
use crate::engine::planner::predicate_transfer::{self, BloomFilter};

use super::observe::JoinOperatorMeasurement;
use super::pipeline::{frame_retained_bytes, join_key};
use super::{Error, ErrorKind, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum StorageRange {
    Empty {
        edge_index: usize,
    },
    Bounded {
        edge_index: usize,
        minimum: Vec<u8>,
        maximum: Vec<u8>,
    },
}

impl StorageRange {
    pub(super) const fn edge_index(&self) -> usize {
        match self {
            Self::Empty { edge_index } | Self::Bounded { edge_index, .. } => *edge_index,
        }
    }
}

pub(super) struct StorageRangeSchedule {
    pending: Vec<Vec<StorageRange>>,
}

impl StorageRangeSchedule {
    pub(super) fn new(input_count: usize) -> Self {
        Self {
            pending: vec![Vec::new(); input_count],
        }
    }

    pub(super) fn ranges(&self, input: usize) -> &[StorageRange] {
        self.pending.get(input).map_or(&[], Vec::as_slice)
    }

    pub(super) fn observe(
        &mut self,
        input: usize,
        rows: &[Env],
        pass: &PredicateTransferPass,
    ) -> Result<()> {
        for (edge_index, edge) in pass.edges.iter().enumerate() {
            if edge.source != input {
                continue;
            }
            let Some(range) = storage_range(edge_index, rows, &edge.keys)? else {
                continue;
            };
            self.pending[edge.target].push(range);
        }
        Ok(())
    }
}

enum StorageKey {
    Null,
    Encoded(Vec<u8>),
    Unavailable,
}

fn storage_range(
    edge_index: usize,
    rows: &[Env],
    keys: &[crate::engine::planner::analysis::EquiJoinKey],
) -> Result<Option<StorageRange>> {
    let mut minimum: Option<Vec<u8>> = None;
    let mut maximum: Option<Vec<u8>> = None;
    for row in rows {
        match storage_key(row, keys)? {
            StorageKey::Null => {}
            StorageKey::Unavailable => return Ok(None),
            StorageKey::Encoded(key) => {
                if minimum.as_ref().is_none_or(|minimum| key < *minimum) {
                    minimum = Some(key.clone());
                }
                if maximum.as_ref().is_none_or(|maximum| key > *maximum) {
                    maximum = Some(key);
                }
            }
        }
    }
    Ok(Some(match minimum.zip(maximum) {
        Some((minimum, maximum)) => StorageRange::Bounded {
            edge_index,
            minimum,
            maximum,
        },
        None => StorageRange::Empty { edge_index },
    }))
}

fn storage_key(
    frame: &Env,
    keys: &[crate::engine::planner::analysis::EquiJoinKey],
) -> Result<StorageKey> {
    let mut encoded = Vec::new();
    for key in keys {
        let field = &key.left;
        let value = frame.scalar_at(field.slot, &field.name, &field.value_type)?;
        if value.is_null() {
            return Ok(StorageKey::Null);
        }
        let Ok(value) = super::codec::encode_value(&value) else {
            return Ok(StorageKey::Unavailable);
        };
        encoded.extend_from_slice(&value);
    }
    Ok(StorageKey::Encoded(encoded))
}

pub(super) fn execute(
    mut inputs: Vec<Vec<Env>>,
    schedule: &PredicateTransferSchedule,
    bits_per_key: u8,
    hash_functions: u8,
    runtime_policy: &PredicateTransferRuntimePolicy,
    memory_limit_bytes: u64,
    track_false_positives: bool,
) -> Result<(Vec<Vec<Env>>, JoinOperatorMeasurement)> {
    if !valid_pass(&schedule.forward, inputs.len())
        || !valid_pass(&schedule.backward, inputs.len())
        || !valid_runtime_policy(runtime_policy)
    {
        return Err(Error::message(
            ErrorKind::Internal,
            "exec: invalid predicate transfer graph",
        ));
    }
    let mut measurement = JoinOperatorMeasurement {
        operator: "PredicateTransferJoin",
        rows_before_reduction: inputs.iter().map(Vec::len).sum::<usize>() as u64,
        filter_paths: schedule
            .forward
            .edges
            .len()
            .saturating_add(schedule.backward.edges.len()) as u64,
        filter_pruned_paths: schedule.pruned_paths as u64,
        filter_false_positive_measurement_complete: track_false_positives,
        ..Default::default()
    };
    let mut scans = vec![0u64; inputs.len()];
    let initial_bytes = retained_frame_bytes(&inputs);
    enforce_memory(initial_bytes, memory_limit_bytes)?;
    measurement.peak_retained_bytes = initial_bytes;
    transfer_pass(
        &mut inputs,
        &schedule.forward,
        bits_per_key,
        hash_functions,
        runtime_policy,
        memory_limit_bytes,
        track_false_positives,
        &mut scans,
        &mut measurement,
    )?;
    transfer_pass(
        &mut inputs,
        &schedule.backward,
        bits_per_key,
        hash_functions,
        runtime_policy,
        memory_limit_bytes,
        track_false_positives,
        &mut scans,
        &mut measurement,
    )?;
    measurement.rows_after_reduction = inputs.iter().map(Vec::len).sum::<usize>() as u64;
    Ok((inputs, measurement))
}

fn valid_runtime_policy(policy: &PredicateTransferRuntimePolicy) -> bool {
    policy.block_rows > 0
        && policy.build_sample_rows > 0
        && policy.build_selectivity_threshold_bps <= 10_000
        && policy.build_progress_threshold_bps <= 10_000
        && policy.probe_sample_rows > 0
        && policy.probe_stop_threshold_bps <= 10_000
}

fn valid_pass(pass: &PredicateTransferPass, input_count: usize) -> bool {
    let mut positions = vec![usize::MAX; input_count];
    for (position, input) in pass.order.iter().copied().enumerate() {
        if input >= input_count || positions[input] != usize::MAX {
            return false;
        }
        positions[input] = position;
    }
    pass.order.len() <= input_count
        && pass.order.iter().all(|input| *input < input_count)
        && pass.edges.iter().all(|edge| {
            edge.source < input_count
                && edge.target < input_count
                && positions[edge.source] < positions[edge.target]
                && positions[edge.target] != usize::MAX
        })
}

#[allow(clippy::too_many_arguments)]
fn transfer_pass(
    inputs: &mut [Vec<Env>],
    pass: &PredicateTransferPass,
    bits_per_key: u8,
    hash_functions: u8,
    runtime_policy: &PredicateTransferRuntimePolicy,
    memory_limit_bytes: u64,
    track_false_positives: bool,
    scans: &mut [u64],
    measurement: &mut JoinOperatorMeasurement,
) -> Result<()> {
    let mut pending = (0..inputs.len())
        .map(|_| Vec::<(usize, usize)>::new())
        .collect::<Vec<_>>();
    let mut storage = SharedFilterStorage::default();
    for &input in &pass.order {
        let pass_frame_bytes = retained_frame_bytes(inputs);
        measurement.filter_input_scans = measurement.filter_input_scans.saturating_add(1);
        if scans[input] > 0 {
            measurement.filter_repeated_scans = measurement.filter_repeated_scans.saturating_add(1);
        }
        scans[input] = scans[input].saturating_add(1);
        let incoming_handles = std::mem::take(&mut pending[input]);
        let mut incoming = incoming_handles
            .iter()
            .map(|(edge_index, storage_id)| {
                Ok(IncomingFilter {
                    edge_index: *edge_index,
                    filter: storage.get(*storage_id)?,
                    entered: 0,
                    passed: 0,
                    enabled: true,
                    decided: false,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let outgoing_groups = predicate_transfer::outgoing_edge_groups(pass, input);
        let expected = inputs[input].len();
        let mut outgoing = outgoing_groups
            .iter()
            .map(|edges| OutgoingFilter {
                edges: edges.clone(),
                filter: CascadeFilter::new(
                    expected,
                    bits_per_key,
                    hash_functions,
                    track_false_positives,
                ),
            })
            .collect::<Vec<_>>();
        cancel_builds_for_memory(
            pass_frame_bytes,
            &storage,
            &mut outgoing,
            memory_limit_bytes,
            measurement,
        )?;

        let current = std::mem::take(&mut inputs[input]);
        let before = current.len() as u64;
        let mut kept = Vec::with_capacity(current.len());
        let mut rows = current.into_iter();
        let mut build_progress = BuildProgress::new(!incoming.is_empty());
        loop {
            let block = rows
                .by_ref()
                .take(runtime_policy.block_rows as usize)
                .collect::<Vec<_>>();
            if block.is_empty() {
                break;
            }
            let scanned = block.len() as u64;
            measurement.filter_rows_scanned =
                measurement.filter_rows_scanned.saturating_add(scanned);
            let block = filter_block(block, &mut incoming, pass, runtime_policy, measurement)?;
            build_progress.observe(scanned, block.len() as u64);
            if build_progress.should_cancel(before, runtime_policy) {
                cancel_outgoing(&mut outgoing, measurement, false);
            }
            for row in &block {
                for outgoing_filter in &mut outgoing {
                    let edge = &pass.edges[outgoing_filter.edges[0]];
                    if let Some(key) = join_key(row, &edge.keys, true)? {
                        outgoing_filter.filter.insert(&key);
                        measurement.filter_insertions =
                            measurement.filter_insertions.saturating_add(1);
                    }
                }
            }
            kept.extend(block);
            cancel_builds_for_memory(
                pass_frame_bytes,
                &storage,
                &mut outgoing,
                memory_limit_bytes,
                measurement,
            )?;
        }
        measurement.filter_rows_skipped = measurement
            .filter_rows_skipped
            .saturating_add(before.saturating_sub(kept.len() as u64));
        inputs[input] = kept;

        for outgoing_filter in outgoing {
            measurement.filter_false_positive_measurement_complete &=
                outgoing_filter.filter.false_positive_tracking_complete();
            let retained_bytes = outgoing_filter.filter.retained_bytes();
            measurement.filter_bytes = measurement.filter_bytes.saturating_add(retained_bytes);
            measurement.filter_builds = measurement.filter_builds.saturating_add(1);
            measurement.filter_shared_paths = measurement
                .filter_shared_paths
                .saturating_add(outgoing_filter.edges.len().saturating_sub(1) as u64);
            let storage_id = storage.insert(outgoing_filter.filter, outgoing_filter.edges.len());
            for edge_index in outgoing_filter.edges {
                let target = pass.edges[edge_index].target;
                pending[target].push((edge_index, storage_id));
            }
        }
        drop(incoming);
        for (_, storage_id) in incoming_handles {
            storage.consume(storage_id)?;
        }
        update_memory(
            inputs,
            storage.retained_bytes,
            memory_limit_bytes,
            measurement,
        )?;
    }
    Ok(())
}

struct CascadeFilter {
    bloom: BloomFilter,
    minimum: Option<Vec<u8>>,
    maximum: Option<Vec<u8>>,
}

impl CascadeFilter {
    fn new(
        expected_entries: usize,
        bits_per_key: u8,
        hash_functions: u8,
        track_false_positives: bool,
    ) -> Self {
        Self {
            bloom: BloomFilter::new(
                expected_entries,
                bits_per_key,
                hash_functions,
                track_false_positives,
            ),
            minimum: None,
            maximum: None,
        }
    }

    fn insert(&mut self, key: &[u8]) {
        if self.minimum.as_deref().is_none_or(|minimum| key < minimum) {
            self.minimum = Some(key.to_vec());
        }
        if self.maximum.as_deref().is_none_or(|maximum| key > maximum) {
            self.maximum = Some(key.to_vec());
        }
        self.bloom.insert(key);
    }

    fn range_disjoint(&self, minimum: &[u8], maximum: &[u8]) -> bool {
        self.minimum
            .as_deref()
            .zip(self.maximum.as_deref())
            .is_none_or(|(filter_minimum, filter_maximum)| {
                maximum < filter_minimum || minimum > filter_maximum
            })
    }

    fn contains_range(&self, key: &[u8]) -> bool {
        self.minimum
            .as_deref()
            .zip(self.maximum.as_deref())
            .is_some_and(|(minimum, maximum)| key >= minimum && key <= maximum)
    }

    fn retained_bytes(&self) -> u64 {
        self.bloom
            .retained_bytes()
            .saturating_add(self.minimum.as_ref().map_or(0, |key| key.len() as u64))
            .saturating_add(self.maximum.as_ref().map_or(0, |key| key.len() as u64))
    }

    fn false_positive_tracking_complete(&self) -> bool {
        self.bloom.false_positive_tracking_complete()
    }
}

struct OutgoingFilter {
    edges: Vec<usize>,
    filter: CascadeFilter,
}

struct IncomingFilter {
    edge_index: usize,
    filter: Arc<CascadeFilter>,
    entered: u64,
    passed: u64,
    enabled: bool,
    decided: bool,
}

struct StoredFilter {
    filter: Arc<CascadeFilter>,
    remaining_consumers: usize,
}

#[derive(Default)]
struct SharedFilterStorage {
    filters: Vec<Option<StoredFilter>>,
    retained_bytes: u64,
}

impl SharedFilterStorage {
    fn insert(&mut self, filter: CascadeFilter, consumers: usize) -> usize {
        let retained_bytes = filter.retained_bytes();
        let id = self.filters.len();
        self.filters.push(Some(StoredFilter {
            filter: Arc::new(filter),
            remaining_consumers: consumers,
        }));
        self.retained_bytes = self.retained_bytes.saturating_add(retained_bytes);
        id
    }

    fn get(&self, id: usize) -> Result<Arc<CascadeFilter>> {
        self.filters
            .get(id)
            .and_then(Option::as_ref)
            .map(|stored| Arc::clone(&stored.filter))
            .ok_or_else(|| Error::message(ErrorKind::Internal, "exec: missing cascade filter"))
    }

    fn consume(&mut self, id: usize) -> Result<()> {
        let stored = self
            .filters
            .get_mut(id)
            .and_then(Option::as_mut)
            .ok_or_else(|| Error::message(ErrorKind::Internal, "exec: missing cascade filter"))?;
        stored.remaining_consumers = stored.remaining_consumers.saturating_sub(1);
        if stored.remaining_consumers == 0 {
            let stored = self.filters[id].take().expect("stored cascade filter");
            self.retained_bytes = self
                .retained_bytes
                .saturating_sub(stored.filter.retained_bytes());
        }
        Ok(())
    }
}

struct BuildProgress {
    can_cancel: bool,
    decided: bool,
    scanned: u64,
    received: u64,
}

impl BuildProgress {
    const fn new(can_cancel: bool) -> Self {
        Self {
            can_cancel,
            decided: false,
            scanned: 0,
            received: 0,
        }
    }

    fn observe(&mut self, scanned: u64, received: u64) {
        self.scanned = self.scanned.saturating_add(scanned);
        self.received = self.received.saturating_add(received);
    }

    fn should_cancel(&mut self, total: u64, policy: &PredicateTransferRuntimePolicy) -> bool {
        if !self.can_cancel || self.decided || self.received < policy.build_sample_rows {
            return false;
        }
        self.decided = true;
        ratio_bps(self.received, self.scanned) > policy.build_selectivity_threshold_bps
            && ratio_bps(self.scanned, total) < policy.build_progress_threshold_bps
    }
}

fn filter_block(
    mut rows: Vec<Env>,
    incoming: &mut [IncomingFilter],
    pass: &PredicateTransferPass,
    policy: &PredicateTransferRuntimePolicy,
    measurement: &mut JoinOperatorMeasurement,
) -> Result<Vec<Env>> {
    for incoming_filter in incoming.iter_mut().filter(|filter| filter.enabled) {
        if rows.is_empty() {
            break;
        }
        let edge = &pass.edges[incoming_filter.edge_index];
        let keyed = rows
            .into_iter()
            .map(|row| Ok((join_key(&row, &edge.keys, false)?, row)))
            .collect::<Result<Vec<_>>>()?;
        let block_range = keyed.iter().filter_map(|(key, _)| key.as_deref()).fold(
            None::<(&[u8], &[u8])>,
            |range, key| {
                Some(match range {
                    Some((minimum, maximum)) => (minimum.min(key), maximum.max(key)),
                    None => (key, key),
                })
            },
        );
        measurement.filter_blocks_scanned = measurement.filter_blocks_scanned.saturating_add(1);
        measurement.filter_min_max_checks = measurement.filter_min_max_checks.saturating_add(1);
        incoming_filter.entered = incoming_filter.entered.saturating_add(keyed.len() as u64);
        if block_range.is_none_or(|(minimum, maximum)| {
            incoming_filter.filter.range_disjoint(minimum, maximum)
        }) {
            measurement.filter_blocks_skipped = measurement.filter_blocks_skipped.saturating_add(1);
            measurement.filter_min_max_rows_skipped = measurement
                .filter_min_max_rows_skipped
                .saturating_add(keyed.len() as u64);
            rows = Vec::new();
        } else {
            rows = Vec::with_capacity(keyed.len());
            for (key, row) in keyed {
                let Some(key) = key else {
                    continue;
                };
                measurement.filter_min_max_checks =
                    measurement.filter_min_max_checks.saturating_add(1);
                if !incoming_filter.filter.contains_range(&key) {
                    measurement.filter_min_max_rows_skipped =
                        measurement.filter_min_max_rows_skipped.saturating_add(1);
                    continue;
                }
                measurement.filter_checks = measurement.filter_checks.saturating_add(1);
                let probe = incoming_filter.filter.bloom.probe(&key);
                measurement.filter_false_positives = measurement
                    .filter_false_positives
                    .saturating_add(u64::from(probe.false_positive));
                if probe.may_contain {
                    incoming_filter.passed = incoming_filter.passed.saturating_add(1);
                    rows.push(row);
                }
            }
        }
        if !incoming_filter.decided && incoming_filter.entered >= policy.probe_sample_rows {
            incoming_filter.decided = true;
            if ratio_bps(incoming_filter.passed, incoming_filter.entered)
                >= policy.probe_stop_threshold_bps
            {
                incoming_filter.enabled = false;
                measurement.filter_probe_cancellations =
                    measurement.filter_probe_cancellations.saturating_add(1);
            }
        }
    }
    Ok(rows)
}

fn cancel_builds_for_memory(
    frame_bytes: u64,
    storage: &SharedFilterStorage,
    outgoing: &mut Vec<OutgoingFilter>,
    memory_limit_bytes: u64,
    measurement: &mut JoinOperatorMeasurement,
) -> Result<()> {
    let base = frame_bytes.saturating_add(storage.retained_bytes);
    enforce_memory(base, memory_limit_bytes)?;
    let outgoing_bytes = outgoing
        .iter()
        .map(|outgoing| outgoing.filter.retained_bytes())
        .fold(0u64, u64::saturating_add);
    if base.saturating_add(outgoing_bytes) > memory_limit_bytes {
        cancel_outgoing(outgoing, measurement, true);
    } else {
        measurement.peak_retained_bytes = measurement
            .peak_retained_bytes
            .max(base.saturating_add(outgoing_bytes));
    }
    Ok(())
}

fn cancel_outgoing(
    outgoing: &mut Vec<OutgoingFilter>,
    measurement: &mut JoinOperatorMeasurement,
    memory: bool,
) {
    if outgoing.is_empty() {
        return;
    }
    measurement.filter_build_cancellations = measurement
        .filter_build_cancellations
        .saturating_add(outgoing.len() as u64);
    measurement.filter_paths_canceled = measurement.filter_paths_canceled.saturating_add(
        outgoing
            .iter()
            .map(|outgoing| outgoing.edges.len() as u64)
            .fold(0u64, u64::saturating_add),
    );
    if memory {
        measurement.filter_memory_cancellations = measurement
            .filter_memory_cancellations
            .saturating_add(outgoing.len() as u64);
    }
    outgoing.clear();
}

fn ratio_bps(numerator: u64, denominator: u64) -> u16 {
    if denominator == 0 {
        return 0;
    }
    numerator
        .saturating_mul(10_000)
        .checked_div(denominator)
        .unwrap_or(u64::MAX)
        .min(10_000) as u16
}

fn retained_frame_bytes(inputs: &[Vec<Env>]) -> u64 {
    inputs
        .iter()
        .flatten()
        .map(frame_retained_bytes)
        .fold(0u64, u64::saturating_add)
}

fn update_memory(
    inputs: &[Vec<Env>],
    filter_bytes: u64,
    memory_limit_bytes: u64,
    measurement: &mut JoinOperatorMeasurement,
) -> Result<()> {
    let retained = retained_frame_bytes(inputs).saturating_add(filter_bytes);
    enforce_memory(retained, memory_limit_bytes)?;
    measurement.peak_retained_bytes = measurement.peak_retained_bytes.max(retained);
    Ok(())
}

fn enforce_memory(retained_bytes: u64, memory_limit_bytes: u64) -> Result<()> {
    if retained_bytes > memory_limit_bytes {
        return Err(Error::message(
            ErrorKind::Runtime,
            format!("exec: predicate transfer retained byte limit {memory_limit_bytes} exceeded"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::engine::lir::{Field, Kind, SlotId, Type, Value};
    use crate::engine::planner::analysis::EquiJoinKey;
    use crate::engine::planner::physical::{
        PredicateTransferEdge, PredicateTransferPass, PredicateTransferSchedule,
    };

    use super::*;

    fn field(name: &str, slot: usize) -> Field {
        Field {
            name: name.into(),
            slot: SlotId(slot),
            value_type: Type::scalar(Kind::Text, false),
        }
    }

    fn row(values: &[(usize, &str)]) -> Env {
        let mut row = Env::new();
        for (slot, value) in values {
            row.set_scalar(SlotId(*slot), Value::Text((*value).into()));
        }
        row
    }

    fn symmetric_schedule(
        forward_edges: Vec<PredicateTransferEdge>,
        forward_order: Vec<usize>,
    ) -> PredicateTransferSchedule {
        let backward_edges = forward_edges
            .iter()
            .map(|edge| PredicateTransferEdge {
                source: edge.target,
                target: edge.source,
                keys: edge
                    .keys
                    .iter()
                    .map(|key| EquiJoinKey {
                        left: key.right.clone(),
                        right: key.left.clone(),
                    })
                    .collect(),
            })
            .collect();
        PredicateTransferSchedule {
            root_input: *forward_order.last().unwrap_or(&0),
            forward: PredicateTransferPass {
                order: forward_order.clone(),
                edges: forward_edges,
            },
            backward: PredicateTransferPass {
                order: forward_order.into_iter().rev().collect(),
                edges: backward_edges,
            },
            pruned_paths: 0,
        }
    }

    fn one_pass_schedule(
        edges: Vec<PredicateTransferEdge>,
        order: Vec<usize>,
    ) -> PredicateTransferSchedule {
        PredicateTransferSchedule {
            root_input: *order.last().unwrap_or(&0),
            forward: PredicateTransferPass { order, edges },
            backward: PredicateTransferPass {
                order: Vec::new(),
                edges: Vec::new(),
            },
            pruned_paths: 0,
        }
    }

    fn test_policy(block_rows: u32) -> PredicateTransferRuntimePolicy {
        PredicateTransferRuntimePolicy {
            block_rows,
            build_sample_rows: 100_000,
            build_selectivity_threshold_bps: 3_500,
            build_progress_threshold_bps: 6_000,
            probe_sample_rows: 100_000,
            probe_stop_threshold_bps: 9_000,
        }
    }

    #[test]
    fn two_pass_transfer_filters_a_cyclic_graph() {
        let edges = vec![
            PredicateTransferEdge {
                source: 0,
                target: 1,
                keys: vec![EquiJoinKey {
                    left: field("r_b", 1),
                    right: field("s_b", 2),
                }],
            },
            PredicateTransferEdge {
                source: 0,
                target: 2,
                keys: vec![EquiJoinKey {
                    left: field("r_a", 0),
                    right: field("t_a", 5),
                }],
            },
            PredicateTransferEdge {
                source: 1,
                target: 2,
                keys: vec![EquiJoinKey {
                    left: field("s_c", 3),
                    right: field("t_c", 4),
                }],
            },
        ];
        let inputs = vec![
            vec![row(&[(0, "a"), (1, "b")])],
            vec![row(&[(2, "b"), (3, "c")])],
            vec![row(&[(4, "c"), (5, "different")])],
        ];

        let schedule = symmetric_schedule(edges, vec![0, 1, 2]);
        let (filtered, measurement) = execute(
            inputs,
            &schedule,
            12,
            8,
            &predicate_transfer::runtime_policy(),
            1_000_000,
            true,
        )
        .expect("predicate transfer");

        assert!(filtered.iter().all(Vec::is_empty));
        assert_eq!(measurement.filter_input_scans, 6);
        assert_eq!(measurement.filter_repeated_scans, 3);
        assert_eq!(measurement.filter_rows_skipped, 3);
        assert_eq!(measurement.filter_false_positives, 0);
    }

    #[test]
    fn retained_byte_limit_fails_before_the_join_phase() {
        let inputs = vec![vec![row(&[(0, "large")])], vec![row(&[(1, "large")])]];
        let edges = vec![PredicateTransferEdge {
            source: 0,
            target: 1,
            keys: vec![EquiJoinKey {
                left: field("left", 0),
                right: field("right", 1),
            }],
        }];

        let schedule = symmetric_schedule(edges, vec![0, 1]);
        let error = execute(
            inputs,
            &schedule,
            12,
            8,
            &predicate_transfer::runtime_policy(),
            1,
            false,
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("predicate transfer retained byte limit")
        );
    }

    #[test]
    fn cascade_min_max_skips_a_disjoint_block() {
        let schedule = one_pass_schedule(
            vec![PredicateTransferEdge {
                source: 0,
                target: 1,
                keys: vec![EquiJoinKey {
                    left: field("left", 0),
                    right: field("right", 1),
                }],
            }],
            vec![0, 1],
        );
        let inputs = vec![
            vec![row(&[(0, "a")]), row(&[(0, "b")])],
            vec![row(&[(1, "x")]), row(&[(1, "y")])],
        ];

        let (filtered, measurement) =
            execute(inputs, &schedule, 20, 7, &test_policy(2), 1_000_000, true)
                .expect("predicate transfer");

        assert!(filtered[1].is_empty());
        assert_eq!(measurement.filter_blocks_scanned, 1);
        assert_eq!(measurement.filter_blocks_skipped, 1);
        assert_eq!(measurement.filter_min_max_rows_skipped, 2);
        assert_eq!(measurement.filter_checks, 0);
    }

    #[test]
    fn broadcast_paths_share_one_cascade_filter() {
        let schedule = one_pass_schedule(
            vec![
                PredicateTransferEdge {
                    source: 0,
                    target: 1,
                    keys: vec![EquiJoinKey {
                        left: field("source", 0),
                        right: field("first_target", 1),
                    }],
                },
                PredicateTransferEdge {
                    source: 0,
                    target: 2,
                    keys: vec![EquiJoinKey {
                        left: field("source", 0),
                        right: field("second_target", 2),
                    }],
                },
            ],
            vec![0, 1, 2],
        );
        let inputs = vec![
            vec![row(&[(0, "shared")])],
            vec![row(&[(1, "shared")])],
            vec![row(&[(2, "shared")])],
        ];

        let (filtered, measurement) =
            execute(inputs, &schedule, 20, 7, &test_policy(2), 1_000_000, true)
                .expect("predicate transfer");

        assert!(filtered.iter().all(|rows| rows.len() == 1));
        assert_eq!(measurement.filter_paths, 2);
        assert_eq!(measurement.filter_builds, 1);
        assert_eq!(measurement.filter_shared_paths, 1);
        assert_eq!(measurement.filter_insertions, 1);
    }

    #[test]
    fn an_unselective_probe_is_canceled_after_one_block() {
        let schedule = one_pass_schedule(
            vec![PredicateTransferEdge {
                source: 0,
                target: 1,
                keys: vec![EquiJoinKey {
                    left: field("left", 0),
                    right: field("right", 1),
                }],
            }],
            vec![0, 1],
        );
        let inputs = vec![
            vec![row(&[(0, "same")])],
            (0..6).map(|_| row(&[(1, "same")])).collect(),
        ];
        let mut policy = test_policy(2);
        policy.probe_sample_rows = 2;

        let (filtered, measurement) = execute(inputs, &schedule, 20, 7, &policy, 1_000_000, true)
            .expect("predicate transfer");

        assert_eq!(filtered[1].len(), 6);
        assert_eq!(measurement.filter_probe_cancellations, 1);
        assert_eq!(measurement.filter_checks, 2);
    }

    #[test]
    fn an_early_unselective_source_cancels_its_outgoing_filter() {
        let schedule = one_pass_schedule(
            vec![
                PredicateTransferEdge {
                    source: 0,
                    target: 1,
                    keys: vec![EquiJoinKey {
                        left: field("first", 0),
                        right: field("middle_in", 1),
                    }],
                },
                PredicateTransferEdge {
                    source: 1,
                    target: 2,
                    keys: vec![EquiJoinKey {
                        left: field("middle_out", 2),
                        right: field("last", 3),
                    }],
                },
            ],
            vec![0, 1, 2],
        );
        let inputs = vec![
            vec![row(&[(0, "same")])],
            (0..10).map(|_| row(&[(1, "same"), (2, "out")])).collect(),
            vec![row(&[(3, "different")])],
        ];
        let mut policy = test_policy(2);
        policy.build_sample_rows = 2;

        let (filtered, measurement) = execute(inputs, &schedule, 20, 7, &policy, 1_000_000, true)
            .expect("predicate transfer");

        assert_eq!(filtered[2].len(), 1);
        assert_eq!(measurement.filter_build_cancellations, 1);
        assert_eq!(measurement.filter_paths_canceled, 1);
        assert_eq!(measurement.filter_builds, 1);
    }

    #[test]
    fn a_filter_build_is_canceled_before_it_exceeds_memory() {
        let schedule = one_pass_schedule(
            vec![PredicateTransferEdge {
                source: 0,
                target: 1,
                keys: vec![EquiJoinKey {
                    left: field("left", 0),
                    right: field("right", 1),
                }],
            }],
            vec![0, 1],
        );
        let inputs = vec![vec![row(&[(0, "same")])], vec![row(&[(1, "same")])]];
        let memory_limit_bytes = retained_frame_bytes(&inputs);

        let (filtered, measurement) = execute(
            inputs,
            &schedule,
            20,
            7,
            &test_policy(2),
            memory_limit_bytes,
            true,
        )
        .expect("predicate transfer");

        assert_eq!(filtered[1].len(), 1);
        assert_eq!(measurement.filter_memory_cancellations, 1);
        assert_eq!(measurement.filter_build_cancellations, 1);
        assert_eq!(measurement.filter_paths_canceled, 1);
        assert_eq!(measurement.filter_builds, 0);
    }
}
