//! Cardinality bounds for inner equality-join graphs.
//!
//! Predicate-conditioned norms follow Deeds et al., "SafeBound: A Practical
//! System for Generating Cardinality Bounds," SIGMOD 2023:
//! <https://doi.org/10.1145/3588907>, and Zhang et al., "LpBound: Pessimistic
//! Cardinality Estimation using l_p-Norms of Degree Sequences," SIGMOD 2025:
//! <https://doi.org/10.1145/3725321>. Conjunctions take the minimum norm bound
//! for each predicate. Ranges sum complete literal-specific norm bounds.
//!
//! Multiway inference uses connected subset dynamic programming over the
//! maximum-degree inequalities described by Zhang et al. It supports acyclic
//! and cyclic graphs. It is not the LpBound linear program.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::engine::catalog::identity::SchemaId;
use crate::engine::catalog::model::{Column, Table};
use crate::engine::lir::bound::{Expr, Relation, RelationNode};
use crate::engine::lir::{BinaryOp, JoinKind, SlotId};

use super::analysis::{ConstValue, Domain, ScanConstraints};
use super::estimator::{
    Estimate, EstimateBoundSource, EstimateInterval, EstimateSource, Estimator,
};
use super::models::{
    DEGREE_SEQUENCE_FORMAT_VERSION, DegreeSequenceNorms,
    PREDICATE_CONDITIONED_DEGREE_FORMAT_VERSION, PlannerStats, PredicateConditionedDegreeSynopsis,
    PredicateConditionedDegreeValue, SynopsisCoverage, SynopsisModel,
};

const MAX_JOIN_GRAPH_INPUTS: usize = 12;

struct GraphInput<'a> {
    relation: &'a Relation,
    table: &'a Table,
    synopsis: &'a SynopsisModel,
    constraints: ScanConstraints,
}

#[derive(Clone, Copy)]
struct GraphEdge {
    left_input: usize,
    left_column: SchemaId,
    right_input: usize,
    right_column: SchemaId,
}

#[derive(Clone, Copy)]
struct ColumnEvidence {
    central_distinct: u64,
    l1_upper: u64,
    l2_upper: u64,
    maximum_degree_upper: u64,
    predicate_row_upper: Option<u64>,
    conditioned: bool,
}

#[derive(Clone, Copy)]
struct PredicateEvidence {
    row_upper: u64,
    l1_upper: u64,
    distinct_upper: u64,
    l2_upper: u64,
    l_infinity_upper: u64,
}

#[derive(Clone, Copy)]
struct InputEvidence {
    central_rows: u64,
    row_upper: u64,
    conditioned: bool,
}

#[derive(Clone, Copy)]
struct BoundState {
    upper: u64,
    conditioned: bool,
    used_degree: bool,
}

pub(super) fn estimate(
    stats: &PlannerStats,
    left: &Relation,
    right: &Relation,
    kind: JoinKind,
    on: &Expr,
) -> Option<Estimate> {
    if kind != JoinKind::Inner {
        return None;
    }
    let mut leaves = Vec::new();
    let mut predicates = Vec::new();
    collect_join_tree(left, &mut leaves, &mut predicates)?;
    collect_join_tree(right, &mut leaves, &mut predicates)?;
    predicates.push(on);
    if !(2..=MAX_JOIN_GRAPH_INPUTS).contains(&leaves.len())
        || (leaves.len() == 2
            && leaves
                .iter()
                .all(|relation| matches!(relation.node, RelationNode::Scan { .. })))
    {
        return None;
    }
    let inputs = leaves
        .into_iter()
        .map(|relation| graph_input(stats, relation))
        .collect::<Option<Vec<_>>>()?;
    let edges = graph_edges(&inputs, &predicates)?;
    if edges.is_empty() {
        return None;
    }

    let estimator = Estimator::new(stats);
    let mut column_evidence = HashMap::new();
    for edge in &edges {
        for (input, column) in [
            (edge.left_input, edge.left_column),
            (edge.right_input, edge.right_column),
        ] {
            column_evidence
                .entry((input, column))
                .or_insert_with(|| evidence_for_column(&inputs[input], column));
        }
    }
    let column_evidence = column_evidence
        .into_iter()
        .map(|(key, evidence)| evidence.map(|evidence| (key, evidence)))
        .collect::<Option<HashMap<_, _>>>()?;

    let input_evidence = inputs
        .iter()
        .enumerate()
        .map(|(index, input)| {
            let estimate = estimator.bound_relation(input.relation);
            let mut row_upper =
                finite_upper(estimate).unwrap_or_else(|| table_row_upper(input.synopsis));
            for ((candidate, _), evidence) in &column_evidence {
                if *candidate == index
                    && let Some(predicate_upper) = evidence.predicate_row_upper
                {
                    row_upper = row_upper.min(predicate_upper);
                }
            }
            Some(InputEvidence {
                central_rows: estimate.cardinality.min(row_upper),
                row_upper,
                conditioned: column_evidence.iter().any(|((candidate, _), evidence)| {
                    *candidate == index
                        && evidence.conditioned
                        && evidence.predicate_row_upper.is_some()
                }),
            })
        })
        .collect::<Option<Vec<_>>>()?;

    let cartesian_upper = input_evidence
        .iter()
        .map(|input| input.row_upper)
        .fold(1u64, u64::saturating_mul);
    let state = connected_degree_bound(&input_evidence, &edges, &column_evidence)?;
    let graph_upper = state.upper.min(cartesian_upper);
    let pair_bound = (inputs.len() == 2)
        .then(|| two_input_norm_bound(&edges, &column_evidence))
        .flatten();
    let pair_selected = pair_bound.is_some_and(|bound| bound.0 < graph_upper);
    let upper = pair_bound.map_or(graph_upper, |bound| graph_upper.min(bound.0));
    let central = factorized_central(&input_evidence, &edges, &column_evidence).min(upper);
    let conditioned = column_evidence
        .values()
        .any(|evidence| evidence.conditioned);
    let upper_source = if pair_selected && pair_bound.is_some_and(|bound| bound.1) {
        EstimateBoundSource::PredicateConditionedDegreeNorms
    } else if pair_selected {
        EstimateBoundSource::DegreeSequenceNorms
    } else if upper == cartesian_upper && !state.used_degree {
        EstimateBoundSource::CartesianProduct
    } else if state.conditioned && inputs.len() > 2 {
        EstimateBoundSource::PredicateConditionedMultiwayDegreeNorms
    } else if state.conditioned {
        EstimateBoundSource::PredicateConditionedDegreeNorms
    } else {
        EstimateBoundSource::MultiwayDegreeNorms
    };
    let changes = inputs.iter().fold(0u64, |total, input| {
        total.saturating_add(input.synopsis.changes_since_collection)
    });
    Some(Estimate {
        cardinality: central,
        interval: EstimateInterval::AttributedRange {
            lower_bound: 0,
            upper_bound: upper,
            lower_source: EstimateBoundSource::Structural,
            central_source: if conditioned {
                EstimateBoundSource::PredicateConditionedDistribution
            } else {
                EstimateBoundSource::FactorizedDistribution
            },
            upper_source,
            drift_widened: changes > 0,
        },
        source: EstimateSource::Join,
        sample_size: inputs
            .iter()
            .map(|input| input.synopsis.sample_size)
            .min()
            .unwrap_or(0),
        changes_since_collection: changes,
        age: inputs
            .iter()
            .map(|input| {
                stats.published_at.saturating_sub(Duration::from_micros(
                    input.synopsis.collected_at_unix_micros,
                ))
            })
            .max()
            .unwrap_or(Duration::ZERO),
    })
}

fn collect_join_tree<'a>(
    relation: &'a Relation,
    leaves: &mut Vec<&'a Relation>,
    predicates: &mut Vec<&'a Expr>,
) -> Option<()> {
    match &relation.node {
        RelationNode::Join {
            left,
            right,
            kind: JoinKind::Inner,
            on,
        } => {
            collect_join_tree(left, leaves, predicates)?;
            collect_join_tree(right, leaves, predicates)?;
            predicates.push(on);
            Some(())
        }
        RelationNode::Scan { .. } | RelationNode::Filter { .. } => {
            base_scan(relation)?;
            leaves.push(relation);
            Some(())
        }
        _ => None,
    }
}

fn base_scan(mut relation: &Relation) -> Option<&Relation> {
    loop {
        match &relation.node {
            RelationNode::Scan { .. } => return Some(relation),
            RelationNode::Filter { input, .. } => relation = input,
            _ => return None,
        }
    }
}

fn graph_input<'a>(stats: &'a PlannerStats, relation: &'a Relation) -> Option<GraphInput<'a>> {
    let scan = base_scan(relation)?;
    let RelationNode::Scan { table, .. } = &scan.node else {
        return None;
    };
    let synopsis = stats.synopsis_models.get(&table.schema_id)?;
    if synopsis.coverage != SynopsisCoverage::Complete
        || synopsis.table_existence_generation != table.existence_generation.get()
    {
        return None;
    }
    Some(GraphInput {
        relation,
        table,
        synopsis,
        constraints: super::analysis::extract_constraints(relation)?,
    })
}

fn graph_edges(inputs: &[GraphInput<'_>], predicates: &[&Expr]) -> Option<Vec<GraphEdge>> {
    let mut edges = Vec::new();
    for predicate in predicates {
        for conjunct in super::analysis::conjuncts(predicate) {
            let Expr::Binary {
                op: BinaryOp::Eq,
                left,
                right,
                ..
            } = conjunct
            else {
                return None;
            };
            let (Expr::SlotRef { slot: left, .. }, Expr::SlotRef { slot: right, .. }) =
                (&**left, &**right)
            else {
                return None;
            };
            let (left_input, left_column) = locate_column(inputs, *left)?;
            let (right_input, right_column) = locate_column(inputs, *right)?;
            if left_input == right_input {
                return None;
            }
            let edge = GraphEdge {
                left_input,
                left_column,
                right_input,
                right_column,
            };
            if !edges.iter().any(|candidate: &GraphEdge| {
                (
                    candidate.left_input,
                    candidate.left_column,
                    candidate.right_input,
                    candidate.right_column,
                ) == (
                    edge.left_input,
                    edge.left_column,
                    edge.right_input,
                    edge.right_column,
                ) || (
                    candidate.left_input,
                    candidate.left_column,
                    candidate.right_input,
                    candidate.right_column,
                ) == (
                    edge.right_input,
                    edge.right_column,
                    edge.left_input,
                    edge.left_column,
                )
            }) {
                edges.push(edge);
            }
        }
    }
    Some(edges)
}

fn locate_column(inputs: &[GraphInput<'_>], slot: SlotId) -> Option<(usize, SchemaId)> {
    inputs.iter().enumerate().find_map(|(index, input)| {
        let field = input
            .relation
            .output()
            .fields
            .iter()
            .find(|field| field.slot == slot)?;
        let column = input.table.column(&field.name)?;
        Some((index, column.schema_id))
    })
}

fn evidence_for_column(input: &GraphInput<'_>, column_id: SchemaId) -> Option<ColumnEvidence> {
    let column = input
        .table
        .columns
        .iter()
        .find(|column| column.schema_id == column_id)?;
    let column_synopsis = input
        .synopsis
        .columns
        .iter()
        .find(|candidate| candidate.column == column_id)?;
    if column_synopsis.value_generation != column.value_generation.get() {
        return None;
    }
    let unconditioned = valid_unconditioned_norms(
        input.synopsis,
        column,
        column_synopsis.degree_sequence.as_ref(),
    );
    let conditioned = predicate_evidence(input, column);
    let changes = input.synopsis.changes_since_collection;
    let table_upper = table_row_upper(input.synopsis);
    let l1_upper = conditioned
        .map(|evidence| evidence.l1_upper.saturating_add(changes))
        .or_else(|| unconditioned.map(|norms| norms.l1.saturating_add(changes)))
        .unwrap_or(table_upper)
        .min(table_upper);
    let l2_upper = conditioned
        .map(|evidence| evidence.l2_upper.saturating_add(changes))
        .or_else(|| unconditioned.map(|norms| norms.l2_upper.saturating_add(changes)))
        .unwrap_or(table_upper)
        .min(table_upper);
    let maximum_degree_upper = conditioned
        .map(|evidence| evidence.l_infinity_upper.saturating_add(changes))
        .or_else(|| unconditioned.map(|norms| norms.l_infinity.saturating_add(changes)))
        .unwrap_or(table_upper)
        .min(table_upper);
    let central_distinct = conditioned
        .map(|evidence| evidence.distinct_upper)
        .unwrap_or(column_synopsis.distinct)
        .min(column_synopsis.distinct)
        .min(table_upper);
    Some(ColumnEvidence {
        central_distinct,
        l1_upper,
        l2_upper,
        maximum_degree_upper,
        predicate_row_upper: conditioned
            .map(|evidence| evidence.row_upper.saturating_add(changes).min(table_upper)),
        conditioned: conditioned.is_some(),
    })
}

fn valid_unconditioned_norms(
    synopsis: &SynopsisModel,
    column: &Column,
    sequence: Option<&super::models::DegreeSequenceSynopsis>,
) -> Option<DegreeSequenceNorms> {
    let sequence = sequence?;
    let norms = sequence.norms;
    (sequence.format_version == DEGREE_SEQUENCE_FORMAT_VERSION
        && sequence.coverage == SynopsisCoverage::Complete
        && sequence.sample_size == synopsis.observed_rows
        && sequence.collected_row_count == synopsis.observed_rows
        && sequence.value_generations == [column.value_generation.get()]
        && sequence.distinct_is_exact
        && norms.exact
        && norms.l1 == sequence.non_null_rows
        && norms.l_infinity <= norms.l2_upper
        && norms.l2_upper <= norms.l1)
        .then_some(norms)
}

fn predicate_evidence(input: &GraphInput<'_>, join_column: &Column) -> Option<PredicateEvidence> {
    let mut result: Option<PredicateEvidence> = None;
    for (predicate_name, domain) in &input.constraints.columns {
        let predicate_column = input.table.column(predicate_name)?;
        let Some(candidate) =
            input
                .synopsis
                .predicate_conditioned_degrees
                .iter()
                .find(|candidate| {
                    candidate.join_columns == [join_column.schema_id]
                        && candidate.predicate_column == predicate_column.schema_id
                })
        else {
            continue;
        };
        if !valid_conditioned_synopsis(input, join_column, predicate_column, candidate) {
            continue;
        }
        let Some(candidate) = conditioned_domain_evidence(candidate, domain) else {
            continue;
        };
        result = Some(match result {
            Some(current) => PredicateEvidence {
                row_upper: current.row_upper.min(candidate.row_upper),
                l1_upper: current.l1_upper.min(candidate.l1_upper),
                distinct_upper: current.distinct_upper.min(candidate.distinct_upper),
                l2_upper: current.l2_upper.min(candidate.l2_upper),
                l_infinity_upper: current.l_infinity_upper.min(candidate.l_infinity_upper),
            },
            None => candidate,
        });
    }
    result
}

fn valid_conditioned_synopsis(
    input: &GraphInput<'_>,
    join_column: &Column,
    predicate_column: &Column,
    conditioned: &PredicateConditionedDegreeSynopsis,
) -> bool {
    let Some(predicate_column_synopsis) = input
        .synopsis
        .columns
        .iter()
        .find(|column| column.column == predicate_column.schema_id)
    else {
        return false;
    };
    if conditioned.format_version != PREDICATE_CONDITIONED_DEGREE_FORMAT_VERSION
        || conditioned.coverage != SynopsisCoverage::Complete
        || conditioned.sample_size != input.synopsis.observed_rows
        || conditioned.join_columns != [join_column.schema_id]
        || conditioned.join_value_generations != [join_column.value_generation.get()]
        || conditioned.predicate_column != predicate_column.schema_id
        || conditioned.predicate_value_generation != predicate_column.value_generation.get()
        || predicate_column_synopsis.value_generation != predicate_column.value_generation.get()
        || !predicate_column_synopsis.distinct_is_exact
        || conditioned.values.len() as u64 != predicate_column_synopsis.distinct
    {
        return false;
    }
    let mut rows = 0u64;
    for (index, value) in conditioned.values.iter().enumerate() {
        let norms = value.norms;
        if value.non_null_join_rows > value.matching_rows
            || value.distinct_join_values > value.non_null_join_rows
            || !norms.exact
            || norms.l1 != value.non_null_join_rows
            || norms.l_infinity > norms.l2_upper
            || norms.l2_upper > norms.l1
            || conditioned.values[index + 1..].iter().any(|candidate| {
                candidate
                    .predicate_value
                    .compare(&value.predicate_value)
                    .is_some_and(|ordering| ordering.is_eq())
            })
        {
            return false;
        }
        rows = rows.saturating_add(value.matching_rows);
    }
    rows == input
        .synopsis
        .observed_rows
        .saturating_sub(predicate_column_synopsis.null_count)
}

fn conditioned_domain_evidence(
    synopsis: &PredicateConditionedDegreeSynopsis,
    domain: &Domain,
) -> Option<PredicateEvidence> {
    let mut selected = Vec::new();
    for value in &synopsis.values {
        if conditioned_value_matches(value, domain)? {
            selected.push(value);
        }
    }
    Some(selected.into_iter().fold(
        PredicateEvidence {
            row_upper: 0,
            l1_upper: 0,
            distinct_upper: 0,
            l2_upper: 0,
            l_infinity_upper: 0,
        },
        |total, value| {
            PredicateEvidence {
                row_upper: total.row_upper.saturating_add(value.matching_rows),
                l1_upper: total.l1_upper.saturating_add(value.non_null_join_rows),
                distinct_upper: total
                    .distinct_upper
                    .saturating_add(value.distinct_join_values),
                l2_upper: total.l2_upper.saturating_add(value.norms.l2_upper),
                l_infinity_upper: total
                    .l_infinity_upper
                    .saturating_add(value.norms.l_infinity),
            }
        },
    ))
}

fn two_input_norm_bound(
    edges: &[GraphEdge],
    evidence: &HashMap<(usize, SchemaId), ColumnEvidence>,
) -> Option<(u64, bool)> {
    edges
        .iter()
        .map(|edge| {
            let left = evidence.get(&(edge.left_input, edge.left_column))?;
            let right = evidence.get(&(edge.right_input, edge.right_column))?;
            Some((
                left.l1_upper
                    .saturating_mul(right.maximum_degree_upper)
                    .min(left.maximum_degree_upper.saturating_mul(right.l1_upper))
                    .min(left.l2_upper.saturating_mul(right.l2_upper)),
                left.conditioned || right.conditioned,
            ))
        })
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .min_by_key(|bound| bound.0)
}

fn conditioned_value_matches(
    value: &PredicateConditionedDegreeValue,
    domain: &Domain,
) -> Option<bool> {
    if let Some(equality) = &domain.equality {
        let ConstValue::Literal(equality) = equality else {
            return None;
        };
        return Some(value.predicate_value.storage_eq(equality));
    }
    let above_lower = if let Some(lower) = &domain.lower {
        let ordering = value.predicate_value.storage_compare(&lower.value)?;
        ordering.is_gt() || (ordering.is_eq() && lower.inclusive)
    } else {
        true
    };
    let below_upper = if let Some(upper) = &domain.upper {
        let ordering = value.predicate_value.storage_compare(&upper.value)?;
        ordering.is_lt() || (ordering.is_eq() && upper.inclusive)
    } else {
        true
    };
    Some(above_lower && below_upper)
}

fn connected_degree_bound(
    inputs: &[InputEvidence],
    edges: &[GraphEdge],
    evidence: &HashMap<(usize, SchemaId), ColumnEvidence>,
) -> Option<BoundState> {
    let state_count = 1usize.checked_shl(inputs.len() as u32)?;
    let mut states = vec![None; state_count];
    for (index, input) in inputs.iter().enumerate() {
        states[1 << index] = Some(BoundState {
            upper: input.row_upper,
            conditioned: input.conditioned,
            used_degree: false,
        });
    }
    for mask in 1..state_count {
        let Some(state) = states[mask] else {
            continue;
        };
        for (input, input_evidence) in inputs.iter().enumerate() {
            if mask & (1 << input) != 0 {
                continue;
            }
            let mut factor = input_evidence.row_upper;
            let mut connected = false;
            let mut factor_conditioned = false;
            let mut factor_uses_degree = false;
            for edge in edges {
                let column = if edge.left_input == input && mask & (1 << edge.right_input) != 0 {
                    Some(edge.left_column)
                } else if edge.right_input == input && mask & (1 << edge.left_input) != 0 {
                    Some(edge.right_column)
                } else {
                    None
                };
                let Some(column) = column else {
                    continue;
                };
                connected = true;
                let column = evidence.get(&(input, column))?;
                if column.maximum_degree_upper < factor {
                    factor = column.maximum_degree_upper;
                    factor_conditioned = column.conditioned;
                    factor_uses_degree = true;
                }
            }
            if !connected {
                continue;
            }
            let candidate = BoundState {
                upper: state.upper.saturating_mul(factor),
                conditioned: state.conditioned || factor_conditioned,
                used_degree: state.used_degree || factor_uses_degree,
            };
            let next = mask | (1 << input);
            if states[next].is_none_or(|current| candidate.upper < current.upper) {
                states[next] = Some(candidate);
            }
        }
    }
    states[state_count - 1]
}

fn factorized_central(
    inputs: &[InputEvidence],
    edges: &[GraphEdge],
    evidence: &HashMap<(usize, SchemaId), ColumnEvidence>,
) -> u64 {
    let mut nodes = Vec::<(usize, SchemaId)>::new();
    for edge in edges {
        for node in [
            (edge.left_input, edge.left_column),
            (edge.right_input, edge.right_column),
        ] {
            if !nodes.contains(&node) {
                nodes.push(node);
            }
        }
    }
    let positions = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (*node, index))
        .collect::<HashMap<_, _>>();
    let mut parent = (0..nodes.len()).collect::<Vec<_>>();
    for edge in edges {
        union(
            &mut parent,
            positions[&(edge.left_input, edge.left_column)],
            positions[&(edge.right_input, edge.right_column)],
        );
    }
    let mut groups = HashMap::<usize, (HashSet<usize>, u64)>::new();
    for (position, node) in nodes.iter().enumerate() {
        let root = find(&mut parent, position);
        let group = groups.entry(root).or_insert_with(|| (HashSet::new(), 0));
        group.0.insert(node.0);
        group.1 = group
            .1
            .max(evidence.get(node).map_or(0, |value| value.central_distinct));
    }
    let mut cardinality = inputs
        .iter()
        .map(|input| input.central_rows)
        .fold(1u64, u64::saturating_mul);
    for (participants, distinct) in groups.into_values() {
        for _ in 1..participants.len() {
            cardinality /= distinct.max(1);
        }
    }
    cardinality
}

fn find(parent: &mut [usize], mut node: usize) -> usize {
    while parent[node] != node {
        parent[node] = parent[parent[node]];
        node = parent[node];
    }
    node
}

fn union(parent: &mut [usize], left: usize, right: usize) {
    let left = find(parent, left);
    let right = find(parent, right);
    if left != right {
        parent[right] = left;
    }
}

fn finite_upper(estimate: Estimate) -> Option<u64> {
    match estimate.interval {
        EstimateInterval::Exact => Some(estimate.cardinality),
        EstimateInterval::Range { upper_bound, .. }
        | EstimateInterval::AttributedRange { upper_bound, .. } => Some(upper_bound),
        EstimateInterval::Confidence { .. }
        | EstimateInterval::LowerBound { .. }
        | EstimateInterval::Unknown => None,
    }
}

fn table_row_upper(synopsis: &SynopsisModel) -> u64 {
    synopsis
        .observed_rows
        .saturating_add(synopsis.changes_since_collection)
}

#[cfg(test)]
mod tests {
    use crate::engine::catalog::identity::{
        DefinitionGeneration, ExistenceGeneration, SchemaId, StorageGeneration, ValueGeneration,
        WriteProtocolGeneration,
    };
    use crate::engine::catalog::model::{Column, ScalarType, Table};
    use crate::engine::lir::bound;
    use crate::engine::lir::{Kind, Type, Value};
    use crate::engine::planner::models::{
        ColumnSynopsis, DEGREE_SEQUENCE_FORMAT_VERSION, DegreeSequenceNorms,
        DegreeSequenceSynopsis, MostCommonValue, PREDICATE_CONDITIONED_DEGREE_FORMAT_VERSION,
        PlannerStats, PredicateConditionedDegreeSynopsis, PredicateConditionedDegreeValue,
        SynopsisCoverage, SynopsisModel, SynopsisValue,
    };

    use super::*;

    fn table(name: &str, schema: u32, column_base: u32) -> Table {
        let columns = [
            ("left_key", ScalarType::Text),
            ("right_key", ScalarType::Text),
            ("class", ScalarType::Text),
        ]
        .into_iter()
        .enumerate()
        .map(|(offset, (column, scalar_type))| Column {
            id: format!("{name}-{column}").into(),
            schema_id: SchemaId::new(column_base + offset as u32).unwrap(),
            name: column.into(),
            value_generation: ValueGeneration::from(offset as u64 + 1),
            scalar_type,
            nullable: false,
            format: String::new(),
            insert_default: None,
            missing_value: None,
        })
        .collect();
        Table {
            id: name.into(),
            schema_id: SchemaId::new(schema).unwrap(),
            name: name.into(),
            definition_generation: DefinitionGeneration::ZERO,
            existence_generation: ExistenceGeneration::from(1),
            write_protocol_generation: WriteProtocolGeneration::ZERO,
            storage_generation: StorageGeneration::INITIAL,
            columns,
            primary_key: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            constraints: Vec::new(),
        }
    }

    fn scan(table: Table, scope: &str, first_slot: usize) -> bound::Relation {
        bound::Relation::scan(
            table,
            scope,
            (first_slot..first_slot + 3).map(SlotId).collect(),
        )
    }

    fn expression(relation: &bound::Relation, name: &str) -> bound::Expr {
        let field = relation.output().lookup(name).unwrap();
        bound::Expr::slot(field.slot, name, Type::scalar(Kind::Text, false))
    }

    fn equality(
        left: &bound::Relation,
        left_name: &str,
        right: &bound::Relation,
        right_name: &str,
    ) -> bound::Expr {
        bound::Expr::binary(
            BinaryOp::Eq,
            expression(left, left_name),
            expression(right, right_name),
        )
    }

    fn degree(
        rows: u64,
        generation: u64,
        distinct: u64,
        l2: u64,
        maximum: u64,
    ) -> DegreeSequenceSynopsis {
        DegreeSequenceSynopsis {
            format_version: DEGREE_SEQUENCE_FORMAT_VERSION,
            coverage: SynopsisCoverage::Complete,
            sample_size: rows,
            value_generations: vec![generation],
            collected_row_count: rows,
            non_null_rows: rows,
            distinct_values: distinct,
            distinct_is_exact: true,
            norms: DegreeSequenceNorms {
                l1: rows,
                l2_upper: l2,
                l_infinity: maximum,
                exact: true,
            },
            segments: Vec::new(),
        }
    }

    fn synopsis(table: &Table, rows: u64, maximum: u64) -> SynopsisModel {
        let columns = table
            .columns
            .iter()
            .map(|column| ColumnSynopsis {
                column: column.schema_id,
                value_generation: column.value_generation.get(),
                null_fraction: 0.0,
                null_count: 0,
                distinct: if column.name == "class" { 2 } else { 100 },
                distinct_is_exact: true,
                average_width: 4,
                maximum_width: Some(4),
                minimum: None,
                maximum: None,
                most_common_values: if column.name == "class" {
                    vec![
                        MostCommonValue {
                            value: SynopsisValue::Text("cold".into()),
                            frequency: rows.saturating_sub(100),
                            maximum_error: 0,
                        },
                        MostCommonValue {
                            value: SynopsisValue::Text("hot".into()),
                            frequency: rows.min(100),
                            maximum_error: 0,
                        },
                    ]
                } else {
                    Vec::new()
                },
                range_distribution: None,
                degree_sequence: (column.name != "class").then(|| {
                    degree(
                        rows,
                        column.value_generation.get(),
                        100,
                        maximum.saturating_mul(10),
                        maximum,
                    )
                }),
            })
            .collect();
        SynopsisModel {
            table: table.schema_id,
            observed_rows: rows,
            coverage: SynopsisCoverage::Complete,
            sample_size: rows,
            changes_since_collection: 0,
            table_existence_generation: table.existence_generation.get(),
            collected_at_unix_micros: 0,
            catalog_version: 1,
            columns,
            column_groups: Vec::new(),
            predicate_conditioned_degrees: Vec::new(),
        }
    }

    fn stats(tables: &[Table], rows: u64, maximum: u64) -> PlannerStats {
        let mut stats = PlannerStats::empty();
        for table in tables {
            stats
                .synopsis_models
                .insert(table.schema_id, synopsis(table, rows, maximum));
        }
        stats
    }

    fn conditioned(table: &Table, join_name: &str) -> PredicateConditionedDegreeSynopsis {
        let join = table.column(join_name).unwrap();
        let predicate = table.column("class").unwrap();
        PredicateConditionedDegreeSynopsis {
            format_version: PREDICATE_CONDITIONED_DEGREE_FORMAT_VERSION,
            coverage: SynopsisCoverage::Complete,
            sample_size: 1_000,
            join_columns: vec![join.schema_id],
            join_value_generations: vec![join.value_generation.get()],
            predicate_column: predicate.schema_id,
            predicate_value_generation: predicate.value_generation.get(),
            values: vec![
                PredicateConditionedDegreeValue {
                    predicate_value: SynopsisValue::Text("cold".into()),
                    matching_rows: 900,
                    non_null_join_rows: 900,
                    distinct_join_values: 1,
                    norms: DegreeSequenceNorms {
                        l1: 900,
                        l2_upper: 900,
                        l_infinity: 900,
                        exact: true,
                    },
                },
                PredicateConditionedDegreeValue {
                    predicate_value: SynopsisValue::Text("hot".into()),
                    matching_rows: 100,
                    non_null_join_rows: 100,
                    distinct_join_values: 100,
                    norms: DegreeSequenceNorms {
                        l1: 100,
                        l2_upper: 10,
                        l_infinity: 1,
                        exact: true,
                    },
                },
            ],
        }
    }

    #[test]
    fn acyclic_three_input_graph_uses_a_connected_degree_bound() {
        let r = table("r", 10, 100);
        let s = table("s", 20, 200);
        let t = table("t", 30, 300);
        let stats = stats(&[r.clone(), s.clone(), t.clone()], 1_000, 10);
        let r_scan = scan(r, "r", 0);
        let s_scan = scan(s, "s", 10);
        let t_scan = scan(t, "t", 20);
        let first_on = equality(&r_scan, "right_key", &s_scan, "left_key");
        let second_on = equality(&s_scan, "right_key", &t_scan, "left_key");
        let first = bound::Relation::join(r_scan, s_scan, JoinKind::Inner, first_on);
        let relation = bound::Relation::join(first, t_scan, JoinKind::Inner, second_on);

        let estimate = Estimator::new(&stats).bound_relation(&relation);
        assert!(matches!(
            estimate.interval,
            EstimateInterval::AttributedRange {
                upper_bound: 100_000,
                upper_source: EstimateBoundSource::MultiwayDegreeNorms,
                ..
            }
        ));
    }

    #[test]
    fn cyclic_three_input_graph_keeps_a_guaranteed_bound() {
        let r = table("r", 10, 100);
        let s = table("s", 20, 200);
        let t = table("t", 30, 300);
        let stats = stats(&[r.clone(), s.clone(), t.clone()], 1_000, 10);
        let r_scan = scan(r, "r", 0);
        let s_scan = scan(s, "s", 10);
        let t_scan = scan(t, "t", 20);
        let first_on = equality(&r_scan, "right_key", &s_scan, "left_key");
        let second_on = bound::Expr::binary(
            BinaryOp::And,
            equality(&s_scan, "right_key", &t_scan, "left_key"),
            equality(&t_scan, "right_key", &r_scan, "left_key"),
        );
        let first = bound::Relation::join(r_scan, s_scan, JoinKind::Inner, first_on);
        let relation = bound::Relation::join(first, t_scan, JoinKind::Inner, second_on);

        let estimate = Estimator::new(&stats).bound_relation(&relation);
        assert!(matches!(
            estimate.interval,
            EstimateInterval::AttributedRange {
                upper_bound: 100_000,
                upper_source: EstimateBoundSource::MultiwayDegreeNorms,
                ..
            }
        ));
    }

    #[test]
    fn conditioned_three_input_graph_identifies_both_bound_sources() {
        let r = table("r", 10, 100);
        let s = table("s", 20, 200);
        let t = table("t", 30, 300);
        let mut stats = stats(&[r.clone(), s.clone(), t.clone()], 1_000, 900);
        for (table, join_names) in [
            (&r, &["right_key"][..]),
            (&s, &["left_key", "right_key"][..]),
            (&t, &["left_key"][..]),
        ] {
            for join_name in join_names {
                stats
                    .synopsis_models
                    .get_mut(&table.schema_id)
                    .unwrap()
                    .predicate_conditioned_degrees
                    .push(conditioned(table, join_name));
            }
        }
        let hot_filter = |scan: bound::Relation| {
            let predicate = bound::Expr::binary(
                BinaryOp::Eq,
                expression(&scan, "class"),
                bound::Expr::literal(Value::Text("hot".into())),
            );
            bound::Relation::filter(scan, predicate)
        };
        let r = hot_filter(scan(r, "r", 0));
        let s = hot_filter(scan(s, "s", 10));
        let t = hot_filter(scan(t, "t", 20));
        let first_on = equality(&r, "right_key", &s, "left_key");
        let second_on = equality(&s, "right_key", &t, "left_key");
        let first = bound::Relation::join(r, s, JoinKind::Inner, first_on);
        let relation = bound::Relation::join(first, t, JoinKind::Inner, second_on);

        let estimate = Estimator::new(&stats).bound_relation(&relation);
        assert!(matches!(
            estimate.interval,
            EstimateInterval::AttributedRange {
                upper_bound: 100,
                upper_source: EstimateBoundSource::PredicateConditionedMultiwayDegreeNorms,
                ..
            }
        ));
    }

    #[test]
    fn equality_filters_use_literal_specific_join_norms() {
        let left = table("left", 10, 100);
        let right = table("right", 20, 200);
        let mut stats = stats(&[left.clone(), right.clone()], 1_000, 900);
        for table in [&left, &right] {
            stats
                .synopsis_models
                .get_mut(&table.schema_id)
                .unwrap()
                .predicate_conditioned_degrees
                .push(conditioned(table, "left_key"));
        }
        let left_scan = scan(left, "left", 0);
        let right_scan = scan(right, "right", 10);
        let left_predicate = bound::Expr::binary(
            BinaryOp::Eq,
            expression(&left_scan, "class"),
            bound::Expr::literal(Value::Text("hot".into())),
        );
        let right_predicate = bound::Expr::binary(
            BinaryOp::Eq,
            expression(&right_scan, "class"),
            bound::Expr::literal(Value::Text("hot".into())),
        );
        let left_filter = bound::Relation::filter(left_scan, left_predicate);
        let right_filter = bound::Relation::filter(right_scan, right_predicate);
        let on = equality(&left_filter, "left_key", &right_filter, "left_key");
        let relation = bound::Relation::join(left_filter, right_filter, JoinKind::Inner, on);

        let estimate = Estimator::new(&stats).bound_relation(&relation);
        assert_eq!(estimate.cardinality, 100);
        assert_eq!(
            estimate.interval,
            EstimateInterval::AttributedRange {
                lower_bound: 0,
                upper_bound: 100,
                lower_source: EstimateBoundSource::Structural,
                central_source: EstimateBoundSource::PredicateConditionedDistribution,
                upper_source: EstimateBoundSource::PredicateConditionedDegreeNorms,
                drift_widened: false,
            }
        );

        for table in [SchemaId::new(10).unwrap(), SchemaId::new(20).unwrap()] {
            stats
                .synopsis_models
                .get_mut(&table)
                .unwrap()
                .predicate_conditioned_degrees[0]
                .predicate_value_generation += 1;
        }
        let estimate = Estimator::new(&stats).bound_relation(&relation);
        assert_ne!(estimate.interval, EstimateInterval::Unknown);
        assert!(!matches!(
            estimate.interval,
            EstimateInterval::AttributedRange {
                upper_source: EstimateBoundSource::PredicateConditionedDegreeNorms,
                ..
            }
        ));
    }

    #[test]
    fn complete_conditioned_ranges_sum_norm_bounds() {
        let synopsis = PredicateConditionedDegreeSynopsis {
            format_version: PREDICATE_CONDITIONED_DEGREE_FORMAT_VERSION,
            coverage: SynopsisCoverage::Complete,
            sample_size: 60,
            join_columns: vec![SchemaId::new(1).unwrap()],
            join_value_generations: vec![1],
            predicate_column: SchemaId::new(2).unwrap(),
            predicate_value_generation: 1,
            values: [10u64, 20, 30]
                .into_iter()
                .enumerate()
                .map(|(index, rows)| PredicateConditionedDegreeValue {
                    predicate_value: SynopsisValue::Int64(index as i64 + 1),
                    matching_rows: rows,
                    non_null_join_rows: rows,
                    distinct_join_values: rows,
                    norms: DegreeSequenceNorms {
                        l1: rows,
                        l2_upper: rows,
                        l_infinity: 1,
                        exact: true,
                    },
                })
                .collect(),
        };
        let domain = Domain {
            equality: None,
            lower: Some(super::super::analysis::RangeBound {
                value: Value::Int64(1),
                inclusive: true,
            }),
            upper: Some(super::super::analysis::RangeBound {
                value: Value::Int64(2),
                inclusive: true,
            }),
        };
        let evidence = conditioned_domain_evidence(&synopsis, &domain).unwrap();
        assert_eq!(evidence.row_upper, 30);
        assert_eq!(evidence.l1_upper, 30);
        assert_eq!(evidence.l2_upper, 30);
        assert_eq!(evidence.l_infinity_upper, 2);
    }
}
