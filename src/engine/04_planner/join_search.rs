//! Bounded search for connected inner-equijoin trees.
//!
//! Connected-subgraph and complement enumeration follows Moerkotte and
//! Neumann, "Analysis of Two Existing and One New Dynamic Programming
//! Algorithm for the Generation of Optimal Bushy Join Trees," VLDB 2006:
//! <https://www.vldb.org/conf/2006/p930-moerkotte.pdf>. Rad retains a fixed
//! number of states per subset and reports when the planning-effort limit
//! stops enumeration.
//!
//! The search keeps lower, central, and upper logical-work scenarios. This
//! makes cardinality uncertainty part of plan selection. The policy is
//! consistent with the upper-bound-driven robust planning described by
//! Hertzschuch, "Robust Query Optimization for Analytical Database Systems,"
//! 2023: <https://tud.qucosa.de/api/qucosa%3A86763/attachment/ATT-0/>.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use crate::engine::lir::bound::{self, RelationNode};
use crate::engine::lir::fingerprint::{self, Fingerprint};
use crate::engine::lir::{BinaryOp, JoinKind, SlotId};

use super::estimator::{Estimate, EstimateInterval, Estimator};
use super::models::PlannerStats;

pub const MAX_JOIN_SEARCH_INPUTS: usize = 8;
pub const MAX_STATES_PER_SUBSET: usize = 8;
pub const MAX_JOIN_SEARCH_PLANNING_EFFORT: u32 = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinSearchStopReason {
    PlanningEffortLimit,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinSearchReport {
    pub input_count: usize,
    pub edge_count: usize,
    pub max_inputs: usize,
    pub max_states_per_subset: usize,
    pub max_planning_effort: u32,
    pub planning_effort: u32,
    pub considered_partitions: u32,
    pub connected_subsets: u32,
    pub retained_states: u32,
    pub alternatives: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<JoinSearchStopReason>,
}

pub(crate) struct JoinSearchResult {
    pub alternatives: Vec<bound::Relation>,
    pub report: JoinSearchReport,
}

#[derive(Clone)]
struct Edge {
    left: usize,
    right: usize,
    predicate: bound::Expr,
}

struct Graph<'a> {
    relation: &'a bound::Relation,
    leaves: Vec<&'a bound::Relation>,
    edges: Vec<Edge>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ScenarioCost {
    pub lower: u64,
    pub central: u64,
    pub upper: u64,
}

impl ScenarioCost {
    pub(crate) fn add(self, other: Self) -> Self {
        Self {
            lower: self.lower.saturating_add(other.lower),
            central: self.central.saturating_add(other.central),
            upper: self.upper.saturating_add(other.upper),
        }
    }

    pub(crate) fn dominates(self, other: Self) -> bool {
        self.lower <= other.lower
            && self.central <= other.central
            && self.upper <= other.upper
            && self != other
    }
}

#[derive(Clone)]
struct State {
    relation: bound::Relation,
    expression: Fingerprint,
    rows: ScenarioCost,
    work: ScenarioCost,
    peak_build_rows_upper: u64,
}

pub(crate) fn search(
    relation: &bound::Relation,
    statistics: &PlannerStats,
    alternative_limit: usize,
    max_planning_effort: u32,
) -> Option<JoinSearchResult> {
    if alternative_limit == 0 || max_planning_effort == 0 {
        return None;
    }
    search_relation(relation, statistics, alternative_limit, max_planning_effort)
}

fn search_relation(
    relation: &bound::Relation,
    statistics: &PlannerStats,
    alternative_limit: usize,
    max_planning_effort: u32,
) -> Option<JoinSearchResult> {
    if let Some(graph) = extract_graph(relation) {
        return search_graph(graph, statistics, alternative_limit, max_planning_effort);
    }
    for (input_index, input) in relation.inputs().into_iter().enumerate() {
        let Some(child) =
            search_relation(input, statistics, alternative_limit, max_planning_effort)
        else {
            continue;
        };
        let alternatives = child
            .alternatives
            .into_iter()
            .filter_map(|replacement| {
                super::memo::replace_input(relation, input_index, replacement)
            })
            .collect::<Vec<_>>();
        return Some(JoinSearchResult {
            alternatives,
            report: child.report,
        });
    }
    None
}

fn extract_graph(relation: &bound::Relation) -> Option<Graph<'_>> {
    if !matches!(relation.node, RelationNode::Join { .. }) {
        return None;
    }
    let mut leaves = Vec::new();
    let mut predicates = Vec::new();
    flatten_inner_joins(relation, &mut leaves, &mut predicates)?;
    if !(3..=MAX_JOIN_SEARCH_INPUTS).contains(&leaves.len()) {
        return None;
    }
    if leaves.iter().any(|leaf| {
        !leaf.free_slots().is_empty() || super::memo::contains_recursive_reference(leaf) || {
            let effects = super::memo::relation_effects(leaf);
            !effects.pure || !effects.deterministic || !effects.total || effects.relational_crossing
        }
    }) {
        return None;
    }
    let mut owners = HashMap::new();
    for (index, leaf) in leaves.iter().enumerate() {
        for slot in leaf.output().slots() {
            if owners.insert(slot, index).is_some() {
                return None;
            }
        }
    }
    let mut edges = Vec::new();
    for predicate in predicates {
        for conjunct in super::analysis::conjuncts(predicate) {
            let effects = super::memo::expression_effects(conjunct);
            if !effects.pure
                || !effects.deterministic
                || !effects.total
                || effects.lazy_ordered_boundary
                || effects.relational_crossing
            {
                return None;
            }
            let RelationEdge {
                left_slot,
                right_slot,
            } = relation_edge(conjunct)?;
            let left = *owners.get(&left_slot)?;
            let right = *owners.get(&right_slot)?;
            if left == right {
                return None;
            }
            edges.push(Edge {
                left,
                right,
                predicate: conjunct.clone(),
            });
        }
    }
    (!edges.is_empty() && graph_is_connected(leaves.len(), &edges)).then_some(Graph {
        relation,
        leaves,
        edges,
    })
}

fn flatten_inner_joins<'a>(
    relation: &'a bound::Relation,
    leaves: &mut Vec<&'a bound::Relation>,
    predicates: &mut Vec<&'a bound::Expr>,
) -> Option<()> {
    match &relation.node {
        RelationNode::Join {
            left,
            right,
            kind: JoinKind::Inner,
            on,
        } => {
            flatten_inner_joins(left, leaves, predicates)?;
            flatten_inner_joins(right, leaves, predicates)?;
            predicates.push(on);
            Some(())
        }
        RelationNode::Join { .. } => None,
        _ => {
            leaves.push(relation);
            Some(())
        }
    }
}

struct RelationEdge {
    left_slot: SlotId,
    right_slot: SlotId,
}

fn relation_edge(predicate: &bound::Expr) -> Option<RelationEdge> {
    let bound::Expr::Binary {
        op: BinaryOp::Eq,
        left,
        right,
        ..
    } = predicate
    else {
        return None;
    };
    let (
        bound::Expr::SlotRef {
            slot: left_slot, ..
        },
        bound::Expr::SlotRef {
            slot: right_slot, ..
        },
    ) = (&**left, &**right)
    else {
        return None;
    };
    Some(RelationEdge {
        left_slot: *left_slot,
        right_slot: *right_slot,
    })
}

fn graph_is_connected(input_count: usize, edges: &[Edge]) -> bool {
    let mut visited = vec![false; input_count];
    let mut stack = vec![0usize];
    visited[0] = true;
    while let Some(input) = stack.pop() {
        for edge in edges {
            let next = if edge.left == input {
                Some(edge.right)
            } else if edge.right == input {
                Some(edge.left)
            } else {
                None
            };
            if let Some(next) = next
                && !visited[next]
            {
                visited[next] = true;
                stack.push(next);
            }
        }
    }
    visited.into_iter().all(|visited| visited)
}

fn search_graph(
    graph: Graph<'_>,
    statistics: &PlannerStats,
    alternative_limit: usize,
    max_planning_effort: u32,
) -> Option<JoinSearchResult> {
    let estimator = Estimator::new(statistics);
    let subset_count = 1usize << graph.leaves.len();
    let mut states = vec![Vec::<State>::new(); subset_count];
    for (index, leaf) in graph.leaves.iter().enumerate() {
        states[1usize << index].push(State {
            relation: (*leaf).clone(),
            expression: fingerprint::relation_fingerprints(leaf).exact,
            rows: scenario_cardinality(estimator.bound_relation(leaf))?,
            work: ScenarioCost::default(),
            peak_build_rows_upper: 0,
        });
    }
    let mut planning_effort = 0u32;
    let mut considered_partitions = 0u32;
    let mut stop_reason = None;
    'subsets: for subset in 1usize..subset_count {
        if subset.count_ones() < 2 {
            continue;
        }
        let mut left_subset = (subset - 1) & subset;
        while left_subset != 0 {
            let right_subset = subset ^ left_subset;
            if right_subset != 0
                && left_subset < right_subset
                && !states[left_subset].is_empty()
                && !states[right_subset].is_empty()
            {
                let crossing = crossing_edges(&graph.edges, left_subset, right_subset);
                if !crossing.is_empty() {
                    considered_partitions = considered_partitions.saturating_add(1);
                    for left_state in states[left_subset].clone() {
                        for right_state in states[right_subset].clone() {
                            for (left, right) in
                                [(&left_state, &right_state), (&right_state, &left_state)]
                            {
                                if planning_effort >= max_planning_effort {
                                    stop_reason = Some(JoinSearchStopReason::PlanningEffortLimit);
                                    break 'subsets;
                                }
                                planning_effort = planning_effort.saturating_add(1);
                                let predicate = conjoin(&crossing);
                                let relation = bound::Relation::join(
                                    left.relation.clone(),
                                    right.relation.clone(),
                                    JoinKind::Inner,
                                    predicate,
                                );
                                let rows =
                                    scenario_cardinality(estimator.bound_relation(&relation))?;
                                let work = left
                                    .work
                                    .add(right.work)
                                    .add(left.rows)
                                    .add(right.rows)
                                    .add(rows);
                                retain_state(
                                    &mut states[subset],
                                    State {
                                        expression: fingerprint::relation_fingerprints(&relation)
                                            .exact,
                                        relation,
                                        rows,
                                        work,
                                        peak_build_rows_upper: left
                                            .peak_build_rows_upper
                                            .max(right.peak_build_rows_upper)
                                            .max(right.rows.upper),
                                    },
                                );
                            }
                        }
                    }
                }
            }
            left_subset = (left_subset - 1) & subset;
        }
    }
    let full = subset_count - 1;
    let structural = fingerprint::relation_fingerprints(graph.relation).exact;
    let mut known = HashSet::from([structural]);
    let mut alternatives = states[full]
        .iter()
        .filter_map(|state| {
            let relation = normalize_output(state.relation.clone(), graph.relation);
            let expression = fingerprint::relation_fingerprints(&relation).exact;
            known.insert(expression).then_some((state, relation))
        })
        .collect::<Vec<_>>();
    alternatives.sort_by_key(|(state, _)| {
        (
            state.work.upper,
            state.work.central,
            state.peak_build_rows_upper,
            state.work.lower,
        )
    });
    alternatives.truncate(alternative_limit);
    let alternatives = alternatives
        .into_iter()
        .map(|(_, relation)| relation)
        .collect::<Vec<_>>();
    let report = JoinSearchReport {
        input_count: graph.leaves.len(),
        edge_count: graph.edges.len(),
        max_inputs: MAX_JOIN_SEARCH_INPUTS,
        max_states_per_subset: MAX_STATES_PER_SUBSET,
        max_planning_effort,
        planning_effort,
        considered_partitions,
        connected_subsets: states.iter().filter(|states| !states.is_empty()).count() as u32,
        retained_states: states.iter().map(Vec::len).sum::<usize>() as u32,
        alternatives: alternatives.len() as u32,
        stop_reason,
    };
    Some(JoinSearchResult {
        alternatives,
        report,
    })
}

fn scenario_cardinality(estimate: Estimate) -> Option<ScenarioCost> {
    let (lower, upper) = match estimate.interval {
        EstimateInterval::Exact => (estimate.cardinality, estimate.cardinality),
        EstimateInterval::Range {
            lower_bound,
            upper_bound,
        }
        | EstimateInterval::AttributedRange {
            lower_bound,
            upper_bound,
            ..
        } => (lower_bound, upper_bound),
        EstimateInterval::LowerBound { .. }
        | EstimateInterval::Confidence { .. }
        | EstimateInterval::Unknown => return None,
    };
    Some(ScenarioCost {
        lower,
        central: estimate.cardinality.clamp(lower, upper),
        upper,
    })
}

fn crossing_edges(edges: &[Edge], left: usize, right: usize) -> Vec<&Edge> {
    edges
        .iter()
        .filter(|edge| {
            let left_bit = 1usize << edge.left;
            let right_bit = 1usize << edge.right;
            (left & left_bit != 0 && right & right_bit != 0)
                || (left & right_bit != 0 && right & left_bit != 0)
        })
        .collect()
}

fn conjoin(edges: &[&Edge]) -> bound::Expr {
    let mut predicates = edges.iter().map(|edge| edge.predicate.clone());
    let first = predicates.next().expect("a join partition has an edge");
    predicates.fold(first, |predicate, next| {
        bound::Expr::binary(BinaryOp::And, predicate, next)
    })
}

fn retain_state(states: &mut Vec<State>, candidate: State) {
    if states
        .iter()
        .any(|state| state.expression == candidate.expression)
    {
        return;
    }
    if states
        .iter()
        .any(|state| state_dominates(state, &candidate))
    {
        return;
    }
    states.retain(|state| !state_dominates(&candidate, state));
    states.push(candidate);
    states.sort_by_key(|state| {
        (
            state.work.upper,
            state.work.central,
            state.peak_build_rows_upper,
            state.work.lower,
        )
    });
    states.truncate(MAX_STATES_PER_SUBSET);
}

fn state_dominates(left: &State, right: &State) -> bool {
    (left.work.dominates(right.work) && left.peak_build_rows_upper <= right.peak_build_rows_upper)
        || (left.work == right.work && left.peak_build_rows_upper < right.peak_build_rows_upper)
}

fn normalize_output(mut relation: bound::Relation, original: &bound::Relation) -> bound::Relation {
    if relation.output() != original.output() {
        let fields = original
            .output()
            .fields
            .iter()
            .map(|field| bound::ProjectField {
                name: field.name.clone(),
                slot: field.slot,
                expression: bound::Expr::slot(
                    field.slot,
                    field.name.clone(),
                    field.value_type.clone(),
                ),
            })
            .collect();
        relation = bound::Relation::project(relation, "join_search_output", fields);
    }
    relation.refine_cardinality(original.cardinality());
    relation
}
