use std::fmt::Write;

use super::bound::{Expr, Query, Relation, RelationNode};
use super::{BinaryOp, RecursiveAccumulation, TextComparison, UnaryOp};

pub fn print(query: &Query) -> String {
    let mut output = String::new();
    writeln!(output, "Query card={}", query.cardinality.as_str()).unwrap();
    for binding in &query.bindings {
        let sensitivity = if binding.plan_sensitive {
            " plan-choice-sensitive"
        } else {
            ""
        };
        if binding.recursive {
            writeln!(
                output,
                "  Binding {} recursive accumulation={}{}",
                binding.name,
                binding
                    .accumulation
                    .map_or("", RecursiveAccumulation::as_str),
                sensitivity
            )
            .unwrap();
            writeln!(output, "    anchor").unwrap();
            print_relation_into(&mut output, &binding.root, 3);
            writeln!(output, "    step").unwrap();
            if let Some(step) = &binding.step {
                print_relation_into(&mut output, step, 3);
            }
        } else {
            writeln!(output, "  Binding {}{}", binding.name, sensitivity).unwrap();
            print_relation_into(&mut output, &binding.root, 2);
        }
    }
    print_relation_into(&mut output, &query.root, 1);
    output
}

fn print_relation_into(output: &mut String, relation: &Relation, depth: usize) {
    let padding = "  ".repeat(depth);
    match &relation.node {
        RelationNode::Scan { table, scope } => {
            writeln!(
                output,
                "{padding}Scan {} ({scope}) {}",
                table.name,
                row_type(relation)
            )
            .unwrap();
        }
        RelationNode::Rows { scope, values } => {
            writeln!(
                output,
                "{padding}Rows ×{} ({scope}) {}",
                values.len(),
                row_type(relation)
            )
            .unwrap();
        }
        RelationNode::Ref { binding, scope, .. } => {
            writeln!(
                output,
                "{padding}Ref {binding} ({scope}) {}",
                row_type(relation)
            )
            .unwrap();
        }
        RelationNode::RecursiveRef { binding, scope, .. } => {
            writeln!(
                output,
                "{padding}RecursiveRef {binding} ({scope}) {}",
                row_type(relation)
            )
            .unwrap();
        }
        RelationNode::Distinct(input) => {
            writeln!(output, "{padding}Distinct{}", suffix(relation)).unwrap();
            print_relation_into(output, input, depth + 1);
        }
        RelationNode::Filter { input, predicate } => {
            writeln!(
                output,
                "{padding}Filter {}{}",
                print_expression(predicate),
                suffix(relation)
            )
            .unwrap();
            print_relation_into(output, input, depth + 1);
        }
        RelationNode::Project { input, fields, .. } => {
            writeln!(output, "{padding}Project{}", suffix(relation)).unwrap();
            for field in fields {
                writeln!(
                    output,
                    "{padding}  {}#{} = {} : {}",
                    field.name,
                    field.slot.0,
                    print_expression(&field.expression),
                    field.expression.value_type()
                )
                .unwrap();
            }
            print_relation_into(output, input, depth + 1);
        }
        RelationNode::Join {
            left,
            right,
            kind,
            on,
        } => {
            writeln!(
                output,
                "{padding}Join {} on {}{}",
                kind.as_str(),
                print_expression(on),
                suffix(relation)
            )
            .unwrap();
            print_relation_into(output, left, depth + 1);
            print_relation_into(output, right, depth + 1);
        }
        RelationNode::Concatenate { inputs, scope } => {
            writeln!(
                output,
                "{padding}Concatenate ({scope}) {}{}",
                row_type(relation),
                suffix(relation)
            )
            .unwrap();
            for input in inputs {
                print_relation_into(output, input, depth + 1);
            }
        }
        RelationNode::Intersect {
            left,
            right,
            quantifier,
            scope,
        } => {
            writeln!(
                output,
                "{padding}Intersect {} ({scope}) {}{}",
                quantifier.as_str(),
                row_type(relation),
                suffix(relation)
            )
            .unwrap();
            print_relation_into(output, left, depth + 1);
            print_relation_into(output, right, depth + 1);
        }
        RelationNode::Except {
            left,
            right,
            quantifier,
            scope,
        } => {
            writeln!(
                output,
                "{padding}Except {} ({scope}) {}{}",
                quantifier.as_str(),
                row_type(relation),
                suffix(relation)
            )
            .unwrap();
            print_relation_into(output, left, depth + 1);
            print_relation_into(output, right, depth + 1);
        }
        RelationNode::Aggregate {
            input,
            groups,
            terms,
        } => {
            writeln!(output, "{padding}Aggregate{}", suffix(relation)).unwrap();
            for group in groups {
                writeln!(
                    output,
                    "{padding}  group {}#{} = {}",
                    group.name,
                    group.slot.0,
                    print_expression(&group.expression)
                )
                .unwrap();
            }
            for term in terms {
                let argument = term
                    .argument
                    .as_ref()
                    .map_or_else(|| "*".into(), print_expression);
                writeln!(
                    output,
                    "{padding}  {}#{} = {}({argument}) : {}",
                    term.name,
                    term.slot.0,
                    term.function.as_str(),
                    term.value_type
                )
                .unwrap();
            }
            print_relation_into(output, input, depth + 1);
        }
        RelationNode::Order { input, terms } => {
            let terms = terms
                .iter()
                .map(|term| {
                    format!(
                        "{} {}",
                        print_expression(&term.expression),
                        if term.descending { "desc" } else { "asc" }
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(output, "{padding}Order {terms}{}", suffix(relation)).unwrap();
            print_relation_into(output, input, depth + 1);
        }
        RelationNode::Slice {
            input,
            offset,
            limit,
        } => {
            let limit = limit.map_or_else(|| "∞".into(), |limit| limit.to_string());
            writeln!(
                output,
                "{padding}Slice offset={offset} limit={limit}{}",
                suffix(relation)
            )
            .unwrap();
            print_relation_into(output, input, depth + 1);
        }
    }
}

fn suffix(relation: &Relation) -> String {
    let mut value = format!("  [card {}", relation.cardinality());
    if !relation.free_slots().is_empty() {
        value.push_str(" free ");
        value.push_str(
            &relation
                .free_slots()
                .slots()
                .iter()
                .map(|slot| slot.0.to_string())
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    value.push(']');
    value
}

pub fn print_expression(expression: &Expr) -> String {
    match expression {
        Expr::Literal(value) => value.to_string(),
        Expr::SlotRef { slot, name, .. } => format!("{name}#{}", slot.0),
        Expr::Unary { op, expression, .. } => {
            format!("{}({})", unary_op(*op), print_expression(expression))
        }
        Expr::Binary {
            op, left, right, ..
        } => format!(
            "{}({}, {})",
            binary_op(*op),
            print_expression(left),
            print_expression(right)
        ),
        Expr::Cast { expression, to, .. } => {
            format!("cast({} as {to})", print_expression(expression))
        }
        Expr::Branch {
            arms, otherwise, ..
        } => {
            let mut value = String::from("branch(");
            for arm in arms {
                write!(
                    value,
                    "{} → {}, ",
                    print_expression(&arm.when),
                    print_expression(&arm.then)
                )
                .unwrap();
            }
            write!(value, "else {})", print_expression(otherwise)).unwrap();
            value
        }
        Expr::TextMatch { value, pattern, .. } => {
            if pattern.comparison() == TextComparison::Exact {
                format!("text_match({}, {pattern})", print_expression(value))
            } else {
                format!(
                    "text_match[unicode_simple_fold]({}, {pattern})",
                    print_expression(value)
                )
            }
        }
        Expr::Exists(relation) => crossing("exists", relation),
        Expr::First { relation, .. } => crossing("first", relation),
        Expr::Scalar { relation, .. } => crossing("scalar", relation),
        Expr::Array { relation, .. } => crossing("array", relation),
    }
}

fn crossing(name: &str, relation: &Relation) -> String {
    let mut value = String::new();
    print_relation_into(&mut value, relation, 3);
    format!("{name}(\n{}\n    )", value.trim_end())
}

fn row_type(relation: &Relation) -> String {
    format!(
        "{{{}}}",
        relation
            .output()
            .fields
            .iter()
            .map(|field| format!("{}#{}:{}", field.name, field.slot.0, field.value_type))
            .collect::<Vec<_>>()
            .join(" ")
    )
}

fn unary_op(value: UnaryOp) -> &'static str {
    match value {
        UnaryOp::Not => "not",
        UnaryOp::Negate => "negate",
        UnaryOp::IsNull => "is_null",
        UnaryOp::IsNotNull => "is_not_null",
    }
}

fn binary_op(value: BinaryOp) -> &'static str {
    match value {
        BinaryOp::Eq => "eq",
        BinaryOp::Ne => "ne",
        BinaryOp::Lt => "lt",
        BinaryOp::Lte => "lte",
        BinaryOp::Gt => "gt",
        BinaryOp::Gte => "gte",
        BinaryOp::And => "and",
        BinaryOp::Or => "or",
        BinaryOp::Add => "add",
        BinaryOp::Sub => "sub",
        BinaryOp::Mul => "mul",
        BinaryOp::Div => "div",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::lir::bound::Relation;
    use crate::engine::lir::{Field, Kind, RootCardinality, SlotId, Type, Value};

    #[test]
    fn printing_pins_slots_and_laws() {
        let rows = Relation::rows(
            "r",
            vec![Field {
                name: "x".into(),
                slot: SlotId(0),
                value_type: Type::scalar(Kind::Int64, false),
            }],
            vec![vec![Value::Int64(1)]],
        );
        let query = Query {
            root: Relation::filter(
                rows,
                Expr::binary(
                    BinaryOp::Gt,
                    Expr::slot(SlotId(0), "r.x", Type::scalar(Kind::Int64, false)),
                    Expr::literal(Value::Int64(0)),
                ),
            ),
            cardinality: RootCardinality::Many,
            bindings: Vec::new(),
            next_slot: SlotId(1),
        };
        assert_eq!(
            print(&query),
            "Query card=many\n  Filter gt(r.x#0, 0)  [card 0..1]\n    Rows ×1 (r) {x#0:int64}\n"
        );
    }

    #[test]
    fn crossing_expression_closes_after_its_indented_relation() {
        let rows = Relation::rows(
            "r",
            vec![Field {
                name: "x".into(),
                slot: SlotId(0),
                value_type: Type::scalar(Kind::Int64, false),
            }],
            vec![vec![Value::Int64(1)]],
        );
        assert_eq!(
            print_expression(&Expr::exists(rows)),
            "exists(\n      Rows ×1 (r) {x#0:int64}\n    )"
        );
    }
}
