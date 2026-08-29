use std::collections::{HashMap, HashSet};

use crate::engine::lir::eval::Env;
use crate::engine::planner::physical::ShreddedJoinEdge;

use super::frames::merge as merge_frames;
use super::observe::JoinOperatorMeasurement;
use super::pipeline::{frame_retained_bytes, join_key};
use super::{Error, ErrorKind, Result};

struct EdgeIndex {
    groups: Vec<Vec<usize>>,
    parent_groups: Vec<Option<usize>>,
}

pub(super) fn execute(
    mut inputs: Vec<Vec<Env>>,
    edges: &[ShreddedJoinEdge],
    root_input: usize,
    memory_limit_bytes: u64,
) -> Result<(Vec<Env>, JoinOperatorMeasurement)> {
    if root_input >= inputs.len() || edges.len() != inputs.len().saturating_sub(1) {
        return Err(Error::message(
            ErrorKind::Internal,
            "exec: invalid shredded join tree",
        ));
    }
    let mut measurement = JoinOperatorMeasurement {
        operator: "ShreddedYannakakisJoin",
        rows_before_reduction: inputs.iter().map(Vec::len).sum::<usize>() as u64,
        ..Default::default()
    };
    let initial_bytes = retained_frame_bytes(&inputs);
    enforce_memory(initial_bytes, memory_limit_bytes)?;
    measurement.peak_retained_bytes = initial_bytes;

    for edge in edges.iter().rev() {
        semijoin_reduce(
            &mut inputs,
            edge.child,
            false,
            edge.parent,
            true,
            edge,
            initial_bytes,
            memory_limit_bytes,
            &mut measurement,
        )?;
    }
    for edge in edges {
        semijoin_reduce(
            &mut inputs,
            edge.parent,
            true,
            edge.child,
            false,
            edge,
            initial_bytes,
            memory_limit_bytes,
            &mut measurement,
        )?;
    }
    measurement.rows_after_reduction = inputs.iter().map(Vec::len).sum::<usize>() as u64;
    measurement.dangling_rows_removed = measurement
        .rows_before_reduction
        .saturating_sub(measurement.rows_after_reduction);

    let reduced_bytes = retained_frame_bytes(&inputs);
    let mut index_bytes = 0u64;
    let mut indexes = Vec::with_capacity(edges.len());
    for edge in edges {
        let (index, bytes) = build_edge_index(&inputs, edge, &mut measurement)?;
        index_bytes = index_bytes.saturating_add(bytes);
        let retained = reduced_bytes.saturating_add(index_bytes);
        enforce_memory(retained, memory_limit_bytes)?;
        measurement.peak_retained_bytes = measurement.peak_retained_bytes.max(retained);
        indexes.push(index);
    }

    let mut children = vec![Vec::<usize>::new(); inputs.len()];
    for (edge_index, edge) in edges.iter().enumerate() {
        children[edge.parent].push(edge_index);
    }
    let mut output = Vec::new();
    for row_index in 0..inputs[root_input].len() {
        output.extend(expand_subtree(
            root_input, row_index, &inputs, edges, &indexes, &children,
        ));
    }
    measurement.expanded_rows = output.len() as u64;
    Ok((output, measurement))
}

#[allow(clippy::too_many_arguments)]
fn semijoin_reduce(
    inputs: &mut [Vec<Env>],
    source: usize,
    source_is_left: bool,
    target: usize,
    target_is_left: bool,
    edge: &ShreddedJoinEdge,
    retained_frame_bytes: u64,
    memory_limit_bytes: u64,
    measurement: &mut JoinOperatorMeasurement,
) -> Result<()> {
    let mut source_keys = HashSet::new();
    let mut key_bytes = 0u64;
    for frame in &inputs[source] {
        measurement.build_rows = measurement.build_rows.saturating_add(1);
        if let Some(key) = join_key(frame, &edge.keys, source_is_left)?
            && source_keys.insert(key.clone())
        {
            key_bytes = key_bytes.saturating_add(key.len() as u64);
        }
    }
    let retained = retained_frame_bytes.saturating_add(key_bytes);
    enforce_memory(retained, memory_limit_bytes)?;
    measurement.peak_retained_bytes = measurement.peak_retained_bytes.max(retained);
    let mut failure = None;
    inputs[target].retain(|frame| {
        measurement.probe_rows = measurement.probe_rows.saturating_add(1);
        let key = match join_key(frame, &edge.keys, target_is_left) {
            Ok(key) => key,
            Err(error) => {
                failure = Some(error);
                return false;
            }
        };
        let Some(key) = key else {
            return false;
        };
        measurement.lookup_requests = measurement.lookup_requests.saturating_add(1);
        let keep = source_keys.contains(&key);
        if keep {
            measurement.key_comparisons = measurement.key_comparisons.saturating_add(1);
        }
        keep
    });
    if let Some(error) = failure {
        return Err(error);
    }
    measurement.reduction_passes = measurement.reduction_passes.saturating_add(1);
    Ok(())
}

fn build_edge_index(
    inputs: &[Vec<Env>],
    edge: &ShreddedJoinEdge,
    measurement: &mut JoinOperatorMeasurement,
) -> Result<(EdgeIndex, u64)> {
    let mut by_key = HashMap::<Vec<u8>, usize>::new();
    let mut groups = Vec::<Vec<usize>>::new();
    let mut retained_bytes = 0u64;
    for (row_index, frame) in inputs[edge.child].iter().enumerate() {
        measurement.build_rows = measurement.build_rows.saturating_add(1);
        let Some(key) = join_key(frame, &edge.keys, false)? else {
            continue;
        };
        let group = if let Some(group) = by_key.get(&key) {
            *group
        } else {
            let group = groups.len();
            retained_bytes = retained_bytes.saturating_add(key.len() as u64);
            by_key.insert(key, group);
            groups.push(Vec::new());
            group
        };
        retained_bytes = retained_bytes.saturating_add(u64::from(u64::BITS / 8));
        groups[group].push(row_index);
    }
    let mut parent_groups = Vec::with_capacity(inputs[edge.parent].len());
    for frame in &inputs[edge.parent] {
        measurement.probe_rows = measurement.probe_rows.saturating_add(1);
        let group = if let Some(key) = join_key(frame, &edge.keys, true)? {
            measurement.lookup_requests = measurement.lookup_requests.saturating_add(1);
            let group = by_key.get(&key).copied();
            if group.is_some() {
                measurement.key_comparisons = measurement.key_comparisons.saturating_add(1);
            }
            group
        } else {
            None
        };
        retained_bytes = retained_bytes.saturating_add(u64::from(u64::BITS / 8));
        parent_groups.push(group);
    }
    Ok((
        EdgeIndex {
            groups,
            parent_groups,
        },
        retained_bytes,
    ))
}

fn expand_subtree(
    input: usize,
    row: usize,
    inputs: &[Vec<Env>],
    edges: &[ShreddedJoinEdge],
    indexes: &[EdgeIndex],
    children: &[Vec<usize>],
) -> Vec<Env> {
    let mut partials = vec![inputs[input][row].clone()];
    for edge_index in &children[input] {
        let edge = &edges[*edge_index];
        let index = &indexes[*edge_index];
        let Some(group) = index.parent_groups[row] else {
            return Vec::new();
        };
        let mut variants = Vec::new();
        for child_row in &index.groups[group] {
            variants.extend(expand_subtree(
                edge.child, *child_row, inputs, edges, indexes, children,
            ));
        }
        let mut combined = Vec::new();
        for partial in &partials {
            for variant in &variants {
                combined.push(merge_frames(partial, variant));
            }
        }
        partials = combined;
    }
    partials
}

fn retained_frame_bytes(inputs: &[Vec<Env>]) -> u64 {
    inputs
        .iter()
        .flatten()
        .map(frame_retained_bytes)
        .fold(0u64, u64::saturating_add)
}

fn enforce_memory(retained_bytes: u64, memory_limit_bytes: u64) -> Result<()> {
    if retained_bytes > memory_limit_bytes {
        return Err(Error::message(
            ErrorKind::Runtime,
            format!("exec: shredded join retained byte limit {memory_limit_bytes} exceeded"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::engine::lir::{Field, Kind, SlotId, Type, Value};
    use crate::engine::planner::analysis::EquiJoinKey;

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

    #[test]
    fn reductions_remove_dangling_rows_and_preserve_bag_multiplicity() {
        let edges = vec![
            ShreddedJoinEdge {
                parent: 0,
                child: 1,
                keys: vec![EquiJoinKey {
                    left: field("r_key", 0),
                    right: field("s_r_key", 1),
                }],
            },
            ShreddedJoinEdge {
                parent: 1,
                child: 2,
                keys: vec![EquiJoinKey {
                    left: field("s_t_key", 2),
                    right: field("t_key", 3),
                }],
            },
        ];
        let inputs = vec![
            vec![row(&[(0, "one")]), row(&[(0, "two")])],
            vec![
                row(&[(1, "one"), (2, "ten")]),
                row(&[(1, "two"), (2, "twenty")]),
                row(&[(1, "three"), (2, "thirty")]),
            ],
            vec![row(&[(3, "ten")]), row(&[(3, "ten")])],
        ];

        let (output, measurement) = execute(inputs, &edges, 0, 1_000_000).unwrap();

        assert_eq!(output.len(), 2);
        assert_eq!(measurement.reduction_passes, 4);
        assert_eq!(measurement.rows_before_reduction, 7);
        assert_eq!(measurement.rows_after_reduction, 4);
        assert_eq!(measurement.dangling_rows_removed, 3);
        assert_eq!(measurement.expanded_rows, 2);
        assert!(measurement.lookup_requests > 0);
        assert!(measurement.peak_retained_bytes > 0);
    }

    #[test]
    fn retained_byte_limit_fails_before_expansion() {
        let edges = vec![ShreddedJoinEdge {
            parent: 0,
            child: 1,
            keys: vec![EquiJoinKey {
                left: field("left", 0),
                right: field("right", 1),
            }],
        }];
        let error = execute(
            vec![vec![row(&[(0, "value")])], vec![row(&[(1, "value")])]],
            &edges,
            0,
            1,
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Runtime);
    }
}
