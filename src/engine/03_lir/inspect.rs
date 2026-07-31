use super::bound::{Expr, Relation, RelationNode};
use super::{Expr as UnboundExpr, Relation as UnboundRelation};

/// Walks an expression tree. Crossing relations are expression leaves.
pub fn walk_expression(expression: &Expr, visitor: &mut impl FnMut(&Expr)) {
    walk_expression_tree(expression, &mut |_| {}, visitor, false);
}

/// Walks relations, their expressions, and relations reached through crossings.
pub fn walk_relation(
    relation: &Relation,
    relation_visitor: &mut impl FnMut(&Relation),
    expression_visitor: &mut impl FnMut(&Expr),
) {
    relation_visitor(relation);
    for expression in relation_expressions(relation) {
        walk_expression_tree(expression, relation_visitor, expression_visitor, true);
    }
    for input in relation.inputs() {
        walk_relation(input, relation_visitor, expression_visitor);
    }
}

/// Walks name-based LIR relations, including relations reached through crossings.
pub(crate) fn walk_unbound_relation(
    relation: &UnboundRelation,
    visitor: &mut impl FnMut(&UnboundRelation),
) {
    visitor(relation);
    for input in unbound_relation_inputs(relation) {
        walk_unbound_relation(input, visitor);
    }
    for expression in unbound_relation_expressions(relation) {
        walk_unbound_expression_relations(expression, visitor);
    }
}

/// Visits one name-based relation's direct inputs and expression roots.
pub(crate) fn try_visit_unbound_relation_parts<E>(
    relation: &UnboundRelation,
    relation_visitor: &mut impl FnMut(&UnboundRelation) -> Result<(), E>,
    expression_visitor: &mut impl FnMut(&UnboundExpr) -> Result<(), E>,
) -> Result<(), E> {
    for input in unbound_relation_inputs(relation) {
        relation_visitor(input)?;
    }
    for expression in unbound_relation_expressions(relation) {
        expression_visitor(expression)?;
    }
    Ok(())
}

/// Visits crossing relations nested in a name-based expression.
pub(crate) fn try_visit_unbound_expression_relations<E>(
    expression: &UnboundExpr,
    visitor: &mut impl FnMut(&UnboundRelation) -> Result<(), E>,
) -> Result<(), E> {
    for child in unbound_expression_children(expression) {
        try_visit_unbound_expression_relations(child, visitor)?;
    }
    if let Some(relation) = unbound_crossing_relation(expression) {
        visitor(relation)?;
    }
    Ok(())
}

fn walk_unbound_expression_relations(
    expression: &UnboundExpr,
    visitor: &mut impl FnMut(&UnboundRelation),
) {
    for child in unbound_expression_children(expression) {
        walk_unbound_expression_relations(child, visitor);
    }
    if let Some(relation) = unbound_crossing_relation(expression) {
        walk_unbound_relation(relation, visitor);
    }
}

fn unbound_relation_inputs(relation: &UnboundRelation) -> Vec<&UnboundRelation> {
    match relation {
        UnboundRelation::Scan { .. }
        | UnboundRelation::Rows { .. }
        | UnboundRelation::Ref { .. }
        | UnboundRelation::RecursiveRef { .. } => Vec::new(),
        UnboundRelation::Filter { input, .. }
        | UnboundRelation::Project { input, .. }
        | UnboundRelation::Aggregate { input, .. }
        | UnboundRelation::Order { input, .. }
        | UnboundRelation::Slice { input, .. }
        | UnboundRelation::Distinct(input) => vec![input],
        UnboundRelation::Join { left, right, .. }
        | UnboundRelation::Intersect { left, right, .. }
        | UnboundRelation::Except { left, right, .. } => vec![left, right],
        UnboundRelation::Concatenate { inputs, .. } => inputs.iter().collect(),
        UnboundRelation::Recursive { anchor, step, .. } => vec![anchor, step],
    }
}

fn unbound_relation_expressions(relation: &UnboundRelation) -> Vec<&UnboundExpr> {
    match relation {
        UnboundRelation::Filter { predicate, .. } | UnboundRelation::Join { on: predicate, .. } => {
            vec![predicate]
        }
        UnboundRelation::Project { fields, .. } => {
            fields.iter().map(|field| &field.expression).collect()
        }
        UnboundRelation::Aggregate { groups, terms, .. } => {
            let mut expressions = Vec::with_capacity(groups.len() + terms.len());
            expressions.extend(groups.iter().map(|group| &group.expression));
            expressions.extend(terms.iter().filter_map(|term| term.argument.as_ref()));
            expressions
        }
        UnboundRelation::Order { terms, .. } => terms.iter().map(|term| &term.expression).collect(),
        _ => Vec::new(),
    }
}

fn unbound_expression_children(expression: &UnboundExpr) -> Vec<&UnboundExpr> {
    match expression {
        UnboundExpr::Literal(_)
        | UnboundExpr::Column { .. }
        | UnboundExpr::Exists(_)
        | UnboundExpr::First(_)
        | UnboundExpr::Scalar(_)
        | UnboundExpr::Array(_) => Vec::new(),
        UnboundExpr::Unary { expression, .. }
        | UnboundExpr::Cast { expression, .. }
        | UnboundExpr::TextMatch {
            value: expression, ..
        } => vec![expression],
        UnboundExpr::Binary { left, right, .. } => vec![left, right],
        UnboundExpr::Branch {
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

fn unbound_crossing_relation(expression: &UnboundExpr) -> Option<&UnboundRelation> {
    match expression {
        UnboundExpr::Exists(relation)
        | UnboundExpr::First(relation)
        | UnboundExpr::Scalar(relation)
        | UnboundExpr::Array(relation) => Some(relation),
        _ => None,
    }
}

fn walk_expression_tree(
    expression: &Expr,
    relation_visitor: &mut impl FnMut(&Relation),
    expression_visitor: &mut impl FnMut(&Expr),
    visit_crossings: bool,
) {
    expression_visitor(expression);
    for child in expression_children(expression) {
        walk_expression_tree(child, relation_visitor, expression_visitor, visit_crossings);
    }
    if visit_crossings {
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

    #[test]
    fn unbound_walk_reaches_inputs_and_crossing_relations() {
        let relation = crate::engine::lir::Relation::Filter {
            input: Box::new(crate::engine::lir::Relation::Ref {
                binding: "input".into(),
                scope: "i".into(),
            }),
            predicate: crate::engine::lir::Expr::Exists(Box::new(
                crate::engine::lir::Relation::Ref {
                    binding: "crossing".into(),
                    scope: "c".into(),
                },
            )),
        };
        let mut bindings = Vec::new();
        walk_unbound_relation(&relation, &mut |relation| {
            if let crate::engine::lir::Relation::Ref { binding, .. } = relation {
                bindings.push(binding.clone());
            }
        });

        assert_eq!(bindings, ["input".to_owned(), "crossing".to_owned()]);
    }
}
