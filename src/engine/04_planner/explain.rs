//! Stable JSON and text observability views for physical plans.

use std::fmt::Write;

use serde::Serialize;

use crate::engine::lir::format::print_expression;
use crate::engine::lir::{
    AggregateFunction, JoinKind, RecursiveAccumulation, RootCardinality, SetQuantifier,
};

use super::analysis::{ConstValue, Correlation, CorrelationKind};
use super::physical::{
    AccessCandidate, BindingPlanKind, BindingStrategy, CrossingKind, Node, Plan, RangeSpec,
};

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanView {
    pub cardinality: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<PlanBindingView>,
    pub root: PlanNodeView,
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
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub access: Vec<AccessCandidate>,
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
        Self {
            cardinality: root_cardinality(plan.cardinality).into(),
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
                            Some(self::accumulation(*accumulation).into()),
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
        }
    }

    /// Diagnostic text including non-trivial access alternatives and scores.
    pub fn render(&self) -> String {
        self.render_inner(true)
    }

    fn render_inner(&self, show_access: bool) -> String {
        let mut output = String::new();
        writeln!(output, "Plan card={}", self.cardinality).unwrap();
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
        detail,
        access,
        children,
        render: Render::Normal,
    }
}

fn view_node(node: &Node) -> PlanNodeView {
    match node {
        Node::PrimaryKeyGet {
            scan, key, access, ..
        } => plain(
            "PKGet",
            format!(
                "{} [{}]",
                table(scan).name,
                key_equalities(&table(scan).primary_key, key)
            ),
            access.candidates.clone(),
            Vec::new(),
        ),
        Node::TableScan { scan, access, .. } => plain(
            "TableScan",
            table(scan).name.clone(),
            access.candidates.clone(),
            Vec::new(),
        ),
        Node::Rows(relation) => {
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
        Node::IndexRangeScan {
            scan,
            index,
            equality_prefix,
            range,
            access,
            ..
        } => {
            let table = table(scan);
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
        Node::Filter { input, predicate } => plain(
            "Filter",
            print_expression(predicate),
            Vec::new(),
            vec![view_node(input)],
        ),
        Node::Reference { binding, .. } => plain("Ref", binding.clone(), Vec::new(), Vec::new()),
        Node::RecursiveReference { binding, .. } => {
            plain("RecursiveRef", binding.clone(), Vec::new(), Vec::new())
        }
        Node::Attach {
            input,
            specifications,
        } => {
            let mut children = Vec::with_capacity(specifications.len() + 1);
            let mut rendered = Vec::with_capacity(specifications.len());
            for specification in specifications {
                let header = format!(
                    "#{} = {} {}{}",
                    specification.slot.0,
                    crossing_kind(specification.kind),
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
                detail: String::new(),
                access: Vec::new(),
                children,
                render: Render::Attach {
                    specifications: rendered,
                    input: Box::new(input),
                },
            }
        }
        Node::Project { input, fields } => {
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
                children: vec![view_node(input)],
                render: Render::Lines(lines),
            }
        }
        Node::Sort { input, terms } => plain(
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
        Node::Slice {
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
        Node::NestedLoopJoin {
            left,
            right,
            kind,
            on,
            ..
        } => plain(
            "NestedLoopJoin",
            format!("{} on {}", join_kind(*kind), print_expression(on)),
            Vec::new(),
            vec![view_node(left), view_node(right)],
        ),
        Node::Concatenate { inputs, .. } => plain(
            "Concatenate",
            String::new(),
            Vec::new(),
            inputs.iter().map(view_node).collect(),
        ),
        Node::Intersect {
            left,
            right,
            quantifier,
            ..
        } => plain(
            "Intersect",
            set_quantifier(*quantifier).into(),
            Vec::new(),
            vec![view_node(left), view_node(right)],
        ),
        Node::Except {
            left,
            right,
            quantifier,
            ..
        } => plain(
            "Except",
            set_quantifier(*quantifier).into(),
            Vec::new(),
            vec![view_node(left), view_node(right)],
        ),
        Node::Distinct { input, .. } => plain(
            "Distinct",
            String::new(),
            Vec::new(),
            vec![view_node(input)],
        ),
        Node::Aggregate {
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
                    aggregate_function(term.function),
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
    }
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
                if candidate.method == "PKGet" {
                    format!("{}{chosen}", candidate.method)
                } else {
                    format!("{}({}){chosen}", candidate.method, candidate.score)
                }
            })
            .collect::<Vec<_>>()
            .join(" · "),
    )
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

fn table(scan: &crate::engine::lir::bound::Relation) -> &crate::engine::catalog::model::Table {
    let crate::engine::lir::bound::RelationNode::Scan { table, .. } = &scan.node else {
        unreachable!()
    };
    table
}

fn root_cardinality(value: RootCardinality) -> &'static str {
    match value {
        RootCardinality::Many => "many",
        RootCardinality::First => "first",
        RootCardinality::ExactlyOne => "exactly_one",
        RootCardinality::Scalar => "scalar",
    }
}
fn binding_strategy(value: BindingStrategy) -> &'static str {
    match value {
        BindingStrategy::Materialize => "materialise",
        BindingStrategy::Replay => "replay",
    }
}
fn accumulation(value: RecursiveAccumulation) -> &'static str {
    match value {
        RecursiveAccumulation::All => "all",
        RecursiveAccumulation::New => "new",
    }
}
fn crossing_kind(value: CrossingKind) -> &'static str {
    match value {
        CrossingKind::Exists => "exists",
        CrossingKind::First => "first",
        CrossingKind::Scalar => "scalar",
        CrossingKind::Array => "array",
    }
}
fn correlation_kind(value: CorrelationKind) -> &'static str {
    match value {
        CorrelationKind::Uncorrelated => "uncorrelated",
        CorrelationKind::Key => "key-correlated",
        CorrelationKind::General => "general-correlated",
    }
}
fn join_kind(value: JoinKind) -> &'static str {
    match value {
        JoinKind::Inner => "inner",
        JoinKind::Left => "left",
    }
}
fn set_quantifier(value: SetQuantifier) -> &'static str {
    match value {
        SetQuantifier::All => "all",
        SetQuantifier::Distinct => "distinct",
    }
}
fn aggregate_function(value: AggregateFunction) -> &'static str {
    match value {
        AggregateFunction::Count => "count",
        AggregateFunction::Sum => "sum",
        AggregateFunction::Average => "avg",
        AggregateFunction::Min => "min",
        AggregateFunction::Max => "max",
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::lir::bound;
    use crate::engine::lir::{BinaryOp, Value};
    use crate::engine::planner::test_support::{column, query, scan};
    use crate::engine::planner::{PlanOptions, plan_query};

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
}
