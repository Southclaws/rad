use crate::engine::catalog::model::ScalarType;
use crate::engine::lir::bound;
use crate::engine::lir::{self, Kind, RawScalar, Type, Value};

use super::{Binder, invalid, require_unique_output};
use crate::engine::planner::{Reason, Result};

impl Binder<'_> {
    #[async_recursion::async_recursion]
    pub(super) async fn bind_expression(&mut self, expression: lir::Expr) -> Result<bound::Expr> {
        match expression {
            lir::Expr::Literal(literal) => {
                if let Some(kind) = literal.kind {
                    if !kind.is_scalar() {
                        return Err(invalid(
                            Reason::TypeMismatch,
                            format!("planner: a literal cannot be of type {kind}"),
                        ));
                    }
                    let nullable = literal.raw == RawScalar::Null;
                    return coerce_literal(literal.raw, &Type::scalar(kind, nullable));
                }
                Ok(bound::Expr::literal(infer_literal(literal.raw)?))
            }
            lir::Expr::Column { scope, name } => self.resolve_column(&scope, &name),
            lir::Expr::Unary { op, expression } => {
                let expression = self.bind_expression(*expression).await?;
                match op {
                    lir::UnaryOp::Not if expression.value_type().kind != Kind::Bool => {
                        return Err(invalid(
                            Reason::TypeMismatch,
                            format!(
                                "planner: not needs a boolean, got {}",
                                expression.value_type()
                            ),
                        ));
                    }
                    lir::UnaryOp::Negate if !expression.value_type().kind.is_numeric() => {
                        return Err(invalid(
                            Reason::TypeMismatch,
                            format!("planner: cannot negate {}", expression.value_type()),
                        ));
                    }
                    lir::UnaryOp::IsNull | lir::UnaryOp::IsNotNull
                        if expression.value_type().kind == Kind::Array =>
                    {
                        return Err(invalid(
                            Reason::TypeMismatch,
                            "planner: null testing is meaningless on an array — arrays are empty, never NULL",
                        ));
                    }
                    _ => {}
                }
                Ok(bound::Expr::unary(op, expression))
            }
            lir::Expr::Binary { op, left, right } => self.bind_binary(op, *left, *right).await,
            lir::Expr::Cast { expression, to } => {
                let expression = self.bind_expression(*expression).await?;
                if !to.is_scalar() {
                    return Err(invalid(
                        Reason::TypeMismatch,
                        format!("planner: cannot cast to {to}"),
                    ));
                }
                let from = expression.value_type().kind;
                if from != to
                    && !matches!(
                        (from, to),
                        (Kind::Int64, Kind::Float64) | (Kind::Float64, Kind::Int64)
                    )
                {
                    return Err(invalid(
                        Reason::TypeMismatch,
                        format!("planner: cannot cast {from} to {to}"),
                    ));
                }
                Ok(bound::Expr::cast(expression, to))
            }
            lir::Expr::Branch { arms, otherwise } => self.bind_branch(arms, *otherwise).await,
            lir::Expr::TextMatch {
                value,
                parts,
                comparison,
            } => {
                let value = self.bind_expression(*value).await?;
                if value.value_type().kind != Kind::Text {
                    return Err(invalid(
                        Reason::TypeMismatch,
                        format!(
                            "planner: text_match value must be text, got {}",
                            value.value_type()
                        ),
                    ));
                }
                bound::Expr::text_match(value, &parts, comparison)
                    .map_err(|error| invalid(Reason::Invalid, format!("planner: {error}")))
            }
            lir::Expr::Exists(relation) => {
                Ok(bound::Expr::exists(self.bind_subrelation(*relation).await?))
            }
            lir::Expr::First(relation) => {
                let relation = self.bind_subrelation(*relation).await?;
                if !relation.cardinality().at_most_one() && !relation.is_ordered() {
                    return Err(invalid(
                        Reason::NondeterministicOrder,
                        "planner: first over an unordered multi-row relation would make results depend on the access path — add an order or make the relation at-most-one",
                    ));
                }
                require_unique_output(&relation, "first crossing output")?;
                Ok(bound::Expr::first(relation))
            }
            lir::Expr::Scalar(relation) => {
                let relation = self.bind_subrelation(*relation).await?;
                if !relation.cardinality().at_most_one() {
                    return Err(invalid(
                        Reason::NondeterministicOrder,
                        "planner: scalar asserts at most one row, but the relation may produce more — aggregate it, slice it, or pin a unique key",
                    ));
                }
                bound::Expr::scalar(relation)
                    .map_err(|error| invalid(Reason::ScalarArity, format!("{error}")))
            }
            lir::Expr::Array(relation) => {
                let relation = self.bind_subrelation(*relation).await?;
                if !relation.is_ordered() {
                    return Err(invalid(
                        Reason::NondeterministicOrder,
                        "planner: array over an unordered relation would make observable collection order depend on the access path — add an order",
                    ));
                }
                require_unique_output(&relation, "array crossing output")?;
                Ok(bound::Expr::array(relation))
            }
        }
    }

    async fn bind_subrelation(&mut self, relation: lir::Relation) -> Result<bound::Relation> {
        let mark = self.scopes.len();
        let relation = self.bind_relation(relation).await?;
        self.scopes.truncate(mark);
        Ok(relation)
    }

    fn resolve_column(&self, scope: &str, name: &str) -> Result<bound::Expr> {
        if scope.is_empty() {
            return Err(invalid(
                Reason::UnknownScope,
                format!("planner: column {name:?} needs a scope qualifier"),
            ));
        }
        let Some(entry) = self.find_scope(scope, 0) else {
            let message = if self.labels.contains(scope) {
                format!(
                    "planner: scope {scope:?} exists but is not visible here — a projection or aggregate closed it, or it belongs to a different sub-relation; label that node's output scope and reference its columns instead"
                )
            } else {
                format!("planner: unknown scope {scope:?}")
            };
            return Err(invalid(Reason::UnknownScope, message));
        };
        let Some(field) = entry.relation.output().lookup(name) else {
            return Err(invalid(
                Reason::UnknownColumn,
                format!("planner: scope {scope:?} has no column {name:?}"),
            ));
        };
        Ok(bound::Expr::slot(
            field.slot,
            format!("{scope}.{name}"),
            field.value_type.clone(),
        ))
    }

    async fn bind_binary(
        &mut self,
        operation: lir::BinaryOp,
        left: lir::Expr,
        right: lir::Expr,
    ) -> Result<bound::Expr> {
        if matches!(operation, lir::BinaryOp::And | lir::BinaryOp::Or) {
            let left = self.bind_expression(left).await?;
            let right = self.bind_expression(right).await?;
            for expression in [&left, &right] {
                if expression.value_type().kind != Kind::Bool {
                    return Err(invalid(
                        Reason::TypeMismatch,
                        format!(
                            "planner: boolean connective needs boolean operands, got {}",
                            expression.value_type()
                        ),
                    ));
                }
            }
            return Ok(bound::Expr::binary(operation, left, right));
        }

        let (left, right) = self.bind_operands(left, right).await?;
        if matches!(
            operation,
            lir::BinaryOp::Eq
                | lir::BinaryOp::Ne
                | lir::BinaryOp::Lt
                | lir::BinaryOp::Lte
                | lir::BinaryOp::Gt
                | lir::BinaryOp::Gte
        ) {
            let left_kind = left.value_type().kind;
            let right_kind = right.value_type().kind;
            if !left_kind.is_scalar() || left_kind != right_kind {
                return Err(invalid(
                    Reason::TypeMismatch,
                    format!("planner: cannot compare {left_kind} with {right_kind}"),
                ));
            }
        } else if !left.value_type().kind.is_numeric() || !right.value_type().kind.is_numeric() {
            return Err(invalid(
                Reason::TypeMismatch,
                format!(
                    "planner: arithmetic needs numeric operands, got {} and {}",
                    left.value_type(),
                    right.value_type()
                ),
            ));
        }
        Ok(bound::Expr::binary(operation, left, right))
    }

    async fn bind_operands(
        &mut self,
        left: lir::Expr,
        right: lir::Expr,
    ) -> Result<(bound::Expr, bound::Expr)> {
        match (left, right) {
            (lir::Expr::Literal(literal), right) if !matches!(right, lir::Expr::Literal(_)) => {
                let right = self.bind_expression(right).await?;
                let left = coerce_literal(literal.raw, &right.value_type())?;
                Ok((left, right))
            }
            (left, lir::Expr::Literal(literal)) if !matches!(left, lir::Expr::Literal(_)) => {
                let left = self.bind_expression(left).await?;
                let right = coerce_literal(literal.raw, &left.value_type())?;
                Ok((left, right))
            }
            (left, right) => Ok((
                self.bind_expression(left).await?,
                self.bind_expression(right).await?,
            )),
        }
    }

    async fn bind_branch(
        &mut self,
        arms: Vec<lir::BranchArm>,
        otherwise: lir::Expr,
    ) -> Result<bound::Expr> {
        if arms.is_empty() {
            return Err(invalid(
                Reason::Invalid,
                "planner: branch needs at least one arm",
            ));
        }
        for (index, arm) in arms.iter().enumerate() {
            reject_branch_crossings(&arm.when, &format!("arm {}'s when", index + 1))?;
            reject_branch_crossings(&arm.then, &format!("arm {}'s then", index + 1))?;
        }
        reject_branch_crossings(&otherwise, "the else")?;

        let mut bound_arms = Vec::with_capacity(arms.len());
        let mut result_kind = None;
        for (index, arm) in arms.into_iter().enumerate() {
            let when = self.bind_expression(arm.when).await?;
            if when.value_type().kind != Kind::Bool {
                return Err(invalid(
                    Reason::TypeMismatch,
                    format!(
                        "planner: branch arm {} when must be boolean, got {}",
                        index + 1,
                        when.value_type()
                    ),
                ));
            }
            let then = self.bind_expression(arm.then).await?;
            let kind = then.value_type().kind;
            if !kind.is_scalar() {
                return Err(invalid(
                    Reason::TypeMismatch,
                    format!("planner: branch arm {} result must be a scalar", index + 1),
                ));
            }
            if let Some(expected) = result_kind
                && expected != kind
            {
                return Err(invalid(
                    Reason::TypeMismatch,
                    format!(
                        "planner: branch arm {} result is {kind} but arm 1 result is {expected}",
                        index + 1
                    ),
                ));
            }
            result_kind = Some(kind);
            bound_arms.push(bound::BranchArm { when, then });
        }
        let otherwise = self.bind_expression(otherwise).await?;
        if Some(otherwise.value_type().kind) != result_kind {
            return Err(invalid(
                Reason::TypeMismatch,
                "planner: branch else and arm results must share one scalar kind",
            ));
        }
        Ok(bound::Expr::branch(bound_arms, otherwise))
    }
}

pub(super) fn coerce_literal(raw: RawScalar, wanted: &Type) -> Result<bound::Expr> {
    let Some(scalar_type) = wanted.kind.catalog_type() else {
        return Err(invalid(
            Reason::TypeMismatch,
            format!("planner: a literal cannot be a {}", wanted.kind),
        ));
    };
    let value = match (raw, scalar_type) {
        (RawScalar::Null, scalar_type) => Value::Null(scalar_type),
        (RawScalar::Text(value), ScalarType::Text) => Value::Text(value),
        (RawScalar::Bool(value), ScalarType::Bool) => Value::Bool(value),
        (RawScalar::Number(value), ScalarType::Int64) => {
            Value::Int64(value.parse().map_err(|_| {
                invalid(
                    Reason::TypeMismatch,
                    format!("planner: expected an int64 value, got {value:?} — cast the column to float64 to compare against fractional values"),
                )
            })?)
        }
        (RawScalar::Number(value), ScalarType::Float64) => {
            Value::Float64(value.parse().map_err(|_| {
                invalid(
                    Reason::TypeMismatch,
                    format!("planner: expected a float64 value, got {value:?}"),
                )
            })?)
        }
        (raw, scalar_type) => {
            return Err(invalid(
                Reason::TypeMismatch,
                format!(
                    "planner: expected a {} value, got {} ({raw:?})",
                    scalar_type_name(scalar_type),
                    raw_scalar_type_name(&raw),
                ),
            ));
        }
    };
    Ok(bound::Expr::literal(value))
}

fn scalar_type_name(scalar_type: ScalarType) -> &'static str {
    match scalar_type {
        ScalarType::Text => "text",
        ScalarType::Int64 => "int64",
        ScalarType::Float64 => "float64",
        ScalarType::Bool => "bool",
    }
}

fn raw_scalar_type_name(raw: &RawScalar) -> &'static str {
    match raw {
        RawScalar::Null => "<nil>",
        RawScalar::Text(_) => "string",
        RawScalar::Number(_) => "json.Number",
        RawScalar::Bool(_) => "bool",
    }
}

fn infer_literal(raw: RawScalar) -> Result<Value> {
    match raw {
        RawScalar::Null => Err(invalid(
            Reason::TypeMismatch,
            "planner: a bare NULL literal needs a typed context",
        )),
        RawScalar::Text(value) => Ok(Value::Text(value)),
        RawScalar::Bool(value) => Ok(Value::Bool(value)),
        RawScalar::Number(value) => {
            if let Ok(integer) = value.parse::<i64>() {
                Ok(Value::Int64(integer))
            } else if let Ok(float) = value.parse::<f64>() {
                Ok(Value::Float64(float))
            } else {
                Err(invalid(
                    Reason::TypeMismatch,
                    format!("planner: malformed number {value:?}"),
                ))
            }
        }
    }
}

fn reject_branch_crossings(expression: &lir::Expr, location: &str) -> Result<()> {
    let crossing = match expression {
        lir::Expr::Exists(_) => Some("exists"),
        lir::Expr::First(_) => Some("first"),
        lir::Expr::Scalar(_) => Some("scalar"),
        lir::Expr::Array(_) => Some("array"),
        _ => None,
    };
    if let Some(crossing) = crossing {
        return Err(invalid(
            Reason::Invalid,
            format!(
                "planner: branch cannot contain a crossing ({crossing} in {location}) — crossings evaluate per row before the branch selects an arm"
            ),
        ));
    }
    match expression {
        lir::Expr::Unary { expression, .. }
        | lir::Expr::Cast { expression, .. }
        | lir::Expr::TextMatch {
            value: expression, ..
        } => reject_branch_crossings(expression, location)?,
        lir::Expr::Binary { left, right, .. } => {
            reject_branch_crossings(left, location)?;
            reject_branch_crossings(right, location)?;
        }
        lir::Expr::Branch {
            arms, otherwise, ..
        } => {
            for arm in arms {
                reject_branch_crossings(&arm.when, location)?;
                reject_branch_crossings(&arm.then, location)?;
            }
            reject_branch_crossings(otherwise, location)?;
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn contains_crossing(expression: &bound::Expr) -> bool {
    let mut found = false;
    lir::inspect::walk_expression(expression, &mut |expression| {
        found |= matches!(
            expression,
            bound::Expr::Exists(_)
                | bound::Expr::First { .. }
                | bound::Expr::Scalar { .. }
                | bound::Expr::Array { .. }
        );
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_type_mismatch_uses_wire_compatible_type_names() {
        let error = coerce_literal(
            RawScalar::Text("not-a-number".to_owned()),
            &Type::scalar(Kind::Int64, false),
        )
        .unwrap_err();

        assert_eq!(error.reason(), Reason::TypeMismatch);
        assert!(
            error
                .to_string()
                .contains("expected a int64 value, got string")
        );
    }
}
