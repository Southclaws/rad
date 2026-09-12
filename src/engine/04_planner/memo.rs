//! Bounded equality saturation for bound LIR.
//!
//! The equivalence-group model and class analysis follow Willsey et al.,
//! "egg: Fast and Extensible Equality Saturation," POPL 2021:
//! <https://doi.org/10.1145/3434304>. The relational properties and checked
//! context follow the open problems described by Hou et al., "Towards
//! Relational Contextual Equality Saturation," 2025:
//! <https://arxiv.org/abs/2507.11897>.
//!
//! This module stores complete bound-LIR expressions. It does not implement
//! compressed e-nodes or congruence rebuilding.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::Serialize;
use smallvec::{SmallVec, smallvec};

use crate::engine::lir::bound::{self, RelationNode};
use crate::engine::lir::fingerprint::{self, Fingerprint};
use crate::engine::lir::{BinaryOp, Cardinality, Kind, SetQuantifier, SlotId, UNBOUNDED, Value};

pub const MEMO_FORMAT: &str = "rad-relational-memo-v2";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoLimits {
    pub max_groups: u32,
    pub max_alternatives_per_group: u32,
    pub max_total_alternatives: u32,
    pub max_rule_applications: u32,
    /// Fixed effort units keep plan selection equal for equal planner inputs.
    pub max_planning_effort: u32,
}

impl Default for MemoLimits {
    fn default() -> Self {
        Self {
            max_groups: 64,
            max_alternatives_per_group: 16,
            max_total_alternatives: 128,
            max_rule_applications: 256,
            max_planning_effort: 1_024,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoUsage {
    pub groups: u32,
    pub alternatives: u32,
    pub rule_applications: u32,
    pub planning_effort: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoStopReason {
    AlternativeLimit,
    GroupLimit,
    PlanningEffortLimit,
    RuleApplicationLimit,
    TotalAlternativeLimit,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoReport {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub format: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub strategy: String,
    pub limits: MemoLimits,
    pub usage: MemoUsage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<MemoStopReason>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<MemoRootReport>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoRootReport {
    pub name: String,
    pub group: u32,
    pub structural_expression: Fingerprint,
    pub selected_expression: Fingerprint,
    pub properties: RelationalProperties,
    pub directed_alternatives: u32,
    pub saturated_alternatives: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<MemoCandidate>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub proofs: Vec<MemoProof>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub selected_proof: Vec<MemoProof>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub join_search: Option<super::join_search::JoinSearchReport>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct MemoCandidate {
    pub expression: Fingerprint,
    pub origin: MemoCandidateOrigin,
    pub logical_operators: u32,
    pub physical_operators: u32,
    pub blocking_operators: u32,
    pub ordering: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_retained_bytes_upper: Option<u64>,
    pub pareto: bool,
    pub structural: bool,
    pub selected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logical_row_operations: Option<MemoScenarioCost>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum_regret: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tail_regression: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_basis: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejection_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoCandidateOrigin {
    EqualitySaturation,
    JoinGraphSearch,
    Structural,
}

impl MemoCandidateOrigin {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::EqualitySaturation => "equality_saturation",
            Self::JoinGraphSearch => "join_graph_search",
            Self::Structural => "structural",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoScenarioCost {
    pub lower: u64,
    pub central: u64,
    pub upper: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoProof {
    pub from: Fingerprint,
    pub to: Fingerprint,
    pub rule: MemoRule,
    pub preconditions: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoRule {
    DistinctAtMostOne,
    DistinctIdempotence,
    FilterTrueIdentity,
    InnerJoinGraph,
    MergeFilters,
    SliceIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelationalProperties {
    pub row_type: Vec<MemoField>,
    pub keys: Vec<Vec<usize>>,
    pub cardinality: MemoCardinality,
    pub ordering: MemoOrdering,
    pub expression_effects: ExpressionEffects,
    pub correlation_slots: Vec<usize>,
    pub recursive_boundary: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoField {
    pub name: String,
    pub slot: usize,
    pub value_type: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoCardinality {
    pub minimum: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoOrdering {
    pub ordered: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub terms: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct ExpressionEffects {
    pub pure: bool,
    pub deterministic: bool,
    pub total: bool,
    pub lazy_ordered_boundary: bool,
    pub relational_crossing: bool,
}

impl ExpressionEffects {
    const TOTAL: Self = Self {
        pure: true,
        deterministic: true,
        total: true,
        lazy_ordered_boundary: false,
        relational_crossing: false,
    };

    fn combine(self, other: Self) -> Self {
        Self {
            pure: self.pure && other.pure,
            deterministic: self.deterministic && other.deterministic,
            total: self.total && other.total,
            lazy_ordered_boundary: self.lazy_ordered_boundary || other.lazy_ordered_boundary,
            relational_crossing: self.relational_crossing || other.relational_crossing,
        }
    }
}

#[derive(Clone)]
pub(crate) struct MemoAlternative {
    pub relation: bound::Relation,
    pub expression: Fingerprint,
    pub depth: u32,
    pub origin: MemoCandidateOrigin,
}

pub(crate) struct MemoExploration {
    pub root_index: usize,
    pub alternatives: Vec<MemoAlternative>,
}

pub(crate) struct MemoSession {
    report: MemoReport,
}

impl MemoSession {
    pub(crate) fn new(limits: MemoLimits) -> Self {
        Self {
            report: MemoReport {
                format: MEMO_FORMAT.into(),
                strategy: "bounded_relational_search".into(),
                limits,
                ..MemoReport::default()
            },
        }
    }

    pub(crate) fn remaining_planning_effort(&self) -> u32 {
        self.report
            .limits
            .max_planning_effort
            .saturating_sub(self.report.usage.planning_effort)
    }

    #[cfg(test)]
    pub(crate) fn explore(
        &mut self,
        name: impl Into<String>,
        relation: &bound::Relation,
    ) -> MemoExploration {
        self.explore_with_join_search(name, relation, None)
    }

    pub(crate) fn explore_with_join_search(
        &mut self,
        name: impl Into<String>,
        relation: &bound::Relation,
        join_search: Option<super::join_search::JoinSearchResult>,
    ) -> MemoExploration {
        let original = fingerprint::relation_fingerprints(relation).exact;
        let group = self.report.usage.groups;
        if group >= self.report.limits.max_groups {
            self.stop(MemoStopReason::GroupLimit);
            return self.original_only(name.into(), group, relation, original);
        }
        self.report.usage.groups += 1;
        self.report.usage.alternatives += 1;
        let mut alternatives = vec![MemoAlternative {
            relation: relation.clone(),
            expression: original,
            depth: 0,
            origin: MemoCandidateOrigin::Structural,
        }];
        let mut known = HashSet::from([original]);
        let mut queue = VecDeque::from([0usize]);
        let mut proofs = Vec::new();
        let join_search_report = join_search.as_ref().map(|search| search.report.clone());
        if let Some(search) = join_search {
            self.report.usage.planning_effort = self
                .report
                .usage
                .planning_effort
                .saturating_add(search.report.planning_effort)
                .min(self.report.limits.max_planning_effort);
            if search.report.stop_reason.is_some() {
                self.stop(MemoStopReason::PlanningEffortLimit);
            }
            for candidate_relation in search.alternatives {
                if alternatives.len() as u32 >= self.report.limits.max_alternatives_per_group {
                    self.stop(MemoStopReason::AlternativeLimit);
                    break;
                }
                if self.report.usage.alternatives >= self.report.limits.max_total_alternatives {
                    self.stop(MemoStopReason::TotalAlternativeLimit);
                    break;
                }
                if self.report.usage.rule_applications >= self.report.limits.max_rule_applications {
                    self.stop(MemoStopReason::RuleApplicationLimit);
                    break;
                }
                if !admissible_equivalence(relation, &candidate_relation) {
                    continue;
                }
                let expression = fingerprint::relation_fingerprints(&candidate_relation).exact;
                if !known.insert(expression) {
                    continue;
                }
                proofs.push(MemoProof {
                    from: original,
                    to: expression,
                    rule: MemoRule::InnerJoinGraph,
                    preconditions: vec![
                        "connected_equality_graph".into(),
                        "correlation_scope_equal".into(),
                        "deterministic_total_predicates".into(),
                        "inner_joins_only".into(),
                        "ordering_equal".into(),
                        "produced_slots_equal".into(),
                        "recursive_boundary_equal".into(),
                        "row_type_equal".into(),
                    ],
                });
                alternatives.push(MemoAlternative {
                    relation: candidate_relation,
                    expression,
                    depth: 1,
                    origin: MemoCandidateOrigin::JoinGraphSearch,
                });
                self.report.usage.alternatives += 1;
                self.report.usage.rule_applications += 1;
                queue.push_back(alternatives.len() - 1);
            }
        }
        while let Some(index) = queue.pop_front() {
            if alternatives.len() as u32 >= self.report.limits.max_alternatives_per_group {
                self.stop(MemoStopReason::AlternativeLimit);
                break;
            }
            if self.report.usage.alternatives >= self.report.limits.max_total_alternatives {
                self.stop(MemoStopReason::TotalAlternativeLimit);
                break;
            }
            if self.report.usage.rule_applications >= self.report.limits.max_rule_applications {
                self.stop(MemoStopReason::RuleApplicationLimit);
                break;
            }
            if self.report.usage.planning_effort >= self.report.limits.max_planning_effort {
                self.stop(MemoStopReason::PlanningEffortLimit);
                break;
            }
            let remaining = self
                .report
                .limits
                .max_alternatives_per_group
                .saturating_sub(alternatives.len() as u32) as usize;
            let rewrites = rewrite_once(&alternatives[index].relation, remaining);
            for rewrite in rewrites {
                if self.report.usage.rule_applications >= self.report.limits.max_rule_applications {
                    self.stop(MemoStopReason::RuleApplicationLimit);
                    break;
                }
                if self.report.usage.planning_effort >= self.report.limits.max_planning_effort {
                    self.stop(MemoStopReason::PlanningEffortLimit);
                    break;
                }
                self.report.usage.rule_applications += 1;
                self.report.usage.planning_effort += 1;
                let expression = fingerprint::relation_fingerprints(&rewrite.relation).exact;
                let mut preconditions = rewrite.preconditions;
                preconditions.sort();
                preconditions.dedup();
                let proof = MemoProof {
                    from: alternatives[index].expression,
                    to: expression,
                    rule: rewrite.rule,
                    preconditions,
                };
                if !known.insert(expression) {
                    continue;
                }
                proofs.push(proof);
                let next = alternatives.len();
                alternatives.push(MemoAlternative {
                    relation: rewrite.relation,
                    expression,
                    depth: alternatives[index].depth.saturating_add(1),
                    origin: MemoCandidateOrigin::EqualitySaturation,
                });
                self.report.usage.alternatives += 1;
                queue.push_back(next);
                if alternatives.len() as u32 >= self.report.limits.max_alternatives_per_group {
                    self.stop(MemoStopReason::AlternativeLimit);
                    break;
                }
            }
        }
        let directed_alternatives = alternatives
            .iter()
            .filter(|alternative| alternative.depth <= 1)
            .count() as u32;
        let root_index = self.report.roots.len();
        self.report.roots.push(MemoRootReport {
            name: name.into(),
            group,
            structural_expression: original,
            selected_expression: original,
            properties: relational_properties_for_class(relation, &alternatives),
            directed_alternatives,
            saturated_alternatives: alternatives.len() as u32,
            candidates: Vec::new(),
            proofs,
            selected_proof: Vec::new(),
            join_search: join_search_report,
        });
        MemoExploration {
            root_index,
            alternatives,
        }
    }

    fn original_only(
        &mut self,
        name: String,
        group: u32,
        relation: &bound::Relation,
        original: Fingerprint,
    ) -> MemoExploration {
        let root_index = self.report.roots.len();
        self.report.roots.push(MemoRootReport {
            name,
            group,
            structural_expression: original,
            selected_expression: original,
            properties: relational_properties(relation),
            directed_alternatives: 1,
            saturated_alternatives: 1,
            candidates: Vec::new(),
            proofs: Vec::new(),
            selected_proof: Vec::new(),
            join_search: None,
        });
        MemoExploration {
            root_index,
            alternatives: vec![MemoAlternative {
                relation: relation.clone(),
                expression: original,
                depth: 0,
                origin: MemoCandidateOrigin::Structural,
            }],
        }
    }

    pub(crate) fn complete_root(
        &mut self,
        root_index: usize,
        selected: Fingerprint,
        candidates: Vec<MemoCandidate>,
    ) {
        let root = &mut self.report.roots[root_index];
        root.selected_expression = selected;
        root.candidates = candidates;
        root.selected_proof = proof_path(root.structural_expression, selected, &root.proofs);
    }

    fn stop(&mut self, reason: MemoStopReason) {
        if self.report.stop_reason.is_none() {
            self.report.stop_reason = Some(reason);
        }
    }

    pub(crate) fn finish(self) -> MemoReport {
        self.report
    }
}

fn proof_path(from: Fingerprint, to: Fingerprint, proofs: &[MemoProof]) -> Vec<MemoProof> {
    if from == to {
        return Vec::new();
    }
    let mut queue = VecDeque::from([from]);
    let mut previous: HashMap<Fingerprint, MemoProof> = HashMap::new();
    while let Some(current) = queue.pop_front() {
        for proof in proofs.iter().filter(|proof| proof.from == current) {
            if proof.to == from || previous.contains_key(&proof.to) {
                continue;
            }
            previous.insert(proof.to, proof.clone());
            if proof.to == to {
                let mut path = Vec::new();
                let mut cursor = to;
                while cursor != from {
                    let edge = previous[&cursor].clone();
                    cursor = edge.from;
                    path.push(edge);
                }
                path.reverse();
                return path;
            }
            queue.push_back(proof.to);
        }
    }
    Vec::new()
}

struct Rewrite {
    relation: bound::Relation,
    rule: MemoRule,
    preconditions: Vec<String>,
}

fn rewrite_once(relation: &bound::Relation, limit: usize) -> Vec<Rewrite> {
    let mut rewrites = Vec::new();
    collect_rewrites(relation, limit, &mut rewrites);
    rewrites
}

fn collect_rewrites(relation: &bound::Relation, limit: usize, rewrites: &mut Vec<Rewrite>) {
    if rewrites.len() >= limit {
        return;
    }
    for rewrite in local_rewrites(relation) {
        if admissible_equivalence(relation, &rewrite.relation) {
            rewrites.push(with_equivalence_preconditions(rewrite));
        }
        if rewrites.len() >= limit {
            return;
        }
    }
    for (input_index, input) in relation.inputs().into_iter().enumerate() {
        let mut child_rewrites = Vec::new();
        collect_rewrites(
            input,
            limit.saturating_sub(rewrites.len()),
            &mut child_rewrites,
        );
        for child in child_rewrites {
            let Some(rebuilt) = replace_input(relation, input_index, child.relation) else {
                continue;
            };
            if admissible_equivalence(relation, &rebuilt) {
                let mut preconditions = child.preconditions;
                preconditions.push("equivalent_child".into());
                rewrites.push(with_equivalence_preconditions(Rewrite {
                    relation: rebuilt,
                    rule: child.rule,
                    preconditions,
                }));
            }
            if rewrites.len() >= limit {
                return;
            }
        }
    }
}

fn local_rewrites(relation: &bound::Relation) -> Vec<Rewrite> {
    let mut rewrites = Vec::new();
    match &relation.node {
        RelationNode::Filter { input, predicate } => {
            if predicate == &bound::Expr::Literal(Value::Bool(true)) {
                rewrites.push(Rewrite {
                    relation: (**input).clone(),
                    rule: MemoRule::FilterTrueIdentity,
                    preconditions: vec!["predicate_is_true".into(), "predicate_is_total".into()],
                });
            }
            if let RelationNode::Filter {
                input: inner,
                predicate: inner_predicate,
            } = &input.node
                && expression_effects(predicate).total
            {
                rewrites.push(Rewrite {
                    relation: bound::Relation::filter(
                        (**inner).clone(),
                        bound::Expr::binary(
                            BinaryOp::And,
                            inner_predicate.clone(),
                            predicate.clone(),
                        ),
                    ),
                    rule: MemoRule::MergeFilters,
                    preconditions: vec![
                        "outer_predicate_is_total".into(),
                        "left_to_right_short_circuit".into(),
                    ],
                });
            }
        }
        RelationNode::Slice {
            input,
            offset: 0,
            limit: None,
        } => rewrites.push(Rewrite {
            relation: (**input).clone(),
            rule: MemoRule::SliceIdentity,
            preconditions: vec!["zero_offset".into(), "unbounded_limit".into()],
        }),
        RelationNode::Distinct(input) => {
            if let RelationNode::Distinct(inner) = &input.node {
                rewrites.push(Rewrite {
                    relation: bound::Relation::distinct((**inner).clone()),
                    rule: MemoRule::DistinctIdempotence,
                    preconditions: vec![
                        "set_membership_is_idempotent".into(),
                        "input_order_is_not_observable".into(),
                    ],
                });
            }
            if input.cardinality().at_most_one() && !input.is_ordered() {
                rewrites.push(Rewrite {
                    relation: (**input).clone(),
                    rule: MemoRule::DistinctAtMostOne,
                    preconditions: vec![
                        "input_cardinality_at_most_one".into(),
                        "input_is_unordered".into(),
                    ],
                });
            }
        }
        _ => {}
    }
    rewrites
}

fn admissible_equivalence(left: &bound::Relation, right: &bound::Relation) -> bool {
    left.output() == right.output()
        && left.produced() == right.produced()
        && left.free_slots() == right.free_slots()
        && left.is_ordered() == right.is_ordered()
        && contains_recursive_reference(left) == contains_recursive_reference(right)
}

fn with_equivalence_preconditions(mut rewrite: Rewrite) -> Rewrite {
    rewrite.preconditions.extend(
        [
            "correlation_scope_equal",
            "ordering_equal",
            "produced_slots_equal",
            "recursive_boundary_equal",
            "row_type_equal",
        ]
        .into_iter()
        .map(str::to_owned),
    );
    rewrite
}

pub(crate) fn replace_input(
    relation: &bound::Relation,
    input_index: usize,
    replacement: bound::Relation,
) -> Option<bound::Relation> {
    let mut rebuilt = match &relation.node {
        RelationNode::Filter { predicate, .. } if input_index == 0 => {
            bound::Relation::filter(replacement, predicate.clone())
        }
        RelationNode::Project { scope, fields, .. } if input_index == 0 => {
            bound::Relation::project(replacement, scope.clone(), fields.clone())
        }
        RelationNode::Join {
            left,
            right,
            kind,
            on,
        } => match input_index {
            0 => bound::Relation::join(replacement, (**right).clone(), *kind, on.clone()),
            1 => bound::Relation::join((**left).clone(), replacement, *kind, on.clone()),
            _ => return None,
        },
        RelationNode::Concatenate { inputs, scope } if input_index < inputs.len() => {
            let mut inputs = inputs.clone();
            inputs[input_index] = replacement;
            bound::Relation::concatenate(inputs, scope.clone(), relation.output().fields.clone())
        }
        RelationNode::Intersect {
            left,
            right,
            quantifier,
            scope,
        } => match input_index {
            0 => bound::Relation::intersect(
                replacement,
                (**right).clone(),
                *quantifier,
                scope.clone(),
                relation.output().fields.clone(),
            ),
            1 => bound::Relation::intersect(
                (**left).clone(),
                replacement,
                *quantifier,
                scope.clone(),
                relation.output().fields.clone(),
            ),
            _ => return None,
        },
        RelationNode::Except {
            left,
            right,
            quantifier,
            scope,
        } => match input_index {
            0 => bound::Relation::except(
                replacement,
                (**right).clone(),
                *quantifier,
                scope.clone(),
                relation.output().fields.clone(),
            ),
            1 => bound::Relation::except(
                (**left).clone(),
                replacement,
                *quantifier,
                scope.clone(),
                relation.output().fields.clone(),
            ),
            _ => return None,
        },
        RelationNode::Aggregate { groups, terms, .. } if input_index == 0 => {
            bound::Relation::aggregate(replacement, groups.clone(), terms.clone())
        }
        RelationNode::Order { terms, .. } if input_index == 0 => {
            bound::Relation::order(replacement, terms.clone())
        }
        RelationNode::Slice { offset, limit, .. } if input_index == 0 => {
            bound::Relation::slice(replacement, *offset, *limit)
        }
        RelationNode::Distinct(_) if input_index == 0 => bound::Relation::distinct(replacement),
        _ => return None,
    };
    rebuilt.refine_cardinality(relation.cardinality());
    Some(rebuilt)
}

pub(crate) fn logical_operator_count(relation: &bound::Relation) -> u32 {
    relation.inputs().into_iter().fold(1u32, |count, input| {
        count.saturating_add(logical_operator_count(input))
    })
}

fn relational_properties(relation: &bound::Relation) -> RelationalProperties {
    RelationalProperties {
        row_type: relation
            .output()
            .fields
            .iter()
            .map(|field| MemoField {
                name: field.name.clone(),
                slot: field.slot.0,
                value_type: field.value_type.to_string(),
            })
            .collect(),
        keys: relation_keys(relation)
            .into_iter()
            .map(|key| key.into_iter().map(|slot| slot.0).collect())
            .collect(),
        cardinality: memo_cardinality(relation.cardinality()),
        ordering: relation_ordering(relation),
        expression_effects: relation_effects(relation),
        correlation_slots: relation
            .free_slots()
            .slots()
            .into_iter()
            .map(|slot| slot.0)
            .collect(),
        recursive_boundary: contains_recursive_reference(relation),
    }
}

fn relational_properties_for_class(
    original: &bound::Relation,
    alternatives: &[MemoAlternative],
) -> RelationalProperties {
    let mut properties = relational_properties(original);
    for alternative in alternatives.iter().skip(1) {
        let candidate = relational_properties(&alternative.relation);
        properties.cardinality.minimum = properties
            .cardinality
            .minimum
            .max(candidate.cardinality.minimum);
        properties.cardinality.maximum = match (
            properties.cardinality.maximum,
            candidate.cardinality.maximum,
        ) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, right) => left.or(right),
        };
        properties.expression_effects.pure &= candidate.expression_effects.pure;
        properties.expression_effects.deterministic &= candidate.expression_effects.deterministic;
        properties.expression_effects.total &= candidate.expression_effects.total;
        properties.expression_effects.lazy_ordered_boundary |=
            candidate.expression_effects.lazy_ordered_boundary;
        properties.expression_effects.relational_crossing |=
            candidate.expression_effects.relational_crossing;
        for key in candidate.keys {
            if !properties.keys.contains(&key) {
                properties.keys.push(key);
            }
        }
    }
    properties.keys.sort();
    properties
}

fn memo_cardinality(cardinality: Cardinality) -> MemoCardinality {
    MemoCardinality {
        minimum: cardinality.min,
        maximum: (cardinality.max != UNBOUNDED).then_some(cardinality.max),
    }
}

fn relation_ordering(relation: &bound::Relation) -> MemoOrdering {
    let terms = match &relation.node {
        RelationNode::Order { terms, .. } => terms
            .iter()
            .map(|term| {
                format!(
                    "{} {}",
                    crate::engine::lir::format::print_expression(&term.expression),
                    if term.descending { "desc" } else { "asc" }
                )
            })
            .collect(),
        RelationNode::Filter { input, .. }
        | RelationNode::Project { input, .. }
        | RelationNode::Slice { input, .. } => relation_ordering(input).terms,
        _ => Vec::new(),
    };
    MemoOrdering {
        ordered: relation.is_ordered(),
        terms,
    }
}

fn relation_keys(relation: &bound::Relation) -> Vec<Vec<SlotId>> {
    match &relation.node {
        RelationNode::Scan { table, .. } => {
            let primary = table
                .primary_key
                .iter()
                .filter_map(|column| relation.output().lookup(column).map(|field| field.slot))
                .collect::<Vec<_>>();
            (primary.len() == table.primary_key.len())
                .then_some(primary)
                .into_iter()
                .collect()
        }
        RelationNode::Rows { values, .. } if values.len() <= 1 => vec![Vec::new()],
        RelationNode::Filter { input, .. }
        | RelationNode::Order { input, .. }
        | RelationNode::Slice { input, .. } => relation_keys(input),
        RelationNode::Project { input, fields, .. } => relation_keys(input)
            .into_iter()
            .filter_map(|key| {
                key.into_iter()
                    .map(|slot| {
                        fields.iter().find_map(|field| match &field.expression {
                            bound::Expr::SlotRef { slot: source, .. } if *source == slot => {
                                Some(field.slot)
                            }
                            _ => None,
                        })
                    })
                    .collect::<Option<Vec<_>>>()
            })
            .collect(),
        RelationNode::Aggregate { groups, .. } => {
            vec![groups.iter().map(|group| group.slot).collect()]
        }
        RelationNode::Distinct(_)
        | RelationNode::Intersect {
            quantifier: SetQuantifier::Distinct,
            ..
        }
        | RelationNode::Except {
            quantifier: SetQuantifier::Distinct,
            ..
        } => vec![relation.output().slots()],
        _ => Vec::new(),
    }
}

pub(crate) fn relation_effects(relation: &bound::Relation) -> ExpressionEffects {
    let mut effects = ExpressionEffects::TOTAL;
    for input in relation.inputs() {
        effects = effects.combine(relation_effects(input));
    }
    for expression in direct_expressions(relation) {
        effects = effects.combine(expression_effects(expression));
    }
    effects
}

fn direct_expressions(relation: &bound::Relation) -> SmallVec<[&bound::Expr; 4]> {
    match &relation.node {
        RelationNode::Filter { predicate, .. } | RelationNode::Join { on: predicate, .. } => {
            smallvec![predicate]
        }
        RelationNode::Project { fields, .. } => {
            fields.iter().map(|field| &field.expression).collect()
        }
        RelationNode::Aggregate { groups, terms, .. } => groups
            .iter()
            .map(|group| &group.expression)
            .chain(terms.iter().filter_map(|term| term.argument.as_ref()))
            .collect(),
        RelationNode::Order { terms, .. } => terms.iter().map(|term| &term.expression).collect(),
        _ => SmallVec::new(),
    }
}

pub(crate) fn expression_effects(expression: &bound::Expr) -> ExpressionEffects {
    match expression {
        bound::Expr::Literal(_) | bound::Expr::SlotRef { .. } => ExpressionEffects::TOTAL,
        bound::Expr::Unary { op, expression, .. } => {
            let mut effects = expression_effects(expression);
            if matches!(op, crate::engine::lir::UnaryOp::Negate)
                && expression.value_type().kind == Kind::Int64
            {
                effects.total = false;
            }
            effects
        }
        bound::Expr::Binary {
            op, left, right, ..
        } => {
            let mut effects = expression_effects(left).combine(expression_effects(right));
            if matches!(
                op,
                BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div
            ) {
                effects.total = false;
            }
            if matches!(op, BinaryOp::And | BinaryOp::Or) {
                effects.lazy_ordered_boundary = true;
            }
            effects
        }
        bound::Expr::Cast { expression, .. } => {
            let mut effects = expression_effects(expression);
            effects.total = false;
            effects
        }
        bound::Expr::Branch {
            arms, otherwise, ..
        } => {
            let mut effects = expression_effects(otherwise);
            for arm in arms {
                effects = effects
                    .combine(expression_effects(&arm.when))
                    .combine(expression_effects(&arm.then));
            }
            effects.lazy_ordered_boundary = true;
            effects
        }
        bound::Expr::TextMatch { value, .. } => expression_effects(value),
        bound::Expr::Exists(relation)
        | bound::Expr::First { relation, .. }
        | bound::Expr::Scalar { relation, .. }
        | bound::Expr::Array { relation, .. } => {
            let mut effects = relation_effects(relation);
            effects.total = false;
            effects.relational_crossing = true;
            effects
        }
    }
}

pub(crate) fn contains_recursive_reference(relation: &bound::Relation) -> bool {
    contains_reference(relation, |node| {
        matches!(node, RelationNode::RecursiveRef { .. })
    })
}

pub(crate) fn contains_binding_reference(relation: &bound::Relation) -> bool {
    contains_reference(relation, |node| {
        matches!(
            node,
            RelationNode::Ref { .. } | RelationNode::RecursiveRef { .. }
        )
    })
}

fn contains_reference(
    relation: &bound::Relation,
    predicate: impl Copy + Fn(&RelationNode) -> bool,
) -> bool {
    predicate(&relation.node)
        || relation
            .inputs()
            .into_iter()
            .any(|input| contains_reference(input, predicate))
        || direct_expressions(relation).into_iter().any(|expression| {
            let mut found = false;
            crate::engine::lir::inspect::walk_expression(expression, &mut |expression| {
                let nested = match expression {
                    bound::Expr::Exists(relation)
                    | bound::Expr::First { relation, .. }
                    | bound::Expr::Scalar { relation, .. }
                    | bound::Expr::Array { relation, .. } => Some(relation),
                    _ => None,
                };
                found |= nested.is_some_and(|relation| contains_reference(relation, predicate));
            });
            found
        })
}

#[cfg(test)]
mod tests {
    use crate::engine::lir::{BinaryOp, SlotId, Type};
    use crate::engine::planner::test_support::{column, scan};

    use super::*;

    #[test]
    fn saturation_records_a_two_edge_distinct_proof() {
        let relation =
            bound::Relation::distinct(bound::Relation::distinct(bound::Relation::distinct(scan())));
        let mut memo = MemoSession::new(MemoLimits::default());
        let exploration = memo.explore("root", &relation);
        assert_eq!(exploration.alternatives.len(), 3);
        assert_eq!(memo.report.roots[0].directed_alternatives, 2);
        assert_eq!(memo.report.roots[0].saturated_alternatives, 3);
        let selected = exploration.alternatives[2].expression;
        memo.complete_root(exploration.root_index, selected, Vec::new());
        assert_eq!(memo.report.roots[0].selected_proof.len(), 2);
    }

    #[test]
    fn fallible_outer_filter_does_not_merge() {
        let scan = scan();
        let inner = bound::Relation::filter(
            scan.clone(),
            bound::Expr::binary(
                BinaryOp::Eq,
                column(&scan, "status"),
                bound::Expr::literal(Value::Text("open".into())),
            ),
        );
        let divisor = bound::Expr::slot(
            SlotId(10),
            "outer.divisor",
            Type::scalar(Kind::Int64, false),
        );
        let division = bound::Expr::binary(
            BinaryOp::Div,
            bound::Expr::literal(Value::Int64(1)),
            divisor,
        );
        let outer = bound::Relation::filter(
            inner,
            bound::Expr::binary(
                BinaryOp::Eq,
                division,
                bound::Expr::literal(Value::Int64(1)),
            ),
        );
        assert!(
            local_rewrites(&outer)
                .iter()
                .all(|rewrite| rewrite.rule != MemoRule::MergeFilters)
        );
    }

    #[test]
    fn total_filters_merge_with_ordered_boolean_evaluation() {
        let scan = scan();
        let inner = bound::Relation::filter(
            scan.clone(),
            bound::Expr::binary(
                BinaryOp::Eq,
                column(&scan, "status"),
                bound::Expr::literal(Value::Text("open".into())),
            ),
        );
        let outer = bound::Relation::filter(
            inner,
            bound::Expr::binary(
                BinaryOp::Eq,
                column(&scan, "id"),
                bound::Expr::literal(Value::Text("one".into())),
            ),
        );
        let rewrite = local_rewrites(&outer)
            .into_iter()
            .find(|rewrite| rewrite.rule == MemoRule::MergeFilters)
            .expect("total filters must merge");
        assert!(relation_effects(&rewrite.relation).lazy_ordered_boundary);
        assert!(
            rewrite
                .preconditions
                .contains(&"left_to_right_short_circuit".to_owned())
        );
    }

    #[test]
    fn properties_include_primary_keys_correlation_and_effects() {
        let scan = scan();
        let outer = bound::Expr::slot(SlotId(10), "outer.id", Type::scalar(Kind::Text, false));
        let filtered = bound::Relation::filter(
            scan.clone(),
            bound::Expr::binary(BinaryOp::Eq, column(&scan, "id"), outer),
        );
        let properties = relational_properties(&filtered);
        assert_eq!(properties.keys, vec![vec![0]]);
        assert_eq!(properties.correlation_slots, vec![10]);
        assert!(properties.expression_effects.total);
        assert!(!properties.recursive_boundary);
    }

    #[test]
    fn tight_limits_return_the_original_expression() {
        let relation = bound::Relation::distinct(bound::Relation::distinct(scan()));
        let mut memo = MemoSession::new(MemoLimits {
            max_rule_applications: 0,
            ..MemoLimits::default()
        });
        let exploration = memo.explore("root", &relation);
        assert_eq!(exploration.alternatives.len(), 1);
        assert_eq!(
            memo.report.stop_reason,
            Some(MemoStopReason::RuleApplicationLimit)
        );
    }
}
