use crate::engine::lir::bound;
use crate::engine::lir::{self, Type};

use super::{Binder, duplicate_column, invalid};
use crate::engine::planner::{Reason, Result};

impl Binder<'_> {
    pub(super) async fn bind_recursive_binding(
        &mut self,
        name: &str,
        anchor: lir::Relation,
        step: lir::Relation,
        accumulation: lir::RecursiveAccumulation,
    ) -> Result<bound::Binding> {
        validate_recursive(name, &anchor, &step)?;
        let anchor = self.bind_relation(anchor).await?;
        self.scopes.clear();
        if let Some(duplicate) = duplicate_column(anchor.output()) {
            return Err(invalid(
                Reason::BindingCollision,
                format!("planner: recursive anchor output has duplicate column {duplicate:?}"),
            ));
        }
        let mut binding = bound::Binding {
            name: name.into(),
            root: anchor.clone(),
            output: anchor.output().clone(),
            plan_sensitive: false,
            recursive: true,
            step: None,
            accumulation: Some(accumulation),
        };
        self.bindings.insert(name.into(), binding.clone());

        let slot_mark = self.next_slot;
        let label_mark = self.labels.clone();
        let previous = self.recursing.clone();
        let bound_step = loop {
            self.next_slot = slot_mark;
            self.labels = label_mark.clone();
            self.scopes.clear();
            self.recursing = Some(name.into());
            let candidate = self.bind_relation(step.clone()).await;
            self.recursing = previous.clone();
            let candidate = candidate?;
            let widened = reconcile_recursive(anchor.output(), candidate.output())?;
            let stable = row_shape_equal(&widened, &binding.output);
            binding.output = widened;
            self.bindings.insert(name.into(), binding.clone());
            if stable {
                break candidate;
            }
        };
        self.scopes.clear();
        binding.step = Some(bound_step.clone());
        binding.plan_sensitive =
            lir::inspect::plan_sensitive(&anchor) || lir::inspect::plan_sensitive(&bound_step);
        self.bindings.insert(name.into(), binding.clone());
        Ok(binding)
    }
}

fn validate_recursive(name: &str, anchor: &lir::Relation, step: &lir::Relation) -> Result<()> {
    check_anchor(name, anchor)?;
    let mut count = 0;
    check_step(name, step, false, &mut count)?;
    match count {
        0 => Err(invalid(
            Reason::Invalid,
            "planner: recursive step contains no recursive_ref",
        )),
        1 => Ok(()),
        count => Err(invalid(
            Reason::Invalid,
            format!(
                "planner: recursive step contains {count} recursive_refs — linear recursion requires exactly one"
            ),
        )),
    }
}

fn check_anchor(name: &str, relation: &lir::Relation) -> Result<()> {
    match relation {
        lir::Relation::RecursiveRef { .. } => {
            return Err(invalid(
                Reason::Invalid,
                "planner: recursive anchor contains a recursive_ref",
            ));
        }
        lir::Relation::Ref { binding, .. } if binding == name => {
            return Err(invalid(
                Reason::BindingCycle,
                "planner: recursive anchor references itself through an ordinary ref",
            ));
        }
        lir::Relation::Recursive { .. } => {
            return Err(invalid(
                Reason::Invalid,
                "planner: nested recursive relations are not valid",
            ));
        }
        _ => {}
    }
    visit_relation(
        relation,
        &mut |child| check_anchor(name, child),
        &mut |expression| {
            check_expression_relations(expression, &mut |child| check_anchor(name, child))
        },
    )
}

fn check_step(
    name: &str,
    relation: &lir::Relation,
    forbidden: bool,
    count: &mut usize,
) -> Result<()> {
    match relation {
        lir::Relation::RecursiveRef { binding, .. } => {
            if binding != name {
                return Err(invalid(
                    Reason::UnknownBinding,
                    format!(
                        "planner: recursive_ref names a different binding {binding:?} — mutual recursion is not supported"
                    ),
                ));
            }
            if forbidden {
                return Err(invalid(
                    Reason::Invalid,
                    "planner: recursive_ref appears in a non-monotone position",
                ));
            }
            *count += 1;
            return Ok(());
        }
        lir::Relation::Ref { binding, .. } if binding == name => {
            return Err(invalid(
                Reason::BindingCycle,
                "planner: recursive step observes itself through an ordinary ref",
            ));
        }
        lir::Relation::Recursive { .. } => {
            return Err(invalid(
                Reason::Invalid,
                "planner: nested recursive relations are not valid",
            ));
        }
        lir::Relation::Join {
            left,
            right,
            kind,
            on,
        } => {
            check_step(name, left, forbidden, count)?;
            check_step(
                name,
                right,
                forbidden || *kind == lir::JoinKind::Left,
                count,
            )?;
            return check_expression_relations(on, &mut |child| {
                let mut ignored = 0;
                check_step(name, child, true, &mut ignored)
            });
        }
        lir::Relation::Except { left, right, .. } => {
            check_step(name, left, forbidden, count)?;
            return check_step(name, right, true, count);
        }
        lir::Relation::Aggregate {
            input,
            groups,
            terms,
            ..
        } => {
            check_step(name, input, true, count)?;
            for group in groups {
                check_expression_relations(&group.expression, &mut |child| {
                    let mut ignored = 0;
                    check_step(name, child, true, &mut ignored)
                })?;
            }
            for term in terms {
                if let Some(argument) = &term.argument {
                    check_expression_relations(argument, &mut |child| {
                        let mut ignored = 0;
                        check_step(name, child, true, &mut ignored)
                    })?;
                }
            }
            return Ok(());
        }
        lir::Relation::Slice { input, .. } => return check_step(name, input, true, count),
        lir::Relation::Order { input, terms } => {
            check_step(name, input, forbidden, count)?;
            for term in terms {
                check_expression_relations(&term.expression, &mut |child| {
                    let mut ignored = 0;
                    check_step(name, child, true, &mut ignored)
                })?;
            }
            return Ok(());
        }
        _ => {}
    }
    visit_relation(
        relation,
        &mut |child| check_step(name, child, forbidden, count),
        &mut |expression| {
            check_expression_relations(expression, &mut |child| {
                let mut ignored = 0;
                check_step(name, child, true, &mut ignored)
            })
        },
    )
}

fn visit_relation(
    relation: &lir::Relation,
    relation_visitor: &mut impl FnMut(&lir::Relation) -> Result<()>,
    expression_visitor: &mut impl FnMut(&lir::Expr) -> Result<()>,
) -> Result<()> {
    match relation {
        lir::Relation::Scan { .. }
        | lir::Relation::Rows { .. }
        | lir::Relation::Ref { .. }
        | lir::Relation::RecursiveRef { .. } => {}
        lir::Relation::Filter { input, predicate } => {
            relation_visitor(input)?;
            expression_visitor(predicate)?;
        }
        lir::Relation::Project { input, fields, .. } => {
            relation_visitor(input)?;
            for field in fields {
                expression_visitor(&field.expression)?;
            }
        }
        lir::Relation::Join {
            left, right, on, ..
        } => {
            relation_visitor(left)?;
            relation_visitor(right)?;
            expression_visitor(on)?;
        }
        lir::Relation::Concatenate { inputs, .. } => {
            for input in inputs {
                relation_visitor(input)?;
            }
        }
        lir::Relation::Intersect { left, right, .. }
        | lir::Relation::Except { left, right, .. } => {
            relation_visitor(left)?;
            relation_visitor(right)?;
        }
        lir::Relation::Aggregate {
            input,
            groups,
            terms,
            ..
        } => {
            relation_visitor(input)?;
            for group in groups {
                expression_visitor(&group.expression)?;
            }
            for term in terms {
                if let Some(argument) = &term.argument {
                    expression_visitor(argument)?;
                }
            }
        }
        lir::Relation::Order { input, terms } => {
            relation_visitor(input)?;
            for term in terms {
                expression_visitor(&term.expression)?;
            }
        }
        lir::Relation::Slice { input, .. } => relation_visitor(input)?,
        lir::Relation::Recursive { anchor, step, .. } => {
            relation_visitor(anchor)?;
            relation_visitor(step)?;
        }
        lir::Relation::Distinct(input) => relation_visitor(input)?,
    }
    Ok(())
}

fn check_expression_relations(
    expression: &lir::Expr,
    relation_visitor: &mut impl FnMut(&lir::Relation) -> Result<()>,
) -> Result<()> {
    match expression {
        lir::Expr::Literal(_) | lir::Expr::Column { .. } => {}
        lir::Expr::Unary { expression, .. }
        | lir::Expr::Cast { expression, .. }
        | lir::Expr::TextMatch {
            value: expression, ..
        } => check_expression_relations(expression, relation_visitor)?,
        lir::Expr::Binary { left, right, .. } => {
            check_expression_relations(left, relation_visitor)?;
            check_expression_relations(right, relation_visitor)?;
        }
        lir::Expr::Branch {
            arms, otherwise, ..
        } => {
            for arm in arms {
                check_expression_relations(&arm.when, relation_visitor)?;
                check_expression_relations(&arm.then, relation_visitor)?;
            }
            check_expression_relations(otherwise, relation_visitor)?;
        }
        lir::Expr::Exists(relation)
        | lir::Expr::First(relation)
        | lir::Expr::Scalar(relation)
        | lir::Expr::Array(relation) => relation_visitor(relation)?,
    }
    Ok(())
}

fn row_shape_equal(left: &lir::RowType, right: &lir::RowType) -> bool {
    left.fields.len() == right.fields.len()
        && left.fields.iter().zip(&right.fields).all(|(left, right)| {
            left.name == right.name && type_shape_equal(&left.value_type, &right.value_type)
        })
}

fn type_shape_equal(left: &Type, right: &Type) -> bool {
    left.kind == right.kind
        && left.nullable == right.nullable
        && match left.kind {
            lir::Kind::Row => match (&left.row, &right.row) {
                (Some(left), Some(right)) => row_shape_equal(left, right),
                (None, None) => true,
                _ => false,
            },
            lir::Kind::Array => match (&left.element, &right.element) {
                (Some(left), Some(right)) => type_shape_equal(left, right),
                (None, None) => true,
                _ => false,
            },
            _ => true,
        }
}

fn reconcile_recursive(anchor: &lir::RowType, step: &lir::RowType) -> Result<lir::RowType> {
    if anchor.fields.len() != step.fields.len() {
        return Err(invalid(
            Reason::TypeMismatch,
            format!(
                "planner: recursive anchor produces {} columns but the step produces {}",
                anchor.fields.len(),
                step.fields.len()
            ),
        ));
    }
    let mut fields = Vec::with_capacity(anchor.fields.len());
    for anchor_field in &anchor.fields {
        let Some(step_field) = step.lookup(&anchor_field.name) else {
            return Err(invalid(
                Reason::TypeMismatch,
                format!(
                    "planner: recursive step is missing anchor column {:?}",
                    anchor_field.name
                ),
            ));
        };
        let mut field = anchor_field.clone();
        field.value_type = reconcile_type(
            &anchor_field.value_type,
            &step_field.value_type,
            &anchor_field.name,
        )?;
        fields.push(field);
    }
    Ok(lir::RowType { fields })
}

fn reconcile_type(anchor: &Type, step: &Type, path: &str) -> Result<Type> {
    if anchor.kind != step.kind {
        return Err(invalid(
            Reason::TypeMismatch,
            format!(
                "planner: recursive column {path:?} is {} in the anchor but {} in the step",
                anchor.kind, step.kind
            ),
        ));
    }
    let mut output = anchor.clone();
    output.nullable |= step.nullable;
    match anchor.kind {
        lir::Kind::Row => match (&anchor.row, &step.row) {
            (Some(anchor_row), Some(step_row)) => {
                output.row = Some(Box::new(reconcile_nested_row(anchor_row, step_row, path)?));
            }
            (None, None) => {}
            _ => {
                return Err(invalid(
                    Reason::TypeMismatch,
                    format!("planner: recursive column {path:?} has incompatible row shapes"),
                ));
            }
        },
        lir::Kind::Array => match (&anchor.element, &step.element) {
            (Some(anchor_element), Some(step_element)) => {
                output.element = Some(Box::new(reconcile_type(
                    anchor_element,
                    step_element,
                    &format!("{path}[]"),
                )?));
            }
            (None, None) => {}
            _ => {
                return Err(invalid(
                    Reason::TypeMismatch,
                    format!("planner: recursive column {path:?} has incompatible array shapes"),
                ));
            }
        },
        _ => {}
    }
    Ok(output)
}

fn reconcile_nested_row(
    anchor: &lir::RowType,
    step: &lir::RowType,
    path: &str,
) -> Result<lir::RowType> {
    if anchor.fields.len() != step.fields.len() {
        return Err(invalid(
            Reason::TypeMismatch,
            format!("planner: recursive column {path:?} has incompatible row shapes"),
        ));
    }
    let mut fields = Vec::with_capacity(anchor.fields.len());
    for anchor_field in &anchor.fields {
        let Some(step_field) = step.lookup(&anchor_field.name) else {
            return Err(invalid(
                Reason::TypeMismatch,
                format!("planner: recursive column {path:?} has incompatible row shapes"),
            ));
        };
        let mut field = anchor_field.clone();
        field.value_type = reconcile_type(
            &anchor_field.value_type,
            &step_field.value_type,
            &format!("{path}.{}", anchor_field.name),
        )?;
        fields.push(field);
    }
    Ok(lir::RowType { fields })
}
