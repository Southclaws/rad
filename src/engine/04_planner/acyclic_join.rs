//! Classification and rooting for inner-equijoin graphs.
//!
//! The reduction requirement follows Yannakakis, "Algorithms for Acyclic
//! Database Schemes," VLDB 1981:
//! <https://www.vldb.org/dblp/db/conf/vldb/Yannakakis81.html>.
//!
//! The physical tree uses Lookup and Expand lists as described by Bekkers et
//! al., "Shredded Yannakakis: Engineering the Yannakakis Algorithm in Column
//! Stores," PVLDB 2025:
//! <https://www.vldb.org/pvldb/vol18/p2413-vansummeren.pdf>.
//! Rad accepts only binary equality edges here. Thus, graph acyclicity is the
//! tree property of relation instances. General join hypergraphs are not
//! eligible for this operator.

use std::collections::{HashMap, VecDeque};

use crate::engine::lir::bound::{self, RelationNode};
use crate::engine::lir::{BinaryOp, JoinKind, SlotId};

use super::analysis::EquiJoinKey;
use super::physical::{JoinGraphClassification, ShreddedJoinEdge};

pub(crate) struct JoinGraph<'a> {
    pub relation: &'a bound::Relation,
    pub inputs: Vec<&'a bound::Relation>,
    pub edges: Vec<JoinGraphEdge>,
    pub classification: JoinGraphClassification,
}

#[derive(Clone)]
pub(crate) struct JoinGraphEdge {
    pub left: usize,
    pub right: usize,
    pub keys: Vec<EquiJoinKey>,
}

pub(crate) fn classify(relation: &bound::Relation) -> Option<JoinGraph<'_>> {
    if !matches!(relation.node, RelationNode::Join { .. }) {
        return None;
    }
    let mut inputs = Vec::new();
    let mut predicates = Vec::new();
    flatten_inner_joins(relation, &mut inputs, &mut predicates)?;
    if inputs.len() < 3 || inputs.len() > super::join_search::MAX_JOIN_SEARCH_INPUTS {
        return None;
    }
    if inputs.iter().any(|input| {
        !input.free_slots().is_empty() || super::memo::contains_recursive_reference(input) || {
            let effects = super::memo::relation_effects(input);
            !effects.pure || !effects.deterministic || !effects.total || effects.relational_crossing
        }
    }) {
        return None;
    }
    let mut owners = HashMap::new();
    for (input_index, input) in inputs.iter().enumerate() {
        for field in &input.output().fields {
            if owners
                .insert(field.slot, (input_index, field.clone()))
                .is_some()
            {
                return None;
            }
        }
    }
    let mut edges = Vec::<JoinGraphEdge>::new();
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
            let (first_slot, second_slot) = equality_slots(conjunct)?;
            let (first_input, first_field) = owners.get(&first_slot)?;
            let (second_input, second_field) = owners.get(&second_slot)?;
            if first_input == second_input
                || !first_field.value_type.kind.is_scalar()
                || first_field.value_type.kind != second_field.value_type.kind
            {
                return None;
            }
            let (left, right, left_field, right_field) = if first_input < second_input {
                (*first_input, *second_input, first_field, second_field)
            } else {
                (*second_input, *first_input, second_field, first_field)
            };
            let edge = if let Some(edge) = edges
                .iter_mut()
                .find(|edge| edge.left == left && edge.right == right)
            {
                edge
            } else {
                edges.push(JoinGraphEdge {
                    left,
                    right,
                    keys: Vec::new(),
                });
                edges.last_mut().expect("edge inserted")
            };
            if !edge
                .keys
                .iter()
                .any(|key| key.left.slot == left_field.slot && key.right.slot == right_field.slot)
            {
                edge.keys.push(EquiJoinKey {
                    left: left_field.clone(),
                    right: right_field.clone(),
                });
            }
        }
    }
    edges.sort_by_key(|edge| (edge.left, edge.right));
    for edge in &mut edges {
        edge.keys
            .sort_by_key(|key| (key.left.slot.0, key.right.slot.0));
    }
    if edges.is_empty() || !connected(inputs.len(), &edges) {
        return None;
    }
    let classification = if edges.len() == inputs.len() - 1 {
        JoinGraphClassification::Acyclic
    } else {
        JoinGraphClassification::Cyclic
    };
    Some(JoinGraph {
        relation,
        inputs,
        edges,
        classification,
    })
}

impl JoinGraph<'_> {
    pub(crate) fn rooted_edges(&self, root: usize) -> Option<Vec<ShreddedJoinEdge>> {
        if self.classification != JoinGraphClassification::Acyclic || root >= self.inputs.len() {
            return None;
        }
        let mut visited = vec![false; self.inputs.len()];
        let mut pending = VecDeque::from([root]);
        let mut rooted = Vec::with_capacity(self.edges.len());
        visited[root] = true;
        while let Some(parent) = pending.pop_front() {
            for edge in &self.edges {
                let child = if edge.left == parent && !visited[edge.right] {
                    Some(edge.right)
                } else if edge.right == parent && !visited[edge.left] {
                    Some(edge.left)
                } else {
                    None
                };
                let Some(child) = child else {
                    continue;
                };
                let keys = if edge.left == parent {
                    edge.keys.clone()
                } else {
                    edge.keys
                        .iter()
                        .map(|key| EquiJoinKey {
                            left: key.right.clone(),
                            right: key.left.clone(),
                        })
                        .collect()
                };
                visited[child] = true;
                pending.push_back(child);
                rooted.push(ShreddedJoinEdge {
                    parent,
                    child,
                    keys,
                });
            }
        }
        (rooted.len() == self.inputs.len() - 1).then_some(rooted)
    }
}

fn flatten_inner_joins<'a>(
    relation: &'a bound::Relation,
    inputs: &mut Vec<&'a bound::Relation>,
    predicates: &mut Vec<&'a bound::Expr>,
) -> Option<()> {
    match &relation.node {
        RelationNode::Join {
            left,
            right,
            kind: JoinKind::Inner,
            on,
        } => {
            flatten_inner_joins(left, inputs, predicates)?;
            flatten_inner_joins(right, inputs, predicates)?;
            predicates.push(on);
            Some(())
        }
        RelationNode::Join { .. } => None,
        _ => {
            inputs.push(relation);
            Some(())
        }
    }
}

fn equality_slots(expression: &bound::Expr) -> Option<(SlotId, SlotId)> {
    let bound::Expr::Binary {
        op: BinaryOp::Eq,
        left,
        right,
        ..
    } = expression
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
    Some((*left_slot, *right_slot))
}

fn connected(input_count: usize, edges: &[JoinGraphEdge]) -> bool {
    let mut visited = vec![false; input_count];
    let mut pending = vec![0usize];
    visited[0] = true;
    while let Some(input) = pending.pop() {
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
                pending.push(next);
            }
        }
    }
    visited.into_iter().all(|value| value)
}
