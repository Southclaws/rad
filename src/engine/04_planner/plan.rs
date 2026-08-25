//! Pure lowering from validated bound LIR to executor-facing operators.

use crate::engine::lir::bound::{self, RelationNode};
use crate::engine::lir::{self, SlotId};

use super::analysis::{self, ConstValue, ScanConstraints};
use super::dependencies::prepare_catalog_dependencies;
use super::join_region::{self, DecisionMetadata, StrategyCandidate};
use super::memo::{MemoCandidate, MemoLimits, MemoScenarioCost, MemoSession};
use super::physical::{
    AccessCandidate, AccessCost, AccessDecision, AccessDecisionBasis, AccessOrdering,
    AccessQuantity, AccessRejectionReason, AccessRowWork, AttachSpec, BindingPlan, BindingPlanKind,
    BindingStrategy, CrossingKind, JoinCandidate, JoinCost, JoinDecision, JoinDecisionBasis,
    JoinGraphClassification, JoinGraphCost, JoinGraphRejectionReason, JoinRejectionReason, Node,
    NodeKind, PhysicalField, Plan, RangeSpec,
};

pub const DEFAULT_HASH_JOIN_MEMORY_LIMIT_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PlannerMode {
    #[default]
    Structural,
    Cost,
}

impl PlannerMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Structural => "structural",
            Self::Cost => "cost",
        }
    }
}

impl std::str::FromStr for PlannerMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "structural" => Ok(Self::Structural),
            "cost" => Ok(Self::Cost),
            _ => Err(format!(
                "unknown planner mode {value:?} (structural or cost)"
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlanOptions {
    /// Force scans while retaining the full residual predicate. This is the
    /// physical-planning conformance oracle used to prove access equivalence.
    pub full_scan_only: bool,
    pub mode: PlannerMode,
    pub hash_join_memory_limit_bytes: u64,
    pub memo_limits: MemoLimits,
}

impl Default for PlanOptions {
    fn default() -> Self {
        Self {
            full_scan_only: false,
            mode: PlannerMode::Structural,
            hash_join_memory_limit_bytes: DEFAULT_HASH_JOIN_MEMORY_LIMIT_BYTES,
            memo_limits: MemoLimits::default(),
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct PlanningContext<'a> {
    pub statistics: Option<&'a super::models::PlannerStats>,
}

pub struct PlannedQuery {
    pub plan: Plan,
    pub estimate: Option<super::estimator::Estimate>,
}

pub fn plan_query_with_context(
    query: &bound::Query,
    options: PlanOptions,
    context: PlanningContext<'_>,
) -> PlannedQuery {
    let estimate = context
        .statistics
        .map(|statistics| super::estimator::for_query(statistics, query));
    let plan = plan_query_inner(query, options, context.statistics);
    PlannedQuery { plan, estimate }
}

pub fn plan_query(query: &bound::Query, options: PlanOptions) -> Plan {
    plan_query_inner(query, options, None)
}

fn plan_query_inner(
    query: &bound::Query,
    options: PlanOptions,
    statistics: Option<&super::models::PlannerStats>,
) -> Plan {
    let mut planner = Planner {
        options,
        next_slot: query.next_slot,
        statistics,
        allow_join_region_strategy: true,
    };
    let mut memo = MemoSession::new(options.memo_limits);
    let bindings = query
        .bindings
        .iter()
        .map(|binding| {
            let anchor = planner.plan_memo_root(
                &mut memo,
                format!("binding:{}:anchor", binding.name),
                &binding.root,
                &[],
            );
            let kind = match (&binding.step, binding.accumulation) {
                (Some(step), Some(accumulation)) if binding.recursive => {
                    BindingPlanKind::Recursive {
                        anchor: Box::new(anchor),
                        step: Box::new(planner.plan_memo_root(
                            &mut memo,
                            format!("binding:{}:step", binding.name),
                            step,
                            &[],
                        )),
                        step_output: step.output().clone(),
                        accumulation,
                    }
                }
                (None, None) if !binding.recursive => BindingPlanKind::Derived {
                    plan: Box::new(anchor),
                    strategy: BindingStrategy::Materialize,
                },
                _ => unreachable!("bound binding has inconsistent recursive state"),
            };
            BindingPlan {
                name: binding.name.clone(),
                output: binding.output.clone(),
                sensitive: binding.plan_sensitive,
                kind,
            }
        })
        .collect::<Vec<_>>();
    let root = planner.plan_memo_root(&mut memo, "root", &query.root, &[]);

    let mut plan = Plan {
        bindings,
        root,
        cardinality: query.cardinality,
        output: query.root.output().clone(),
        dependencies: Default::default(),
        next_slot: planner.next_slot,
        memo: memo.finish(),
    };
    let references = reference_counts(&plan);
    for binding in &mut plan.bindings {
        if let BindingPlanKind::Derived { strategy, .. } = &mut binding.kind {
            *strategy = if references.get(&binding.name) == Some(&1) {
                BindingStrategy::Replay
            } else {
                BindingStrategy::Materialize
            };
        }
    }
    prepare_catalog_dependencies(&mut plan);
    plan
}

fn reference_counts(plan: &Plan) -> std::collections::HashMap<String, usize> {
    let mut references = std::collections::HashMap::new();
    plan.walk(&mut |node| {
        if let NodeKind::Reference { binding, .. } = &node.kind {
            *references.entry(binding.clone()).or_default() += 1;
        }
    });
    references
}

struct Planner<'a> {
    options: PlanOptions,
    next_slot: SlotId,
    statistics: Option<&'a super::models::PlannerStats>,
    allow_join_region_strategy: bool,
}

struct MemoPlannedCandidate {
    expression: crate::engine::lir::fingerprint::Fingerprint,
    origin: super::memo::MemoCandidateOrigin,
    metrics: MemoPhysicalMetrics,
    node: Node,
    next_slot: SlotId,
}

struct PredicateTransferPlanned {
    cost: JoinGraphCost,
    operator_work: AccessQuantity,
    memory: AccessQuantity,
    schedule: super::physical::PredicateTransferSchedule,
    join_plan: Node,
}

struct JoinRegionContext<'relation, 'statistics, 'order> {
    graph: super::acyclic_join::JoinGraph<'relation>,
    statistics: Option<&'statistics super::models::PlannerStats>,
    required_order: &'order [bound::BoundOrderTerm],
    binary: Node,
    binary_cost: Option<JoinGraphCost>,
    inputs: Vec<Node>,
    input_rows: Option<Vec<AccessQuantity>>,
    root_input: usize,
    rooted_edges: Option<Vec<super::physical::ShreddedJoinEdge>>,
    memory_limit_bytes: u64,
}

enum JoinRegionPlan {
    Binary,
    Shredded {
        logical_row_operations: AccessQuantity,
        estimated_peak_retained_bytes: AccessQuantity,
    },
    PredicateTransfer(Box<PredicateTransferPlanned>),
}

#[derive(Clone, Copy)]
struct MemoPhysicalMetrics {
    logical_operators: u32,
    physical_operators: u32,
    blocking_operators: u32,
    ordering_satisfied: bool,
    peak_retained_bytes_upper: Option<u64>,
    logical_row_operations: Option<MemoScenarioCost>,
}

fn memo_physical_metrics(
    relation: &bound::Relation,
    node: &Node,
    required_order: &[bound::BoundOrderTerm],
) -> MemoPhysicalMetrics {
    let mut physical_operators = 0u32;
    let mut blocking_operators = 0u32;
    let mut peak_retained_bytes_upper = Some(0u64);
    let mut logical_row_operations = AccessQuantity::exact(0);
    let mut has_logical_row_operations = false;
    let mut complete_logical_row_operations = true;
    node.walk(&mut |node| {
        if !matches!(node.kind, NodeKind::JoinGraphChoice { .. }) {
            physical_operators = physical_operators.saturating_add(1);
        }
        let row_operations = match &node.kind {
            NodeKind::PrimaryKeyGet { access, .. }
            | NodeKind::TableScan { access, .. }
            | NodeKind::IndexRangeScan { access, .. } => access
                .candidates
                .iter()
                .find(|candidate| candidate.chosen)
                .and_then(|candidate| candidate.cost)
                .map(|cost| cost.logical_row_operations),
            NodeKind::Rows(relation) => {
                let RelationNode::Rows { values, .. } = &relation.node else {
                    unreachable!()
                };
                Some(AccessQuantity::exact(values.len() as u64))
            }
            NodeKind::NestedLoopJoin { decision, .. }
            | NodeKind::HashJoin { decision, .. }
            | NodeKind::IndexedLookupJoin { decision, .. } => decision
                .candidates
                .iter()
                .find(|candidate| candidate.chosen)
                .and_then(|candidate| candidate.cost)
                .map(|cost| cost.logical_row_operations),
            NodeKind::ShreddedYannakakisJoin {
                logical_row_operations,
                ..
            }
            | NodeKind::PredicateTransferJoin {
                logical_row_operations,
                ..
            } => Some(*logical_row_operations),
            _ => None,
        };
        let is_costed_operator = matches!(
            node.kind,
            NodeKind::PrimaryKeyGet { .. }
                | NodeKind::TableScan { .. }
                | NodeKind::IndexRangeScan { .. }
                | NodeKind::Rows(_)
                | NodeKind::NestedLoopJoin { .. }
                | NodeKind::HashJoin { .. }
                | NodeKind::IndexedLookupJoin { .. }
                | NodeKind::ShreddedYannakakisJoin { .. }
                | NodeKind::PredicateTransferJoin { .. }
        );
        if is_costed_operator {
            has_logical_row_operations = true;
            if let Some(row_operations) = row_operations {
                logical_row_operations = quantity_sum(&[logical_row_operations, row_operations]);
            } else {
                complete_logical_row_operations = false;
            }
        }
        match &node.kind {
            NodeKind::HashJoin { decision, .. } => {
                blocking_operators = blocking_operators.saturating_add(1);
                let memory = decision
                    .candidates
                    .iter()
                    .find(|candidate| candidate.chosen)
                    .and_then(|candidate| candidate.cost)
                    .and_then(|cost| cost.peak_retained_bytes)
                    .and_then(|quantity| quantity.upper_bound);
                peak_retained_bytes_upper = peak_retained_bytes_upper
                    .zip(memory)
                    .map(|(total, memory)| total.saturating_add(memory));
            }
            NodeKind::ShreddedYannakakisJoin {
                estimated_peak_retained_bytes,
                ..
            } => {
                blocking_operators = blocking_operators.saturating_add(1);
                peak_retained_bytes_upper = peak_retained_bytes_upper
                    .zip(estimated_peak_retained_bytes.upper_bound)
                    .map(|(total, memory)| total.saturating_add(memory));
            }
            NodeKind::PredicateTransferJoin {
                estimated_peak_retained_bytes,
                ..
            } => {
                blocking_operators = blocking_operators.saturating_add(1);
                peak_retained_bytes_upper = peak_retained_bytes_upper
                    .zip(estimated_peak_retained_bytes.upper_bound)
                    .map(|(total, memory)| total.saturating_add(memory));
            }
            NodeKind::Sort { .. }
            | NodeKind::Distinct { .. }
            | NodeKind::Aggregate { .. }
            | NodeKind::NestedLoopJoin { .. }
            | NodeKind::IndexedLookupJoin { .. } => {
                blocking_operators = blocking_operators.saturating_add(1);
                peak_retained_bytes_upper = None;
            }
            _ => {}
        }
    });
    MemoPhysicalMetrics {
        logical_operators: super::memo::logical_operator_count(relation),
        physical_operators,
        blocking_operators,
        ordering_satisfied: required_order.is_empty()
            || satisfies_order(&node.kind, required_order),
        peak_retained_bytes_upper,
        logical_row_operations: (has_logical_row_operations
            && complete_logical_row_operations
            && logical_row_operations.upper_bound.is_some())
        .then(|| MemoScenarioCost {
            lower: logical_row_operations.lower_bound,
            central: logical_row_operations.central,
            upper: logical_row_operations
                .upper_bound
                .expect("complete logical work has an upper bound"),
        }),
    }
}

fn memo_metrics_dominate(left: &MemoPhysicalMetrics, right: &MemoPhysicalMetrics) -> bool {
    if right.ordering_satisfied && !left.ordering_satisfied {
        return false;
    }
    if let (Some(left_work), Some(right_work)) =
        (left.logical_row_operations, right.logical_row_operations)
    {
        let work_not_worse = left_work.lower <= right_work.lower
            && left_work.central <= right_work.central
            && left_work.upper <= right_work.upper;
        let work_strict = left_work != right_work;
        if work_not_worse && work_strict {
            return memory_not_worse(left, right);
        }
        if left_work != right_work {
            return false;
        }
    }
    if left.logical_operators > right.logical_operators
        || left.physical_operators > right.physical_operators
        || left.blocking_operators > right.blocking_operators
    {
        return false;
    }
    let memory_strict = match (
        left.peak_retained_bytes_upper,
        right.peak_retained_bytes_upper,
    ) {
        (Some(left), Some(right)) if left <= right => left < right,
        (None, None) => false,
        _ => return false,
    };
    left.logical_operators < right.logical_operators
        || left.physical_operators < right.physical_operators
        || left.blocking_operators < right.blocking_operators
        || (left.ordering_satisfied && !right.ordering_satisfied)
        || memory_strict
}

fn memory_not_worse(left: &MemoPhysicalMetrics, right: &MemoPhysicalMetrics) -> bool {
    match (
        left.peak_retained_bytes_upper,
        right.peak_retained_bytes_upper,
    ) {
        (Some(left), Some(right)) => left <= right,
        (None, None) => true,
        _ => false,
    }
}

fn memo_metric_order(metrics: &MemoPhysicalMetrics) -> (u32, u32, u32, u64) {
    (
        metrics.logical_operators,
        metrics.physical_operators,
        metrics.blocking_operators,
        metrics.peak_retained_bytes_upper.unwrap_or(u64::MAX),
    )
}

fn memo_maximum_regrets(planned: &[MemoPlannedCandidate]) -> Vec<Option<u64>> {
    let minima = planned
        .iter()
        .filter_map(|candidate| candidate.metrics.logical_row_operations)
        .fold(None, |minimum: Option<MemoScenarioCost>, cost| {
            Some(minimum.map_or(cost, |minimum| MemoScenarioCost {
                lower: minimum.lower.min(cost.lower),
                central: minimum.central.min(cost.central),
                upper: minimum.upper.min(cost.upper),
            }))
        });
    planned
        .iter()
        .map(|candidate| {
            let minimum = minima?;
            let cost = candidate.metrics.logical_row_operations?;
            Some(
                cost.lower
                    .saturating_sub(minimum.lower)
                    .max(cost.central.saturating_sub(minimum.central))
                    .max(cost.upper.saturating_sub(minimum.upper)),
            )
        })
        .collect()
}

/// A reordered tree must not increase the structural upper-tail cost. Among
/// eligible trees, the planner selects a strict reduction in maximum regret
/// across lower, central, and upper cardinality scenarios.
fn robust_memo_winner(
    planned: &[MemoPlannedCandidate],
    maximum_regrets: &[Option<u64>],
    structural: usize,
) -> Option<usize> {
    let structural_cost = planned[structural].metrics.logical_row_operations?;
    let structural_regret = maximum_regrets[structural]?;
    (0..planned.len())
        .filter(|candidate| *candidate != structural)
        .filter(|candidate| {
            !planned[structural].metrics.ordering_satisfied
                || planned[*candidate].metrics.ordering_satisfied
        })
        .filter_map(|candidate| {
            let cost = planned[candidate].metrics.logical_row_operations?;
            let regret = maximum_regrets[candidate]?;
            (cost.upper <= structural_cost.upper && regret < structural_regret).then_some((
                candidate,
                regret,
                cost.central,
                cost.upper,
            ))
        })
        .min_by_key(|(candidate, regret, central, upper)| (*regret, *central, *upper, *candidate))
        .map(|(candidate, _, _, _)| candidate)
}

impl Planner<'_> {
    fn allocate_slot(&mut self) -> SlotId {
        let slot = self.next_slot;
        self.next_slot.0 += 1;
        slot
    }

    fn plan_memo_root(
        &mut self,
        memo: &mut MemoSession,
        name: impl Into<String>,
        relation: &bound::Relation,
        required_order: &[bound::BoundOrderTerm],
    ) -> Node {
        let join_search_effort = memo
            .remaining_planning_effort()
            .min(super::join_search::MAX_JOIN_SEARCH_PLANNING_EFFORT);
        let join_search = self.statistics.and_then(|statistics| {
            super::join_search::search(
                relation,
                statistics,
                self.options
                    .memo_limits
                    .max_alternatives_per_group
                    .saturating_sub(1)
                    .min(super::join_search::MAX_STATES_PER_SUBSET as u32) as usize,
                join_search_effort,
            )
        });
        let exploration = memo.explore_with_join_search(name, relation, join_search);
        let start_slot = self.next_slot;
        let mut planned = exploration
            .alternatives
            .iter()
            .map(|alternative| {
                let mut candidate_planner = Planner {
                    options: self.options,
                    next_slot: start_slot,
                    statistics: self.statistics,
                    allow_join_region_strategy: true,
                };
                let node = candidate_planner.plan(&alternative.relation, required_order);
                MemoPlannedCandidate {
                    expression: alternative.expression,
                    origin: alternative.origin,
                    metrics: memo_physical_metrics(&alternative.relation, &node, required_order),
                    node,
                    next_slot: candidate_planner.next_slot,
                }
            })
            .collect::<Vec<_>>();
        let mut pareto = vec![true; planned.len()];
        for candidate in 0..planned.len() {
            pareto[candidate] = !(0..planned.len()).any(|other| {
                other != candidate
                    && memo_metrics_dominate(&planned[other].metrics, &planned[candidate].metrics)
            });
        }
        let structural = 0;
        let maximum_regrets = memo_maximum_regrets(&planned);
        let robust_chosen = (self.options.mode == PlannerMode::Cost
            && !self.options.full_scan_only)
            .then(|| robust_memo_winner(&planned, &maximum_regrets, structural))
            .flatten();
        let chosen = if self.options.mode == PlannerMode::Cost && !self.options.full_scan_only {
            robust_chosen.unwrap_or_else(|| {
                (1..planned.len())
                    .filter(|candidate| {
                        pareto[*candidate]
                            && memo_metrics_dominate(
                                &planned[*candidate].metrics,
                                &planned[structural].metrics,
                            )
                    })
                    .min_by_key(|candidate| memo_metric_order(&planned[*candidate].metrics))
                    .unwrap_or(structural)
            })
        } else {
            structural
        };
        let structural_cost = planned[structural].metrics.logical_row_operations;
        let winner_regret = maximum_regrets[chosen];
        let views = planned
            .iter()
            .enumerate()
            .map(|(index, candidate)| MemoCandidate {
                expression: candidate.expression,
                origin: candidate.origin,
                logical_operators: candidate.metrics.logical_operators,
                physical_operators: candidate.metrics.physical_operators,
                blocking_operators: candidate.metrics.blocking_operators,
                ordering: if required_order.is_empty() {
                    "not_required".into()
                } else if candidate.metrics.ordering_satisfied {
                    "satisfied".into()
                } else {
                    "unsatisfied".into()
                },
                peak_retained_bytes_upper: candidate.metrics.peak_retained_bytes_upper,
                pareto: pareto[index],
                structural: index == structural,
                selected: index == chosen,
                logical_row_operations: candidate.metrics.logical_row_operations,
                maximum_regret: maximum_regrets[index],
                tail_regression: structural_cost
                    .zip(candidate.metrics.logical_row_operations)
                    .is_some_and(|(structural, candidate)| candidate.upper > structural.upper),
                decision_basis: (index == chosen).then(|| {
                    if chosen == structural {
                        "structural".into()
                    } else if robust_chosen == Some(chosen) {
                        "minimax_regret".into()
                    } else {
                        "pareto_dominance".into()
                    }
                }),
                rejection_reason: (index != chosen).then(|| {
                    if chosen == structural || self.options.mode == PlannerMode::Structural {
                        "structural_fallback".into()
                    } else if candidate.metrics.logical_row_operations.is_none() {
                        "missing_scenario_evidence".into()
                    } else if planned[structural].metrics.ordering_satisfied
                        && !candidate.metrics.ordering_satisfied
                    {
                        "ordering_regression".into()
                    } else if structural_cost
                        .zip(candidate.metrics.logical_row_operations)
                        .is_some_and(|(structural, candidate)| candidate.upper > structural.upper)
                    {
                        "tail_regression".into()
                    } else if robust_chosen.is_some()
                        && maximum_regrets[index]
                            .zip(winner_regret)
                            .is_some_and(|(candidate, winner)| candidate >= winner)
                    {
                        "higher_maximum_regret".into()
                    } else if !pareto[index] {
                        "dominated".into()
                    } else if index == structural {
                        "pareto_dominated".into()
                    } else {
                        "overlapping_properties".into()
                    }
                }),
            })
            .collect();
        let selected_expression = planned[chosen].expression;
        self.next_slot = planned[chosen].next_slot;
        memo.complete_root(exploration.root_index, selected_expression, views);
        planned.swap_remove(chosen).node
    }

    /// Lower one bound relation. The node returned is the one whose output
    /// equals this relation's output, which is the only correspondence the
    /// planner can state: operators fused into an access path below it have
    /// no logical counterpart and stay unattributed.
    fn plan(
        &mut self,
        relation: &bound::Relation,
        required_order: &[bound::BoundOrderTerm],
    ) -> Node {
        Node {
            attribution: Some(lir::fingerprint::relation_family(relation)),
            kind: self.plan_kind(relation, required_order),
        }
    }

    fn plan_kind(
        &mut self,
        relation: &bound::Relation,
        required_order: &[bound::BoundOrderTerm],
    ) -> NodeKind {
        match &relation.node {
            RelationNode::Scan { .. } => self.choose_access_path(
                &ScanConstraints {
                    scan: relation.clone(),
                    columns: Default::default(),
                },
                required_order,
            ),
            RelationNode::Rows { .. } => NodeKind::Rows(relation.clone()),
            RelationNode::Filter { input, predicate } => {
                if let Some(predicate) = merged_scan_predicate(relation) {
                    let constraints = analysis::extract_constraints(relation)
                        .expect("a filter chain terminating in a scan has constraints");
                    let access = self.choose_access_path(&constraints, required_order).bare();
                    let (predicate, specifications) = self.extract_expr(&predicate);
                    NodeKind::Filter {
                        input: Box::new(attach_wrap(access, specifications)),
                        predicate,
                    }
                } else {
                    let (predicate, specifications) = self.extract_expr(predicate);
                    let input = self.plan(input, required_order);
                    NodeKind::Filter {
                        input: Box::new(attach_wrap(input, specifications)),
                        predicate,
                    }
                }
            }
            RelationNode::Order { input, terms } => {
                let input = self.plan(input, terms);
                let mut rewritten = Vec::with_capacity(terms.len());
                let mut specifications = Vec::new();
                for term in terms {
                    let (expression, mut extracted) = self.extract_expr(&term.expression);
                    rewritten.push(bound::BoundOrderTerm {
                        expression,
                        descending: term.descending,
                    });
                    specifications.append(&mut extracted);
                }
                if specifications.is_empty() && satisfies_order(&input.kind, &rewritten) {
                    input.kind
                } else {
                    NodeKind::Sort {
                        input: Box::new(attach_wrap(input, specifications)),
                        terms: rewritten,
                    }
                }
            }
            RelationNode::Slice {
                input,
                offset,
                limit,
            } => NodeKind::Slice {
                input: Box::new(self.plan(input, &[])),
                offset: *offset,
                limit: *limit,
            },
            RelationNode::Project { input, fields, .. } => {
                let mut planned_fields = Vec::with_capacity(fields.len());
                let mut specifications = Vec::new();
                for field in fields {
                    let (expression, mut extracted) = self.extract_field(field);
                    planned_fields.push(PhysicalField {
                        name: field.name.clone(),
                        slot: field.slot,
                        expression,
                    });
                    specifications.append(&mut extracted);
                }
                let input = self.plan(input, &[]);
                NodeKind::Project {
                    input: Box::new(attach_wrap(input, specifications)),
                    fields: planned_fields,
                }
            }
            RelationNode::Aggregate {
                input,
                groups,
                terms,
            } => {
                let mut planned_groups = Vec::with_capacity(groups.len());
                let mut specifications = Vec::new();
                for group in groups {
                    let (expression, mut extracted) = self.extract_expr(&group.expression);
                    planned_groups.push(bound::BoundGroupTerm {
                        name: group.name.clone(),
                        slot: group.slot,
                        expression,
                    });
                    specifications.append(&mut extracted);
                }
                let mut planned_terms = terms.clone();
                for term in &mut planned_terms {
                    if let Some(argument) = &term.argument {
                        let (expression, mut extracted) = self.extract_expr(argument);
                        term.argument = Some(expression);
                        specifications.append(&mut extracted);
                    }
                }
                let input = self.plan(input, &[]);
                NodeKind::Aggregate {
                    input: Box::new(attach_wrap(input, specifications)),
                    groups: planned_groups,
                    terms: planned_terms,
                }
            }
            RelationNode::Join {
                left,
                right,
                kind,
                on,
            } => {
                if self.allow_join_region_strategy
                    && let Some(choice) = self.choose_join_region_strategy(relation, required_order)
                {
                    choice
                } else {
                    self.choose_binary_join_method(relation, left, right, *kind, on, required_order)
                }
            }
            RelationNode::Concatenate { inputs, .. } => NodeKind::Concatenate {
                inputs: inputs.iter().map(|input| self.plan(input, &[])).collect(),
                input_outputs: inputs.iter().map(|input| input.output().clone()).collect(),
                output: relation.output().clone(),
            },
            RelationNode::Intersect {
                left,
                right,
                quantifier,
                ..
            } => NodeKind::Intersect {
                left: Box::new(self.plan(left, &[])),
                right: Box::new(self.plan(right, &[])),
                quantifier: *quantifier,
                left_output: left.output().clone(),
                right_output: right.output().clone(),
                output: relation.output().clone(),
            },
            RelationNode::Except {
                left,
                right,
                quantifier,
                ..
            } => NodeKind::Except {
                left: Box::new(self.plan(left, &[])),
                right: Box::new(self.plan(right, &[])),
                quantifier: *quantifier,
                left_output: left.output().clone(),
                right_output: right.output().clone(),
                output: relation.output().clone(),
            },
            RelationNode::Ref {
                binding, canonical, ..
            } => NodeKind::Reference {
                binding: binding.clone(),
                output: relation.output().clone(),
                canonical: canonical.clone(),
            },
            RelationNode::RecursiveRef {
                binding, canonical, ..
            } => NodeKind::RecursiveReference {
                binding: binding.clone(),
                output: relation.output().clone(),
                canonical: canonical.clone(),
            },
            RelationNode::Distinct(input) => NodeKind::Distinct {
                input: Box::new(self.plan(input, &[])),
                output: relation.output().clone(),
            },
        }
    }

    fn extract_field(&mut self, field: &bound::ProjectField) -> (bound::Expr, Vec<AttachSpec>) {
        if let Some((kind, relation)) = crossing(&field.expression) {
            let specification = self.attach_spec(field.slot, kind, relation);
            return (
                bound::Expr::slot(
                    field.slot,
                    field.name.clone(),
                    field.expression.value_type(),
                ),
                vec![specification],
            );
        }
        self.extract_expr(&field.expression)
    }

    fn extract_expr(&mut self, expression: &bound::Expr) -> (bound::Expr, Vec<AttachSpec>) {
        if let Some((kind, relation)) = crossing(expression) {
            let slot = self.allocate_slot();
            let specification = self.attach_spec(slot, kind, relation);
            return (
                bound::Expr::slot(slot, kind.label(), expression.value_type()),
                vec![specification],
            );
        }
        match expression {
            bound::Expr::Unary {
                op,
                expression: inner,
                ..
            } => {
                let (inner, specifications) = self.extract_expr(inner);
                if specifications.is_empty() {
                    (expression.clone(), specifications)
                } else {
                    (bound::Expr::unary(*op, inner), specifications)
                }
            }
            bound::Expr::Binary {
                op, left, right, ..
            } => {
                let (left, mut specifications) = self.extract_expr(left);
                let (right, mut right_specifications) = self.extract_expr(right);
                specifications.append(&mut right_specifications);
                if specifications.is_empty() {
                    (expression.clone(), specifications)
                } else {
                    (bound::Expr::binary(*op, left, right), specifications)
                }
            }
            bound::Expr::Cast {
                expression: inner,
                to,
                ..
            } => {
                let (inner, specifications) = self.extract_expr(inner);
                if specifications.is_empty() {
                    (expression.clone(), specifications)
                } else {
                    (bound::Expr::cast(inner, *to), specifications)
                }
            }
            bound::Expr::TextMatch { value, pattern, .. } => {
                let (value, specifications) = self.extract_expr(value);
                if specifications.is_empty() {
                    (expression.clone(), specifications)
                } else {
                    (
                        bound::Expr::TextMatch {
                            value: Box::new(value),
                            pattern: pattern.clone(),
                            value_type: expression.value_type(),
                        },
                        specifications,
                    )
                }
            }
            // Branch crossings are rejected by the binder because eager
            // attachment would violate lazy arm evaluation. All other forms
            // have no nested relation to extract.
            _ => (expression.clone(), Vec::new()),
        }
    }

    fn attach_spec(
        &mut self,
        slot: SlotId,
        kind: CrossingKind,
        relation: &bound::Relation,
    ) -> AttachSpec {
        AttachSpec {
            slot,
            kind,
            correlation: analysis::classify_correlation(relation),
            plan: self.plan(relation, &[]),
            output: relation.output().clone(),
        }
    }

    fn choose_join_region_strategy(
        &mut self,
        relation: &bound::Relation,
        required_order: &[bound::BoundOrderTerm],
    ) -> Option<NodeKind> {
        let graph = super::acyclic_join::classify(relation)?;
        let RelationNode::Join {
            left,
            right,
            kind,
            on,
        } = &relation.node
        else {
            return None;
        };
        let previous = self.allow_join_region_strategy;
        self.allow_join_region_strategy = false;
        let binary_kind =
            self.choose_binary_join_method(relation, left, right, *kind, on, required_order);
        self.allow_join_region_strategy = previous;
        let binary = Node {
            attribution: None,
            kind: binary_kind,
        };
        let statistics = self.statistics;
        let input_rows = statistics.map(|statistics| {
            let estimator = super::estimator::Estimator::new(statistics);
            graph
                .inputs
                .iter()
                .map(|input| estimate_quantity(estimator.bound_relation(input), 1))
                .collect::<Vec<_>>()
        });
        let root_input = input_rows
            .as_ref()
            .and_then(|rows| {
                rows.iter()
                    .enumerate()
                    .min_by_key(|(index, rows)| {
                        (rows.upper_bound.unwrap_or(u64::MAX), rows.central, *index)
                    })
                    .map(|(index, _)| index)
            })
            .unwrap_or(0);
        let rooted_edges = graph.rooted_edges(root_input);
        self.allow_join_region_strategy = false;
        let inputs = graph
            .inputs
            .iter()
            .map(|input| self.plan(input, &[]))
            .collect::<Vec<_>>();
        self.allow_join_region_strategy = previous;
        let context = JoinRegionContext {
            binary_cost: binary_join_region_cost(relation, &binary, required_order),
            graph,
            statistics,
            required_order,
            binary,
            inputs,
            input_rows,
            root_input,
            rooted_edges,
            memory_limit_bytes: self.options.hash_join_memory_limit_bytes,
        };
        let candidates = vec![
            binary_join_region_candidate(&context),
            shredded_join_region_candidate(&context),
            predicate_transfer_join_region_candidate(&context),
        ];
        let metadata = DecisionMetadata {
            classification: context.graph.classification,
            input_count: context.graph.inputs.len(),
            edge_count: context.graph.edges.len(),
            root_input: context.root_input,
            semijoin_passes: context.graph.edges.len().saturating_mul(2) as u32,
            predicate_transfer_passes: 2,
        };
        let selection = join_region::select(
            candidates,
            0,
            self.options.mode == PlannerMode::Cost && !self.options.full_scan_only,
        );
        let (_, selected, decision) = selection.finish(metadata);
        let input = build_join_region_plan(context, selected);
        Some(NodeKind::JoinGraphChoice {
            input: Box::new(input),
            decision,
        })
    }

    fn choose_binary_join_method(
        &mut self,
        relation: &bound::Relation,
        left: &bound::Relation,
        right: &bound::Relation,
        kind: lir::JoinKind,
        on: &bound::Expr,
        required_order: &[bound::BoundOrderTerm],
    ) -> NodeKind {
        let keys = analysis::equi_join_keys(left, right, on).unwrap_or_default();
        let planned_left = self.plan(left, &[]);
        let planned_right = self.plan(right, &[]);
        let ordering = if required_order.is_empty() {
            AccessOrdering::NotRequired
        } else {
            AccessOrdering::SortRequired
        };
        let nested_cost = self.statistics.and_then(|statistics| {
            join_costs(statistics, relation, left, right, &keys, ordering).map(|costs| costs.nested)
        });
        let hash_result = if keys.is_empty() {
            Err(JoinRejectionReason::UnsupportedPredicate)
        } else {
            self.statistics
                .ok_or(JoinRejectionReason::MissingEvidence)
                .and_then(|statistics| {
                    let costs = join_costs(statistics, relation, left, right, &keys, ordering)
                        .ok_or(JoinRejectionReason::MissingEvidence)?;
                    let memory = hash_join_memory_bound(statistics, right, &keys, costs.right)?;
                    if memory > self.options.hash_join_memory_limit_bytes {
                        return Err(JoinRejectionReason::MemoryLimit);
                    }
                    let mut cost = costs.hash;
                    cost.peak_retained_bytes = Some(AccessQuantity {
                        central: memory,
                        lower_bound: 0,
                        upper_bound: Some(memory),
                    });
                    Ok(cost)
                })
        };
        let lookup_plan = if keys.is_empty() {
            Err(JoinRejectionReason::UnsupportedPredicate)
        } else if self.options.full_scan_only {
            Err(JoinRejectionReason::UnsupportedInput)
        } else {
            self.indexed_lookup_input(right, &keys)
                .ok_or(JoinRejectionReason::UnsupportedInput)
        };
        let lookup_result = lookup_plan
            .as_ref()
            .map_err(|reason| *reason)
            .and_then(|_| {
                self.statistics
                    .ok_or(JoinRejectionReason::MissingEvidence)
                    .and_then(|statistics| {
                        join_costs(statistics, relation, left, right, &keys, ordering)
                            .map(|costs| costs.lookup)
                            .ok_or(JoinRejectionReason::MissingEvidence)
                    })
            });
        let mut candidates = vec![
            JoinCandidate {
                method: "NestedLoopJoin".into(),
                cost: nested_cost,
                decision_basis: None,
                rejection_reason: None,
                chosen: false,
            },
            JoinCandidate {
                method: "HashJoin".into(),
                cost: hash_result.as_ref().ok().copied(),
                decision_basis: None,
                rejection_reason: hash_result.as_ref().err().copied(),
                chosen: false,
            },
            JoinCandidate {
                method: "IndexedLookupJoin".into(),
                cost: lookup_result.as_ref().ok().copied(),
                decision_basis: None,
                rejection_reason: lookup_result.as_ref().err().copied(),
                chosen: false,
            },
        ];
        let structural_fallback = 0;
        let chosen = if self.options.mode == PlannerMode::Cost && !self.options.full_scan_only {
            strict_join_cost_winner(&candidates).unwrap_or(structural_fallback)
        } else {
            structural_fallback
        };
        candidates[chosen].chosen = true;
        candidates[chosen].decision_basis = Some(if chosen == structural_fallback {
            JoinDecisionBasis::Structural
        } else {
            JoinDecisionBasis::CostDominance
        });
        let winner = candidates[chosen].cost;
        for (index, candidate) in candidates.iter_mut().enumerate() {
            if index == chosen || candidate.rejection_reason.is_some() {
                continue;
            }
            candidate.rejection_reason = Some(
                if chosen == structural_fallback || index == structural_fallback {
                    JoinRejectionReason::StructuralFallback
                } else if join_cost_is_more_expensive(candidate.cost, winner) {
                    JoinRejectionReason::MoreExpensive
                } else {
                    JoinRejectionReason::OverlappingCost
                },
            );
        }
        let decision = JoinDecision {
            candidates,
            structural_fallback,
        };
        match chosen {
            1 => NodeKind::HashJoin {
                left: Box::new(planned_left),
                right: Box::new(planned_right),
                kind,
                on: on.clone(),
                keys,
                right_output: right.output().clone(),
                memory_limit_bytes: self.options.hash_join_memory_limit_bytes,
                decision,
            },
            2 => NodeKind::IndexedLookupJoin {
                left: Box::new(planned_left),
                right: Box::new(lookup_plan.expect("chosen lookup plan")),
                kind,
                on: on.clone(),
                keys,
                right_output: right.output().clone(),
                decision,
            },
            _ => NodeKind::NestedLoopJoin {
                left: Box::new(planned_left),
                right: Box::new(planned_right),
                kind,
                on: on.clone(),
                keys,
                right_output: right.output().clone(),
                decision,
            },
        }
    }

    fn indexed_lookup_input(
        &self,
        right: &bound::Relation,
        keys: &[analysis::EquiJoinKey],
    ) -> Option<Node> {
        if !matches!(right.node, RelationNode::Scan { .. })
            || keys
                .iter()
                .any(|key| key.right.value_type.kind == crate::engine::lir::Kind::Float64)
        {
            return None;
        }
        let mut constraints = analysis::extract_constraints(right)?;
        for key in keys {
            constraints.columns.insert(
                key.right.name.clone(),
                analysis::Domain {
                    equality: Some(ConstValue::Outer(key.left.slot)),
                    lower: None,
                    upper: None,
                },
            );
        }
        let access = self.choose_access_path(&constraints, &[]);
        let table = constraints.scan.scan_table();
        let key_columns: std::collections::HashSet<_> =
            keys.iter().map(|key| key.right.name.as_str()).collect();
        let supported = match &access {
            NodeKind::PrimaryKeyGet { .. } => {
                table.primary_key.len() == keys.len()
                    && table
                        .primary_key
                        .iter()
                        .all(|column| key_columns.contains(column.as_str()))
            }
            NodeKind::IndexRangeScan {
                index,
                equality_prefix,
                range,
                ..
            } => {
                let columns = table.index_column_names(index);
                range.is_none()
                    && columns.len() == keys.len()
                    && equality_prefix.len() == columns.len()
                    && columns.iter().all(|column| key_columns.contains(*column))
            }
            _ => false,
        };
        supported.then(|| access.bare())
    }

    fn choose_access_path(
        &self,
        constraints: &ScanConstraints,
        required_order: &[bound::BoundOrderTerm],
    ) -> NodeKind {
        let table = constraints.scan.scan_table();
        if self.options.full_scan_only {
            return NodeKind::TableScan {
                scan: Box::new(constraints.scan.clone()),
                decode_columns: Vec::new(),
                access: Default::default(),
            };
        }
        if let Some(key) = pinned_key(constraints, &table.primary_key) {
            return NodeKind::PrimaryKeyGet {
                scan: Box::new(constraints.scan.clone()),
                key,
                decode_columns: Vec::new(),
                access: AccessDecision {
                    candidates: vec![AccessCandidate {
                        method: "PKGet".into(),
                        score: 0,
                        estimated_row_work: Some(AccessRowWork {
                            lower_bound: 0,
                            upper_bound: Some(1),
                        }),
                        cost: Some(primary_key_cost()),
                        decision_basis: Some(AccessDecisionBasis::PrimaryKey),
                        rejection_reason: None,
                        chosen: true,
                    }],
                    structural_fallback: 0,
                },
            };
        }

        let mut options = vec![NodeKind::TableScan {
            scan: Box::new(constraints.scan.clone()),
            decode_columns: Vec::new(),
            access: Default::default(),
        }];
        let table_estimate = self.statistics.and_then(|statistics| {
            super::estimator::Estimator::new(statistics).scan_for_table(table)
        });
        let table_bounds = table_estimate.and_then(hard_cardinality_bounds);
        let table_work = table_bounds.map(|(lower_bound, upper_bound)| AccessRowWork {
            lower_bound,
            upper_bound,
        });
        let small_table = matches!(table_bounds, Some((_, Some(upper_bound))) if upper_bound <= 1);
        let mut best_score = score(&options[0], 0, false, required_order);
        let mut candidates = vec![AccessCandidate {
            method: "TableScan".into(),
            score: best_score,
            estimated_row_work: table_work,
            cost: table_cost(
                table_estimate,
                table_width(self.statistics, table),
                &options[0],
                required_order,
            ),
            decision_basis: None,
            rejection_reason: None,
            chosen: false,
        }];
        let mut chosen = 0;
        let mut unique_points = Vec::new();
        let mut prefix_estimates = Vec::new();
        for index in table.indexes.iter().filter(|index| index.is_ready()) {
            let column_names = table.index_column_names(index);
            let mut equality_prefix = Vec::new();
            for column in &column_names {
                let Some(equality) = constraints
                    .columns
                    .get(*column)
                    .and_then(|domain| domain.equality.clone())
                else {
                    break;
                };
                equality_prefix.push(equality);
            }
            let range = column_names
                .get(equality_prefix.len())
                .and_then(|column| {
                    constraints
                        .columns
                        .get(*column)
                        .map(|domain| (*column, domain))
                })
                .and_then(|(column, domain)| {
                    (domain.lower.is_some() || domain.upper.is_some()).then(|| RangeSpec {
                        column: column.to_owned(),
                        lower: domain.lower.clone(),
                        upper: domain.upper.clone(),
                    })
                });
            let equality_prefix_len = equality_prefix.len();
            let has_range = range.is_some();
            let candidate = NodeKind::IndexRangeScan {
                scan: Box::new(constraints.scan.clone()),
                index: index.clone(),
                equality_prefix: equality_prefix.clone(),
                range: range.clone(),
                decode_columns: Vec::new(),
                access: Default::default(),
            };
            let candidate_score = score(&candidate, equality_prefix_len, has_range, required_order);
            let unique_point = index.unique
                && equality_prefix_len == column_names.len()
                && !column_names.is_empty();
            let estimated_row_work = if unique_point {
                Some(AccessRowWork {
                    lower_bound: 0,
                    upper_bound: Some(2),
                })
            } else if small_table {
                Some(AccessRowWork {
                    lower_bound: 0,
                    upper_bound: table_bounds
                        .and_then(|(_, upper_bound)| upper_bound)
                        .map(|upper_bound| upper_bound.saturating_mul(2)),
                })
            } else {
                None
            };
            options.push(candidate);
            candidates.push(AccessCandidate {
                method: format!("IndexRangeScan {}", index.name),
                score: candidate_score,
                estimated_row_work,
                cost: index_cost(
                    self.statistics,
                    table,
                    &column_names[..equality_prefix_len],
                    &equality_prefix,
                    range.as_ref(),
                    table_estimate,
                    table_bounds,
                    table_width(self.statistics, table),
                    &options[options.len() - 1],
                    required_order,
                    unique_point,
                    &mut prefix_estimates,
                ),
                decision_basis: None,
                rejection_reason: None,
                chosen: false,
            });
            unique_points.push(unique_point);
            if candidate_score > best_score {
                best_score = candidate_score;
                chosen = candidates.len() - 1;
            }
        }

        if candidates.len() > 1
            && (required_order.is_empty() || satisfies_order(&options[0], required_order))
        {
            if small_table {
                chosen = 0;
                candidates[chosen].decision_basis = Some(AccessDecisionBasis::BoundedRowWork);
            } else if let Some((table_lower_bound, _)) = table_bounds
                && table_lower_bound > 2
                && let Some(candidate_offset) = unique_points
                    .iter()
                    .enumerate()
                    .filter(|(_, unique_point)| **unique_point)
                    .map(|(offset, _)| offset)
                    .reduce(|best, offset| {
                        if candidates[offset + 1].score > candidates[best + 1].score {
                            offset
                        } else {
                            best
                        }
                    })
            {
                chosen = candidate_offset + 1;
                candidates[chosen].decision_basis = Some(AccessDecisionBasis::BoundedRowWork);
            }
        }

        let structural_fallback = chosen;
        if self.options.mode == PlannerMode::Cost
            && self.statistics.is_some()
            && let Some(cost_winner) = strict_cost_winner(&candidates, structural_fallback)
        {
            chosen = cost_winner;
            candidates[chosen].decision_basis = Some(AccessDecisionBasis::CostDominance);
        }

        let winner = candidates[chosen].clone();
        for (index, candidate) in candidates.iter_mut().enumerate() {
            if index == chosen {
                continue;
            }
            candidate.rejection_reason = Some(rejection_reason(
                candidate,
                &winner,
                index == structural_fallback,
            ));
        }
        if candidates[chosen].decision_basis.is_none() {
            candidates[chosen].decision_basis = Some(AccessDecisionBasis::Structural);
        }

        candidates[chosen].chosen = true;
        let decision = AccessDecision {
            candidates,
            structural_fallback,
        };
        let mut best = options.swap_remove(chosen);
        match &mut best {
            NodeKind::TableScan { access, .. } | NodeKind::IndexRangeScan { access, .. } => {
                *access = decision
            }
            _ => unreachable!(),
        }
        best
    }
}

struct AccessPrefixEstimate {
    columns: Vec<String>,
    values: Vec<ConstValue>,
    range: Option<RangeSpec>,
    estimate: Option<super::estimator::Estimate>,
}

fn crossing(expression: &bound::Expr) -> Option<(CrossingKind, &bound::Relation)> {
    match expression {
        bound::Expr::Exists(relation) => Some((CrossingKind::Exists, relation)),
        bound::Expr::First { relation, .. } => Some((CrossingKind::First, relation)),
        bound::Expr::Scalar { relation, .. } => Some((CrossingKind::Scalar, relation)),
        bound::Expr::Array { relation, .. } => Some((CrossingKind::Array, relation)),
        _ => None,
    }
}

fn attach_wrap(input: Node, specifications: Vec<AttachSpec>) -> Node {
    if specifications.is_empty() {
        input
    } else {
        NodeKind::Attach {
            input: Box::new(input),
            specifications,
        }
        .bare()
    }
}

fn merged_scan_predicate(relation: &bound::Relation) -> Option<bound::Expr> {
    let mut relation = relation;
    let mut predicate = None;
    while let RelationNode::Filter {
        input,
        predicate: next,
    } = &relation.node
    {
        predicate = Some(match predicate {
            None => next.clone(),
            Some(predicate) => bound::Expr::binary(lir::BinaryOp::And, predicate, next.clone()),
        });
        relation = input;
    }
    matches!(relation.node, RelationNode::Scan { .. }).then_some(predicate?)
}

fn pinned_key(constraints: &ScanConstraints, columns: &[String]) -> Option<Vec<ConstValue>> {
    if columns.is_empty() {
        return None;
    }
    columns
        .iter()
        .map(|column| constraints.columns.get(column)?.equality.clone())
        .collect()
}

fn hard_cardinality_bounds(estimate: super::estimator::Estimate) -> Option<(u64, Option<u64>)> {
    use super::estimator::EstimateInterval;

    match estimate.interval {
        EstimateInterval::Exact => Some((estimate.cardinality, Some(estimate.cardinality))),
        EstimateInterval::LowerBound { lower_bound } => Some((lower_bound, None)),
        EstimateInterval::Range {
            lower_bound,
            upper_bound,
        }
        | EstimateInterval::AttributedRange {
            lower_bound,
            upper_bound,
            ..
        } => Some((lower_bound, Some(upper_bound))),
        EstimateInterval::Confidence { .. } | EstimateInterval::Unknown => None,
    }
}

struct JoinCosts {
    nested: JoinCost,
    hash: JoinCost,
    lookup: JoinCost,
    right: AccessQuantity,
}

fn join_costs(
    statistics: &super::models::PlannerStats,
    relation: &bound::Relation,
    left: &bound::Relation,
    right: &bound::Relation,
    keys: &[analysis::EquiJoinKey],
    ordering: AccessOrdering,
) -> Option<JoinCosts> {
    let estimator = super::estimator::Estimator::new(statistics);
    let left_estimate = estimator.bound_relation(left);
    let right_estimate = estimator.bound_relation(right);
    let output_estimate = estimator.bound_relation(relation);
    let left_rows = estimate_quantity(left_estimate, 1);
    let right_rows = estimate_quantity(right_estimate, 1);
    let output_rows = estimate_quantity(output_estimate, 1);
    left_rows.upper_bound?;
    right_rows.upper_bound?;
    output_rows.upper_bound?;
    let pairs = quantity_product(left_rows, right_rows);
    let nested_key_comparisons = AccessQuantity {
        central: pairs.central.saturating_mul(keys.len() as u64),
        lower_bound: if keys.is_empty() {
            0
        } else {
            pairs.lower_bound
        },
        upper_bound: pairs
            .upper_bound
            .map(|value| value.saturating_mul(keys.len() as u64)),
    };
    let nested = JoinCost {
        expected_output_rows: output_estimate,
        build_rows: right_rows,
        probe_rows: left_rows,
        lookup_requests: AccessQuantity::exact(0),
        key_comparisons: nested_key_comparisons,
        residual_predicate_evaluations: AccessQuantity::exact(0),
        logical_row_operations: quantity_sum(&[left_rows, right_rows, pairs]),
        peak_retained_bytes: None,
        ordering,
    };
    let hash = JoinCost {
        expected_output_rows: output_estimate,
        build_rows: right_rows,
        probe_rows: left_rows,
        lookup_requests: AccessQuantity::exact(0),
        key_comparisons: output_rows,
        residual_predicate_evaluations: AccessQuantity::exact(0),
        logical_row_operations: quantity_sum(&[left_rows, right_rows, output_rows]),
        peak_retained_bytes: None,
        ordering,
    };
    let lookup = JoinCost {
        expected_output_rows: output_estimate,
        build_rows: AccessQuantity::exact(0),
        probe_rows: left_rows,
        lookup_requests: left_rows,
        key_comparisons: output_rows,
        residual_predicate_evaluations: AccessQuantity::exact(0),
        logical_row_operations: quantity_sum(&[
            left_rows,
            left_rows,
            multiply_quantity(output_rows, 3),
        ]),
        peak_retained_bytes: None,
        ordering,
    };
    Some(JoinCosts {
        nested,
        hash,
        lookup,
        right: right_rows,
    })
}

fn binary_join_region_cost(
    relation: &bound::Relation,
    binary: &Node,
    required_order: &[bound::BoundOrderTerm],
) -> Option<JoinGraphCost> {
    let metrics = memo_physical_metrics(relation, binary, required_order);
    metrics
        .logical_row_operations
        .map(|logical_row_operations| JoinGraphCost {
            logical_row_operations: quantity_from_scenario(logical_row_operations),
            reduction_row_operations: None,
            lookup_row_operations: None,
            expanded_rows: None,
            filter_row_operations: None,
            filtered_rows: None,
            filter_bytes: None,
            filter_paths: None,
            filter_builds: None,
            shared_filter_paths: None,
            pruned_filter_paths: None,
            filter_input_scans: None,
            filter_schedule_root: None,
            peak_retained_bytes: metrics
                .peak_retained_bytes_upper
                .map(|value| AccessQuantity {
                    central: value,
                    lower_bound: 0,
                    upper_bound: Some(value),
                }),
            ordering: if required_order.is_empty() {
                AccessOrdering::NotRequired
            } else if metrics.ordering_satisfied {
                AccessOrdering::Satisfied
            } else {
                AccessOrdering::SortRequired
            },
        })
}

fn binary_join_region_candidate(
    context: &JoinRegionContext<'_, '_, '_>,
) -> StrategyCandidate<JoinRegionPlan> {
    StrategyCandidate {
        method: "BinaryJoinPlan",
        cost: context.binary_cost,
        rejection_reason: None,
        plan: Some(JoinRegionPlan::Binary),
    }
}

fn shredded_join_region_candidate(
    context: &JoinRegionContext<'_, '_, '_>,
) -> StrategyCandidate<JoinRegionPlan> {
    let mut planned = None;
    let mut rejection_reason = match context.graph.classification {
        JoinGraphClassification::Acyclic => None,
        JoinGraphClassification::Cyclic => Some(JoinGraphRejectionReason::CyclicGraph),
    };
    if rejection_reason.is_none() {
        planned = context
            .statistics
            .zip(context.rooted_edges.as_ref())
            .and_then(|(statistics, edges)| {
                shredded_join_cost(
                    statistics,
                    &context.graph,
                    &context.inputs,
                    edges,
                    context.required_order,
                )
            });
        if planned.is_none() {
            rejection_reason = Some(JoinGraphRejectionReason::MissingEvidence);
        }
    }
    if !context.required_order.is_empty() {
        rejection_reason = Some(JoinGraphRejectionReason::OrderingConflict);
    }
    if planned.is_some_and(|(_, _, memory)| {
        memory
            .upper_bound
            .is_some_and(|memory| memory > context.memory_limit_bytes)
    }) {
        rejection_reason = Some(JoinGraphRejectionReason::MemoryLimit);
    }
    StrategyCandidate {
        method: "ShreddedYannakakisJoin",
        cost: planned.map(|(cost, _, _)| cost),
        rejection_reason,
        plan: planned.map(
            |(_, logical_row_operations, estimated_peak_retained_bytes)| JoinRegionPlan::Shredded {
                logical_row_operations,
                estimated_peak_retained_bytes,
            },
        ),
    }
}

fn predicate_transfer_join_region_candidate(
    context: &JoinRegionContext<'_, '_, '_>,
) -> StrategyCandidate<JoinRegionPlan> {
    let planned = context
        .statistics
        .zip(context.input_rows.as_ref())
        .and_then(|(statistics, input_rows)| {
            predicate_transfer_plan(
                statistics,
                &context.graph,
                &context.inputs,
                &context.binary,
                input_rows,
                context.required_order,
            )
        });
    let mut rejection_reason = planned
        .is_none()
        .then_some(JoinGraphRejectionReason::MissingEvidence);
    if !context.required_order.is_empty() {
        rejection_reason = Some(JoinGraphRejectionReason::OrderingConflict);
    }
    if planned.as_ref().is_some_and(|planned| {
        planned
            .memory
            .upper_bound
            .is_some_and(|memory| memory > context.memory_limit_bytes)
    }) {
        rejection_reason = Some(JoinGraphRejectionReason::MemoryLimit);
    }
    StrategyCandidate {
        method: "PredicateTransferJoin",
        cost: planned.as_ref().map(|planned| planned.cost),
        rejection_reason,
        plan: planned.map(|planned| JoinRegionPlan::PredicateTransfer(Box::new(planned))),
    }
}

fn build_join_region_plan(
    context: JoinRegionContext<'_, '_, '_>,
    selected: JoinRegionPlan,
) -> Node {
    match selected {
        JoinRegionPlan::Binary => context.binary,
        JoinRegionPlan::Shredded {
            logical_row_operations,
            estimated_peak_retained_bytes,
        } => NodeKind::ShreddedYannakakisJoin {
            inputs: context.inputs,
            edges: context
                .rooted_edges
                .expect("an eligible shredded plan has rooted edges"),
            root_input: context.root_input,
            output: context.graph.relation.output().clone(),
            memory_limit_bytes: context.memory_limit_bytes,
            logical_row_operations,
            estimated_peak_retained_bytes,
        }
        .bare(),
        JoinRegionPlan::PredicateTransfer(planned) => NodeKind::PredicateTransferJoin {
            inputs: context.inputs,
            schedule: planned.schedule,
            join_plan: Box::new(planned.join_plan),
            output: context.graph.relation.output().clone(),
            bits_per_key: super::predicate_transfer::DEFAULT_BITS_PER_KEY,
            hash_functions: super::predicate_transfer::DEFAULT_HASH_FUNCTIONS,
            runtime_policy: super::predicate_transfer::runtime_policy(),
            memory_limit_bytes: context.memory_limit_bytes,
            logical_row_operations: planned.operator_work,
            estimated_peak_retained_bytes: planned.memory,
        }
        .bare(),
    }
}

fn hash_join_memory_bound(
    statistics: &super::models::PlannerStats,
    right: &bound::Relation,
    keys: &[analysis::EquiJoinKey],
    right_rows: AccessQuantity,
) -> Result<u64, JoinRejectionReason> {
    if !right.free_slots().is_empty() {
        return Err(JoinRejectionReason::UnsupportedInput);
    }
    let scan = analysis::underlying_scan(right).ok_or(JoinRejectionReason::UnsupportedInput)?;
    let table = scan.scan_table();
    let estimator = super::estimator::Estimator::new(statistics);
    let row_width = estimator
        .maximum_row_width(table)
        .ok_or(JoinRejectionReason::MissingEvidence)?;
    let mut key_width = 0u64;
    for key in keys {
        let scan_field = scan
            .output()
            .fields
            .iter()
            .find(|field| field.slot == key.right.slot)
            .ok_or(JoinRejectionReason::UnsupportedInput)?;
        let width = estimator
            .maximum_column_width(table, &scan_field.name)
            .ok_or(JoinRejectionReason::MissingEvidence)?;
        let encoded_width = match scan_field.value_type.kind {
            crate::engine::lir::Kind::Text => width.saturating_add(9),
            crate::engine::lir::Kind::Int64 | crate::engine::lir::Kind::Float64 => 9,
            crate::engine::lir::Kind::Bool => 2,
            _ => return Err(JoinRejectionReason::UnsupportedInput),
        };
        key_width = key_width.saturating_add(encoded_width);
    }
    let upper_rows = right_rows
        .upper_bound
        .ok_or(JoinRejectionReason::MissingEvidence)?;
    Ok(upper_rows.saturating_mul(row_width.saturating_add(key_width)))
}

fn predicate_transfer_plan(
    statistics: &super::models::PlannerStats,
    graph: &super::acyclic_join::JoinGraph<'_>,
    inputs: &[Node],
    binary: &Node,
    input_rows: &[AccessQuantity],
    required_order: &[bound::BoundOrderTerm],
) -> Option<PredicateTransferPlanned> {
    if !required_order.is_empty() {
        return None;
    }
    let mut schedule = super::predicate_transfer::schedule(graph, input_rows)?;
    let transfer = if let Some(simulation) = super::predicate_transfer::simulate_literal_rows(
        graph,
        &schedule,
        super::predicate_transfer::DEFAULT_BITS_PER_KEY,
        super::predicate_transfer::DEFAULT_HASH_FUNCTIONS,
    ) {
        super::predicate_transfer::TransferEstimate {
            filtered_rows: simulation
                .filtered_rows
                .iter()
                .copied()
                .map(AccessQuantity::exact)
                .collect(),
            filter_work: AccessQuantity::exact(
                simulation
                    .rows_scanned
                    .saturating_add(simulation.insertions)
                    .saturating_add(simulation.checks)
                    .saturating_add(simulation.min_max_checks),
            ),
            filter_bytes: AccessQuantity::exact(simulation.filter_bytes),
            peak_filter_bytes: AccessQuantity::exact(simulation.peak_filter_bytes),
        }
    } else {
        super::predicate_transfer::estimate_synopsis_rows(
            statistics,
            graph,
            &mut schedule,
            input_rows,
        )?
    };
    let mut join_plan = binary.clone();
    replace_with_predicate_transfer_inputs(&mut join_plan, inputs, graph)?;
    let (join_work, expanded_rows) = buffered_join_work(&join_plan, &transfer.filtered_rows)?;
    let filter_work = transfer.filter_work;
    let operator_work = quantity_sum(&[filter_work, join_work]);
    let input_work = inputs
        .iter()
        .zip(&graph.inputs)
        .map(|(node, relation)| {
            memo_physical_metrics(relation, node, &[])
                .logical_row_operations
                .map(quantity_from_scenario)
        })
        .collect::<Option<Vec<_>>>()?;
    let total = quantity_sum(&[quantity_sum(&input_work), operator_work]);
    total.upper_bound?;

    let estimator = super::estimator::Estimator::new(statistics);
    let input_bytes =
        graph
            .inputs
            .iter()
            .zip(input_rows)
            .try_fold(0u64, |total, (input, rows)| {
                Some(
                    total.saturating_add(
                        rows.upper_bound?
                            .saturating_mul(maximum_relation_row_width(&estimator, input)?),
                    ),
                )
            })?;
    let output_width = graph.inputs.iter().try_fold(0u64, |total, input| {
        Some(total.saturating_add(maximum_relation_row_width(&estimator, input)?))
    })?;
    let join_bytes = join_work.upper_bound?.saturating_mul(output_width);
    let memory_upper = input_bytes
        .saturating_add(transfer.peak_filter_bytes.upper_bound?)
        .saturating_add(join_bytes);
    let memory = AccessQuantity {
        central: memory_upper,
        lower_bound: 0,
        upper_bound: Some(memory_upper),
    };
    let filtered_rows = AccessQuantity::exact(
        transfer
            .filtered_rows
            .iter()
            .map(|rows| rows.central)
            .fold(0u64, u64::saturating_add),
    );
    let filtered_rows = AccessQuantity {
        central: filtered_rows.central,
        lower_bound: transfer
            .filtered_rows
            .iter()
            .map(|rows| rows.lower_bound)
            .fold(0u64, u64::saturating_add),
        upper_bound: transfer.filtered_rows.iter().try_fold(0u64, |total, rows| {
            Some(total.saturating_add(rows.upper_bound?))
        }),
    };
    let filter_bytes = transfer.filter_bytes;
    let (filter_builds, shared_filter_paths) =
        super::predicate_transfer::filter_storage_shape(&schedule);
    Some(PredicateTransferPlanned {
        cost: JoinGraphCost {
            logical_row_operations: total,
            reduction_row_operations: None,
            lookup_row_operations: None,
            expanded_rows: Some(expanded_rows),
            filter_row_operations: Some(filter_work),
            filtered_rows: Some(filtered_rows),
            filter_bytes: Some(filter_bytes),
            filter_paths: Some(
                schedule
                    .forward
                    .edges
                    .len()
                    .saturating_add(schedule.backward.edges.len()) as u32,
            ),
            filter_builds: Some(filter_builds),
            shared_filter_paths: Some(shared_filter_paths),
            pruned_filter_paths: Some(schedule.pruned_paths as u32),
            filter_input_scans: Some(AccessQuantity::exact(
                schedule
                    .forward
                    .order
                    .len()
                    .saturating_add(schedule.backward.order.len()) as u64,
            )),
            filter_schedule_root: Some(schedule.root_input),
            peak_retained_bytes: Some(memory),
            ordering: AccessOrdering::NotRequired,
        },
        operator_work,
        memory,
        schedule,
        join_plan,
    })
}

fn replace_with_predicate_transfer_inputs(
    join_plan: &mut Node,
    inputs: &[Node],
    graph: &super::acyclic_join::JoinGraph<'_>,
) -> Option<()> {
    let mut replacements = vec![0usize; inputs.len()];
    join_plan.walk_mut(&mut |node| {
        let Some(input) = inputs.iter().position(|candidate| node == candidate) else {
            return;
        };
        *node = NodeKind::PredicateTransferInput {
            input,
            output: graph.inputs[input].output().clone(),
        }
        .bare();
        replacements[input] = replacements[input].saturating_add(1);
    });
    replacements.iter().all(|count| *count == 1).then_some(())
}

fn buffered_join_work(
    node: &Node,
    filtered_rows: &[AccessQuantity],
) -> Option<(AccessQuantity, AccessQuantity)> {
    match &node.kind {
        NodeKind::PredicateTransferInput { input, .. } => {
            Some((AccessQuantity::exact(0), *filtered_rows.get(*input)?))
        }
        NodeKind::NestedLoopJoin {
            left, right, keys, ..
        } => {
            let (left_work, left_rows) = buffered_join_work(left, filtered_rows)?;
            let (right_work, right_rows) = buffered_join_work(right, filtered_rows)?;
            let pairs = quantity_product(left_rows, right_rows);
            let comparisons = quantity_scale(pairs, keys.len().max(1) as u64);
            let work = quantity_sum(&[left_work, right_work, left_rows, right_rows, comparisons]);
            Some((work, pairs))
        }
        NodeKind::HashJoin { left, right, .. } => {
            let (left_work, left_rows) = buffered_join_work(left, filtered_rows)?;
            let (right_work, right_rows) = buffered_join_work(right, filtered_rows)?;
            let pairs = quantity_product(left_rows, right_rows);
            let work = quantity_sum(&[left_work, right_work, left_rows, right_rows, pairs]);
            Some((work, pairs))
        }
        _ => None,
    }
}

fn shredded_join_cost(
    statistics: &super::models::PlannerStats,
    graph: &super::acyclic_join::JoinGraph<'_>,
    inputs: &[Node],
    rooted_edges: &[super::physical::ShreddedJoinEdge],
    required_order: &[bound::BoundOrderTerm],
) -> Option<(JoinGraphCost, AccessQuantity, AccessQuantity)> {
    let estimator = super::estimator::Estimator::new(statistics);
    let input_rows = graph
        .inputs
        .iter()
        .map(|input| estimate_quantity(estimator.bound_relation(input), 1))
        .collect::<Vec<_>>();
    if input_rows.iter().any(|rows| rows.upper_bound.is_none()) {
        return None;
    }
    let input_work = inputs
        .iter()
        .zip(&graph.inputs)
        .map(|(node, relation)| {
            memo_physical_metrics(relation, node, &[])
                .logical_row_operations
                .map(quantity_from_scenario)
        })
        .collect::<Option<Vec<_>>>()?;
    let input_work = quantity_sum(&input_work);
    let reduction_row_operations = graph
        .edges
        .iter()
        .flat_map(|edge| {
            [
                semijoin_pass_cost(input_rows[edge.right], input_rows[edge.left]),
                semijoin_pass_cost(input_rows[edge.left], input_rows[edge.right]),
            ]
        })
        .fold(AccessQuantity::exact(0), |total, value| {
            quantity_sum(&[total, value])
        });
    let lookup_row_operations = rooted_edges
        .iter()
        .map(|edge| semijoin_pass_cost(input_rows[edge.child], input_rows[edge.parent]))
        .fold(AccessQuantity::exact(0), |total, value| {
            quantity_sum(&[total, value])
        });
    let expanded_rows = estimate_quantity(estimator.bound_relation(graph.relation), 1);
    let logical_row_operations = quantity_sum(&[
        reduction_row_operations,
        lookup_row_operations,
        expanded_rows,
    ]);
    let total = quantity_sum(&[input_work, logical_row_operations]);
    total.upper_bound?;
    let memory_upper = shredded_join_memory_bound(statistics, graph, &input_rows, rooted_edges)?;
    let memory = AccessQuantity {
        central: memory_upper,
        lower_bound: 0,
        upper_bound: Some(memory_upper),
    };
    Some((
        JoinGraphCost {
            logical_row_operations: total,
            reduction_row_operations: Some(reduction_row_operations),
            lookup_row_operations: Some(lookup_row_operations),
            expanded_rows: Some(expanded_rows),
            filter_row_operations: None,
            filtered_rows: None,
            filter_bytes: None,
            filter_paths: None,
            filter_builds: None,
            shared_filter_paths: None,
            pruned_filter_paths: None,
            filter_input_scans: None,
            filter_schedule_root: None,
            peak_retained_bytes: Some(memory),
            ordering: if required_order.is_empty() {
                AccessOrdering::NotRequired
            } else {
                AccessOrdering::SortRequired
            },
        },
        logical_row_operations,
        memory,
    ))
}

fn semijoin_pass_cost(source: AccessQuantity, target: AccessQuantity) -> AccessQuantity {
    AccessQuantity {
        central: source
            .central
            .saturating_add(target.central.saturating_mul(3)),
        lower_bound: source.lower_bound.saturating_add(target.lower_bound),
        upper_bound: source
            .upper_bound
            .zip(target.upper_bound)
            .map(|(source, target)| source.saturating_add(target.saturating_mul(3))),
    }
}

fn quantity_from_scenario(cost: MemoScenarioCost) -> AccessQuantity {
    AccessQuantity {
        central: cost.central,
        lower_bound: cost.lower,
        upper_bound: Some(cost.upper),
    }
}

fn shredded_join_memory_bound(
    statistics: &super::models::PlannerStats,
    graph: &super::acyclic_join::JoinGraph<'_>,
    input_rows: &[AccessQuantity],
    edges: &[super::physical::ShreddedJoinEdge],
) -> Option<u64> {
    let estimator = super::estimator::Estimator::new(statistics);
    let mut retained = 0u64;
    for (input, rows) in graph.inputs.iter().zip(input_rows) {
        let width = maximum_relation_row_width(&estimator, input)?;
        retained = retained.saturating_add(rows.upper_bound?.saturating_mul(width));
    }
    let mut transient_keys = 0u64;
    for edge in edges {
        let child = graph.inputs[edge.child];
        let key_width = encoded_join_key_width(&estimator, child, &edge.keys, false)?;
        let child_rows = input_rows[edge.child].upper_bound?;
        let parent_rows = input_rows[edge.parent].upper_bound?;
        retained = retained.saturating_add(
            child_rows.saturating_mul(key_width.saturating_add(u64::from(u64::BITS / 8))),
        );
        retained = retained.saturating_add(parent_rows.saturating_mul(u64::from(u64::BITS / 8)));
        transient_keys = transient_keys.max(child_rows.saturating_mul(key_width));
    }
    Some(retained.saturating_add(transient_keys))
}

fn encoded_join_key_width(
    estimator: &super::estimator::Estimator<'_>,
    input: &bound::Relation,
    keys: &[analysis::EquiJoinKey],
    left: bool,
) -> Option<u64> {
    if let Some(scan) = analysis::underlying_scan(input) {
        let table = scan.scan_table();
        return keys.iter().try_fold(0u64, |total, key| {
            let field = if left { &key.left } else { &key.right };
            let scan_field = scan
                .output()
                .fields
                .iter()
                .find(|candidate| candidate.slot == field.slot)?;
            let width = estimator.maximum_column_width(table, &scan_field.name)?;
            let width = encoded_scalar_width(scan_field.value_type.kind, width)?;
            Some(total.saturating_add(width))
        });
    }
    let RelationNode::Rows { values, .. } = &input.node else {
        return None;
    };
    keys.iter().try_fold(0u64, |total, key| {
        let field = if left { &key.left } else { &key.right };
        let column = input
            .output()
            .fields
            .iter()
            .position(|candidate| candidate.slot == field.slot)?;
        let width = values
            .iter()
            .map(|row| scalar_value_width(&row[column]))
            .max()
            .unwrap_or(0);
        let width = encoded_scalar_width(field.value_type.kind, width)?;
        Some(total.saturating_add(width))
    })
}

fn maximum_relation_row_width(
    estimator: &super::estimator::Estimator<'_>,
    relation: &bound::Relation,
) -> Option<u64> {
    if let Some(scan) = analysis::underlying_scan(relation) {
        return estimator.maximum_row_width(scan.scan_table());
    }
    let RelationNode::Rows { values, .. } = &relation.node else {
        return None;
    };
    Some(
        values
            .iter()
            .map(|row| {
                row.iter()
                    .map(scalar_value_width)
                    .fold(0u64, u64::saturating_add)
            })
            .max()
            .unwrap_or(0),
    )
}

fn scalar_value_width(value: &crate::engine::lir::Value) -> u64 {
    match value {
        crate::engine::lir::Value::Text(value) => value.len() as u64,
        crate::engine::lir::Value::Int64(_) | crate::engine::lir::Value::Float64(_) => 8,
        crate::engine::lir::Value::Bool(_) => 1,
        crate::engine::lir::Value::Null(_) => 0,
    }
}

fn encoded_scalar_width(kind: crate::engine::lir::Kind, value_width: u64) -> Option<u64> {
    match kind {
        crate::engine::lir::Kind::Text => Some(value_width.saturating_add(9)),
        crate::engine::lir::Kind::Int64 | crate::engine::lir::Kind::Float64 => Some(9),
        crate::engine::lir::Kind::Bool => Some(2),
        _ => None,
    }
}

fn strict_join_cost_winner(candidates: &[JoinCandidate]) -> Option<usize> {
    candidates
        .iter()
        .enumerate()
        .find_map(|(index, candidate)| {
            let upper_bound = candidate.cost?.logical_row_operations.upper_bound?;
            candidates
                .iter()
                .enumerate()
                .filter(|(_, other)| other.cost.is_some())
                .all(|(other_index, other)| {
                    other_index == index
                        || upper_bound
                            < other
                                .cost
                                .expect("eligible join cost")
                                .logical_row_operations
                                .lower_bound
                })
                .then_some(index)
        })
}

fn join_cost_is_more_expensive(candidate: Option<JoinCost>, winner: Option<JoinCost>) -> bool {
    let (Some(candidate), Some(winner)) = (candidate, winner) else {
        return false;
    };
    winner
        .logical_row_operations
        .upper_bound
        .is_some_and(|upper| upper < candidate.logical_row_operations.lower_bound)
}

fn quantity_product(left: AccessQuantity, right: AccessQuantity) -> AccessQuantity {
    AccessQuantity {
        central: left.central.saturating_mul(right.central),
        lower_bound: left.lower_bound.saturating_mul(right.lower_bound),
        upper_bound: left
            .upper_bound
            .zip(right.upper_bound)
            .map(|(left, right)| left.saturating_mul(right)),
    }
}

fn quantity_scale(quantity: AccessQuantity, factor: u64) -> AccessQuantity {
    AccessQuantity {
        central: quantity.central.saturating_mul(factor),
        lower_bound: quantity.lower_bound.saturating_mul(factor),
        upper_bound: quantity
            .upper_bound
            .map(|value| value.saturating_mul(factor)),
    }
}

fn quantity_sum(values: &[AccessQuantity]) -> AccessQuantity {
    values
        .iter()
        .fold(AccessQuantity::exact(0), |total, value| AccessQuantity {
            central: total.central.saturating_add(value.central),
            lower_bound: total.lower_bound.saturating_add(value.lower_bound),
            upper_bound: total
                .upper_bound
                .zip(value.upper_bound)
                .map(|(left, right)| left.saturating_add(right)),
        })
}

fn table_width(
    statistics: Option<&super::models::PlannerStats>,
    table: &crate::engine::catalog::model::Table,
) -> Option<u64> {
    statistics.and_then(|statistics| {
        super::estimator::Estimator::new(statistics).average_row_width(table)
    })
}

fn primary_key_cost() -> AccessCost {
    let entries = AccessQuantity {
        central: 1,
        lower_bound: 0,
        upper_bound: Some(1),
    };
    AccessCost {
        expected_entries: super::estimator::Estimate {
            cardinality: 1,
            interval: super::estimator::EstimateInterval::Range {
                lower_bound: 0,
                upper_bound: 1,
            },
            source: super::estimator::EstimateSource::Structural,
            sample_size: 0,
            changes_since_collection: 0,
            age: std::time::Duration::ZERO,
        },
        point_gets: entries,
        range_scans: AccessQuantity::exact(0),
        logical_row_operations: entries,
        decoded_bytes: None,
        ordering: AccessOrdering::Satisfied,
    }
}

fn table_cost(
    estimate: Option<super::estimator::Estimate>,
    row_width: Option<u64>,
    node: &NodeKind,
    required_order: &[bound::BoundOrderTerm],
) -> Option<AccessCost> {
    let estimate = estimate?;
    let entries = estimate_quantity(estimate, 1);
    Some(AccessCost {
        expected_entries: estimate,
        point_gets: AccessQuantity::exact(0),
        range_scans: AccessQuantity::exact(1),
        logical_row_operations: entries,
        decoded_bytes: row_width.map(|width| multiply_quantity(entries, width)),
        ordering: ordering(node, required_order),
    })
}

#[allow(clippy::too_many_arguments)]
fn index_cost(
    statistics: Option<&super::models::PlannerStats>,
    table: &crate::engine::catalog::model::Table,
    columns: &[&str],
    values: &[ConstValue],
    range: Option<&RangeSpec>,
    table_estimate: Option<super::estimator::Estimate>,
    table_bounds: Option<(u64, Option<u64>)>,
    row_width: Option<u64>,
    node: &NodeKind,
    required_order: &[bound::BoundOrderTerm],
    unique_point: bool,
    prefix_estimates: &mut Vec<AccessPrefixEstimate>,
) -> Option<AccessCost> {
    let statistics = statistics?;
    let estimator = super::estimator::Estimator::new(statistics);
    let estimate = if unique_point {
        super::estimator::Estimate {
            cardinality: 1,
            interval: super::estimator::EstimateInterval::Range {
                lower_bound: 0,
                upper_bound: 1,
            },
            source: super::estimator::EstimateSource::Structural,
            sample_size: 0,
            changes_since_collection: 0,
            age: std::time::Duration::ZERO,
        }
    } else if columns.is_empty() && range.is_none() {
        table_estimate?
    } else {
        let estimate = if let Some(cached) = prefix_estimates.iter().find(|cached| {
            cached
                .columns
                .iter()
                .map(String::as_str)
                .eq(columns.iter().copied())
                && cached.values == values
                && cached.range.as_ref() == range
        }) {
            cached.estimate
        } else {
            let estimate = estimator.access_prefix(table, columns, values, range);
            prefix_estimates.push(AccessPrefixEstimate {
                columns: columns.iter().map(|column| (*column).to_owned()).collect(),
                values: values.to_vec(),
                range: range.cloned(),
                estimate,
            });
            estimate
        };
        if let Some(estimate) = estimate {
            estimate
        } else {
            let (_, upper_bound) = table_bounds?;
            super::estimator::Estimate {
                cardinality: upper_bound.unwrap_or_default(),
                interval: upper_bound.map_or(
                    super::estimator::EstimateInterval::Unknown,
                    |upper_bound| super::estimator::EstimateInterval::Range {
                        lower_bound: 0,
                        upper_bound,
                    },
                ),
                source: super::estimator::EstimateSource::Synopsis,
                sample_size: 0,
                changes_since_collection: 0,
                age: std::time::Duration::ZERO,
            }
        }
    };
    let entries = estimate_quantity(estimate, 1);
    Some(AccessCost {
        expected_entries: estimate,
        point_gets: entries,
        range_scans: AccessQuantity::exact(1),
        logical_row_operations: multiply_quantity(entries, 2),
        decoded_bytes: row_width.map(|width| multiply_quantity(entries, width)),
        ordering: ordering(node, required_order),
    })
}

fn estimate_quantity(estimate: super::estimator::Estimate, multiplier: u64) -> AccessQuantity {
    use super::estimator::EstimateInterval;

    let (lower_bound, upper_bound) = match estimate.interval {
        EstimateInterval::Exact => (estimate.cardinality, Some(estimate.cardinality)),
        EstimateInterval::Range {
            lower_bound,
            upper_bound,
        }
        | EstimateInterval::AttributedRange {
            lower_bound,
            upper_bound,
            ..
        } => (lower_bound, Some(upper_bound)),
        EstimateInterval::LowerBound { lower_bound } => (lower_bound, None),
        EstimateInterval::Confidence { .. } | EstimateInterval::Unknown => (0, None),
    };
    AccessQuantity {
        central: estimate.cardinality.saturating_mul(multiplier),
        lower_bound: lower_bound.saturating_mul(multiplier),
        upper_bound: upper_bound.map(|value| value.saturating_mul(multiplier)),
    }
}

fn multiply_quantity(quantity: AccessQuantity, multiplier: u64) -> AccessQuantity {
    AccessQuantity {
        central: quantity.central.saturating_mul(multiplier),
        lower_bound: quantity.lower_bound.saturating_mul(multiplier),
        upper_bound: quantity
            .upper_bound
            .map(|value| value.saturating_mul(multiplier)),
    }
}

fn ordering(node: &NodeKind, required_order: &[bound::BoundOrderTerm]) -> AccessOrdering {
    if required_order.is_empty() {
        AccessOrdering::NotRequired
    } else if satisfies_order(node, required_order) {
        AccessOrdering::Satisfied
    } else {
        AccessOrdering::SortRequired
    }
}

fn strict_cost_winner(candidates: &[AccessCandidate], fallback: usize) -> Option<usize> {
    let fallback_ordering = candidates.get(fallback)?.cost?.ordering;
    let mut winner = None;
    for (index, candidate) in candidates.iter().enumerate() {
        let cost = candidate.cost?;
        if fallback_ordering == AccessOrdering::Satisfied
            && cost.ordering == AccessOrdering::SortRequired
        {
            continue;
        }
        let Some(upper_bound) = cost.logical_row_operations.upper_bound else {
            continue;
        };
        if candidates.iter().enumerate().all(|(other_index, other)| {
            if other_index == index {
                return true;
            }
            other.cost.is_some_and(|other| {
                (fallback_ordering == AccessOrdering::Satisfied
                    && other.ordering == AccessOrdering::SortRequired)
                    || upper_bound < other.logical_row_operations.lower_bound
            })
        }) {
            if winner.is_some() {
                return None;
            }
            winner = Some(index);
        }
    }
    winner
}

fn rejection_reason(
    candidate: &AccessCandidate,
    winner: &AccessCandidate,
    structural_fallback: bool,
) -> AccessRejectionReason {
    let (Some(candidate_cost), Some(winner_cost)) = (candidate.cost, winner.cost) else {
        return AccessRejectionReason::InconclusiveEvidence;
    };
    if winner_cost.ordering == AccessOrdering::Satisfied
        && candidate_cost.ordering == AccessOrdering::SortRequired
    {
        return AccessRejectionReason::OrderingRegression;
    }
    if winner_cost.logical_row_operations.upper_bound.is_none()
        || candidate_cost.logical_row_operations.upper_bound.is_none()
    {
        return AccessRejectionReason::InconclusiveEvidence;
    }
    if winner_cost
        .logical_row_operations
        .upper_bound
        .is_some_and(|upper_bound| upper_bound < candidate_cost.logical_row_operations.lower_bound)
    {
        return AccessRejectionReason::MoreExpensive;
    }
    if structural_fallback {
        AccessRejectionReason::StructuralFallback
    } else {
        AccessRejectionReason::OverlappingCost
    }
}

fn score(
    node: &NodeKind,
    equality_prefix_len: usize,
    has_range: bool,
    required_order: &[bound::BoundOrderTerm],
) -> usize {
    (equality_prefix_len << 2)
        | (usize::from(has_range) << 1)
        | usize::from(!required_order.is_empty() && satisfies_order(node, required_order))
}

fn satisfies_order(node: &NodeKind, required: &[bound::BoundOrderTerm]) -> bool {
    if required.is_empty() {
        return true;
    }
    let (provided, singleton) = provided_order(node);
    singleton
        || (required.len() <= provided.len()
            && required.iter().zip(provided).all(|(term, provided)| {
                !term.descending
                    && matches!(&term.expression, bound::Expr::SlotRef { slot, .. } if *slot == provided)
            }))
}

fn provided_order(node: &NodeKind) -> (Vec<SlotId>, bool) {
    match node {
        NodeKind::PrimaryKeyGet { .. } => (Vec::new(), true),
        NodeKind::TableScan { scan, .. } => (primary_key_slots(scan), false),
        NodeKind::IndexRangeScan {
            scan,
            index,
            equality_prefix,
            ..
        } => {
            let table = scan.scan_table();
            let mut slots = table
                .index_column_names(index)
                .into_iter()
                .skip(equality_prefix.len())
                .filter_map(|column| scan.output().lookup(column).map(|field| field.slot))
                .collect::<Vec<_>>();
            slots.extend(primary_key_slots(scan));
            (slots, false)
        }
        NodeKind::Filter { input, .. }
        | NodeKind::Attach { input, .. }
        | NodeKind::Slice { input, .. }
        | NodeKind::JoinGraphChoice { input, .. } => provided_order(&input.kind),
        _ => (Vec::new(), false),
    }
}

fn primary_key_slots(scan: &bound::Relation) -> Vec<SlotId> {
    let table = scan.scan_table();
    table
        .primary_key
        .iter()
        .filter_map(|column| scan.output().lookup(column).map(|field| field.slot))
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::engine::lir::{BinaryOp, Kind, Type, Value};
    use crate::engine::planner::models::{
        ColumnGroupSynopsis, ColumnSynopsis, DEGREE_SEQUENCE_FORMAT_VERSION, DegreeSequenceNorms,
        DegreeSequenceSegment, DegreeSequenceSynopsis, MostCommonColumnGroup, MostCommonValue,
        PlannerStats, RANGE_DISTRIBUTION_FORMAT_VERSION, RangeDistribution,
        RangeDistributionBucket, SynopsisCountBounds, SynopsisCoverage, SynopsisModel,
        SynopsisValue,
    };

    use super::super::estimator::EstimateSource;
    use super::super::physical::JoinGraphDecisionBasis;
    use super::*;
    use crate::engine::planner::test_support::{column, query, scan, table};

    fn complete_stats(
        table: &crate::engine::catalog::model::Table,
        observed_rows: u64,
        changes_since_collection: u64,
    ) -> PlannerStats {
        let mut statistics = PlannerStats::empty();
        statistics.synopsis_models.insert(
            table.schema_id,
            SynopsisModel {
                table: table.schema_id,
                observed_rows,
                coverage: SynopsisCoverage::Complete,
                sample_size: observed_rows,
                changes_since_collection,
                table_existence_generation: table.existence_generation.get(),
                collected_at_unix_micros: 0,
                catalog_version: 1,
                columns: Vec::new(),
                column_groups: Vec::new(),
                predicate_conditioned_degrees: Vec::new(),
            },
        );
        statistics
    }

    fn mcv_stats(
        table: &crate::engine::catalog::model::Table,
        column_name: &str,
        values: &[(&str, u64)],
    ) -> PlannerStats {
        let mut statistics = complete_stats(table, 1_000, 0);
        let column = table.column(column_name).expect("test column");
        statistics
            .synopsis_models
            .get_mut(&table.schema_id)
            .expect("test synopsis")
            .columns
            .push(ColumnSynopsis {
                column: column.schema_id,
                value_generation: column.value_generation.get(),
                null_fraction: 0.0,
                null_count: 0,
                distinct: values.len() as u64 + 10,
                distinct_is_exact: false,
                average_width: 8,
                maximum_width: Some(8),
                minimum: None,
                maximum: None,
                most_common_values: values
                    .iter()
                    .map(|(value, frequency)| MostCommonValue {
                        value: SynopsisValue::Text((*value).into()),
                        frequency: *frequency,
                        maximum_error: 0,
                    })
                    .collect(),
                range_distribution: None,
                degree_sequence: None,
            });
        statistics
    }

    fn range_stats(
        table: &crate::engine::catalog::model::Table,
        changes_since_collection: u64,
    ) -> PlannerStats {
        let mut statistics = complete_stats(table, 1_000, changes_since_collection);
        let column = table.column("board_id").expect("test column");
        let bucket = |lower: &str, upper: &str, rows, cumulative_rows, lower_rows, upper_rows| {
            RangeDistributionBucket {
                lower: SynopsisValue::Text(lower.into()),
                upper: SynopsisValue::Text(upper.into()),
                rows: SynopsisCountBounds::exact(rows),
                cumulative_rows: SynopsisCountBounds::exact(cumulative_rows),
                lower_endpoint_rows: SynopsisCountBounds::exact(lower_rows),
                upper_endpoint_rows: SynopsisCountBounds::exact(upper_rows),
            }
        };
        statistics
            .synopsis_models
            .get_mut(&table.schema_id)
            .expect("test synopsis")
            .columns
            .push(ColumnSynopsis {
                column: column.schema_id,
                value_generation: column.value_generation.get(),
                null_fraction: 0.0,
                null_count: 0,
                distinct: 5,
                distinct_is_exact: true,
                average_width: 1,
                maximum_width: Some(1),
                minimum: Some("\"a\"".into()),
                maximum: Some("\"e\"".into()),
                most_common_values: Vec::new(),
                range_distribution: Some(RangeDistribution {
                    format_version: RANGE_DISTRIBUTION_FORMAT_VERSION,
                    coverage: SynopsisCoverage::Complete,
                    sample_size: 1_000,
                    value_generation: column.value_generation.get(),
                    collected_row_count: 1_000,
                    buckets: vec![
                        bucket("a", "a", 100, 100, 100, 100),
                        bucket("b", "d", 800, 900, 100, 100),
                        bucket("e", "e", 100, 1_000, 100, 100),
                    ],
                }),
                degree_sequence: None,
            });
        statistics
    }

    fn range_query(
        lower: (&str, bool),
        upper: (&str, bool),
    ) -> (bound::Query, crate::engine::catalog::model::Table) {
        let scan = scan();
        let table = scan.scan_table().clone();
        let lower = bound::Expr::binary(
            if lower.1 { BinaryOp::Gte } else { BinaryOp::Gt },
            column(&scan, "board_id"),
            bound::Expr::literal(Value::Text(lower.0.into())),
        );
        let upper = bound::Expr::binary(
            if upper.1 { BinaryOp::Lte } else { BinaryOp::Lt },
            column(&scan, "board_id"),
            bound::Expr::literal(Value::Text(upper.0.into())),
        );
        (
            query(
                bound::Relation::filter(scan, bound::Expr::binary(BinaryOp::And, lower, upper)),
                3,
            ),
            table,
        )
    }

    fn join_table(
        schema_id: u32,
        physical_id: &str,
        name: &str,
    ) -> crate::engine::catalog::model::Table {
        let mut value = table();
        value.schema_id = crate::engine::catalog::identity::SchemaId::new(schema_id).unwrap();
        value.id = physical_id.into();
        value.name = name.into();
        for (index, column) in value.columns.iter_mut().enumerate() {
            column.id = format!("{physical_id}-c{}", index + 1).into();
            column.schema_id = crate::engine::catalog::identity::SchemaId::new(
                schema_id
                    .saturating_mul(10)
                    .saturating_add(index as u32 + 1),
            )
            .unwrap();
        }
        value.indexes[0].id = format!("{physical_id}-i1").into();
        value.indexes[0].logical_id = format!("{physical_id}-board-status").into();
        value.indexes[0].column_ids =
            vec![value.columns[1].id.clone(), value.columns[2].id.clone()];
        value
    }

    fn join_relation(
        left_table: crate::engine::catalog::model::Table,
        right_table: crate::engine::catalog::model::Table,
        left_column: &str,
        right_column: &str,
        kind: lir::JoinKind,
    ) -> bound::Query {
        let left = bound::Relation::scan(left_table, "left", vec![SlotId(0), SlotId(1), SlotId(2)]);
        let right =
            bound::Relation::scan(right_table, "right", vec![SlotId(3), SlotId(4), SlotId(5)]);
        let left_key = left.output().lookup(left_column).unwrap();
        let right_key = right.output().lookup(right_column).unwrap();
        let on = bound::Expr::binary(
            BinaryOp::Eq,
            bound::Expr::slot(
                left_key.slot,
                left_key.name.clone(),
                left_key.value_type.clone(),
            ),
            bound::Expr::slot(
                right_key.slot,
                right_key.name.clone(),
                right_key.value_type.clone(),
            ),
        );
        query(bound::Relation::join(left, right, kind, on), 6)
    }

    fn add_join_synopsis(
        statistics: &mut PlannerStats,
        table: &crate::engine::catalog::model::Table,
        rows: u64,
        key: &str,
        common: &[(&str, u64)],
    ) {
        let mut synopsis = complete_stats(table, rows, 0)
            .synopsis_models
            .remove(&table.schema_id)
            .unwrap();
        synopsis.columns = table
            .columns
            .iter()
            .map(|column| ColumnSynopsis {
                column: column.schema_id,
                value_generation: column.value_generation.get(),
                null_fraction: 0.0,
                null_count: 0,
                distinct: if column.name == key {
                    common.len().max(1) as u64
                } else {
                    rows
                },
                distinct_is_exact: column.name == key && !common.is_empty(),
                average_width: 8,
                maximum_width: Some(8),
                minimum: None,
                maximum: None,
                most_common_values: if column.name == key {
                    common
                        .iter()
                        .map(|(value, frequency)| MostCommonValue {
                            value: SynopsisValue::Text((*value).into()),
                            frequency: *frequency,
                            maximum_error: 0,
                        })
                        .collect()
                } else {
                    Vec::new()
                },
                range_distribution: None,
                degree_sequence: None,
            })
            .collect();
        statistics.synopsis_models.insert(table.schema_id, synopsis);
    }

    fn add_complete_join_domains(
        statistics: &mut PlannerStats,
        table: &crate::engine::catalog::model::Table,
        rows: u64,
        domains: &[(&str, &[(&str, u64)])],
    ) {
        let mut synopsis = complete_stats(table, rows, 0)
            .synopsis_models
            .remove(&table.schema_id)
            .unwrap();
        synopsis.columns = table
            .columns
            .iter()
            .map(|column| {
                let values = domains
                    .iter()
                    .find(|(name, _)| *name == column.name)
                    .map_or(&[][..], |(_, values)| *values);
                ColumnSynopsis {
                    column: column.schema_id,
                    value_generation: column.value_generation.get(),
                    null_fraction: 0.0,
                    null_count: 0,
                    distinct: values.len() as u64,
                    distinct_is_exact: !values.is_empty(),
                    average_width: 8,
                    maximum_width: Some(8),
                    minimum: None,
                    maximum: None,
                    most_common_values: values
                        .iter()
                        .map(|(value, frequency)| MostCommonValue {
                            value: SynopsisValue::Text((*value).into()),
                            frequency: *frequency,
                            maximum_error: 0,
                        })
                        .collect(),
                    range_distribution: None,
                    degree_sequence: None,
                }
            })
            .collect();
        statistics.synopsis_models.insert(table.schema_id, synopsis);
    }

    fn add_complete_join_group(
        statistics: &mut PlannerStats,
        table: &crate::engine::catalog::model::Table,
        columns: &[&str],
        values: &[(&[&str], u64)],
    ) {
        let synopsis = statistics
            .synopsis_models
            .get_mut(&table.schema_id)
            .unwrap();
        let columns = columns
            .iter()
            .map(|name| table.column(name).unwrap())
            .collect::<Vec<_>>();
        synopsis.column_groups.push(ColumnGroupSynopsis {
            columns: columns.iter().map(|column| column.schema_id).collect(),
            value_generations: columns
                .iter()
                .map(|column| column.value_generation.get())
                .collect(),
            null_count: 0,
            distinct: values.len() as u64,
            distinct_is_exact: true,
            most_common_values: values
                .iter()
                .map(|(values, frequency)| MostCommonColumnGroup {
                    values: values
                        .iter()
                        .map(|value| SynopsisValue::Text((*value).into()))
                        .collect(),
                    frequency: *frequency,
                    maximum_error: 0,
                })
                .collect(),
            degree_sequence: None,
        });
    }

    fn add_degree_norms(
        statistics: &mut PlannerStats,
        table: &crate::engine::catalog::model::Table,
        key: &str,
        rows: u64,
        distinct: u64,
        l2_upper: u64,
        l_infinity: u64,
    ) {
        let column = table.column(key).unwrap();
        let synopsis = statistics
            .synopsis_models
            .get_mut(&table.schema_id)
            .unwrap();
        let column_synopsis = synopsis
            .columns
            .iter_mut()
            .find(|candidate| candidate.column == column.schema_id)
            .unwrap();
        column_synopsis.distinct = distinct;
        column_synopsis.distinct_is_exact = true;
        column_synopsis.degree_sequence = Some(DegreeSequenceSynopsis {
            format_version: DEGREE_SEQUENCE_FORMAT_VERSION,
            coverage: SynopsisCoverage::Complete,
            sample_size: rows,
            value_generations: vec![column.value_generation.get()],
            collected_row_count: rows,
            non_null_rows: rows,
            distinct_values: distinct,
            distinct_is_exact: true,
            norms: DegreeSequenceNorms {
                l1: rows,
                l2_upper,
                l_infinity,
                exact: true,
            },
            segments: vec![DegreeSequenceSegment {
                rank_start: 0,
                rank_end: distinct,
                frequency_upper: l_infinity,
            }],
        });
    }

    fn three_way_join_query(
        first: crate::engine::catalog::model::Table,
        second: crate::engine::catalog::model::Table,
        third: crate::engine::catalog::model::Table,
        final_kind: lir::JoinKind,
    ) -> bound::Query {
        let first = bound::Relation::scan(first, "first", vec![SlotId(0), SlotId(1), SlotId(2)]);
        let second = bound::Relation::scan(second, "second", vec![SlotId(3), SlotId(4), SlotId(5)]);
        let third = bound::Relation::scan(third, "third", vec![SlotId(6), SlotId(7), SlotId(8)]);
        let equality = |left: &bound::Relation, right: &bound::Relation| {
            let left = left.output().lookup("board_id").unwrap();
            let right = right.output().lookup("board_id").unwrap();
            bound::Expr::binary(
                BinaryOp::Eq,
                bound::Expr::slot(left.slot, left.name.clone(), left.value_type.clone()),
                bound::Expr::slot(right.slot, right.name.clone(), right.value_type.clone()),
            )
        };
        let first_second_on = equality(&first, &second);
        let second_third_on = equality(&second, &third);
        let first_second =
            bound::Relation::join(first, second, lir::JoinKind::Inner, first_second_on);
        query(
            bound::Relation::join(first_second, third, final_kind, second_third_on),
            9,
        )
    }

    fn literal_three_way_join_query(rows: usize, cyclic: bool) -> bound::Query {
        use crate::engine::lir::Field;

        let input = |scope: &str, slot: usize| {
            bound::Relation::rows(
                scope,
                vec![Field {
                    name: "key".into(),
                    slot: SlotId(slot),
                    value_type: Type::scalar(Kind::Text, false),
                }],
                (0..rows)
                    .map(|_| vec![Value::Text("same".into())])
                    .collect(),
            )
        };
        let first = input("first", 0);
        let second = input("second", 1);
        let third = input("third", 2);
        let equality = |left: SlotId, right: SlotId| {
            bound::Expr::binary(
                BinaryOp::Eq,
                bound::Expr::slot(left, "key", Type::scalar(Kind::Text, false)),
                bound::Expr::slot(right, "key", Type::scalar(Kind::Text, false)),
            )
        };
        let first_second = bound::Relation::join(
            first,
            second,
            lir::JoinKind::Inner,
            equality(SlotId(0), SlotId(1)),
        );
        let final_predicate = if cyclic {
            bound::Expr::binary(
                BinaryOp::And,
                equality(SlotId(1), SlotId(2)),
                equality(SlotId(0), SlotId(2)),
            )
        } else {
            equality(SlotId(1), SlotId(2))
        };
        query(
            bound::Relation::join(first_second, third, lir::JoinKind::Inner, final_predicate),
            3,
        )
    }

    fn selective_cyclic_literal_join_query(rows: usize) -> bound::Query {
        use crate::engine::lir::Field;

        let input = |scope: &str, slots: [usize; 2], values: [&str; 2]| {
            bound::Relation::rows(
                scope,
                vec![
                    Field {
                        name: "first_key".into(),
                        slot: SlotId(slots[0]),
                        value_type: Type::scalar(Kind::Text, false),
                    },
                    Field {
                        name: "second_key".into(),
                        slot: SlotId(slots[1]),
                        value_type: Type::scalar(Kind::Text, false),
                    },
                ],
                (0..rows)
                    .map(|_| vec![Value::Text(values[0].into()), Value::Text(values[1].into())])
                    .collect(),
            )
        };
        let equality = |left: usize, right: usize| {
            bound::Expr::binary(
                BinaryOp::Eq,
                bound::Expr::slot(SlotId(left), "join_key", Type::scalar(Kind::Text, false)),
                bound::Expr::slot(SlotId(right), "join_key", Type::scalar(Kind::Text, false)),
            )
        };
        let first = input("first", [0, 1], ["a", "b"]);
        let second = input("second", [2, 3], ["b", "c"]);
        let third = input("third", [4, 5], ["c", "different"]);
        let first_second =
            bound::Relation::join(first, second, lir::JoinKind::Inner, equality(1, 2));
        let final_predicate = bound::Expr::binary(BinaryOp::And, equality(3, 4), equality(0, 5));
        query(
            bound::Relation::join(first_second, third, lir::JoinKind::Inner, final_predicate),
            6,
        )
    }

    fn selective_cyclic_table_join_query(
        first_table: crate::engine::catalog::model::Table,
        second_table: crate::engine::catalog::model::Table,
        third_table: crate::engine::catalog::model::Table,
    ) -> bound::Query {
        let first =
            bound::Relation::scan(first_table, "first", vec![SlotId(0), SlotId(1), SlotId(2)]);
        let second = bound::Relation::scan(
            second_table,
            "second",
            vec![SlotId(3), SlotId(4), SlotId(5)],
        );
        let third =
            bound::Relation::scan(third_table, "third", vec![SlotId(6), SlotId(7), SlotId(8)]);
        let equality =
            |left: &bound::Relation, left_name: &str, right: &bound::Relation, right_name: &str| {
                let left = left.output().lookup(left_name).unwrap();
                let right = right.output().lookup(right_name).unwrap();
                bound::Expr::binary(
                    BinaryOp::Eq,
                    bound::Expr::slot(left.slot, left.name.clone(), left.value_type.clone()),
                    bound::Expr::slot(right.slot, right.name.clone(), right.value_type.clone()),
                )
            };
        let first_second_on = equality(&first, "board_id", &second, "board_id");
        let second_third = equality(&second, "status", &third, "board_id");
        let first_third = equality(&first, "status", &third, "status");
        let first_second =
            bound::Relation::join(first, second, lir::JoinKind::Inner, first_second_on);
        query(
            bound::Relation::join(
                first_second,
                third,
                lir::JoinKind::Inner,
                bound::Expr::binary(BinaryOp::And, second_third, first_third),
            ),
            9,
        )
    }

    #[test]
    fn structural_mode_classifies_acyclic_and_cyclic_join_graphs() {
        let statistics = PlannerStats::empty();
        for (cyclic, classification, rejection) in [
            (
                false,
                JoinGraphClassification::Acyclic,
                JoinGraphRejectionReason::StructuralFallback,
            ),
            (
                true,
                JoinGraphClassification::Cyclic,
                JoinGraphRejectionReason::CyclicGraph,
            ),
        ] {
            let planned = plan_query_with_context(
                &literal_three_way_join_query(2, cyclic),
                PlanOptions::default(),
                PlanningContext {
                    statistics: Some(&statistics),
                },
            );
            let NodeKind::JoinGraphChoice { input, decision } = &planned.plan.root.kind else {
                panic!("expected join graph choice")
            };
            assert_eq!(decision.classification, classification);
            assert!(matches!(input.kind, NodeKind::NestedLoopJoin { .. }));
            assert_eq!(decision.candidates[1].rejection_reason, Some(rejection));
            assert!(decision.candidates[0].chosen);
        }
    }

    #[test]
    fn cost_mode_selects_shredded_yannakakis_with_strict_work_dominance() {
        let statistics = PlannerStats::empty();
        let planned = plan_query_with_context(
            &literal_three_way_join_query(30, false),
            PlanOptions {
                mode: PlannerMode::Cost,
                ..PlanOptions::default()
            },
            PlanningContext {
                statistics: Some(&statistics),
            },
        );
        let mut decision = None;
        let mut shredded = false;
        planned.plan.walk(&mut |node| {
            if let NodeKind::JoinGraphChoice {
                input,
                decision: candidate,
            } = &node.kind
            {
                decision = Some(candidate.clone());
                shredded = matches!(input.kind, NodeKind::ShreddedYannakakisJoin { .. });
            }
        });
        let decision = decision.expect("acyclic choice");
        assert!(shredded, "{decision:#?}");
        assert!(decision.candidates[1].chosen);
        assert_eq!(
            decision.candidates[1].decision_basis,
            Some(JoinGraphDecisionBasis::BoundedNoRegret)
        );
        let binary = decision.candidates[0]
            .cost
            .expect("binary cost")
            .logical_row_operations;
        let shredded = decision.candidates[1]
            .cost
            .expect("shredded cost")
            .logical_row_operations;
        assert!(join_region::scenario_cost_dominates(shredded, binary));
        let text = super::super::explain::PlanView::new(&planned.plan).render();
        assert!(text.contains("classification=Acyclic"));
        assert!(text.contains("ShreddedYannakakisJoin"));
        assert!(text.contains("bounded_no_regret"));
    }

    #[test]
    fn cost_mode_selects_predicate_transfer_for_a_selective_cyclic_graph() {
        let statistics = PlannerStats::empty();
        let planned = plan_query_with_context(
            &selective_cyclic_literal_join_query(30),
            PlanOptions {
                mode: PlannerMode::Cost,
                ..PlanOptions::default()
            },
            PlanningContext {
                statistics: Some(&statistics),
            },
        );
        let mut selected = false;
        let mut schedule = None;
        let mut decision = None;
        planned.plan.walk(&mut |node| {
            if let NodeKind::JoinGraphChoice {
                input,
                decision: candidate,
            } = &node.kind
            {
                selected = matches!(input.kind, NodeKind::PredicateTransferJoin { .. });
                if let NodeKind::PredicateTransferJoin {
                    schedule: candidate,
                    ..
                } = &input.kind
                {
                    schedule = Some(candidate.clone());
                }
                decision = Some(candidate.clone());
            }
        });
        let decision = decision.expect("join graph choice");
        assert_eq!(decision.classification, JoinGraphClassification::Cyclic);
        assert!(selected, "{decision:#?}");
        assert_eq!(
            decision.candidates[1].rejection_reason,
            Some(JoinGraphRejectionReason::CyclicGraph)
        );
        assert!(decision.candidates[2].chosen);
        assert_eq!(
            decision.candidates[2].decision_basis,
            Some(JoinGraphDecisionBasis::BoundedNoRegret)
        );
        let binary = decision.candidates[0]
            .cost
            .expect("binary cost")
            .logical_row_operations;
        let transfer = decision.candidates[2]
            .cost
            .expect("predicate transfer cost")
            .logical_row_operations;
        assert!(join_region::scenario_cost_dominates(transfer, binary));
        let schedule = schedule.expect("predicate transfer schedule");
        assert_eq!(schedule.forward.edges.len(), 2);
        assert_eq!(schedule.backward.edges.len(), 2);
        assert_eq!(schedule.pruned_paths, 2);
        assert_eq!(
            schedule
                .forward
                .edges
                .iter()
                .map(|edge| (edge.source, edge.target))
                .collect::<Vec<_>>(),
            vec![(1, 0), (2, 1)]
        );
        assert_eq!(
            schedule
                .backward
                .edges
                .iter()
                .map(|edge| (edge.source, edge.target))
                .collect::<Vec<_>>(),
            vec![(0, 1), (0, 2)]
        );
        let text = super::super::explain::PlanView::new(&planned.plan).render();
        assert!(text.contains("classification=Cyclic"));
        assert!(text.contains("PredicateTransferJoin"));
        assert!(text.contains("filter=cascade"));
        assert!(text.contains("storagePruning=compatible_ordered_key_prefix"));
        assert!(text.contains("bitsPerKey=20"));
        assert!(text.contains("hashFunctions=7"));
        assert!(text.contains("schedule=asymmetric"));
        assert!(text.contains("prunedPaths=2"));
        assert!(text.contains("bounded_no_regret"));
    }

    #[test]
    fn complete_table_domains_bound_pruned_asymmetric_transfer() {
        let first = join_table(10, "first-table", "first_items");
        let second = join_table(20, "second-table", "second_items");
        let third = join_table(30, "third-table", "third_items");
        let query = selective_cyclic_table_join_query(first.clone(), second.clone(), third.clone());
        let mut statistics = PlannerStats::empty();
        add_complete_join_domains(
            &mut statistics,
            &first,
            60,
            &[
                ("board_id", &[("a", 30), ("d", 30)]),
                ("status", &[("b", 30), ("x", 30)]),
            ],
        );
        add_complete_join_group(
            &mut statistics,
            &first,
            &["board_id", "status"],
            &[(&["a", "b"], 30), (&["d", "x"], 30)],
        );
        add_complete_join_domains(
            &mut statistics,
            &second,
            60,
            &[
                ("board_id", &[("q", 30), ("x", 30)]),
                ("status", &[("c", 30), ("y", 30)]),
            ],
        );
        add_complete_join_group(
            &mut statistics,
            &second,
            &["board_id", "status"],
            &[(&["q", "c"], 30), (&["x", "y"], 30)],
        );
        add_complete_join_domains(
            &mut statistics,
            &third,
            30,
            &[("board_id", &[("c", 30)]), ("status", &[("d", 30)])],
        );
        add_complete_join_group(
            &mut statistics,
            &third,
            &["board_id", "status"],
            &[(&["c", "d"], 30)],
        );

        let planned = plan_query_with_context(
            &query,
            PlanOptions {
                mode: PlannerMode::Cost,
                ..PlanOptions::default()
            },
            PlanningContext {
                statistics: Some(&statistics),
            },
        );
        let mut decision = None;
        planned.plan.walk(&mut |node| {
            if let NodeKind::JoinGraphChoice {
                decision: candidate,
                ..
            } = &node.kind
            {
                decision = Some(candidate.clone());
            }
        });
        let decision = decision.unwrap();
        let transfer = decision.candidates[2].cost.unwrap();
        assert_eq!(transfer.filtered_rows, Some(AccessQuantity::exact(0)));
        assert_eq!(transfer.filter_paths, Some(4));
        assert_eq!(transfer.pruned_filter_paths, Some(2));
        assert_eq!(transfer.filter_schedule_root, Some(0));
        assert_eq!(
            decision.candidates[2].rejection_reason,
            Some(JoinGraphRejectionReason::MoreExpensive)
        );
    }

    #[test]
    fn complete_table_domains_prune_a_proven_no_effect_path() {
        let first = join_table(10, "first-table", "first_items");
        let second = join_table(20, "second-table", "second_items");
        let third = join_table(30, "third-table", "third_items");
        let query = selective_cyclic_table_join_query(first.clone(), second.clone(), third.clone());
        let mut statistics = PlannerStats::empty();
        add_complete_join_domains(
            &mut statistics,
            &first,
            30,
            &[("board_id", &[("shared", 30)]), ("status", &[("left", 30)])],
        );
        add_complete_join_domains(
            &mut statistics,
            &second,
            30,
            &[
                ("board_id", &[("shared", 30)]),
                ("status", &[("middle", 30)]),
            ],
        );
        add_complete_join_domains(
            &mut statistics,
            &third,
            30,
            &[
                ("board_id", &[("different", 30)]),
                ("status", &[("left", 30)]),
            ],
        );
        let planned = plan_query_with_context(
            &query,
            PlanOptions {
                mode: PlannerMode::Cost,
                ..PlanOptions::default()
            },
            PlanningContext {
                statistics: Some(&statistics),
            },
        );
        let mut decision = None;
        planned.plan.walk(&mut |node| {
            if let NodeKind::JoinGraphChoice {
                decision: candidate,
                ..
            } = &node.kind
            {
                decision = Some(candidate.clone());
            }
        });
        let transfer = decision.unwrap().candidates[2].cost.unwrap();
        assert_eq!(transfer.filter_paths, Some(3));
        assert_eq!(transfer.pruned_filter_paths, Some(3));
        assert_eq!(transfer.filtered_rows, Some(AccessQuantity::exact(0)));
    }

    #[test]
    fn synopsis_drift_rejects_table_predicate_transfer_bounds() {
        let first = join_table(10, "first-table", "first_items");
        let second = join_table(20, "second-table", "second_items");
        let third = join_table(30, "third-table", "third_items");
        let query = selective_cyclic_table_join_query(first.clone(), second.clone(), third.clone());
        let mut statistics = PlannerStats::empty();
        for table in [&first, &second, &third] {
            add_complete_join_domains(
                &mut statistics,
                table,
                30,
                &[("board_id", &[("shared", 30)]), ("status", &[("left", 30)])],
            );
        }
        statistics
            .synopsis_models
            .get_mut(&second.schema_id)
            .unwrap()
            .changes_since_collection = 1;

        let planned = plan_query_with_context(
            &query,
            PlanOptions {
                mode: PlannerMode::Cost,
                ..PlanOptions::default()
            },
            PlanningContext {
                statistics: Some(&statistics),
            },
        );
        let mut decision = None;
        planned.plan.walk(&mut |node| {
            if let NodeKind::JoinGraphChoice {
                decision: candidate,
                ..
            } = &node.kind
            {
                decision = Some(candidate.clone());
            }
        });
        assert_eq!(
            decision.unwrap().candidates[2].rejection_reason,
            Some(JoinGraphRejectionReason::MissingEvidence)
        );
    }

    #[test]
    fn cost_mode_selects_a_lower_regret_bushy_join_tree() {
        let first = join_table(10, "first-table", "first_items");
        let second = join_table(20, "second-table", "second_items");
        let third = join_table(30, "third-table", "third_items");
        let query = three_way_join_query(
            first.clone(),
            second.clone(),
            third.clone(),
            lir::JoinKind::Inner,
        );
        let mut statistics = PlannerStats::empty();
        add_join_synopsis(&mut statistics, &first, 1_000, "board_id", &[]);
        add_join_synopsis(&mut statistics, &second, 1_000, "board_id", &[]);
        add_join_synopsis(&mut statistics, &third, 1, "board_id", &[]);

        let structural = plan_query_with_context(
            &query,
            PlanOptions::default(),
            PlanningContext {
                statistics: Some(&statistics),
            },
        );
        let structural_root = &structural.plan.memo.roots[0];
        assert_eq!(structural_root.join_search.as_ref().unwrap().input_count, 3);
        assert_eq!(
            structural_root.selected_expression,
            structural_root.structural_expression
        );

        let cost = plan_query_with_context(
            &query,
            PlanOptions {
                mode: PlannerMode::Cost,
                ..PlanOptions::default()
            },
            PlanningContext {
                statistics: Some(&statistics),
            },
        );
        let root = &cost.plan.memo.roots[0];
        assert_ne!(root.selected_expression, root.structural_expression);
        assert_eq!(root.selected_proof.len(), 1);
        assert_eq!(
            root.selected_proof[0].rule,
            super::super::memo::MemoRule::InnerJoinGraph
        );
        let structural = root
            .candidates
            .iter()
            .find(|candidate| candidate.structural)
            .unwrap();
        let selected = root
            .candidates
            .iter()
            .find(|candidate| candidate.selected)
            .unwrap();
        assert_eq!(
            selected.origin,
            super::super::memo::MemoCandidateOrigin::JoinGraphSearch
        );
        assert_eq!(selected.decision_basis.as_deref(), Some("minimax_regret"));
        assert!(
            selected.logical_row_operations.unwrap().upper
                < structural.logical_row_operations.unwrap().upper
        );
        assert!(selected.maximum_regret.unwrap() < structural.maximum_regret.unwrap());
        assert!(!selected.tail_regression);
        let text = super::super::explain::PlanView::new(&cost.plan).render();
        assert!(text.contains("join-search inputs=3 edges=2"));
        assert!(text.contains("origin=join_graph_search"));
        assert!(text.contains("minimax_regret"));
    }

    #[test]
    fn outer_join_boundary_prevents_join_graph_search() {
        let first = join_table(10, "first-table", "first_items");
        let second = join_table(20, "second-table", "second_items");
        let third = join_table(30, "third-table", "third_items");
        let query = three_way_join_query(
            first.clone(),
            second.clone(),
            third.clone(),
            lir::JoinKind::Left,
        );
        let mut statistics = PlannerStats::empty();
        add_join_synopsis(&mut statistics, &first, 1_000, "board_id", &[]);
        add_join_synopsis(&mut statistics, &second, 1_000, "board_id", &[]);
        add_join_synopsis(&mut statistics, &third, 1, "board_id", &[]);

        let planned = plan_query_with_context(
            &query,
            PlanOptions {
                mode: PlannerMode::Cost,
                ..PlanOptions::default()
            },
            PlanningContext {
                statistics: Some(&statistics),
            },
        );
        assert!(planned.plan.memo.roots[0].join_search.is_none());
        assert!(matches!(
            planned.plan.root.kind,
            NodeKind::NestedLoopJoin {
                kind: lir::JoinKind::Left,
                ..
            }
        ));
    }

    #[test]
    fn join_graph_effort_limit_reports_stop_and_keeps_structural_tree() {
        let first = join_table(10, "first-table", "first_items");
        let second = join_table(20, "second-table", "second_items");
        let third = join_table(30, "third-table", "third_items");
        let query = three_way_join_query(
            first.clone(),
            second.clone(),
            third.clone(),
            lir::JoinKind::Inner,
        );
        let mut statistics = PlannerStats::empty();
        add_join_synopsis(&mut statistics, &first, 1_000, "board_id", &[]);
        add_join_synopsis(&mut statistics, &second, 1_000, "board_id", &[]);
        add_join_synopsis(&mut statistics, &third, 1, "board_id", &[]);

        let planned = plan_query_with_context(
            &query,
            PlanOptions {
                mode: PlannerMode::Cost,
                memo_limits: super::super::memo::MemoLimits {
                    max_planning_effort: 1,
                    ..super::super::memo::MemoLimits::default()
                },
                ..PlanOptions::default()
            },
            PlanningContext {
                statistics: Some(&statistics),
            },
        );
        let root = &planned.plan.memo.roots[0];
        assert_eq!(
            root.join_search.as_ref().unwrap().stop_reason,
            Some(super::super::join_search::JoinSearchStopReason::PlanningEffortLimit)
        );
        assert_eq!(root.join_search.as_ref().unwrap().alternatives, 0);
        assert_eq!(root.selected_expression, root.structural_expression);
        assert_eq!(
            planned.plan.memo.stop_reason,
            Some(super::super::memo::MemoStopReason::PlanningEffortLimit)
        );
    }

    #[test]
    fn cost_mode_selects_an_indexed_lookup_for_a_small_left_input() {
        let left = join_table(10, "left-table", "left_items");
        let right = join_table(20, "right-table", "right_items");
        let query = join_relation(
            left.clone(),
            right.clone(),
            "board_id",
            "id",
            lir::JoinKind::Inner,
        );
        let mut statistics = PlannerStats::empty();
        add_join_synopsis(&mut statistics, &left, 10, "board_id", &[]);
        add_join_synopsis(&mut statistics, &right, 1_000, "id", &[]);
        let planned = plan_query_with_context(
            &query,
            PlanOptions {
                mode: PlannerMode::Cost,
                ..PlanOptions::default()
            },
            PlanningContext {
                statistics: Some(&statistics),
            },
        );
        let NodeKind::IndexedLookupJoin {
            decision, right, ..
        } = &planned.plan.root.kind
        else {
            panic!("expected indexed lookup join")
        };
        assert!(matches!(
            right.kind,
            NodeKind::PrimaryKeyGet { .. } | NodeKind::IndexRangeScan { .. }
        ));
        assert_eq!(
            decision.candidates[2].decision_basis,
            Some(JoinDecisionBasis::CostDominance)
        );
    }

    #[test]
    fn cost_mode_selects_hash_join_for_complete_skew_evidence() {
        let left = join_table(10, "left-table", "left_items");
        let right = join_table(20, "right-table", "right_items");
        let query = join_relation(
            left.clone(),
            right.clone(),
            "board_id",
            "board_id",
            lir::JoinKind::Inner,
        );
        let mut statistics = PlannerStats::empty();
        add_join_synopsis(
            &mut statistics,
            &left,
            1_000,
            "board_id",
            &[("a", 500), ("b", 500)],
        );
        add_join_synopsis(
            &mut statistics,
            &right,
            1_000,
            "board_id",
            &[("a", 1), ("b", 999)],
        );
        let planned = plan_query_with_context(
            &query,
            PlanOptions {
                mode: PlannerMode::Cost,
                ..PlanOptions::default()
            },
            PlanningContext {
                statistics: Some(&statistics),
            },
        );
        let NodeKind::HashJoin {
            decision,
            memory_limit_bytes,
            ..
        } = &planned.plan.root.kind
        else {
            panic!("expected hash join")
        };
        assert_eq!(*memory_limit_bytes, DEFAULT_HASH_JOIN_MEMORY_LIMIT_BYTES);
        assert_eq!(
            decision.candidates[1].decision_basis,
            Some(JoinDecisionBasis::CostDominance)
        );
        assert!(
            decision.candidates[1]
                .cost
                .and_then(|cost| cost.peak_retained_bytes)
                .and_then(|bytes| bytes.upper_bound)
                .is_some_and(|bytes| bytes > 0)
        );
    }

    #[test]
    fn degree_norm_bound_makes_a_hash_join_selectable() {
        let left = join_table(10, "left-table", "left_items");
        let right = join_table(20, "right-table", "right_items");
        let query = join_relation(
            left.clone(),
            right.clone(),
            "board_id",
            "board_id",
            lir::JoinKind::Inner,
        );
        let mut statistics = PlannerStats::empty();
        add_join_synopsis(&mut statistics, &left, 1_000, "board_id", &[]);
        add_join_synopsis(&mut statistics, &right, 1_000, "board_id", &[]);
        add_degree_norms(&mut statistics, &left, "board_id", 1_000, 101, 901, 900);
        add_degree_norms(&mut statistics, &right, "board_id", 1_000, 101, 503, 500);

        let planned = plan_query_with_context(
            &query,
            PlanOptions {
                mode: PlannerMode::Cost,
                ..PlanOptions::default()
            },
            PlanningContext {
                statistics: Some(&statistics),
            },
        );
        let NodeKind::HashJoin { decision, .. } = &planned.plan.root.kind else {
            panic!("expected hash join")
        };
        let cost = decision.candidates[1].cost.unwrap();
        assert_eq!(
            cost.expected_output_rows.interval,
            super::super::estimator::EstimateInterval::AttributedRange {
                lower_bound: 0,
                upper_bound: 453_203,
                lower_source: super::super::estimator::EstimateBoundSource::Structural,
                central_source:
                    super::super::estimator::EstimateBoundSource::FactorizedDistribution,
                upper_source: super::super::estimator::EstimateBoundSource::DegreeSequenceNorms,
                drift_widened: false,
            }
        );
        assert_eq!(
            decision.candidates[1].decision_basis,
            Some(JoinDecisionBasis::CostDominance)
        );
    }

    #[test]
    fn hash_memory_limit_and_structural_mode_keep_nested_loop() {
        let left = join_table(10, "left-table", "left_items");
        let right = join_table(20, "right-table", "right_items");
        let query = join_relation(
            left.clone(),
            right.clone(),
            "board_id",
            "board_id",
            lir::JoinKind::Left,
        );
        let mut statistics = PlannerStats::empty();
        add_join_synopsis(
            &mut statistics,
            &left,
            1_000,
            "board_id",
            &[("a", 500), ("b", 500)],
        );
        add_join_synopsis(
            &mut statistics,
            &right,
            1_000,
            "board_id",
            &[("a", 1), ("b", 999)],
        );
        let memory_limited = plan_query_with_context(
            &query,
            PlanOptions {
                mode: PlannerMode::Cost,
                hash_join_memory_limit_bytes: 1,
                ..PlanOptions::default()
            },
            PlanningContext {
                statistics: Some(&statistics),
            },
        );
        let NodeKind::NestedLoopJoin { decision, .. } = &memory_limited.plan.root.kind else {
            panic!("expected nested-loop join")
        };
        assert_eq!(
            decision.candidates[1].rejection_reason,
            Some(JoinRejectionReason::MemoryLimit)
        );
        let structural = plan_query_with_context(
            &query,
            PlanOptions::default(),
            PlanningContext {
                statistics: Some(&statistics),
            },
        );
        assert!(matches!(
            structural.plan.root.kind,
            NodeKind::NestedLoopJoin { .. }
        ));
    }

    #[test]
    fn complete_primary_key_equality_uses_point_get_with_residual() {
        let scan = scan();
        let predicate = bound::Expr::binary(
            BinaryOp::Eq,
            column(&scan, "id"),
            bound::Expr::literal(Value::Text("t1".into())),
        );
        let plan = plan_query(
            &query(bound::Relation::filter(scan, predicate.clone()), 3),
            PlanOptions::default(),
        );
        assert!(super::super::explain::print_plan(&plan).contains("PKGet tasks [id = \"t1\"]"));
        let view = serde_json::to_value(super::super::explain::PlanView::new(&plan)).unwrap();
        assert_eq!(view["root"]["op"], "Filter");
        assert_eq!(view["root"]["children"][0]["op"], "PKGet");
        let NodeKind::Filter {
            input,
            predicate: residual,
        } = plan.root.kind
        else {
            panic!("expected residual filter")
        };
        assert_eq!(residual, predicate);
        assert!(matches!(input.kind, NodeKind::PrimaryKeyGet { .. }));
        assert_eq!(plan.dependencies.table_existence.len(), 1);
        assert!(plan.dependencies.index_access.is_empty());
    }

    #[test]
    fn longest_index_prefix_and_range_win_but_full_scan_oracle_is_available() {
        let scan = scan();
        let predicate = bound::Expr::binary(
            BinaryOp::And,
            bound::Expr::binary(
                BinaryOp::Eq,
                column(&scan, "board_id"),
                bound::Expr::literal(Value::Text("b1".into())),
            ),
            bound::Expr::binary(
                BinaryOp::Gte,
                column(&scan, "status"),
                bound::Expr::literal(Value::Text("m".into())),
            ),
        );
        let bound = query(bound::Relation::filter(scan, predicate.clone()), 3);
        let chosen = plan_query(&bound, PlanOptions::default());
        let NodeKind::Filter { input, .. } = &chosen.root.kind else {
            panic!("expected filter")
        };
        assert!(matches!(
            &input.kind,
            NodeKind::IndexRangeScan {
                equality_prefix,
                range: Some(RangeSpec { column, .. }),
                ..
            } if equality_prefix.len() == 1 && column == "status"
        ));
        assert_eq!(chosen.dependencies.index_access.len(), 1);

        let oracle = plan_query(
            &bound,
            PlanOptions {
                full_scan_only: true,
                ..PlanOptions::default()
            },
        );
        let NodeKind::Filter { input, .. } = oracle.root.kind else {
            panic!("expected filter")
        };
        assert!(matches!(input.kind, NodeKind::TableScan { .. }));
    }

    #[test]
    fn hard_small_table_bound_selects_scan_but_drift_range_keeps_structural_choice() {
        let scan = scan();
        let table = scan.scan_table().clone();
        let predicate = bound::Expr::binary(
            BinaryOp::Eq,
            column(&scan, "board_id"),
            bound::Expr::literal(Value::Text("b1".into())),
        );
        let bound = query(bound::Relation::filter(scan, predicate.clone()), 3);

        let exact = complete_stats(&table, 1, 0);
        let planned = plan_query_with_context(
            &bound,
            PlanOptions::default(),
            PlanningContext {
                statistics: Some(&exact),
            },
        );
        let NodeKind::Filter {
            input,
            predicate: residual,
        } = &planned.plan.root.kind
        else {
            panic!("expected filter")
        };
        assert_eq!(residual, &predicate);
        let NodeKind::TableScan { access, .. } = &input.kind else {
            panic!("expected table scan")
        };
        let winner = access
            .candidates
            .iter()
            .find(|candidate| candidate.chosen)
            .unwrap();
        assert_eq!(winner.method, "TableScan");
        assert_eq!(
            winner.decision_basis,
            Some(AccessDecisionBasis::BoundedRowWork)
        );
        assert_eq!(
            winner.estimated_row_work,
            Some(AccessRowWork {
                lower_bound: 1,
                upper_bound: Some(1),
            })
        );
        let rendered = super::super::explain::PlanView::new(&planned.plan).render();
        assert!(rendered.contains("✓ [bounded_row_work]"));

        let drifted = complete_stats(&table, 1, 1);
        let planned = plan_query_with_context(
            &bound,
            PlanOptions::default(),
            PlanningContext {
                statistics: Some(&drifted),
            },
        );
        let NodeKind::Filter { input, .. } = &planned.plan.root.kind else {
            panic!("expected filter")
        };
        let NodeKind::IndexRangeScan { access, .. } = &input.kind else {
            panic!("expected index range scan")
        };
        let winner = access
            .candidates
            .iter()
            .find(|candidate| candidate.chosen)
            .unwrap();
        assert_eq!(winner.decision_basis, Some(AccessDecisionBasis::Structural));
    }

    #[test]
    fn unique_point_bound_selects_index_when_scan_lower_bound_is_larger() {
        let mut table = table();
        let mut unique_index = table.indexes[0].clone();
        unique_index.id = "status-unique-index".into();
        unique_index.logical_id = "status-unique".into();
        unique_index.name = "tasks_status_unique_idx".into();
        unique_index.columns = vec!["status".into()];
        unique_index.column_ids = vec!["column-status".into()];
        unique_index.unique = true;
        table.indexes.push(unique_index);
        let scan = bound::Relation::scan(table.clone(), "t", vec![SlotId(0), SlotId(1), SlotId(2)]);
        let predicate = bound::Expr::binary(
            BinaryOp::And,
            bound::Expr::binary(
                BinaryOp::Eq,
                column(&scan, "board_id"),
                bound::Expr::literal(Value::Text("b1".into())),
            ),
            bound::Expr::binary(
                BinaryOp::Eq,
                column(&scan, "status"),
                bound::Expr::literal(Value::Text("open".into())),
            ),
        );
        let bound = query(bound::Relation::filter(scan, predicate), 3);
        let statistics = complete_stats(&table, 100, 0);

        let structural = plan_query(&bound, PlanOptions::default());
        let NodeKind::Filter { input, .. } = &structural.root.kind else {
            panic!("expected filter")
        };
        let NodeKind::IndexRangeScan { index, .. } = &input.kind else {
            panic!("expected index range scan")
        };
        assert_eq!(index.name, "tasks_board_status_idx");

        let planned = plan_query_with_context(
            &bound,
            PlanOptions::default(),
            PlanningContext {
                statistics: Some(&statistics),
            },
        );
        let NodeKind::Filter { input, .. } = &planned.plan.root.kind else {
            panic!("expected filter")
        };
        let NodeKind::IndexRangeScan { index, access, .. } = &input.kind else {
            panic!("expected index range scan")
        };
        assert_eq!(index.name, "tasks_status_unique_idx");
        let winner = access
            .candidates
            .iter()
            .find(|candidate| candidate.chosen)
            .unwrap();
        assert_eq!(
            winner.decision_basis,
            Some(AccessDecisionBasis::BoundedRowWork)
        );
        assert_eq!(
            winner.estimated_row_work,
            Some(AccessRowWork {
                lower_bound: 0,
                upper_bound: Some(2),
            })
        );
        let rendered = super::super::explain::PlanView::new(&planned.plan).render();
        assert!(rendered.contains("✓ [bounded_row_work]"));
    }

    #[test]
    fn cost_mode_selects_scan_for_common_value_and_index_for_rare_value() {
        let build = |value: &str| {
            let scan = scan();
            let table = scan.scan_table().clone();
            let predicate = bound::Expr::binary(
                BinaryOp::Eq,
                column(&scan, "board_id"),
                bound::Expr::literal(Value::Text(value.into())),
            );
            (query(bound::Relation::filter(scan, predicate), 3), table)
        };
        let (common, table) = build("hot");
        let statistics = mcv_stats(&table, "board_id", &[("hot", 700), ("needle", 1)]);
        let options = PlanOptions {
            mode: PlannerMode::Cost,
            ..PlanOptions::default()
        };
        let common = plan_query_with_context(
            &common,
            options,
            PlanningContext {
                statistics: Some(&statistics),
            },
        );
        let NodeKind::Filter { input, .. } = &common.plan.root.kind else {
            panic!("expected filter")
        };
        let NodeKind::TableScan { access, .. } = &input.kind else {
            panic!("expected table scan")
        };
        assert_eq!(
            access
                .candidates
                .iter()
                .find(|candidate| candidate.chosen)
                .and_then(|candidate| candidate.decision_basis),
            Some(AccessDecisionBasis::CostDominance)
        );

        let (rare, _) = build("needle");
        let rare = plan_query_with_context(
            &rare,
            options,
            PlanningContext {
                statistics: Some(&statistics),
            },
        );
        let NodeKind::Filter { input, .. } = &rare.plan.root.kind else {
            panic!("expected filter")
        };
        let NodeKind::IndexRangeScan { access, .. } = &input.kind else {
            panic!("expected index range scan")
        };
        let winner = access
            .candidates
            .iter()
            .find(|candidate| candidate.chosen)
            .expect("chosen access");
        assert_eq!(
            winner.decision_basis,
            Some(AccessDecisionBasis::CostDominance)
        );
        assert_eq!(
            winner.cost.expect("index cost").logical_row_operations,
            AccessQuantity {
                central: 2,
                lower_bound: 2,
                upper_bound: Some(2),
            }
        );
    }

    #[test]
    fn cost_mode_keeps_range_access_when_cost_ranges_overlap() {
        let scan = scan();
        let table = scan.scan_table().clone();
        let predicate = bound::Expr::binary(
            BinaryOp::Gte,
            column(&scan, "board_id"),
            bound::Expr::literal(Value::Text("m".into())),
        );
        let bound = query(bound::Relation::filter(scan, predicate), 3);
        let statistics = complete_stats(&table, 1_000, 0);
        let planned = plan_query_with_context(
            &bound,
            PlanOptions {
                mode: PlannerMode::Cost,
                ..PlanOptions::default()
            },
            PlanningContext {
                statistics: Some(&statistics),
            },
        );
        let NodeKind::Filter { input, .. } = &planned.plan.root.kind else {
            panic!("expected filter")
        };
        let NodeKind::IndexRangeScan { access, .. } = &input.kind else {
            panic!("expected index range scan")
        };
        assert_eq!(access.structural_fallback, 1);
        assert_eq!(
            access.candidates[1].decision_basis,
            Some(AccessDecisionBasis::Structural)
        );
    }

    #[test]
    fn cost_mode_selects_scan_for_a_proven_broad_range() {
        let (query, table) = range_query(("b", true), ("d", true));
        let statistics = range_stats(&table, 0);
        let planned = plan_query_with_context(
            &query,
            PlanOptions {
                mode: PlannerMode::Cost,
                ..PlanOptions::default()
            },
            PlanningContext {
                statistics: Some(&statistics),
            },
        );
        let NodeKind::Filter { input, .. } = &planned.plan.root.kind else {
            panic!("expected filter")
        };
        let NodeKind::TableScan { access, .. } = &input.kind else {
            panic!("expected table scan")
        };
        let winner = access
            .candidates
            .iter()
            .find(|candidate| candidate.chosen)
            .expect("chosen access");
        assert_eq!(winner.method, "TableScan");
        assert_eq!(
            winner.decision_basis,
            Some(AccessDecisionBasis::CostDominance)
        );
        let range = access
            .candidates
            .iter()
            .find(|candidate| candidate.method.contains("tasks_board_status_idx"))
            .and_then(|candidate| candidate.cost)
            .expect("range cost");
        assert_eq!(range.expected_entries.source, EstimateSource::Distribution);
        assert_eq!(range.logical_row_operations, AccessQuantity::exact(1_600));
    }

    #[test]
    fn cost_mode_retains_index_for_a_proven_narrow_range() {
        let (query, table) = range_query(("a", true), ("a", true));
        let statistics = range_stats(&table, 0);
        let planned = plan_query_with_context(
            &query,
            PlanOptions {
                mode: PlannerMode::Cost,
                ..PlanOptions::default()
            },
            PlanningContext {
                statistics: Some(&statistics),
            },
        );
        let NodeKind::Filter { input, .. } = &planned.plan.root.kind else {
            panic!("expected filter")
        };
        let NodeKind::IndexRangeScan { access, .. } = &input.kind else {
            panic!("expected index range scan")
        };
        let winner = access
            .candidates
            .iter()
            .find(|candidate| candidate.chosen)
            .expect("chosen access");
        assert_eq!(
            winner.decision_basis,
            Some(AccessDecisionBasis::CostDominance)
        );
        assert_eq!(
            winner.cost.expect("index cost").logical_row_operations,
            AccessQuantity::exact(200)
        );
    }

    #[test]
    fn range_drift_that_overlaps_scan_cost_keeps_the_structural_path() {
        let (query, table) = range_query(("b", true), ("d", true));
        let statistics = range_stats(&table, 400);
        let planned = plan_query_with_context(
            &query,
            PlanOptions {
                mode: PlannerMode::Cost,
                ..PlanOptions::default()
            },
            PlanningContext {
                statistics: Some(&statistics),
            },
        );
        let NodeKind::Filter { input, .. } = &planned.plan.root.kind else {
            panic!("expected filter")
        };
        let NodeKind::IndexRangeScan { access, .. } = &input.kind else {
            panic!("expected index range scan")
        };
        assert_eq!(access.structural_fallback, 1);
        assert_eq!(
            access.candidates[1].decision_basis,
            Some(AccessDecisionBasis::Structural)
        );
    }

    #[test]
    fn cost_mode_does_not_replace_an_ordered_structural_path() {
        let scan = scan();
        let table = scan.scan_table().clone();
        let predicate = bound::Expr::binary(
            BinaryOp::Eq,
            column(&scan, "board_id"),
            bound::Expr::literal(Value::Text("hot".into())),
        );
        let filtered = bound::Relation::filter(scan.clone(), predicate);
        let ordered = bound::Relation::order(
            filtered,
            vec![bound::BoundOrderTerm {
                expression: column(&scan, "status"),
                descending: false,
            }],
        );
        let statistics = mcv_stats(&table, "board_id", &[("hot", 700)]);
        let planned = plan_query_with_context(
            &query(ordered, 3),
            PlanOptions {
                mode: PlannerMode::Cost,
                ..PlanOptions::default()
            },
            PlanningContext {
                statistics: Some(&statistics),
            },
        );
        let NodeKind::Filter { input, .. } = &planned.plan.root.kind else {
            panic!("expected filter")
        };
        let NodeKind::IndexRangeScan { access, .. } = &input.kind else {
            panic!("expected ordered index range scan")
        };
        assert_eq!(access.structural_fallback, 1);
        assert_eq!(
            access.candidates[0].rejection_reason,
            Some(AccessRejectionReason::OrderingRegression)
        );
    }

    #[test]
    fn correlated_equality_cost_tie_keeps_the_structural_index() {
        let mut table = table();
        let mut duplicate = table.indexes[0].clone();
        duplicate.id = "board-status-index-copy".into();
        duplicate.logical_id = "board-status-copy".into();
        duplicate.name = "tasks_board_status_copy_idx".into();
        table.indexes.push(duplicate);
        let scan = bound::Relation::scan(table.clone(), "t", vec![SlotId(0), SlotId(1), SlotId(2)]);
        let predicate = bound::Expr::binary(
            BinaryOp::And,
            bound::Expr::binary(
                BinaryOp::Eq,
                column(&scan, "board_id"),
                bound::Expr::literal(Value::Text("b1".into())),
            ),
            bound::Expr::binary(
                BinaryOp::Eq,
                column(&scan, "status"),
                bound::Expr::literal(Value::Text("open".into())),
            ),
        );
        let board = table.column("board_id").expect("board column");
        let status = table.column("status").expect("status column");
        let mut statistics = complete_stats(&table, 1_000, 0);
        statistics
            .synopsis_models
            .get_mut(&table.schema_id)
            .expect("test synopsis")
            .column_groups
            .push(ColumnGroupSynopsis {
                columns: vec![board.schema_id, status.schema_id],
                value_generations: vec![
                    board.value_generation.get(),
                    status.value_generation.get(),
                ],
                null_count: 0,
                distinct: 100,
                distinct_is_exact: false,
                most_common_values: vec![MostCommonColumnGroup {
                    values: vec![
                        SynopsisValue::Text("b1".into()),
                        SynopsisValue::Text("open".into()),
                    ],
                    frequency: 10,
                    maximum_error: 0,
                }],
                degree_sequence: None,
            });
        let planned = plan_query_with_context(
            &query(bound::Relation::filter(scan, predicate), 3),
            PlanOptions {
                mode: PlannerMode::Cost,
                ..PlanOptions::default()
            },
            PlanningContext {
                statistics: Some(&statistics),
            },
        );
        let NodeKind::Filter { input, .. } = &planned.plan.root.kind else {
            panic!("expected filter")
        };
        let NodeKind::IndexRangeScan { index, access, .. } = &input.kind else {
            panic!("expected index range scan")
        };
        assert_eq!(index.name, "tasks_board_status_idx");
        assert_eq!(access.structural_fallback, 1);
        assert_eq!(
            access.candidates[1].decision_basis,
            Some(AccessDecisionBasis::Structural)
        );
        assert_eq!(
            access.candidates[2].rejection_reason,
            Some(AccessRejectionReason::OverlappingCost)
        );
        assert_eq!(
            access.candidates[1]
                .cost
                .expect("first index cost")
                .expected_entries
                .source,
            super::super::estimator::EstimateSource::ColumnGroup
        );
    }

    #[test]
    fn access_quantity_arithmetic_saturates() {
        let estimate = super::super::estimator::Estimate {
            cardinality: u64::MAX,
            interval: super::super::estimator::EstimateInterval::Range {
                lower_bound: u64::MAX - 1,
                upper_bound: u64::MAX,
            },
            source: super::super::estimator::EstimateSource::Synopsis,
            sample_size: u64::MAX,
            changes_since_collection: u64::MAX,
            age: std::time::Duration::ZERO,
        };
        assert_eq!(
            estimate_quantity(estimate, 2),
            AccessQuantity {
                central: u64::MAX,
                lower_bound: u64::MAX,
                upper_bound: Some(u64::MAX),
            }
        );
    }

    #[test]
    fn crossing_becomes_key_correlated_attach_in_a_fresh_slot() {
        let outer = scan();
        let inner = bound::Relation::scan(table(), "inner", vec![SlotId(3), SlotId(4), SlotId(5)]);
        let inner = bound::Relation::filter(
            inner.clone(),
            bound::Expr::binary(
                BinaryOp::Eq,
                column(&inner, "board_id"),
                bound::Expr::slot(SlotId(0), "outer.id", Type::scalar(Kind::Text, false)),
            ),
        );
        let root = bound::Relation::project(
            outer,
            "result",
            vec![bound::ProjectField {
                name: "has_tasks".into(),
                slot: SlotId(6),
                expression: bound::Expr::exists(inner),
            }],
        );
        let plan = plan_query(&query(root, 7), PlanOptions::default());
        let NodeKind::Project { input, fields } = plan.root.kind else {
            panic!("expected project")
        };
        assert_eq!(fields[0].slot, SlotId(6));
        let NodeKind::Attach { specifications, .. } = input.kind else {
            panic!("expected attach")
        };
        assert_eq!(specifications[0].slot, SlotId(6));
        assert_eq!(
            specifications[0].correlation.kind,
            analysis::CorrelationKind::Key
        );
        // A projection field which is itself a crossing writes directly to
        // its assigned field slot and therefore consumes no extra slot.
        assert_eq!(plan.next_slot, SlotId(7));
    }

    #[test]
    fn repeated_binding_references_materialize_one_sensitive_commitment() {
        use crate::engine::lir::{Field, RowType};

        let field = |slot| Field {
            name: "n".into(),
            slot: SlotId(slot),
            value_type: Type::scalar(Kind::Int64, false),
        };
        let body = bound::Relation::slice(
            bound::Relation::rows("body", vec![field(0)], vec![vec![Value::Int64(1)]]),
            0,
            Some(1),
        );
        let left = bound::Relation::reference("numbers", "left", vec![field(1)], vec![SlotId(0)]);
        let right = bound::Relation::reference("numbers", "right", vec![field(2)], vec![SlotId(0)]);
        let root = bound::Relation::concatenate(vec![left, right], "both", vec![field(3)]);
        let plan = plan_query(
            &bound::Query {
                root,
                cardinality: crate::engine::lir::RootCardinality::Many,
                bindings: vec![bound::Binding {
                    name: "numbers".into(),
                    output: RowType {
                        fields: vec![field(0)],
                    },
                    root: body,
                    plan_sensitive: true,
                    recursive: false,
                    step: None,
                    accumulation: None,
                }],
                next_slot: SlotId(4),
            },
            PlanOptions::default(),
        );

        assert!(plan.bindings[0].sensitive);
        assert!(matches!(
            plan.bindings[0].kind,
            BindingPlanKind::Derived {
                strategy: BindingStrategy::Materialize,
                ..
            }
        ));
        let rendered = super::super::explain::print_plan(&plan);
        assert!(rendered.contains("Binding numbers materialise plan-choice-sensitive"));
    }

    #[test]
    fn cost_mode_extracts_the_pareto_distinct_expression_with_a_proof() {
        let relation =
            bound::Relation::distinct(bound::Relation::distinct(bound::Relation::distinct(scan())));
        let query = query(relation, 3);
        let structural = plan_query(&query, PlanOptions::default());
        let cost = plan_query(
            &query,
            PlanOptions {
                mode: PlannerMode::Cost,
                ..PlanOptions::default()
            },
        );
        let distinct_count = |plan: &Plan| {
            let mut count = 0;
            plan.root.walk(&mut |node| {
                count += usize::from(matches!(node.kind, NodeKind::Distinct { .. }));
            });
            count
        };
        assert_eq!(distinct_count(&structural), 3);
        assert_eq!(distinct_count(&cost), 1);
        let root = &cost.memo.roots[0];
        assert_eq!(root.directed_alternatives, 2);
        assert_eq!(root.saturated_alternatives, 3);
        assert_eq!(root.selected_proof.len(), 2);
        assert_eq!(
            root.selected_proof
                .iter()
                .map(|proof| proof.rule)
                .collect::<Vec<_>>(),
            vec![
                super::super::memo::MemoRule::DistinctIdempotence,
                super::super::memo::MemoRule::DistinctIdempotence,
            ]
        );
        assert_eq!(
            root.candidates
                .iter()
                .filter(|candidate| candidate.pareto)
                .count(),
            1
        );
    }

    #[test]
    fn memo_budget_stop_keeps_the_structural_expression() {
        let relation = bound::Relation::distinct(bound::Relation::distinct(scan()));
        let query = query(relation, 3);
        let plan = plan_query(
            &query,
            PlanOptions {
                mode: PlannerMode::Cost,
                memo_limits: MemoLimits {
                    max_rule_applications: 0,
                    ..MemoLimits::default()
                },
                ..PlanOptions::default()
            },
        );
        let mut count = 0;
        plan.root.walk(&mut |node| {
            count += usize::from(matches!(node.kind, NodeKind::Distinct { .. }));
        });
        assert_eq!(count, 2);
        assert_eq!(
            plan.memo.stop_reason,
            Some(super::super::memo::MemoStopReason::RuleApplicationLimit)
        );
        assert!(plan.memo.roots[0].selected_proof.is_empty());
    }
}
