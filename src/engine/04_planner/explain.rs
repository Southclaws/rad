//! Stable JSON and text observability views for physical plans.

use std::fmt::Write;

use serde::Serialize;

use crate::engine::lir::format::print_expression;

use super::analysis::{ConstValue, Correlation, CorrelationKind};
use super::physical::{
    AccessCandidate, BindingPlanKind, BindingStrategy, JoinCandidate, JoinGraphCandidate,
    JoinGraphDecision, Node, NodeKind, Plan, RangeSpec,
};

pub const PLAN_VIEW_FORMAT: &str = "rad-plan-view-v2";

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanView {
    pub format: &'static str,
    pub fingerprint: crate::engine::lir::fingerprint::Fingerprint,
    pub planner_mode: String,
    pub cardinality: String,
    pub memo: super::memo::MemoReport,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<PlanBindingView>,
    pub root: PlanNodeView,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub estimates: Vec<PlanEstimateView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statistics_published_at_micros: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statistics_snapshot_identity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statistics_scope: Option<super::models::StatisticsScope>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanEstimateView {
    /// `root`, `relation`, or `table:<schema id>`.
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation: Option<crate::engine::lir::fingerprint::Fingerprint>,
    #[serde(flatten)]
    pub estimate: super::estimator::Estimate,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanBindingView {
    pub name: String,
    pub strategy: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub plan_choice_sensitive: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub recursive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accumulation: Option<String>,
    pub plan: PlanNodeView,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<PlanNodeView>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PlanNodeView {
    pub op: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation: Option<crate::engine::lir::fingerprint::Fingerprint>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub access: Vec<AccessCandidate>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub join: Vec<JoinCandidate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub join_graph: Option<JoinGraphDecision>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<PlanNodeView>,
    #[serde(skip)]
    render: Render,
}

#[derive(Clone, Debug, Default, PartialEq)]
enum Render {
    #[default]
    Normal,
    Lines(Vec<String>),
    Attach {
        specifications: Vec<(String, PlanNodeView)>,
        input: Box<PlanNodeView>,
    },
}

impl PlanView {
    pub fn new(plan: &Plan) -> Self {
        Self::with_mode(plan, super::PlannerMode::Structural)
    }

    pub fn with_mode(plan: &Plan, planner_mode: super::PlannerMode) -> Self {
        Self {
            format: PLAN_VIEW_FORMAT,
            fingerprint: plan.fingerprint(),
            planner_mode: planner_mode.as_str().into(),
            cardinality: plan.cardinality.as_str().into(),
            memo: plan.memo.clone(),
            bindings: plan
                .bindings
                .iter()
                .map(|binding| {
                    let (strategy, recursive, accumulation, plan, step) = match &binding.kind {
                        BindingPlanKind::Derived { plan, strategy } => (
                            binding_strategy(*strategy).into(),
                            false,
                            None,
                            view_node(plan),
                            None,
                        ),
                        BindingPlanKind::Recursive {
                            anchor,
                            step,
                            accumulation,
                            ..
                        } => (
                            binding_strategy(BindingStrategy::Materialize).into(),
                            true,
                            Some(accumulation.as_str().into()),
                            view_node(anchor),
                            Some(view_node(step)),
                        ),
                    };
                    PlanBindingView {
                        name: binding.name.clone(),
                        strategy,
                        plan_choice_sensitive: binding.sensitive,
                        recursive,
                        accumulation,
                        plan,
                        step,
                    }
                })
                .collect(),
            root: view_node(&plan.root),
            estimates: Vec::new(),
            statistics_published_at_micros: None,
            statistics_snapshot_identity: None,
            statistics_scope: None,
        }
    }

    pub fn annotate_estimates(
        &mut self,
        stats: &crate::engine::planner::models::PlannerStats,
        root: Option<super::estimator::Estimate>,
        query: &crate::engine::lir::bound::Query,
        plan: &Plan,
    ) {
        self.statistics_published_at_micros =
            Some(stats.published_at.as_micros().min(u128::from(u64::MAX)) as u64);
        self.statistics_snapshot_identity = Some(stats.snapshot_identity.clone());
        self.statistics_scope = Some(stats.scope);
        let estimator = super::estimator::Estimator::new(stats);
        let root_relation = crate::engine::lir::fingerprint::relation_family(&query.root);
        if let Some(estimate) = root {
            self.estimates.push(PlanEstimateView {
                target: "root".into(),
                relation: Some(root_relation),
                estimate,
            });
        }
        let mut tables = std::collections::HashSet::new();
        plan.walk(&mut |node| {
            let scan = match &node.kind {
                NodeKind::PrimaryKeyGet { scan, .. }
                | NodeKind::TableScan { scan, .. }
                | NodeKind::IndexRangeScan { scan, .. } => scan,
                _ => return,
            };
            let table = scan.scan_table();
            if tables.insert(table.schema_id)
                && let Some(estimate) = estimator.scan_for_table(table)
            {
                self.estimates.push(PlanEstimateView {
                    target: format!("table:{}", table.schema_id.get()),
                    relation: None,
                    estimate,
                });
            }
        });

        let mut relations = std::collections::HashSet::from([root_relation]);
        let mut add = |relation: &crate::engine::lir::bound::Relation| {
            let fingerprint = crate::engine::lir::fingerprint::relation_family(relation);
            if relations.insert(fingerprint) {
                self.estimates.push(PlanEstimateView {
                    target: "relation".into(),
                    relation: Some(fingerprint),
                    estimate: estimator.bound_relation(relation),
                });
            }
        };
        crate::engine::lir::inspect::walk_relation(&query.root, &mut add, &mut |_| {});
        for binding in &query.bindings {
            crate::engine::lir::inspect::walk_relation(&binding.root, &mut add, &mut |_| {});
            if let Some(step) = &binding.step {
                crate::engine::lir::inspect::walk_relation(step, &mut add, &mut |_| {});
            }
        }
    }

    /// Diagnostic text including non-trivial access alternatives and scores.
    pub fn render(&self) -> String {
        self.render_inner(true)
    }

    fn render_inner(&self, show_access: bool) -> String {
        let mut output = String::new();
        writeln!(output, "Plan card={}", self.cardinality).unwrap();
        if show_access && !self.memo.roots.is_empty() {
            let stop = self
                .memo
                .stop_reason
                .map_or_else(|| "complete".into(), |reason| format!("stopped:{reason:?}"));
            writeln!(
                output,
                "  Memo strategy={} groups={} alternatives={} rules={} effort={} {stop}",
                self.memo.strategy,
                self.memo.usage.groups,
                self.memo.usage.alternatives,
                self.memo.usage.rule_applications,
                self.memo.usage.planning_effort,
            )
            .unwrap();
            for root in &self.memo.roots {
                if root.structural_expression != root.selected_expression || !root.proofs.is_empty()
                {
                    writeln!(
                        output,
                        "    Group {} {} directed={} saturated={} selected={}",
                        root.group,
                        root.name,
                        root.directed_alternatives,
                        root.saturated_alternatives,
                        root.selected_expression,
                    )
                    .unwrap();
                    for proof in &root.selected_proof {
                        writeln!(
                            output,
                            "      proof {:?} {} -> {} [{}]",
                            proof.rule,
                            proof.from,
                            proof.to,
                            proof.preconditions.join(","),
                        )
                        .unwrap();
                    }
                    if let Some(search) = &root.join_search {
                        let search_stop = search.stop_reason.map_or_else(
                            || "complete".into(),
                            |reason| format!("stopped:{reason:?}"),
                        );
                        writeln!(
                            output,
                            "      join-search inputs={} edges={} subsets={} partitions={} states={} effort={} {search_stop}",
                            search.input_count,
                            search.edge_count,
                            search.connected_subsets,
                            search.considered_partitions,
                            search.retained_states,
                            search.planning_effort,
                        )
                        .unwrap();
                        for candidate in &root.candidates {
                            let work = candidate.logical_row_operations.map_or_else(
                                || "unknown".into(),
                                |work| {
                                    format!(
                                        "{}..{} central={}",
                                        work.lower, work.upper, work.central
                                    )
                                },
                            );
                            let regret = candidate
                                .maximum_regret
                                .map_or_else(|| "unknown".into(), |regret| regret.to_string());
                            let decision = if candidate.selected {
                                candidate.decision_basis.as_deref().unwrap_or("selected")
                            } else {
                                candidate.rejection_reason.as_deref().unwrap_or("rejected")
                            };
                            writeln!(
                                output,
                                "      candidate {} origin={} work={work} maxRegret={regret} tailRegression={} {decision}",
                                candidate.expression,
                                candidate.origin.label(),
                                candidate.tail_regression,
                            )
                            .unwrap();
                        }
                    }
                }
            }
        }
        for binding in &self.bindings {
            let sensitive = if binding.plan_choice_sensitive {
                " plan-choice-sensitive"
            } else {
                ""
            };
            if binding.recursive {
                writeln!(
                    output,
                    "  Binding {} {}{} recursive accumulation={}",
                    binding.name,
                    binding.strategy,
                    sensitive,
                    binding.accumulation.as_deref().unwrap_or("")
                )
                .unwrap();
                writeln!(output, "    Anchor").unwrap();
                write_node(&mut output, &binding.plan, 3, show_access);
                writeln!(output, "    Step").unwrap();
                if let Some(step) = &binding.step {
                    write_node(&mut output, step, 3, show_access);
                }
            } else {
                writeln!(
                    output,
                    "  Binding {} {}{}",
                    binding.name, binding.strategy, sensitive
                )
                .unwrap();
                write_node(&mut output, &binding.plan, 2, show_access);
            }
        }
        write_node(&mut output, &self.root, 1, show_access);
        output
    }
}

impl std::fmt::Display for PlanView {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.render())
    }
}

/// Golden-test form: deterministic tree without candidate scoring noise.
pub fn print_plan(plan: &Plan) -> String {
    PlanView::new(plan).render_inner(false)
}

fn plain(
    op: &str,
    detail: String,
    access: Vec<AccessCandidate>,
    children: Vec<PlanNodeView>,
) -> PlanNodeView {
    PlanNodeView {
        op: op.into(),
        relation: None,
        detail,
        access,
        join: Vec::new(),
        join_graph: None,
        children,
        render: Render::Normal,
    }
}

fn join_node(
    op: &str,
    detail: String,
    join: Vec<JoinCandidate>,
    left: &Node,
    right: &Node,
) -> PlanNodeView {
    let mut view = plain(
        op,
        detail,
        Vec::new(),
        vec![view_node(left), view_node(right)],
    );
    view.join = join;
    view
}

fn view_node(node: &Node) -> PlanNodeView {
    let mut view = match &node.kind {
        NodeKind::PrimaryKeyGet {
            scan, key, access, ..
        } => {
            let table = scan.scan_table();
            plain(
                "PKGet",
                format!(
                    "{} [{}]",
                    table.name,
                    key_equalities(&table.primary_key, key)
                ),
                access.candidates.clone(),
                Vec::new(),
            )
        }
        NodeKind::TableScan { scan, access, .. } => plain(
            "TableScan",
            scan.scan_table().name.clone(),
            access.candidates.clone(),
            Vec::new(),
        ),
        NodeKind::Rows(relation) => {
            let crate::engine::lir::bound::RelationNode::Rows { scope, values } = &relation.node
            else {
                unreachable!()
            };
            plain(
                "Rows",
                format!("×{} ({scope})", values.len()),
                Vec::new(),
                Vec::new(),
            )
        }
        NodeKind::IndexRangeScan {
            scan,
            index,
            equality_prefix,
            range,
            access,
            ..
        } => {
            let table = scan.scan_table();
            let index_columns = table.index_column_names(index);
            let mut constraints = Vec::new();
            if !equality_prefix.is_empty() {
                constraints.push(key_equalities(
                    &index_columns[..equality_prefix.len()],
                    equality_prefix,
                ));
            }
            if let Some(range) = range {
                constraints.push(range_string(range));
            }
            let constraints = if constraints.is_empty() {
                String::new()
            } else {
                format!(" [{}]", constraints.join(", "))
            };
            plain(
                "IndexRangeScan",
                format!("{} {}{constraints}", table.name, index.name),
                access.candidates.clone(),
                Vec::new(),
            )
        }
        NodeKind::Filter { input, predicate } => plain(
            "Filter",
            print_expression(predicate),
            Vec::new(),
            vec![view_node(input)],
        ),
        NodeKind::Reference { binding, .. } => {
            plain("Ref", binding.clone(), Vec::new(), Vec::new())
        }
        NodeKind::RecursiveReference { binding, .. } => {
            plain("RecursiveRef", binding.clone(), Vec::new(), Vec::new())
        }
        NodeKind::Attach {
            input,
            specifications,
        } => {
            let mut children = Vec::with_capacity(specifications.len() + 1);
            let mut rendered = Vec::with_capacity(specifications.len());
            for specification in specifications {
                let header = format!(
                    "#{} = {} {}{}",
                    specification.slot.0,
                    specification.kind.label(),
                    correlation_kind(specification.correlation.kind),
                    correlation_keys(&specification.correlation)
                );
                let plan = view_node(&specification.plan);
                let mut child = plan.clone();
                child.detail = if plan.detail.is_empty() {
                    header.clone()
                } else {
                    format!("{header} {}", plan.detail)
                };
                children.push(child);
                rendered.push((header, plan));
            }
            let input = view_node(input);
            children.push(input.clone());
            PlanNodeView {
                op: "Attach".into(),
                relation: None,
                detail: String::new(),
                access: Vec::new(),
                join: Vec::new(),
                join_graph: None,
                children,
                render: Render::Attach {
                    specifications: rendered,
                    input: Box::new(input),
                },
            }
        }
        NodeKind::Project { input, fields } => {
            let lines = fields
                .iter()
                .map(|field| {
                    format!(
                        "{}#{} = {}",
                        field.name,
                        field.slot.0,
                        print_expression(&field.expression)
                    )
                })
                .collect();
            PlanNodeView {
                op: "Project".into(),
                relation: None,
                detail: fields
                    .iter()
                    .map(|field| {
                        format!(
                            "{}#{}={}",
                            field.name,
                            field.slot.0,
                            print_expression(&field.expression)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                access: Vec::new(),
                join: Vec::new(),
                join_graph: None,
                children: vec![view_node(input)],
                render: Render::Lines(lines),
            }
        }
        NodeKind::Sort { input, terms } => plain(
            "Sort",
            terms
                .iter()
                .map(|term| {
                    format!(
                        "{} {}",
                        print_expression(&term.expression),
                        if term.descending { "desc" } else { "asc" }
                    )
                })
                .collect::<Vec<_>>()
                .join(", "),
            Vec::new(),
            vec![view_node(input)],
        ),
        NodeKind::Slice {
            input,
            offset,
            limit,
        } => plain(
            "Slice",
            format!(
                "offset={offset} limit={}",
                limit.map_or_else(|| "∞".into(), |limit| limit.to_string())
            ),
            Vec::new(),
            vec![view_node(input)],
        ),
        NodeKind::NestedLoopJoin {
            left,
            right,
            kind,
            on,
            decision,
            ..
        } => join_node(
            "NestedLoopJoin",
            format!("{} on {}", kind.as_str(), print_expression(on)),
            decision.candidates.clone(),
            left,
            right,
        ),
        NodeKind::HashJoin {
            left,
            right,
            kind,
            on,
            memory_limit_bytes,
            decision,
            ..
        } => join_node(
            "HashJoin",
            format!(
                "{} on {} memoryLimitBytes={memory_limit_bytes}",
                kind.as_str(),
                print_expression(on)
            ),
            decision.candidates.clone(),
            left,
            right,
        ),
        NodeKind::IndexedLookupJoin {
            left,
            right,
            kind,
            on,
            decision,
            ..
        } => join_node(
            "IndexedLookupJoin",
            format!("{} on {}", kind.as_str(), print_expression(on)),
            decision.candidates.clone(),
            left,
            right,
        ),
        NodeKind::JoinGraphChoice { input, decision } => {
            let mut view = plain(
                "JoinGraphChoice",
                format!(
                    "classification={:?} inputs={} edges={} root={}",
                    decision.classification,
                    decision.input_count,
                    decision.edge_count,
                    decision.root_input
                ),
                Vec::new(),
                vec![view_node(input)],
            );
            view.join_graph = Some(decision.clone());
            view
        }
        NodeKind::ShreddedYannakakisJoin {
            inputs,
            edges,
            root_input,
            memory_limit_bytes,
            ..
        } => plain(
            "ShreddedYannakakisJoin",
            format!(
                "inputs={} edges={} root={} memoryLimitBytes={memory_limit_bytes}",
                inputs.len(),
                edges.len(),
                root_input
            ),
            Vec::new(),
            inputs.iter().map(view_node).collect(),
        ),
        NodeKind::PredicateTransferJoin {
            inputs,
            schedule,
            join_plan,
            bits_per_key,
            hash_functions,
            runtime_policy,
            memory_limit_bytes,
            ..
        } => plain(
            "PredicateTransferJoin",
            format!(
                "inputs={} schedule=asymmetric root={} forwardEdges={} backwardEdges={} prunedPaths={} forwardOrder={:?} backwardOrder={:?} filter=cascade storagePruning=compatible_ordered_key_prefix blockRows={} bitsPerKey={bits_per_key} hashFunctions={hash_functions} buildSampleRows={} buildSelectivityBps={} buildProgressBps={} probeSampleRows={} probeStopBps={} memoryLimitBytes={memory_limit_bytes}",
                inputs.len(),
                schedule.root_input,
                schedule.forward.edges.len(),
                schedule.backward.edges.len(),
                schedule.pruned_paths,
                schedule.forward.order,
                schedule.backward.order,
                runtime_policy.block_rows,
                runtime_policy.build_sample_rows,
                runtime_policy.build_selectivity_threshold_bps,
                runtime_policy.build_progress_threshold_bps,
                runtime_policy.probe_sample_rows,
                runtime_policy.probe_stop_threshold_bps,
            ),
            Vec::new(),
            inputs
                .iter()
                .map(view_node)
                .chain(std::iter::once(view_node(join_plan)))
                .collect(),
        ),
        NodeKind::PredicateTransferInput { input, .. } => plain(
            "PredicateTransferInput",
            format!("input={input}"),
            Vec::new(),
            Vec::new(),
        ),
        NodeKind::Concatenate { inputs, .. } => plain(
            "Concatenate",
            String::new(),
            Vec::new(),
            inputs.iter().map(view_node).collect(),
        ),
        NodeKind::Intersect {
            left,
            right,
            quantifier,
            ..
        } => plain(
            "Intersect",
            quantifier.as_str().into(),
            Vec::new(),
            vec![view_node(left), view_node(right)],
        ),
        NodeKind::Except {
            left,
            right,
            quantifier,
            ..
        } => plain(
            "Except",
            quantifier.as_str().into(),
            Vec::new(),
            vec![view_node(left), view_node(right)],
        ),
        NodeKind::Distinct { input, .. } => plain(
            "Distinct",
            String::new(),
            Vec::new(),
            vec![view_node(input)],
        ),
        NodeKind::Aggregate {
            input,
            groups,
            terms,
        } => {
            let mut parts = groups
                .iter()
                .map(|group| {
                    format!(
                        "group {}#{}={}",
                        group.name,
                        group.slot.0,
                        print_expression(&group.expression)
                    )
                })
                .collect::<Vec<_>>();
            parts.extend(terms.iter().map(|term| {
                format!(
                    "{}#{}={}({})",
                    term.name,
                    term.slot.0,
                    term.function.as_str(),
                    term.argument
                        .as_ref()
                        .map_or_else(|| "*".into(), print_expression)
                )
            }));
            plain(
                "Aggregate",
                parts.join(", "),
                Vec::new(),
                vec![view_node(input)],
            )
        }
    };
    view.relation = node.attribution;
    view
}

fn write_node(output: &mut String, node: &PlanNodeView, depth: usize, show_access: bool) {
    let padding = "  ".repeat(depth);
    match &node.render {
        Render::Attach {
            specifications,
            input,
        } => {
            writeln!(output, "{padding}{}", node.op).unwrap();
            for (header, plan) in specifications {
                writeln!(output, "{padding}  {header}").unwrap();
                write_node(output, plan, depth + 2, show_access);
            }
            write_node(output, input, depth + 1, show_access);
            return;
        }
        Render::Lines(lines) => {
            writeln!(output, "{padding}{}", node.op).unwrap();
            for line in lines {
                writeln!(output, "{padding}  {line}").unwrap();
            }
        }
        Render::Normal if node.detail.is_empty() => {
            writeln!(output, "{padding}{}", node.op).unwrap()
        }
        Render::Normal => writeln!(output, "{padding}{} {}", node.op, node.detail).unwrap(),
    }
    if show_access && let Some(line) = access_line(&node.access) {
        writeln!(output, "{padding}  access: {line}").unwrap();
    }
    if show_access && let Some(line) = join_line(&node.join) {
        writeln!(output, "{padding}  join: {line}").unwrap();
    }
    if show_access && let Some(decision) = &node.join_graph {
        writeln!(
            output,
            "{padding}  join-graph: {}",
            join_graph_line(&decision.candidates)
        )
        .unwrap();
    }
    for child in &node.children {
        write_node(output, child, depth + 1, show_access);
    }
}

fn access_line(candidates: &[AccessCandidate]) -> Option<String> {
    let winner = candidates.iter().find(|candidate| candidate.chosen)?;
    if winner.method == "TableScan" && candidates.iter().all(|candidate| candidate.score == 0) {
        return None;
    }
    Some(
        candidates
            .iter()
            .map(|candidate| {
                let chosen = if candidate.chosen { " ✓" } else { "" };
                let basis = candidate
                    .decision_basis
                    .map(|basis| format!(" [{}]", basis.label()))
                    .unwrap_or_default();
                let row_work = candidate
                    .estimated_row_work
                    .map(|work| {
                        let upper_bound = work
                            .upper_bound
                            .map(|upper_bound| upper_bound.to_string())
                            .unwrap_or_else(|| "unbounded".into());
                        format!(" {{rowWork={}..{upper_bound}}}", work.lower_bound)
                    })
                    .unwrap_or_default();
                let cost = candidate
                    .cost
                    .map(|cost| {
                        let work = cost.logical_row_operations;
                        let upper = work
                            .upper_bound
                            .map_or_else(|| "unbounded".into(), |value| value.to_string());
                        format!(
                            " {{cost={}..{upper}, gets={}, scans={}, ordering={:?}}}",
                            work.lower_bound,
                            cost.point_gets.central,
                            cost.range_scans.central,
                            cost.ordering
                        )
                    })
                    .unwrap_or_default();
                if candidate.method == "PKGet" {
                    format!("{}{row_work}{cost}{chosen}{basis}", candidate.method)
                } else {
                    format!(
                        "{}({}){row_work}{cost}{chosen}{basis}",
                        candidate.method, candidate.score
                    )
                }
            })
            .collect::<Vec<_>>()
            .join(" · "),
    )
}

fn join_line(candidates: &[JoinCandidate]) -> Option<String> {
    candidates.iter().find(|candidate| candidate.chosen)?;
    Some(
        candidates
            .iter()
            .map(|candidate| {
                let chosen = if candidate.chosen { " ✓" } else { "" };
                let basis = candidate
                    .decision_basis
                    .map(|basis| format!(" [{}]", basis.label()))
                    .unwrap_or_default();
                let reason = candidate
                    .rejection_reason
                    .map(|reason| format!(" [{}]", reason.label()))
                    .unwrap_or_default();
                let cost = candidate.cost.map(|cost| {
                    let work = cost.logical_row_operations;
                    let upper = work
                        .upper_bound
                        .map_or_else(|| "unbounded".into(), |value| value.to_string());
                    let memory = cost.peak_retained_bytes.map_or_else(
                        || "unknown".into(),
                        |bytes| {
                            bytes
                                .upper_bound
                                .map_or_else(|| "unbounded".into(), |value| value.to_string())
                        },
                    );
                    format!(
                        " {{cost={}..{upper}, memoryUpper={memory}}}",
                        work.lower_bound
                    )
                });
                format!(
                    "{}{}{}{}{}",
                    candidate.method,
                    cost.unwrap_or_default(),
                    chosen,
                    basis,
                    reason
                )
            })
            .collect::<Vec<_>>()
            .join(" · "),
    )
}

fn join_graph_line(candidates: &[JoinGraphCandidate]) -> String {
    candidates
        .iter()
        .map(|candidate| {
            let chosen = if candidate.chosen { " ✓" } else { "" };
            let basis = candidate
                .decision_basis
                .map(|basis| format!(" [{}]", basis.label()))
                .unwrap_or_default();
            let reason = candidate
                .rejection_reason
                .map(|reason| format!(" [{}]", reason.label()))
                .unwrap_or_default();
            let cost = candidate.cost.map(|cost| {
                let work = cost.logical_row_operations;
                let upper = work
                    .upper_bound
                    .map_or_else(|| "unbounded".into(), |value| value.to_string());
                let memory = cost.peak_retained_bytes.map_or_else(
                    || "unknown".into(),
                    |bytes| {
                        bytes
                            .upper_bound
                            .map_or_else(|| "unbounded".into(), |value| value.to_string())
                    },
                );
                let filter = cost
                    .filter_row_operations
                    .map_or_else(String::new, |operations| {
                        let rows = cost
                            .filtered_rows
                            .map_or_else(|| "unknown".into(), |rows| rows.central.to_string());
                        let bytes = cost
                            .filter_bytes
                            .map_or_else(|| "unknown".into(), |bytes| bytes.central.to_string());
                        let builds = cost
                            .filter_builds
                            .map_or_else(|| "unknown".into(), |builds| builds.to_string());
                        let shared = cost.shared_filter_paths.map_or_else(
                            || "unknown".into(),
                            |paths| paths.to_string(),
                        );
                        format!(
                            ", filterOps={}, filteredRows={rows}, filterBytes={bytes}, filterBuilds={builds}, sharedPaths={shared}",
                            operations.central
                        )
                    });
                format!(
                    " {{cost={}..{upper}, memoryUpper={memory}{filter}}}",
                    work.lower_bound
                )
            });
            format!(
                "{}{}{}{}{}",
                candidate.method,
                cost.unwrap_or_default(),
                chosen,
                basis,
                reason
            )
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

fn key_equalities<T: AsRef<str>>(columns: &[T], values: &[ConstValue]) -> String {
    columns
        .iter()
        .zip(values)
        .map(|(column, value)| format!("{} = {}", column.as_ref(), constant(value)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn constant(value: &ConstValue) -> String {
    match value {
        ConstValue::Literal(value) => value.to_string(),
        ConstValue::Outer(slot) => format!("@{}", slot.0),
    }
}

fn range_string(range: &RangeSpec) -> String {
    let mut values = Vec::new();
    if let Some(lower) = &range.lower {
        values.push(format!(
            "{} {} {}",
            range.column,
            if lower.inclusive { ">=" } else { ">" },
            lower.value
        ));
    }
    if let Some(upper) = &range.upper {
        values.push(format!(
            "{} {} {}",
            range.column,
            if upper.inclusive { "<=" } else { "<" },
            upper.value
        ));
    }
    values.join(", ")
}

fn correlation_keys(correlation: &Correlation) -> String {
    if correlation.keys.is_empty() {
        return String::new();
    }
    format!(
        " [{}]",
        correlation
            .keys
            .iter()
            .map(|key| format!(
                "{}#{} = @{}",
                key.inner_column, key.inner_slot.0, key.outer_slot.0
            ))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn binding_strategy(value: BindingStrategy) -> &'static str {
    match value {
        BindingStrategy::Materialize => "materialise",
        BindingStrategy::Replay => "replay",
    }
}
fn correlation_kind(value: CorrelationKind) -> &'static str {
    match value {
        CorrelationKind::Uncorrelated => "uncorrelated",
        CorrelationKind::Key => "key-correlated",
        CorrelationKind::General => "general-correlated",
    }
}
#[cfg(test)]
mod tests {
    use crate::engine::lir::bound;
    use crate::engine::lir::{BinaryOp, Value};
    use crate::engine::planner::test_support::{column, query, scan};
    use crate::engine::planner::{PlanOptions, PlannerMode, plan_query};

    use super::*;

    fn equality(left: bound::Expr, value: &str) -> bound::Expr {
        bound::Expr::binary(
            BinaryOp::Eq,
            left,
            bound::Expr::literal(Value::Text(value.into())),
        )
    }

    #[test]
    fn plan_view_retains_the_winner_and_rejected_access_candidates() {
        let scan = scan();
        let predicate = bound::Expr::binary(
            BinaryOp::And,
            equality(column(&scan, "board_id"), "b1"),
            equality(column(&scan, "status"), "open"),
        );
        let plan = plan_query(
            &query(bound::Relation::filter(scan, predicate), 3),
            PlanOptions::default(),
        );
        let view = PlanView::new(&plan);
        let rendered = view.render();
        for expected in ["access:", "tasks_board_status_idx", "TableScan(0)"] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}:\n{rendered}"
            );
        }

        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["format"], PLAN_VIEW_FORMAT);
        assert!(
            json["fingerprint"]
                .as_str()
                .is_some_and(|fingerprint| fingerprint.starts_with("c1h1:"))
        );
        let scan = &json["root"]["children"][0];
        assert_eq!(scan["op"], "IndexRangeScan");
        let candidates = scan["access"].as_array().unwrap();
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate["method"] == "TableScan")
        );
        assert!(candidates.iter().any(|candidate| {
            candidate["chosen"] == true
                && candidate["method"]
                    .as_str()
                    .is_some_and(|method| method.contains("tasks_board_status_idx"))
        }));
    }

    #[test]
    fn uncontested_table_scan_has_no_access_noise() {
        let plan = plan_query(&query(scan(), 3), PlanOptions::default());
        assert!(!PlanView::new(&plan).render().contains("access:"));
    }

    #[test]
    fn plan_view_contains_the_selected_memo_proof() {
        let relation =
            bound::Relation::distinct(bound::Relation::distinct(bound::Relation::distinct(scan())));
        let plan = plan_query(
            &query(relation, 3),
            PlanOptions {
                mode: PlannerMode::Cost,
                ..PlanOptions::default()
            },
        );
        let view = PlanView::with_mode(&plan, PlannerMode::Cost);
        let rendered = view.render();
        assert!(rendered.contains("directed=2 saturated=3"));
        assert_eq!(rendered.matches("proof DistinctIdempotence").count(), 2);

        let json = serde_json::to_value(view).unwrap();
        assert_eq!(
            json["memo"]["format"],
            crate::engine::planner::memo::MEMO_FORMAT
        );
        assert_eq!(
            json["memo"]["roots"][0]["selectedProof"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }
}
