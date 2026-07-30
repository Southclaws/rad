use super::bound::{Expr, Relation, RelationNode};

/// Walks an expression tree. Crossing relations are expression leaves.
pub fn walk_expression(expression: &Expr, visitor: &mut impl FnMut(&Expr)) {
    visitor(expression);
    for child in expression_children(expression) {
        walk_expression(child, visitor);
    }
}

/// Walks relations, their expressions, and relations reached through crossings.
pub fn walk_relation(
    relation: &Relation,
    relation_visitor: &mut impl FnMut(&Relation),
    expression_visitor: &mut impl FnMut(&Expr),
) {
    relation_visitor(relation);
    for expression in relation_expressions(relation) {
        walk_crossing_expression(expression, relation_visitor, expression_visitor);
    }
    for input in relation.inputs() {
        walk_relation(input, relation_visitor, expression_visitor);
    }
}

fn walk_crossing_expression(
    expression: &Expr,
    relation_visitor: &mut impl FnMut(&Relation),
    expression_visitor: &mut impl FnMut(&Expr),
) {
    expression_visitor(expression);
    for child in expression_children(expression) {
        walk_crossing_expression(child, relation_visitor, expression_visitor);
    }
    match expression {
        Expr::Exists(relation)
        | Expr::First { relation, .. }
        | Expr::Scalar { relation, .. }
        | Expr::Array { relation, .. } => {
            walk_relation(relation, relation_visitor, expression_visitor);
        }
        _ => {}
    }
}

fn expression_children(expression: &Expr) -> Vec<&Expr> {
    match expression {
        Expr::Literal(_)
        | Expr::SlotRef { .. }
        | Expr::Exists(_)
        | Expr::First { .. }
        | Expr::Scalar { .. }
        | Expr::Array { .. } => Vec::new(),
        Expr::Unary { expression, .. }
        | Expr::Cast { expression, .. }
        | Expr::TextMatch {
            value: expression, ..
        } => vec![expression],
        Expr::Binary { left, right, .. } => vec![left, right],
        Expr::Branch {
            arms, otherwise, ..
        } => {
            let mut children = Vec::with_capacity(arms.len() * 2 + 1);
            for arm in arms {
                children.push(&arm.when);
                children.push(&arm.then);
            }
            children.push(otherwise);
            children
        }
    }
}

fn relation_expressions(relation: &Relation) -> Vec<&Expr> {
    match &relation.node {
        RelationNode::Filter { predicate, .. } | RelationNode::Join { on: predicate, .. } => {
            vec![predicate]
        }
        RelationNode::Project { fields, .. } => {
            fields.iter().map(|field| &field.expression).collect()
        }
        RelationNode::Aggregate { groups, terms, .. } => {
            let mut expressions = Vec::with_capacity(groups.len() + terms.len());
            for group in groups {
                expressions.push(&group.expression);
            }
            for term in terms {
                if let Some(argument) = &term.argument {
                    expressions.push(argument);
                }
            }
            expressions
        }
        RelationNode::Order { terms, .. } => terms.iter().map(|term| &term.expression).collect(),
        _ => Vec::new(),
    }
}

/// Conservatively reports whether valid physical plans may commit different values.
pub fn plan_sensitive(relation: &Relation) -> bool {
    let sensitive = std::cell::Cell::new(false);
    walk_relation(
        relation,
        &mut |relation| {
            if matches!(relation.node, RelationNode::Slice { .. }) {
                sensitive.set(true);
            }
        },
        &mut |expression| {
            if matches!(expression, Expr::First { .. } | Expr::Array { .. }) {
                sensitive.set(true);
            }
        },
    );
    sensitive.get()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::lir::bound::Relation;
    use crate::engine::lir::{Field, Kind, SlotId, Type, Value};

    #[test]
    fn sensitivity_reaches_slices_and_crossing_relations() {
        let rows = Relation::rows(
            "r",
            vec![Field {
                name: "x".into(),
                slot: SlotId(0),
                value_type: Type::scalar(Kind::Int64, false),
            }],
            vec![vec![Value::Int64(1)]],
        );
        assert!(!plan_sensitive(&rows));
        assert!(plan_sensitive(&Relation::slice(rows.clone(), 0, Some(1))));
        let projected = Relation::project(
            rows.clone(),
            "p",
            vec![crate::engine::lir::bound::ProjectField {
                name: "nested".into(),
                slot: SlotId(1),
                expression: Expr::first(rows),
            }],
        );
        assert!(plan_sensitive(&projected));

        let reference = Relation::reference(
            "committed",
            "r",
            vec![Field {
                name: "x".into(),
                slot: SlotId(2),
                value_type: Type::scalar(Kind::Int64, false),
            }],
            vec![SlotId(0)],
        );
        assert!(
            !plan_sensitive(&reference),
            "a reference observes an already committed binding value"
        );
    }
}
