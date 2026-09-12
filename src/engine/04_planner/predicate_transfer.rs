//! Predicate-transfer scheduling and deterministic Bloom filters.
//!
//! The filter transformation follows Yang et al., "Predicate Transfer:
//! Efficient Pre-Filtering on Multi-Join Queries," CIDR 2024:
//! <https://www.vldb.org/cidrdb/papers/2024/p22-yang.pdf>.
//! The maximum-weight tree and largest-input root follow Zhao et al.,
//! "Debunking the Myth of Join Ordering," SIGMOD 2025:
//! <https://people.iiis.tsinghua.edu.cn/~huanchen/publications/rpt-sigmod25.pdf>.
//! Distinct deep-forward and shallow-backward trees follow Qiao et al.,
//! "Robust Predicate Transfer with Dynamic Execution," PVLDB 2026:
//! <https://people.iiis.tsinghua.edu.cn/~huanchen/publications/rpt%2B-vldb26.pdf>.
//! The same work defines the cascade filter, shared broadcast filter, and
//! dynamic build and probe cancellation policies used here.
//! Bloom filters can reject a row with no false negative. A positive result is
//! not cardinality evidence because it can be a false positive.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::engine::catalog::model::Table;
use crate::engine::lir::Value;
use crate::engine::lir::bound::{Relation, RelationNode};

use super::acyclic_join::JoinGraph;
use super::models::{PlannerStats, SynopsisCoverage, SynopsisModel, SynopsisValue};
use super::physical::{
    AccessQuantity, PredicateTransferEdge, PredicateTransferPass, PredicateTransferSchedule,
};

pub(crate) const DEFAULT_BITS_PER_KEY: u8 = 20;
pub(crate) const DEFAULT_HASH_FUNCTIONS: u8 = 7;
pub(crate) const DEFAULT_CASCADE_BLOCK_ROWS: u32 = 256;
pub(crate) const DEFAULT_BUILD_SAMPLE_ROWS: u64 = 100_000;
pub(crate) const DEFAULT_BUILD_SELECTIVITY_THRESHOLD_BPS: u16 = 3_500;
pub(crate) const DEFAULT_BUILD_PROGRESS_THRESHOLD_BPS: u16 = 6_000;
pub(crate) const DEFAULT_PROBE_SAMPLE_ROWS: u64 = 100_000;
pub(crate) const DEFAULT_PROBE_STOP_THRESHOLD_BPS: u16 = 9_000;
const FALSE_POSITIVE_TRACKING_LIMIT_BYTES: u64 = 64 * 1024;

pub(crate) const fn runtime_policy() -> super::physical::PredicateTransferRuntimePolicy {
    super::physical::PredicateTransferRuntimePolicy {
        block_rows: DEFAULT_CASCADE_BLOCK_ROWS,
        build_sample_rows: DEFAULT_BUILD_SAMPLE_ROWS,
        build_selectivity_threshold_bps: DEFAULT_BUILD_SELECTIVITY_THRESHOLD_BPS,
        build_progress_threshold_bps: DEFAULT_BUILD_PROGRESS_THRESHOLD_BPS,
        probe_sample_rows: DEFAULT_PROBE_SAMPLE_ROWS,
        probe_stop_threshold_bps: DEFAULT_PROBE_STOP_THRESHOLD_BPS,
    }
}

pub(crate) fn outgoing_edge_groups(pass: &PredicateTransferPass, input: usize) -> Vec<Vec<usize>> {
    let mut groups = Vec::<(Vec<crate::engine::lir::SlotId>, Vec<usize>)>::new();
    for (edge_index, edge) in pass
        .edges
        .iter()
        .enumerate()
        .filter(|(_, edge)| edge.source == input)
    {
        let signature = source_signature(edge);
        if let Some((_, edges)) = groups
            .iter_mut()
            .find(|(candidate, _)| *candidate == signature)
        {
            edges.push(edge_index);
        } else {
            groups.push((signature, vec![edge_index]));
        }
    }
    groups.into_iter().map(|(_, edges)| edges).collect()
}

pub(crate) fn filter_storage_shape(schedule: &PredicateTransferSchedule) -> (u32, u32) {
    let paths = schedule
        .forward
        .edges
        .len()
        .saturating_add(schedule.backward.edges.len());
    let builds = [&schedule.forward, &schedule.backward]
        .into_iter()
        .flat_map(|pass| pass.order.iter().map(move |input| (pass, *input)))
        .map(|(pass, input)| outgoing_edge_groups(pass, input).len())
        .fold(0usize, usize::saturating_add);
    (
        builds.try_into().unwrap_or(u32::MAX),
        paths.saturating_sub(builds).try_into().unwrap_or(u32::MAX),
    )
}

pub(crate) struct TransferSimulation {
    pub filtered_rows: Vec<u64>,
    pub rows_scanned: u64,
    pub insertions: u64,
    pub checks: u64,
    pub min_max_checks: u64,
    pub false_positives: u64,
    pub rows_skipped: u64,
    pub input_scans: u64,
    pub repeated_scans: u64,
    pub filter_bytes: u64,
    pub peak_filter_bytes: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct TransferEstimate {
    pub filtered_rows: Vec<AccessQuantity>,
    pub filter_work: AccessQuantity,
    pub filter_bytes: AccessQuantity,
    pub peak_filter_bytes: AccessQuantity,
}

#[derive(Clone)]
struct CompleteDomain {
    entries: Vec<DomainEntry>,
}

#[derive(Clone)]
struct DomainEntry {
    key: Vec<u8>,
    rows: u64,
}

struct BoundState {
    rows: AccessQuantity,
    current_domains: HashMap<Vec<crate::engine::lir::SlotId>, CompleteDomain>,
    original_domains: HashMap<Vec<crate::engine::lir::SlotId>, CompleteDomain>,
}

struct PendingBoundFilter {
    target_signature: Vec<crate::engine::lir::SlotId>,
    filter: Option<Arc<BloomFilter>>,
}

struct BoundSimulation {
    estimate: TransferEstimate,
    forward_no_effect: Vec<usize>,
    backward_no_effect: Vec<usize>,
}

#[derive(Clone)]
struct WeightedSynopsisRow {
    values: HashMap<crate::engine::lir::SlotId, SynopsisValue>,
    frequency: u64,
}

#[derive(Clone)]
pub(crate) struct BloomFilter {
    words: Vec<u64>,
    bit_count: u64,
    hash_functions: u8,
    inserted: bool,
    exact_keys: Option<HashSet<Vec<u8>>>,
    exact_key_bytes: u64,
    tracking_complete: bool,
}

pub(crate) struct BloomProbe {
    pub may_contain: bool,
    pub false_positive: bool,
}

#[derive(Clone)]
struct SimulatedCascadeFilter {
    bloom: BloomFilter,
    minimum: Option<Vec<u8>>,
    maximum: Option<Vec<u8>>,
}

impl SimulatedCascadeFilter {
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

    fn probe(&self, key: &[u8]) -> BloomProbe {
        self.bloom.probe(key)
    }

    fn retained_bytes(&self) -> u64 {
        self.bloom
            .retained_bytes()
            .saturating_add(self.range_bytes())
    }

    fn range_bytes(&self) -> u64 {
        self.minimum
            .as_ref()
            .map_or(0, |key| key.len() as u64)
            .saturating_add(self.maximum.as_ref().map_or(0, |key| key.len() as u64))
    }
}

impl BloomFilter {
    pub(crate) fn new(
        expected_entries: usize,
        bits_per_key: u8,
        hash_functions: u8,
        track_false_positives: bool,
    ) -> Self {
        let requested_bits = (expected_entries as u64).saturating_mul(u64::from(bits_per_key));
        let word_count = if requested_bits == 0 {
            0
        } else {
            requested_bits.div_ceil(u64::from(u64::BITS)) as usize
        };
        Self {
            words: vec![0; word_count],
            bit_count: (word_count as u64).saturating_mul(u64::from(u64::BITS)),
            hash_functions: hash_functions.max(1),
            inserted: false,
            exact_keys: track_false_positives.then(HashSet::new),
            exact_key_bytes: 0,
            tracking_complete: track_false_positives,
        }
    }

    pub(crate) fn insert(&mut self, key: &[u8]) {
        if self.bit_count == 0 {
            return;
        }
        let (first, second) = hash_pair(key);
        for hash in 0..self.hash_functions {
            let bit = first.wrapping_add(u64::from(hash).wrapping_mul(second)) % self.bit_count;
            self.words[bit as usize / u64::BITS as usize] |= 1 << (bit % u64::BITS as u64);
        }
        self.inserted = true;
        let mut stop_tracking = false;
        if let Some(exact_keys) = &mut self.exact_keys
            && !exact_keys.contains(key)
        {
            let next_bytes = self.exact_key_bytes.saturating_add(key.len() as u64);
            if next_bytes > FALSE_POSITIVE_TRACKING_LIMIT_BYTES {
                stop_tracking = true;
            } else {
                exact_keys.insert(key.to_vec());
                self.exact_key_bytes = next_bytes;
            }
        }
        if stop_tracking {
            self.exact_keys = None;
            self.exact_key_bytes = 0;
            self.tracking_complete = false;
        }
    }

    pub(crate) fn probe(&self, key: &[u8]) -> BloomProbe {
        if !self.inserted {
            return BloomProbe {
                may_contain: false,
                false_positive: false,
            };
        }
        let (first, second) = hash_pair(key);
        let may_contain = (0..self.hash_functions).all(|hash| {
            let bit = first.wrapping_add(u64::from(hash).wrapping_mul(second)) % self.bit_count;
            self.words[bit as usize / u64::BITS as usize] & (1 << (bit % u64::BITS as u64)) != 0
        });
        let false_positive = may_contain
            && self
                .exact_keys
                .as_ref()
                .is_some_and(|keys| !keys.contains(key));
        BloomProbe {
            may_contain,
            false_positive,
        }
    }

    pub(crate) fn retained_bytes(&self) -> u64 {
        (self.words.len() as u64).saturating_mul(u64::from(u64::BITS / 8))
    }

    pub(crate) const fn false_positive_tracking_complete(&self) -> bool {
        self.tracking_complete
    }
}

pub(crate) fn schedule(
    graph: &JoinGraph<'_>,
    rows: &[AccessQuantity],
) -> Option<PredicateTransferSchedule> {
    if rows.len() != graph.inputs.len() || rows.iter().any(|rows| rows.upper_bound.is_none()) {
        return None;
    }
    let root = (0..rows.len()).min_by_key(|input| {
        (
            Reverse(rows[*input].upper_bound.expect("finite row bound")),
            Reverse(rows[*input].central),
            *input,
        )
    })?;
    let mut inserted = vec![false; rows.len()];
    inserted[root] = true;
    let mut insertion_order = vec![root];
    let mut forward_depth = vec![0usize; rows.len()];
    let mut backward_depth = vec![0usize; rows.len()];
    let mut forward_edges = Vec::with_capacity(rows.len().saturating_sub(1));
    let mut backward_edges = Vec::with_capacity(rows.len().saturating_sub(1));

    while insertion_order.len() < rows.len() {
        let input = (0..rows.len())
            .filter(|input| !inserted[*input])
            .filter_map(|input| {
                graph
                    .edges
                    .iter()
                    .filter(|edge| edge_connects(edge, input, &inserted))
                    .map(|edge| edge.keys.len())
                    .max()
                    .map(|weight| (input, weight))
            })
            .max_by_key(|(input, weight)| {
                (
                    *weight,
                    rows[*input].upper_bound.expect("finite row bound"),
                    rows[*input].central,
                    Reverse(*input),
                )
            })?
            .0;
        let maximum_weight = graph
            .edges
            .iter()
            .filter(|edge| edge_connects(edge, input, &inserted))
            .map(|edge| edge.keys.len())
            .max()?;
        let candidates = graph
            .edges
            .iter()
            .filter(|edge| {
                edge.keys.len() == maximum_weight && edge_connects(edge, input, &inserted)
            })
            .collect::<Vec<_>>();
        let forward_edge = candidates
            .iter()
            .min_by_key(|edge| {
                let parent = connected_parent(edge, input);
                (Reverse(forward_depth[parent]), parent)
            })
            .copied()?;
        let backward_edge = candidates
            .iter()
            .min_by_key(|edge| {
                let parent = connected_parent(edge, input);
                (backward_depth[parent], parent)
            })
            .copied()?;
        let forward_parent = connected_parent(forward_edge, input);
        let backward_parent = connected_parent(backward_edge, input);
        forward_depth[input] = forward_depth[forward_parent].saturating_add(1);
        backward_depth[input] = backward_depth[backward_parent].saturating_add(1);
        forward_edges.push(oriented_edge(forward_edge, input, forward_parent));
        backward_edges.push(oriented_edge(backward_edge, backward_parent, input));
        inserted[input] = true;
        insertion_order.push(input);
    }

    let mut forward_order = (0..rows.len()).collect::<Vec<_>>();
    forward_order.sort_by_key(|input| (Reverse(forward_depth[*input]), *input));
    let mut backward_order = (0..rows.len()).collect::<Vec<_>>();
    backward_order.sort_by_key(|input| (backward_depth[*input], *input));
    let planned_paths = forward_edges.len().saturating_add(backward_edges.len());
    let available_paths = graph.edges.len().saturating_mul(2);
    Some(PredicateTransferSchedule {
        root_input: root,
        forward: PredicateTransferPass {
            order: forward_order,
            edges: forward_edges,
        },
        backward: PredicateTransferPass {
            order: backward_order,
            edges: backward_edges,
        },
        pruned_paths: available_paths.saturating_sub(planned_paths),
    })
}

fn edge_connects(
    edge: &super::acyclic_join::JoinGraphEdge,
    input: usize,
    inserted: &[bool],
) -> bool {
    (edge.left == input && inserted[edge.right]) || (edge.right == input && inserted[edge.left])
}

fn connected_parent(edge: &super::acyclic_join::JoinGraphEdge, input: usize) -> usize {
    if edge.left == input {
        edge.right
    } else {
        edge.left
    }
}

fn oriented_edge(
    edge: &super::acyclic_join::JoinGraphEdge,
    source: usize,
    target: usize,
) -> PredicateTransferEdge {
    let keys = if edge.left == source && edge.right == target {
        edge.keys.clone()
    } else {
        edge.keys
            .iter()
            .map(|key| super::analysis::EquiJoinKey {
                left: key.right.clone(),
                right: key.left.clone(),
            })
            .collect()
    };
    PredicateTransferEdge {
        source,
        target,
        keys,
    }
}

pub(crate) fn estimate_synopsis_rows(
    statistics: &PlannerStats,
    graph: &JoinGraph<'_>,
    schedule: &mut PredicateTransferSchedule,
    input_rows: &[AccessQuantity],
) -> Option<TransferEstimate> {
    let first = simulate_synopsis(statistics, graph, schedule, input_rows)?;
    let removed = first
        .forward_no_effect
        .len()
        .saturating_add(first.backward_no_effect.len());
    if removed == 0 {
        return Some(first.estimate);
    }
    remove_paths(
        &mut schedule.forward,
        &first.forward_no_effect,
        graph.inputs.len(),
    );
    remove_paths(
        &mut schedule.backward,
        &first.backward_no_effect,
        graph.inputs.len(),
    );
    schedule.pruned_paths = schedule.pruned_paths.saturating_add(removed);
    simulate_synopsis(statistics, graph, schedule, input_rows).map(|result| result.estimate)
}

fn remove_paths(pass: &mut PredicateTransferPass, removed: &[usize], input_count: usize) {
    pass.edges = pass
        .edges
        .drain(..)
        .enumerate()
        .filter(|(index, _)| !removed.contains(index))
        .map(|(_, edge)| edge)
        .collect();
    let active = pass
        .edges
        .iter()
        .flat_map(|edge| [edge.source, edge.target])
        .collect::<HashSet<_>>();
    pass.order
        .retain(|input| *input < input_count && active.contains(input));
}

fn simulate_synopsis(
    statistics: &PlannerStats,
    graph: &JoinGraph<'_>,
    schedule: &PredicateTransferSchedule,
    input_rows: &[AccessQuantity],
) -> Option<BoundSimulation> {
    if let Some(result) = simulate_joint_synopsis(statistics, graph, schedule, input_rows) {
        return Some(result);
    }
    if graph.inputs.len() != input_rows.len()
        || input_rows
            .iter()
            .any(|rows| rows.upper_bound != Some(rows.central) || rows.lower_bound != rows.central)
    {
        return None;
    }
    let mut signatures = (0..graph.inputs.len())
        .map(|_| HashSet::<Vec<crate::engine::lir::SlotId>>::new())
        .collect::<Vec<_>>();
    for pass in [&schedule.forward, &schedule.backward] {
        for edge in &pass.edges {
            signatures[edge.source].insert(source_signature(edge));
            signatures[edge.target].insert(target_signature(edge));
        }
    }
    let mut states = graph
        .inputs
        .iter()
        .enumerate()
        .map(|(input, relation)| {
            let domains = signatures[input]
                .iter()
                .map(|signature| {
                    complete_domain(statistics, relation, signature, input_rows[input].central)
                        .map(|domain| (signature.clone(), domain))
                })
                .collect::<Option<HashMap<_, _>>>()?;
            Some(BoundState {
                rows: input_rows[input],
                current_domains: domains.clone(),
                original_domains: domains,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let mut work = AccessQuantity::exact(0);
    let mut filter_bytes = AccessQuantity::exact(0);
    let mut peak_filter_bytes = AccessQuantity::exact(0);
    let forward_no_effect = simulate_synopsis_pass(
        &mut states,
        &schedule.forward,
        &mut work,
        &mut filter_bytes,
        &mut peak_filter_bytes,
    )?;
    let backward_no_effect = simulate_synopsis_pass(
        &mut states,
        &schedule.backward,
        &mut work,
        &mut filter_bytes,
        &mut peak_filter_bytes,
    )?;
    Some(BoundSimulation {
        estimate: TransferEstimate {
            filtered_rows: states.iter().map(|state| state.rows).collect(),
            filter_work: work,
            filter_bytes,
            peak_filter_bytes,
        },
        forward_no_effect,
        backward_no_effect,
    })
}

fn simulate_joint_synopsis(
    statistics: &PlannerStats,
    graph: &JoinGraph<'_>,
    schedule: &PredicateTransferSchedule,
    input_rows: &[AccessQuantity],
) -> Option<BoundSimulation> {
    if graph.inputs.len() != input_rows.len()
        || input_rows
            .iter()
            .any(|rows| rows.upper_bound != Some(rows.central) || rows.lower_bound != rows.central)
    {
        return None;
    }
    let mut slots = (0..graph.inputs.len())
        .map(|_| HashSet::new())
        .collect::<Vec<_>>();
    for pass in [&schedule.forward, &schedule.backward] {
        for edge in &pass.edges {
            slots[edge.source].extend(source_signature(edge));
            slots[edge.target].extend(target_signature(edge));
        }
    }
    let mut rows = graph
        .inputs
        .iter()
        .enumerate()
        .map(|(input, relation)| {
            let mut slots = slots[input].iter().copied().collect::<Vec<_>>();
            slots.sort_unstable();
            complete_weighted_rows(statistics, relation, &slots, input_rows[input].central)
        })
        .collect::<Option<Vec<_>>>()?;
    let mut metrics = WeightedMetrics::default();
    let forward_no_effect = simulate_weighted_pass(&mut rows, &schedule.forward, &mut metrics)?;
    let backward_no_effect = simulate_weighted_pass(&mut rows, &schedule.backward, &mut metrics)?;
    Some(BoundSimulation {
        estimate: TransferEstimate {
            filtered_rows: rows
                .iter()
                .map(|rows| AccessQuantity::exact(weighted_rows(rows)))
                .collect(),
            filter_work: AccessQuantity::exact(
                metrics
                    .rows_scanned
                    .saturating_add(metrics.insertions)
                    .saturating_add(metrics.checks)
                    .saturating_add(metrics.min_max_checks),
            ),
            filter_bytes: AccessQuantity::exact(metrics.filter_bytes),
            peak_filter_bytes: AccessQuantity::exact(metrics.peak_filter_bytes),
        },
        forward_no_effect,
        backward_no_effect,
    })
}

#[derive(Default)]
struct WeightedMetrics {
    rows_scanned: u64,
    insertions: u64,
    checks: u64,
    min_max_checks: u64,
    filter_bytes: u64,
    peak_filter_bytes: u64,
}

fn simulate_weighted_pass(
    rows: &mut [Vec<WeightedSynopsisRow>],
    pass: &PredicateTransferPass,
    metrics: &mut WeightedMetrics,
) -> Option<Vec<usize>> {
    let mut pending = (0..rows.len())
        .map(|_| Vec::<(usize, Arc<SimulatedCascadeFilter>)>::new())
        .collect::<Vec<_>>();
    let mut pass_bytes = 0u64;
    let mut rejections = vec![0u64; pass.edges.len()];
    for &input in &pass.order {
        let incoming = std::mem::take(&mut pending[input]);
        let outgoing_groups = outgoing_edge_groups(pass, input);
        let expected = weighted_rows(&rows[input]);
        let mut outgoing = outgoing_groups
            .iter()
            .map(|edges| {
                (
                    edges.clone(),
                    SimulatedCascadeFilter::new(
                        usize::try_from(expected).unwrap_or(usize::MAX),
                        DEFAULT_BITS_PER_KEY,
                        DEFAULT_HASH_FUNCTIONS,
                        false,
                    ),
                )
            })
            .collect::<Vec<_>>();
        let outgoing_bytes = outgoing
            .iter()
            .map(|(_, filter)| filter.retained_bytes())
            .fold(0u64, u64::saturating_add);
        metrics.filter_bytes = metrics.filter_bytes.saturating_add(outgoing_bytes);
        pass_bytes = pass_bytes.saturating_add(outgoing_bytes);
        metrics.peak_filter_bytes = metrics.peak_filter_bytes.max(pass_bytes);
        let current = std::mem::take(&mut rows[input]);
        let before = weighted_rows(&current);
        metrics.min_max_checks = metrics.min_max_checks.saturating_add(
            before
                .div_ceil(u64::from(DEFAULT_CASCADE_BLOCK_ROWS))
                .saturating_mul(incoming.len() as u64),
        );
        let mut kept = Vec::with_capacity(current.len());
        for row in current {
            metrics.rows_scanned = metrics.rows_scanned.saturating_add(row.frequency);
            let mut keep = true;
            for (edge_index, filter) in &incoming {
                metrics.min_max_checks = metrics.min_max_checks.saturating_add(row.frequency);
                metrics.checks = metrics.checks.saturating_add(row.frequency);
                let key = weighted_key(&row, &target_signature(&pass.edges[*edge_index]))?;
                if !filter.probe(&key).may_contain {
                    rejections[*edge_index] = rejections[*edge_index].saturating_add(row.frequency);
                    keep = false;
                    break;
                }
            }
            if keep {
                for (edge_indices, filter) in &mut outgoing {
                    let key = weighted_key(&row, &source_signature(&pass.edges[edge_indices[0]]))?;
                    filter.insert(&key);
                    metrics.insertions = metrics.insertions.saturating_add(row.frequency);
                }
                kept.push(row);
            }
        }
        let range_bytes = outgoing
            .iter()
            .map(|(_, filter)| filter.range_bytes())
            .fold(0u64, u64::saturating_add);
        metrics.filter_bytes = metrics.filter_bytes.saturating_add(range_bytes);
        pass_bytes = pass_bytes.saturating_add(range_bytes);
        metrics.peak_filter_bytes = metrics.peak_filter_bytes.max(pass_bytes);
        rows[input] = kept;
        for (edge_indices, filter) in outgoing {
            let filter = Arc::new(filter);
            for edge_index in edge_indices {
                pending[pass.edges[edge_index].target].push((edge_index, Arc::clone(&filter)));
            }
        }
    }
    Some(
        rejections
            .iter()
            .enumerate()
            .filter_map(|(edge, rows)| (*rows == 0).then_some(edge))
            .collect(),
    )
}

fn weighted_rows(rows: &[WeightedSynopsisRow]) -> u64 {
    rows.iter()
        .map(|row| row.frequency)
        .fold(0u64, u64::saturating_add)
}

fn weighted_key(
    row: &WeightedSynopsisRow,
    signature: &[crate::engine::lir::SlotId],
) -> Option<Vec<u8>> {
    Some(encode_synopsis_values(
        signature
            .iter()
            .map(|slot| row.values.get(slot))
            .collect::<Option<Vec<_>>>()?
            .into_iter(),
    ))
}

fn complete_weighted_rows(
    statistics: &PlannerStats,
    relation: &Relation,
    slots: &[crate::engine::lir::SlotId],
    rows: u64,
) -> Option<Vec<WeightedSynopsisRow>> {
    if !matches!(relation.node, RelationNode::Scan { .. }) {
        return None;
    }
    if slots.is_empty() {
        return Some(vec![WeightedSynopsisRow {
            values: HashMap::new(),
            frequency: rows,
        }]);
    }
    let scan = super::analysis::underlying_scan(relation)?;
    let table = scan.scan_table();
    let synopsis = statistics.synopsis_models.get(&table.schema_id)?;
    if !valid_synopsis_header(synopsis, table, rows) {
        return None;
    }
    let columns = slots
        .iter()
        .map(|slot| {
            let field = scan
                .output()
                .fields
                .iter()
                .find(|field| field.slot == *slot)?;
            table.column(&field.name)
        })
        .collect::<Option<Vec<_>>>()?;
    if columns.len() == 1 {
        let column = columns[0];
        let model = synopsis
            .columns
            .iter()
            .find(|candidate| candidate.column == column.schema_id)?;
        if model.value_generation != column.value_generation.get()
            || model.null_count != 0
            || !model.distinct_is_exact
            || model.distinct as usize != model.most_common_values.len()
            || model
                .most_common_values
                .iter()
                .any(|value| value.maximum_error != 0)
        {
            return None;
        }
        let result = model
            .most_common_values
            .iter()
            .map(|value| WeightedSynopsisRow {
                values: HashMap::from([(slots[0], value.value.clone())]),
                frequency: value.frequency,
            })
            .collect::<Vec<_>>();
        return (weighted_rows(&result) == rows).then_some(result);
    }
    let requested = columns
        .iter()
        .map(|column| column.schema_id)
        .collect::<Vec<_>>();
    let group = synopsis.column_groups.iter().find(|group| {
        group.columns.len() == requested.len()
            && requested
                .iter()
                .all(|column| group.columns.contains(column))
    })?;
    if group.value_generations.len() != group.columns.len()
        || !group
            .columns
            .iter()
            .zip(&group.value_generations)
            .all(|(column, generation)| {
                table
                    .columns
                    .iter()
                    .find(|candidate| candidate.schema_id == *column)
                    .is_some_and(|column| column.value_generation.get() == *generation)
            })
        || !super::models::declares_column_group(table, &group.columns)
        || group.null_count != 0
        || !group.distinct_is_exact
        || group.distinct as usize != group.most_common_values.len()
        || group
            .most_common_values
            .iter()
            .any(|value| value.maximum_error != 0 || value.values.len() != group.columns.len())
    {
        return None;
    }
    let positions = requested
        .iter()
        .map(|column| {
            group
                .columns
                .iter()
                .position(|candidate| candidate == column)
        })
        .collect::<Option<Vec<_>>>()?;
    let result = group
        .most_common_values
        .iter()
        .map(|value| WeightedSynopsisRow {
            values: slots
                .iter()
                .zip(&positions)
                .map(|(slot, position)| (*slot, value.values[*position].clone()))
                .collect(),
            frequency: value.frequency,
        })
        .collect::<Vec<_>>();
    (weighted_rows(&result) == rows).then_some(result)
}

fn simulate_synopsis_pass(
    states: &mut [BoundState],
    pass: &PredicateTransferPass,
    work: &mut AccessQuantity,
    filter_bytes: &mut AccessQuantity,
    peak_filter_bytes: &mut AccessQuantity,
) -> Option<Vec<usize>> {
    let mut pending = (0..states.len())
        .map(|_| Vec::<(usize, PendingBoundFilter)>::new())
        .collect::<Vec<_>>();
    let mut no_effect = Vec::new();
    let mut pass_bytes = AccessQuantity::exact(0);
    for &input in &pass.order {
        let incoming = std::mem::take(&mut pending[input]);
        let before = states[input].rows;
        *work = quantity_add(*work, before);
        if !incoming.is_empty() {
            *work = quantity_add(
                *work,
                quantity_scale(before, (incoming.len() as u64).saturating_mul(3)),
            );
        }
        for (edge_index, incoming_filter) in incoming {
            let before_filter = states[input].rows;
            if before_filter.upper_bound == Some(0) {
                no_effect.push(edge_index);
                continue;
            }
            let Some(filter) = incoming_filter.filter else {
                states[input].current_domains.clear();
                continue;
            };
            let original = states[input]
                .original_domains
                .get(&incoming_filter.target_signature)
                .cloned()?;
            let retained = retained_domain(&original, &filter);
            let retained_rows = domain_rows(&retained);
            let exact_current = states[input]
                .current_domains
                .get(&incoming_filter.target_signature)
                .cloned();
            if let Some(current) = exact_current {
                let retained = retained_domain(&current, &filter);
                let exact_rows = domain_rows(&retained);
                if before_filter.upper_bound == Some(before_filter.lower_bound)
                    && exact_rows == before_filter.central
                {
                    no_effect.push(edge_index);
                }
                let reduced = exact_rows < before_filter.central;
                states[input].rows = AccessQuantity::exact(exact_rows);
                if reduced {
                    states[input].current_domains.clear();
                }
                states[input]
                    .current_domains
                    .insert(incoming_filter.target_signature, retained);
            } else {
                let upper = before_filter.upper_bound?.min(retained_rows);
                states[input].rows = AccessQuantity {
                    central: upper,
                    lower_bound: 0,
                    upper_bound: Some(upper),
                };
                states[input].current_domains.clear();
            }
        }

        let outgoing = outgoing_edge_groups(pass, input);
        let allocated = outgoing
            .iter()
            .map(|edge_indices| {
                let signature = source_signature(&pass.edges[edge_indices[0]]);
                Some(quantity_add(
                    bloom_bytes(before),
                    AccessQuantity::exact(maximum_range_bytes(&states[input], &signature)?),
                ))
            })
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .fold(AccessQuantity::exact(0), quantity_add);
        pass_bytes = quantity_add(pass_bytes, allocated);
        *filter_bytes = quantity_add(*filter_bytes, allocated);
        let insertions = quantity_scale(states[input].rows, outgoing.len() as u64);
        *work = quantity_add(*work, insertions);
        for edge_indices in outgoing {
            let edge = &pass.edges[edge_indices[0]];
            let signature = source_signature(edge);
            let filter = exact_filter(&states[input], &signature, before.central).map(Arc::new);
            for edge_index in edge_indices {
                let edge = &pass.edges[edge_index];
                pending[edge.target].push((
                    edge_index,
                    PendingBoundFilter {
                        target_signature: target_signature(edge),
                        filter: filter.as_ref().map(Arc::clone),
                    },
                ));
            }
        }
    }
    *peak_filter_bytes = quantity_max(*peak_filter_bytes, pass_bytes);
    Some(no_effect)
}

fn exact_filter(
    state: &BoundState,
    signature: &[crate::engine::lir::SlotId],
    expected: u64,
) -> Option<BloomFilter> {
    if state.rows.upper_bound != Some(state.rows.central)
        || state.rows.lower_bound != state.rows.central
    {
        return None;
    }
    let domain = if state.rows.central == 0 {
        CompleteDomain {
            entries: Vec::new(),
        }
    } else {
        state.current_domains.get(signature)?.clone()
    };
    let mut filter = BloomFilter::new(
        usize::try_from(expected).unwrap_or(usize::MAX),
        DEFAULT_BITS_PER_KEY,
        DEFAULT_HASH_FUNCTIONS,
        false,
    );
    for entry in domain.entries {
        filter.insert(&entry.key);
    }
    Some(filter)
}

fn maximum_range_bytes(
    state: &BoundState,
    signature: &[crate::engine::lir::SlotId],
) -> Option<u64> {
    if state.rows.upper_bound == Some(0) {
        return Some(0);
    }
    state
        .original_domains
        .get(signature)?
        .entries
        .iter()
        .map(|entry| (entry.key.len() as u64).saturating_mul(2))
        .max()
        .or(Some(0))
}

fn retained_domain(domain: &CompleteDomain, filter: &BloomFilter) -> CompleteDomain {
    CompleteDomain {
        entries: domain
            .entries
            .iter()
            .filter(|entry| filter.probe(&entry.key).may_contain)
            .cloned()
            .collect(),
    }
}

fn domain_rows(domain: &CompleteDomain) -> u64 {
    domain
        .entries
        .iter()
        .map(|entry| entry.rows)
        .fold(0u64, u64::saturating_add)
}

fn complete_domain(
    statistics: &PlannerStats,
    relation: &Relation,
    signature: &[crate::engine::lir::SlotId],
    rows: u64,
) -> Option<CompleteDomain> {
    if !matches!(relation.node, RelationNode::Scan { .. }) {
        return None;
    }
    let scan = super::analysis::underlying_scan(relation)?;
    let table = scan.scan_table();
    let synopsis = statistics.synopsis_models.get(&table.schema_id)?;
    if !valid_synopsis_header(synopsis, table, rows) {
        return None;
    }
    let columns = signature
        .iter()
        .map(|slot| {
            let field = scan
                .output()
                .fields
                .iter()
                .find(|field| field.slot == *slot)?;
            table.column(&field.name)
        })
        .collect::<Option<Vec<_>>>()?;
    if columns.len() == 1 {
        let column = columns[0];
        let model = synopsis
            .columns
            .iter()
            .find(|candidate| candidate.column == column.schema_id)?;
        if model.value_generation != column.value_generation.get()
            || !model.distinct_is_exact
            || model.distinct as usize != model.most_common_values.len()
            || model.null_count > rows
            || model
                .most_common_values
                .iter()
                .any(|value| value.maximum_error != 0)
        {
            return None;
        }
        let entries = model
            .most_common_values
            .iter()
            .map(|value| {
                Some(DomainEntry {
                    key: encode_synopsis_values(std::iter::once(&value.value)),
                    rows: value.frequency,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        return valid_domain(entries, rows.saturating_sub(model.null_count));
    }
    let requested = columns
        .iter()
        .map(|column| column.schema_id)
        .collect::<Vec<_>>();
    let group = synopsis.column_groups.iter().find(|group| {
        group.columns.len() == requested.len()
            && requested
                .iter()
                .all(|column| group.columns.contains(column))
    })?;
    if group.value_generations.len() != group.columns.len()
        || !group
            .columns
            .iter()
            .zip(&group.value_generations)
            .all(|(column, generation)| {
                table
                    .columns
                    .iter()
                    .find(|candidate| candidate.schema_id == *column)
                    .is_some_and(|column| column.value_generation.get() == *generation)
            })
        || !super::models::declares_column_group(table, &group.columns)
        || !group.distinct_is_exact
        || group.distinct as usize != group.most_common_values.len()
        || group.null_count > rows
        || group
            .most_common_values
            .iter()
            .any(|value| value.maximum_error != 0 || value.values.len() != group.columns.len())
    {
        return None;
    }
    let positions = requested
        .iter()
        .map(|column| {
            group
                .columns
                .iter()
                .position(|candidate| candidate == column)
        })
        .collect::<Option<Vec<_>>>()?;
    let entries = group
        .most_common_values
        .iter()
        .map(|value| {
            Some(DomainEntry {
                key: encode_synopsis_values(
                    positions.iter().map(|position| &value.values[*position]),
                ),
                rows: value.frequency,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    valid_domain(entries, rows.saturating_sub(group.null_count))
}

fn valid_domain(entries: Vec<DomainEntry>, expected_rows: u64) -> Option<CompleteDomain> {
    let mut keys = HashSet::new();
    let rows = entries
        .iter()
        .map(|entry| entry.rows)
        .fold(0u64, u64::saturating_add);
    (rows == expected_rows && entries.iter().all(|entry| keys.insert(entry.key.clone())))
        .then_some(CompleteDomain { entries })
}

fn valid_synopsis_header(synopsis: &SynopsisModel, table: &Table, rows: u64) -> bool {
    // One new source key can retain a complete target-key group. A hard
    // post-filter bound therefore requires a synopsis with no recorded drift.
    synopsis.table == table.schema_id
        && synopsis.coverage == SynopsisCoverage::Complete
        && synopsis.sample_size == synopsis.observed_rows
        && synopsis.observed_rows == rows
        && synopsis.changes_since_collection == 0
        && synopsis.table_existence_generation == table.existence_generation.get()
}

fn source_signature(edge: &PredicateTransferEdge) -> Vec<crate::engine::lir::SlotId> {
    edge.keys.iter().map(|key| key.left.slot).collect()
}

fn target_signature(edge: &PredicateTransferEdge) -> Vec<crate::engine::lir::SlotId> {
    edge.keys.iter().map(|key| key.right.slot).collect()
}

fn encode_synopsis_values<'a>(values: impl Iterator<Item = &'a SynopsisValue>) -> Vec<u8> {
    let mut output = Vec::new();
    for value in values {
        match value {
            SynopsisValue::Text(value) => {
                output.push(1);
                output.extend_from_slice(&(value.len() as u64).to_be_bytes());
                output.extend_from_slice(value.as_bytes());
            }
            SynopsisValue::Int64(value) => {
                output.push(2);
                output.extend_from_slice(&value.to_be_bytes());
            }
            SynopsisValue::Float64(value) => {
                output.push(3);
                let bits = if *value == 0.0 {
                    0
                } else if value.is_nan() {
                    f64::NAN.to_bits()
                } else {
                    value.to_bits()
                };
                output.extend_from_slice(&bits.to_be_bytes());
            }
            SynopsisValue::Bool(value) => output.extend_from_slice(&[4, u8::from(*value)]),
            SynopsisValue::Bytes(value) => {
                output.push(5);
                output.extend_from_slice(&(value.len() as u64).to_be_bytes());
                output.extend_from_slice(value);
            }
        }
    }
    output
}

fn bloom_bytes(rows: AccessQuantity) -> AccessQuantity {
    let bytes = |rows: u64| {
        rows.saturating_mul(u64::from(DEFAULT_BITS_PER_KEY))
            .div_ceil(u64::from(u64::BITS))
            .saturating_mul(u64::from(u64::BITS / 8))
    };
    AccessQuantity {
        central: bytes(rows.central),
        lower_bound: bytes(rows.lower_bound),
        upper_bound: rows.upper_bound.map(bytes),
    }
}

fn quantity_add(left: AccessQuantity, right: AccessQuantity) -> AccessQuantity {
    AccessQuantity {
        central: left.central.saturating_add(right.central),
        lower_bound: left.lower_bound.saturating_add(right.lower_bound),
        upper_bound: left
            .upper_bound
            .zip(right.upper_bound)
            .map(|(left, right)| left.saturating_add(right)),
    }
}

fn quantity_scale(value: AccessQuantity, factor: u64) -> AccessQuantity {
    AccessQuantity {
        central: value.central.saturating_mul(factor),
        lower_bound: value.lower_bound.saturating_mul(factor),
        upper_bound: value.upper_bound.map(|value| value.saturating_mul(factor)),
    }
}

fn quantity_max(left: AccessQuantity, right: AccessQuantity) -> AccessQuantity {
    AccessQuantity {
        central: left.central.max(right.central),
        lower_bound: left.lower_bound.max(right.lower_bound),
        upper_bound: left
            .upper_bound
            .zip(right.upper_bound)
            .map(|(left, right)| left.max(right)),
    }
}

pub(crate) fn simulate_literal_rows(
    graph: &JoinGraph<'_>,
    schedule: &PredicateTransferSchedule,
    bits_per_key: u8,
    hash_functions: u8,
) -> Option<TransferSimulation> {
    let mut rows = graph
        .inputs
        .iter()
        .map(|input| match &input.node {
            RelationNode::Rows { values, .. } => Some(values.clone()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let mut simulation = TransferSimulation {
        filtered_rows: Vec::new(),
        rows_scanned: 0,
        insertions: 0,
        checks: 0,
        min_max_checks: 0,
        false_positives: 0,
        rows_skipped: 0,
        input_scans: 0,
        repeated_scans: 0,
        filter_bytes: 0,
        peak_filter_bytes: 0,
    };
    let mut scans = vec![0u64; rows.len()];
    simulate_pass(
        &mut rows,
        graph,
        &schedule.forward,
        bits_per_key,
        hash_functions,
        &mut scans,
        &mut simulation,
    );
    simulate_pass(
        &mut rows,
        graph,
        &schedule.backward,
        bits_per_key,
        hash_functions,
        &mut scans,
        &mut simulation,
    );
    simulation.filtered_rows = rows.iter().map(|rows| rows.len() as u64).collect();
    Some(simulation)
}

#[allow(clippy::too_many_arguments)]
fn simulate_pass(
    rows: &mut [Vec<Vec<Value>>],
    graph: &JoinGraph<'_>,
    pass: &PredicateTransferPass,
    bits_per_key: u8,
    hash_functions: u8,
    scans: &mut [u64],
    simulation: &mut TransferSimulation,
) {
    let mut pending = (0..rows.len())
        .map(|_| Vec::<(usize, Arc<SimulatedCascadeFilter>)>::new())
        .collect::<Vec<_>>();
    let mut pass_bytes = 0u64;
    for &input in &pass.order {
        simulation.input_scans = simulation.input_scans.saturating_add(1);
        if scans[input] > 0 {
            simulation.repeated_scans = simulation.repeated_scans.saturating_add(1);
        }
        scans[input] = scans[input].saturating_add(1);
        let incoming = std::mem::take(&mut pending[input]);
        let outgoing_groups = outgoing_edge_groups(pass, input);
        let expected = rows[input].len();
        let mut outgoing = outgoing_groups
            .iter()
            .map(|edges| {
                (
                    edges.clone(),
                    SimulatedCascadeFilter::new(expected, bits_per_key, hash_functions, true),
                )
            })
            .collect::<Vec<_>>();
        let outgoing_bytes = outgoing
            .iter()
            .map(|(_, filter)| filter.retained_bytes())
            .fold(0u64, u64::saturating_add);
        simulation.filter_bytes = simulation.filter_bytes.saturating_add(outgoing_bytes);
        pass_bytes = pass_bytes.saturating_add(outgoing_bytes);
        simulation.peak_filter_bytes = simulation.peak_filter_bytes.max(pass_bytes);

        let current = std::mem::take(&mut rows[input]);
        let before = current.len() as u64;
        simulation.min_max_checks = simulation.min_max_checks.saturating_add(
            before
                .div_ceil(u64::from(DEFAULT_CASCADE_BLOCK_ROWS))
                .saturating_mul(incoming.len() as u64),
        );
        let mut kept = Vec::with_capacity(current.len());
        for row in current {
            simulation.rows_scanned = simulation.rows_scanned.saturating_add(1);
            let mut keep = true;
            for (edge_index, filter) in &incoming {
                simulation.min_max_checks = simulation.min_max_checks.saturating_add(1);
                simulation.checks = simulation.checks.saturating_add(1);
                let edge = &pass.edges[*edge_index];
                let Some(key) = literal_join_key(graph, input, &row, edge, false) else {
                    keep = false;
                    break;
                };
                let probe = filter.probe(&key);
                simulation.false_positives = simulation
                    .false_positives
                    .saturating_add(u64::from(probe.false_positive));
                if !probe.may_contain {
                    keep = false;
                    break;
                }
            }
            if keep {
                for (edge_indices, filter) in &mut outgoing {
                    let edge = &pass.edges[edge_indices[0]];
                    if let Some(key) = literal_join_key(graph, input, &row, edge, true) {
                        filter.insert(&key);
                        simulation.insertions = simulation.insertions.saturating_add(1);
                    }
                }
                kept.push(row);
            }
        }
        simulation.rows_skipped = simulation
            .rows_skipped
            .saturating_add(before.saturating_sub(kept.len() as u64));
        let range_bytes = outgoing
            .iter()
            .map(|(_, filter)| filter.range_bytes())
            .fold(0u64, u64::saturating_add);
        simulation.filter_bytes = simulation.filter_bytes.saturating_add(range_bytes);
        pass_bytes = pass_bytes.saturating_add(range_bytes);
        simulation.peak_filter_bytes = simulation.peak_filter_bytes.max(pass_bytes);
        rows[input] = kept;

        for (edge_indices, filter) in outgoing {
            let filter = Arc::new(filter);
            for edge_index in edge_indices {
                let target = pass.edges[edge_index].target;
                pending[target].push((edge_index, Arc::clone(&filter)));
            }
        }
    }
}

fn literal_join_key(
    graph: &JoinGraph<'_>,
    input: usize,
    row: &[Value],
    edge: &PredicateTransferEdge,
    left: bool,
) -> Option<Vec<u8>> {
    let relation = graph.inputs[input];
    let values = edge.keys.iter().map(|key| {
        let field = if left { &key.left } else { &key.right };
        let column = relation
            .output()
            .fields
            .iter()
            .position(|candidate| candidate.slot == field.slot)?;
        row.get(column)
    });
    encode_values(values)
}

fn encode_values<'a>(values: impl Iterator<Item = Option<&'a Value>>) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    for value in values {
        let value = value?;
        if value.is_null() {
            return None;
        }
        match value {
            Value::Text(value) => {
                output.push(1);
                output.extend_from_slice(&(value.len() as u64).to_be_bytes());
                output.extend_from_slice(value.as_bytes());
            }
            Value::Int64(value) => {
                output.push(2);
                output.extend_from_slice(&value.to_be_bytes());
            }
            Value::Float64(value) => {
                output.push(3);
                let bits = if *value == 0.0 {
                    0
                } else if value.is_nan() {
                    f64::NAN.to_bits()
                } else {
                    value.to_bits()
                };
                output.extend_from_slice(&bits.to_be_bytes());
            }
            Value::Bool(value) => output.extend_from_slice(&[4, u8::from(*value)]),
            Value::Bytes(value) => {
                output.push(5);
                output.extend_from_slice(&(value.as_slice().len() as u64).to_be_bytes());
                output.extend_from_slice(value.as_slice());
            }
            Value::Null(_) => return None,
        }
    }
    Some(output)
}

fn hash_pair(key: &[u8]) -> (u64, u64) {
    let mut first = 0xcbf2_9ce4_8422_2325u64;
    for byte in key {
        first ^= u64::from(*byte);
        first = first.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut second = first ^ (key.len() as u64).rotate_left(32) ^ 0x9e37_79b9_7f4a_7c15;
    second ^= second >> 30;
    second = second.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    second ^= second >> 27;
    second = second.wrapping_mul(0x94d0_49bb_1331_11eb);
    second ^= second >> 31;
    (first, second | 1)
}

#[cfg(test)]
mod tests {
    use crate::engine::lir::{Field, Kind, SlotId, Type};
    use crate::engine::planner::analysis::EquiJoinKey;

    use super::*;

    fn text_field(name: &str, slot: usize) -> Field {
        Field {
            name: name.into(),
            slot: SlotId(slot),
            value_type: Type::scalar(Kind::Text, false),
        }
    }

    fn edge(source_slot: usize, target: usize, target_slot: usize) -> PredicateTransferEdge {
        PredicateTransferEdge {
            source: 0,
            target,
            keys: vec![EquiJoinKey {
                left: text_field("source", source_slot),
                right: text_field("target", target_slot),
            }],
        }
    }

    #[test]
    fn bloom_filter_has_no_false_negative() {
        let mut filter = BloomFilter::new(3, DEFAULT_BITS_PER_KEY, DEFAULT_HASH_FUNCTIONS, true);
        for key in [b"alpha".as_slice(), b"beta", b"gamma"] {
            filter.insert(key);
        }
        for key in [b"alpha".as_slice(), b"beta", b"gamma"] {
            let probe = filter.probe(key);
            assert!(probe.may_contain);
            assert!(!probe.false_positive);
        }
    }

    #[test]
    fn tracked_filter_reports_a_false_positive() {
        let mut filter = BloomFilter::new(1, 1, 1, true);
        filter.insert(b"present");
        let false_positive = (0u64..10_000)
            .map(|candidate| candidate.to_be_bytes())
            .find(|candidate| filter.probe(candidate).false_positive);
        assert!(false_positive.is_some());
    }

    #[test]
    fn storage_shape_counts_one_build_for_a_broadcast_source_key() {
        let schedule = PredicateTransferSchedule {
            root_input: 3,
            forward: PredicateTransferPass {
                order: vec![0, 1, 2, 3],
                edges: vec![edge(0, 1, 1), edge(0, 2, 2), edge(3, 3, 4)],
            },
            backward: PredicateTransferPass {
                order: Vec::new(),
                edges: Vec::new(),
            },
            pruned_paths: 0,
        };

        assert_eq!(filter_storage_shape(&schedule), (2, 1));
    }

    #[test]
    fn cascade_filter_accounts_for_range_endpoints() {
        let mut filter =
            SimulatedCascadeFilter::new(3, DEFAULT_BITS_PER_KEY, DEFAULT_HASH_FUNCTIONS, false);
        filter.insert(b"alpha");
        filter.insert(b"beta");

        assert_eq!(filter.retained_bytes(), 17);
    }
}
