//! Pure lowering from validated bound LIR to executor-facing operators.

use crate::engine::lir::bound::{self, RelationNode};
use crate::engine::lir::{self, SlotId};

use super::analysis::{self, ConstValue, ScanConstraints};
use super::dependencies::prepare_catalog_dependencies;
use super::physical::{
    AccessCandidate, AccessDecision, AccessDecisionBasis, AccessRowWork, AttachSpec, BindingPlan,
    BindingPlanKind, BindingStrategy, CrossingKind, Node, NodeKind, PhysicalField, Plan, RangeSpec,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlanOptions {
    /// Force scans while retaining the full residual predicate. This is the
    /// physical-planning conformance oracle used to prove access equivalence.
    pub full_scan_only: bool,
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
    };
    let bindings = query
        .bindings
        .iter()
        .map(|binding| {
            let anchor = planner.plan(&binding.root, &[]);
            let kind = match (&binding.step, binding.accumulation) {
                (Some(step), Some(accumulation)) if binding.recursive => {
                    BindingPlanKind::Recursive {
                        anchor: Box::new(anchor),
                        step: Box::new(planner.plan(step, &[])),
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
    let root = planner.plan(&query.root, &[]);

    let mut plan = Plan {
        bindings,
        root,
        cardinality: query.cardinality,
        output: query.root.output().clone(),
        dependencies: Default::default(),
        next_slot: planner.next_slot,
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
}

impl Planner<'_> {
    fn allocate_slot(&mut self) -> SlotId {
        let slot = self.next_slot;
        self.next_slot.0 += 1;
        slot
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
            } => NodeKind::NestedLoopJoin {
                left: Box::new(self.plan(left, &[])),
                right: Box::new(self.plan(right, &[])),
                kind: *kind,
                on: on.clone(),
                right_output: right.output().clone(),
            },
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
                        decision_basis: None,
                        chosen: true,
                    }],
                },
            };
        }

        let mut options = vec![NodeKind::TableScan {
            scan: Box::new(constraints.scan.clone()),
            decode_columns: Vec::new(),
            access: Default::default(),
        }];
        let table_bounds = self
            .statistics
            .and_then(|statistics| {
                super::estimator::Estimator::new(statistics).scan_for_table(table)
            })
            .and_then(hard_cardinality_bounds);
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
            decision_basis: None,
            chosen: false,
        }];
        let mut chosen = 0;
        let mut unique_points = Vec::new();
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
                equality_prefix,
                range,
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
                decision_basis: None,
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

        candidates[chosen].chosen = true;
        let decision = AccessDecision { candidates };
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
        } => Some((lower_bound, Some(upper_bound))),
        EstimateInterval::Confidence { .. } | EstimateInterval::Unknown => None,
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
        | NodeKind::Slice { input, .. } => provided_order(&input.kind),
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
    use crate::engine::planner::models::{PlannerStats, SynopsisCoverage, SynopsisModel};

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
            },
        );
        statistics
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
        assert!(rendered.contains("{rowWork=1..1} ✓ [bounded_row_work]"));

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
        assert_eq!(winner.decision_basis, None);
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
        assert!(rendered.contains("{rowWork=0..2} ✓ [bounded_row_work]"));
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
}
