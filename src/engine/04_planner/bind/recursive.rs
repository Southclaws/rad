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
            let stable = widened == binding.output;
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
    lir::inspect::try_visit_unbound_relation_parts(
        relation,
        &mut |child| check_anchor(name, child),
        &mut |expression| {
            lir::inspect::try_visit_unbound_expression_relations(expression, &mut |child| {
                check_anchor(name, child)
            })
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
            return check_forbidden_expression(name, on);
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
                check_forbidden_expression(name, &group.expression)?;
            }
            for term in terms {
                if let Some(argument) = &term.argument {
                    check_forbidden_expression(name, argument)?;
                }
            }
            return Ok(());
        }
        lir::Relation::Slice { input, .. } => return check_step(name, input, true, count),
        lir::Relation::Order { input, terms } => {
            check_step(name, input, forbidden, count)?;
            for term in terms {
                check_forbidden_expression(name, &term.expression)?;
            }
            return Ok(());
        }
        _ => {}
    }
    lir::inspect::try_visit_unbound_relation_parts(
        relation,
        &mut |child| check_step(name, child, forbidden, count),
        &mut |expression| check_forbidden_expression(name, expression),
    )
}

fn check_forbidden_expression(name: &str, expression: &lir::Expr) -> Result<()> {
    lir::inspect::try_visit_unbound_expression_relations(expression, &mut |child| {
        let mut ignored = 0;
        check_step(name, child, true, &mut ignored)
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn recursive_ref() -> lir::Relation {
        lir::Relation::RecursiveRef {
            binding: "walk".into(),
            scope: "current".into(),
        }
    }

    fn scan() -> lir::Relation {
        lir::Relation::Scan {
            table: "nodes".into(),
            scope: "n".into(),
        }
    }

    #[test]
    fn recursive_crossings_preserve_anchor_and_non_monotone_rules() {
        let anchor_crossing = lir::Relation::Filter {
            input: Box::new(scan()),
            predicate: lir::Expr::Exists(Box::new(recursive_ref())),
        };
        let error = validate_recursive("walk", &anchor_crossing, &recursive_ref()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("anchor contains a recursive_ref")
        );

        let step_crossing = lir::Relation::Filter {
            input: Box::new(recursive_ref()),
            predicate: lir::Expr::Exists(Box::new(recursive_ref())),
        };
        let error = validate_recursive("walk", &scan(), &step_crossing).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("recursive_ref appears in a non-monotone position")
        );
    }
}
