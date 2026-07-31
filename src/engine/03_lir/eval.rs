//! Pure reference evaluation for bound scalar expressions.

use std::cmp::Ordering;
use std::collections::HashSet;

use crate::engine::catalog::model::ScalarType;

use super::bound::{Expr, TextPattern};
use super::{BinaryOp, Datum, Field, Kind, SlotId, SlotRow, TriBool, Type, UnaryOp, Value};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Env {
    values: SlotRow,
}

impl Env {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, slot: SlotId, datum: Datum) -> Option<Datum> {
        self.values.insert(slot, datum)
    }

    pub fn set_scalar(&mut self, slot: SlotId, value: Value) {
        self.values.insert(slot, Datum::scalar(value));
    }

    pub fn get(&self, slot: SlotId) -> Option<&Datum> {
        self.values.get(&slot)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&SlotId, &Datum)> {
        self.values.iter()
    }

    pub fn extend_from(&mut self, other: &Self) {
        self.values.extend(
            other
                .values
                .iter()
                .map(|(slot, datum)| (*slot, datum.clone())),
        );
    }

    pub fn scalar_at(&self, slot: SlotId, name: &str, value_type: &Type) -> Result<Value> {
        match self.values.get(&slot) {
            Some(Datum::Scalar(value)) => Ok(value.clone()),
            Some(Datum::Null) => Ok(Value::Null(value_type.kind.catalog_type().ok_or_else(
                || {
                    EvalError::internal(format!(
                        "exec: slot {} ({name}) has non-scalar type {}",
                        slot.0, value_type.kind
                    ))
                },
            )?)),
            Some(Datum::Object(_) | Datum::Array(_)) => Err(EvalError::internal(format!(
                "exec: slot {} ({name}) holds a nested value, not a scalar",
                slot.0
            ))),
            None => Err(EvalError::internal(format!(
                "exec: slot {} ({name}) not in scope",
                slot.0
            ))),
        }
    }
}

pub fn evaluate_predicate(expression: &Expr, environment: &Env) -> Result<TriBool> {
    match expression {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
            ..
        } => {
            let left = evaluate_predicate(left, environment)?;
            if left == TriBool::False {
                return Ok(TriBool::False);
            }
            Ok(left.and(evaluate_predicate(right, environment)?))
        }
        Expr::Binary {
            op: BinaryOp::Or,
            left,
            right,
            ..
        } => {
            let left = evaluate_predicate(left, environment)?;
            if left == TriBool::True {
                return Ok(TriBool::True);
            }
            Ok(left.or(evaluate_predicate(right, environment)?))
        }
        Expr::Binary {
            op:
                op @ (BinaryOp::Eq
                | BinaryOp::Ne
                | BinaryOp::Lt
                | BinaryOp::Lte
                | BinaryOp::Gt
                | BinaryOp::Gte),
            left,
            right,
            ..
        } => evaluate_comparison(*op, left, right, environment),
        Expr::Unary {
            op: UnaryOp::Not,
            expression,
            ..
        } => Ok(!evaluate_predicate(expression, environment)?),
        Expr::Unary {
            op: op @ (UnaryOp::IsNull | UnaryOp::IsNotNull),
            expression,
            ..
        } => {
            let is_null = matches!(evaluate_datum(expression, environment)?, Datum::Null);
            Ok(TriBool::from_bool(if *op == UnaryOp::IsNull {
                is_null
            } else {
                !is_null
            }))
        }
        _ => match evaluate(expression, environment)? {
            Value::Bool(value) => Ok(TriBool::from_bool(value)),
            Value::Null(ScalarType::Bool) => Ok(TriBool::Unknown),
            value => Err(EvalError::internal(format!(
                "exec: predicate evaluated to {:?}, want bool",
                value.scalar_type()
            ))),
        },
    }
}

fn evaluate_comparison(
    operation: BinaryOp,
    left: &Expr,
    right: &Expr,
    environment: &Env,
) -> Result<TriBool> {
    let left = evaluate(left, environment)?;
    let right = evaluate(right, environment)?;
    if left.is_null() || right.is_null() {
        return Ok(TriBool::Unknown);
    }
    let ordering = left
        .compare(&right)
        .map_err(|error| EvalError::internal(format!("exec: {error}")))?;
    Ok(TriBool::from_bool(match operation {
        BinaryOp::Eq => ordering == Ordering::Equal,
        BinaryOp::Ne => ordering != Ordering::Equal,
        BinaryOp::Lt => ordering == Ordering::Less,
        BinaryOp::Lte => ordering != Ordering::Greater,
        BinaryOp::Gt => ordering == Ordering::Greater,
        BinaryOp::Gte => ordering != Ordering::Less,
        _ => unreachable!("comparison operation"),
    }))
}

pub fn evaluate(expression: &Expr, environment: &Env) -> Result<Value> {
    match expression {
        Expr::Literal(value) => Ok(value.clone()),
        Expr::SlotRef {
            slot,
            name,
            value_type,
        } => {
            if !value_type.kind.is_scalar() {
                return Err(EvalError::internal(format!(
                    "exec: slot {} ({name}) holds a {}, not a scalar",
                    slot.0, value_type.kind
                )));
            }
            environment.scalar_at(*slot, name, value_type)
        }
        Expr::Binary {
            op: op @ (BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div),
            left,
            right,
            value_type,
        } => evaluate_arithmetic(*op, left, right, value_type.kind, environment),
        Expr::Unary {
            op: UnaryOp::Negate,
            expression,
            ..
        } => match evaluate(expression, environment)? {
            Value::Null(value_type) => Ok(Value::Null(value_type)),
            Value::Int64(value) => value.checked_neg().map(Value::Int64).ok_or_else(|| {
                EvalError::numeric_overflow(format!("exec: integer overflow: negate {value}"))
            }),
            Value::Float64(value) => Ok(Value::Float64(-value)),
            value => Err(EvalError::internal(format!(
                "exec: cannot negate {:?}",
                value.scalar_type()
            ))),
        },
        Expr::Binary { .. } | Expr::Unary { .. } => {
            Ok(tri_value(evaluate_predicate(expression, environment)?))
        }
        Expr::Cast { expression, to, .. } => evaluate_cast(expression, *to, environment),
        Expr::Branch {
            arms, otherwise, ..
        } => {
            for arm in arms {
                if evaluate_predicate(&arm.when, environment)? == TriBool::True {
                    return evaluate(&arm.then, environment);
                }
            }
            evaluate(otherwise, environment)
        }
        Expr::TextMatch { value, pattern, .. } => evaluate_text_match(value, pattern, environment),
        Expr::Exists(_) | Expr::First { .. } | Expr::Scalar { .. } | Expr::Array { .. } => Err(
            EvalError::internal("exec: unextracted cardinality crossing reached scalar evaluation"),
        ),
    }
}

pub fn evaluate_datum(expression: &Expr, environment: &Env) -> Result<Datum> {
    if let Expr::SlotRef {
        slot,
        name,
        value_type,
    } = expression
        && !value_type.kind.is_scalar()
    {
        return environment.get(*slot).cloned().ok_or_else(|| {
            EvalError::internal(format!("exec: slot {} ({name}) not in scope", slot.0))
        });
    }
    evaluate(expression, environment).map(Datum::scalar)
}

fn tri_value(value: TriBool) -> Value {
    match value {
        TriBool::True => Value::Bool(true),
        TriBool::False => Value::Bool(false),
        TriBool::Unknown => Value::Null(ScalarType::Bool),
    }
}

fn evaluate_arithmetic(
    operation: BinaryOp,
    left: &Expr,
    right: &Expr,
    result_kind: Kind,
    environment: &Env,
) -> Result<Value> {
    let left = evaluate(left, environment)?;
    let right = evaluate(right, environment)?;
    let result_type = result_kind
        .catalog_type()
        .expect("bound arithmetic has a scalar result");
    if left.is_null() || right.is_null() {
        return Ok(Value::Null(result_type));
    }

    if result_kind == Kind::Int64 {
        let (Value::Int64(left), Value::Int64(right)) = (left, right) else {
            return Err(EvalError::internal(
                "exec: non-int operands for int arithmetic",
            ));
        };
        let value = match operation {
            BinaryOp::Add => left.checked_add(right),
            BinaryOp::Sub => left.checked_sub(right),
            BinaryOp::Mul => left.checked_mul(right),
            BinaryOp::Div if right == 0 => {
                return Err(EvalError::division_by_zero("exec: division by zero"));
            }
            BinaryOp::Div => left.checked_div(right),
            _ => unreachable!("arithmetic operation"),
        };
        return value.map(Value::Int64).ok_or_else(|| {
            EvalError::numeric_overflow(format!(
                "exec: integer overflow: {left} {} {right}",
                operation_name(operation)
            ))
        });
    }

    let left = as_float(left)?;
    let right = as_float(right)?;
    if operation == BinaryOp::Div && right == 0.0 {
        return Err(EvalError::division_by_zero("exec: division by zero"));
    }
    let value = match operation {
        BinaryOp::Add => left + right,
        BinaryOp::Sub => left - right,
        BinaryOp::Mul => left * right,
        BinaryOp::Div => left / right,
        _ => unreachable!("arithmetic operation"),
    };
    if !value.is_finite() {
        return Err(EvalError::numeric_overflow(format!(
            "exec: float overflow: {left} {} {right}",
            operation_name(operation)
        )));
    }
    Ok(Value::Float64(value))
}

fn as_float(value: Value) -> Result<f64> {
    match value {
        Value::Int64(value) => Ok(value as f64),
        Value::Float64(value) => Ok(value),
        value => Err(EvalError::internal(format!(
            "exec: cannot use {:?} in float arithmetic",
            value.scalar_type()
        ))),
    }
}

fn evaluate_cast(expression: &Expr, to: Kind, environment: &Env) -> Result<Value> {
    let value = evaluate(expression, environment)?;
    let target = to
        .catalog_type()
        .ok_or_else(|| EvalError::internal("exec: cast target is not scalar"))?;
    if value.is_null() {
        return Ok(Value::Null(target));
    }
    if value.scalar_type() == target {
        return Ok(value);
    }
    match (value, target) {
        (Value::Int64(value), ScalarType::Float64) => Ok(Value::Float64(value as f64)),
        (Value::Float64(value), ScalarType::Int64) => {
            if !value.is_finite() || value >= i64::MAX as f64 || value < i64::MIN as f64 {
                return Err(EvalError::numeric_overflow(format!(
                    "exec: cast {value} to int64 is out of range"
                )));
            }
            Ok(Value::Int64(value as i64))
        }
        (value, target) => Err(EvalError::internal(format!(
            "exec: cannot cast {:?} to {target:?}",
            value.scalar_type()
        ))),
    }
}

fn evaluate_text_match(value: &Expr, pattern: &TextPattern, environment: &Env) -> Result<Value> {
    match evaluate(value, environment)? {
        Value::Null(_) => Ok(Value::Null(ScalarType::Bool)),
        Value::Text(value) => Ok(Value::Bool(pattern.is_match(&value))),
        value => Err(EvalError::internal(format!(
            "exec: text_match received {:?}",
            value.scalar_type()
        ))),
    }
}

fn operation_name(operation: BinaryOp) -> &'static str {
    match operation {
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        _ => "?",
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvalErrorKind {
    Runtime,
    Internal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvalErrorReason {
    Runtime,
    DivisionByZero,
    NumericOverflow,
    Internal,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct EvalError {
    reason: EvalErrorReason,
    message: String,
}

impl EvalError {
    fn division_by_zero(message: impl Into<String>) -> Self {
        Self::with_reason(EvalErrorReason::DivisionByZero, message)
    }

    fn numeric_overflow(message: impl Into<String>) -> Self {
        Self::with_reason(EvalErrorReason::NumericOverflow, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::with_reason(EvalErrorReason::Internal, message)
    }

    fn with_reason(reason: EvalErrorReason, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }

    pub fn kind(&self) -> EvalErrorKind {
        match self.reason {
            EvalErrorReason::Runtime
            | EvalErrorReason::DivisionByZero
            | EvalErrorReason::NumericOverflow => EvalErrorKind::Runtime,
            EvalErrorReason::Internal => EvalErrorKind::Internal,
        }
    }

    pub fn reason(&self) -> EvalErrorReason {
        self.reason
    }
}

pub type Result<T> = std::result::Result<T, EvalError>;

/// Shared full-row identity for distinct, recursion, intersect, and except.
pub struct CanonicalRowSet {
    slots: Vec<SlotId>,
    seen: HashSet<CanonicalRowKey>,
}

impl CanonicalRowSet {
    pub fn new(fields: Vec<Field>) -> Self {
        Self {
            slots: fields.into_iter().map(|field| field.slot).collect(),
            seen: HashSet::new(),
        }
    }

    pub fn insert(&mut self, row: &Env) -> bool {
        self.seen
            .insert(canonical_key(self.slots.iter().copied(), row))
    }

    pub fn contains(&self, row: &Env) -> bool {
        self.seen
            .contains(&canonical_key(self.slots.iter().copied(), row))
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CanonicalRowKey(Vec<CanonicalDatum>);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum CanonicalDatum {
    Missing,
    Null,
    Text(String),
    Int64(i64),
    Float64(u64),
    Bool(bool),
    Object(Vec<(String, CanonicalDatum)>),
    Array(Vec<CanonicalDatum>),
}

impl CanonicalDatum {
    fn from_datum(datum: Option<&Datum>) -> Self {
        match datum {
            None => Self::Missing,
            Some(Datum::Null | Datum::Scalar(Value::Null(_))) => Self::Null,
            Some(Datum::Scalar(Value::Text(value))) => Self::Text(value.clone()),
            Some(Datum::Scalar(Value::Int64(value))) => Self::Int64(*value),
            Some(Datum::Scalar(Value::Float64(value))) => {
                let bits = if value.is_nan() {
                    f64::NAN.to_bits()
                } else if *value == 0.0 {
                    0.0_f64.to_bits()
                } else {
                    value.to_bits()
                };
                Self::Float64(bits)
            }
            Some(Datum::Scalar(Value::Bool(value))) => Self::Bool(*value),
            Some(Datum::Object(fields)) => Self::Object(
                fields
                    .iter()
                    .map(|field| (field.name.clone(), Self::from_datum(Some(&field.datum))))
                    .collect(),
            ),
            Some(Datum::Array(elements)) => Self::Array(
                elements
                    .iter()
                    .map(|element| Self::from_datum(Some(element)))
                    .collect(),
            ),
        }
    }
}

pub fn canonical_row_key(fields: &[Field], row: &Env) -> CanonicalRowKey {
    canonical_key(fields.iter().map(|field| field.slot), row)
}

fn canonical_key(slots: impl IntoIterator<Item = SlotId>, row: &Env) -> CanonicalRowKey {
    CanonicalRowKey(
        slots
            .into_iter()
            .map(|slot| CanonicalDatum::from_datum(row.get(slot)))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::super::bound::BranchArm;
    use super::super::{TextComparison, TextMatchPart};
    use super::*;
    use crate::engine::lir::RowType;

    fn literal(value: Value) -> Expr {
        Expr::literal(value)
    }

    fn predicate(expression: &Expr) -> TriBool {
        evaluate_predicate(expression, &Env::new()).unwrap()
    }

    #[test]
    fn comparisons_and_connectives_use_kleene_logic() {
        let null_int = literal(Value::Null(ScalarType::Int64));
        let one = literal(Value::Int64(1));
        for operation in [
            BinaryOp::Eq,
            BinaryOp::Ne,
            BinaryOp::Lt,
            BinaryOp::Lte,
            BinaryOp::Gt,
            BinaryOp::Gte,
        ] {
            assert_eq!(
                predicate(&Expr::binary(operation, one.clone(), null_int.clone())),
                TriBool::Unknown
            );
        }
        assert_eq!(
            predicate(&Expr::unary(
                UnaryOp::Not,
                Expr::binary(BinaryOp::Eq, null_int, one)
            )),
            TriBool::Unknown
        );

        let values = [
            (TriBool::False, Value::Bool(false)),
            (TriBool::Unknown, Value::Null(ScalarType::Bool)),
            (TriBool::True, Value::Bool(true)),
        ];
        for (left_tri, left) in &values {
            for (right_tri, right) in &values {
                assert_eq!(
                    predicate(&Expr::binary(
                        BinaryOp::And,
                        literal(left.clone()),
                        literal(right.clone())
                    )),
                    left_tri.and(*right_tri)
                );
                assert_eq!(
                    predicate(&Expr::binary(
                        BinaryOp::Or,
                        literal(left.clone()),
                        literal(right.clone())
                    )),
                    left_tri.or(*right_tri)
                );
            }
        }
    }

    #[test]
    fn nested_and_scalar_slots_round_trip_as_one_datum_vocabulary() {
        let mut environment = Env::new();
        environment.insert(
            SlotId(3),
            Datum::Object(vec![super::super::ObjectField {
                name: "id".into(),
                datum: Datum::scalar(Value::Text("u1".into())),
            }]),
        );
        environment.set_scalar(SlotId(4), Value::Null(ScalarType::Float64));
        let nested = Expr::slot(SlotId(3), "owner", Type::row(RowType::default(), true));
        assert!(matches!(
            evaluate_datum(&nested, &environment).unwrap(),
            Datum::Object(_)
        ));
        assert_eq!(
            predicate_with_env(&Expr::unary(UnaryOp::IsNull, nested), &environment),
            TriBool::False
        );
        environment.insert(SlotId(3), Datum::Null);
        assert_eq!(
            predicate_with_env(
                &Expr::unary(
                    UnaryOp::IsNull,
                    Expr::slot(SlotId(3), "owner", Type::row(RowType::default(), true)),
                ),
                &environment,
            ),
            TriBool::True
        );
        assert_eq!(
            evaluate(
                &Expr::slot(SlotId(4), "estimate", Type::scalar(Kind::Float64, true)),
                &environment
            )
            .unwrap(),
            Value::Null(ScalarType::Float64)
        );
    }

    fn predicate_with_env(expression: &Expr, environment: &Env) -> TriBool {
        evaluate_predicate(expression, environment).unwrap()
    }

    #[test]
    fn integer_arithmetic_is_checked_and_float_casts_are_deterministic() {
        for (operation, left, right, expected) in [
            (BinaryOp::Add, 2, 3, 5),
            (BinaryOp::Sub, 2, 3, -1),
            (BinaryOp::Mul, 2, 3, 6),
            (BinaryOp::Div, 7, 3, 2),
        ] {
            assert_eq!(
                evaluate(
                    &Expr::binary(
                        operation,
                        literal(Value::Int64(left)),
                        literal(Value::Int64(right))
                    ),
                    &Env::new()
                )
                .unwrap(),
                Value::Int64(expected)
            );
        }
        for expression in [
            Expr::binary(
                BinaryOp::Add,
                literal(Value::Int64(i64::MAX)),
                literal(Value::Int64(1)),
            ),
            Expr::binary(
                BinaryOp::Div,
                literal(Value::Int64(1)),
                literal(Value::Int64(0)),
            ),
            Expr::unary(UnaryOp::Negate, literal(Value::Int64(i64::MIN))),
        ] {
            assert_eq!(
                evaluate(&expression, &Env::new()).unwrap_err().kind(),
                EvalErrorKind::Runtime
            );
        }
        assert_eq!(
            evaluate(
                &Expr::cast(literal(Value::Float64(-3.9)), Kind::Int64),
                &Env::new()
            )
            .unwrap(),
            Value::Int64(-3)
        );
        for value in [f64::NAN, f64::INFINITY, i64::MAX as f64] {
            assert_eq!(
                evaluate(
                    &Expr::cast(literal(Value::Float64(value)), Kind::Int64),
                    &Env::new()
                )
                .unwrap_err()
                .kind(),
                EvalErrorKind::Runtime
            );
        }
    }

    #[test]
    fn ordered_comparison_matrix_matches_independent_ordering() {
        let operations = [
            BinaryOp::Eq,
            BinaryOp::Ne,
            BinaryOp::Lt,
            BinaryOp::Lte,
            BinaryOp::Gt,
            BinaryOp::Gte,
        ];
        let expected = |operation, ordering| {
            TriBool::from_bool(match operation {
                BinaryOp::Eq => ordering == Ordering::Equal,
                BinaryOp::Ne => ordering != Ordering::Equal,
                BinaryOp::Lt => ordering == Ordering::Less,
                BinaryOp::Lte => ordering != Ordering::Greater,
                BinaryOp::Gt => ordering == Ordering::Greater,
                BinaryOp::Gte => ordering != Ordering::Less,
                _ => unreachable!(),
            })
        };

        let integers = [i64::MIN, -1, 0, 1, i64::MAX];
        for left in integers {
            for right in integers {
                for operation in operations {
                    assert_eq!(
                        predicate(&Expr::binary(
                            operation,
                            literal(Value::Int64(left)),
                            literal(Value::Int64(right)),
                        )),
                        expected(operation, left.cmp(&right))
                    );
                }
            }
        }

        let floats = [-1.5, -0.0, 0.0, 1.5, 2.5];
        for left in floats {
            for right in floats {
                for operation in operations {
                    assert_eq!(
                        predicate(&Expr::binary(
                            operation,
                            literal(Value::Float64(left)),
                            literal(Value::Float64(right)),
                        )),
                        expected(operation, left.partial_cmp(&right).unwrap())
                    );
                }
            }
        }

        let texts = ["", "a", "ab", "b"];
        for left in texts {
            for right in texts {
                for operation in operations {
                    assert_eq!(
                        predicate(&Expr::binary(
                            operation,
                            literal(Value::Text(left.into())),
                            literal(Value::Text(right.into())),
                        )),
                        expected(operation, left.cmp(right))
                    );
                }
            }
        }
    }

    #[test]
    fn integer_arithmetic_matrix_never_wraps() {
        let values = [
            i64::MIN,
            i64::MIN + 1,
            -3,
            -2,
            -1,
            0,
            1,
            2,
            3,
            i64::MAX - 1,
            i64::MAX,
        ];
        for left in values {
            let negated = evaluate(
                &Expr::unary(UnaryOp::Negate, literal(Value::Int64(left))),
                &Env::new(),
            );
            if left == i64::MIN {
                assert_eq!(negated.unwrap_err().kind(), EvalErrorKind::Runtime);
            } else {
                assert_eq!(negated.unwrap(), Value::Int64(-left));
            }

            for right in values {
                for operation in [BinaryOp::Add, BinaryOp::Sub, BinaryOp::Mul, BinaryOp::Div] {
                    let actual = evaluate(
                        &Expr::binary(
                            operation,
                            literal(Value::Int64(left)),
                            literal(Value::Int64(right)),
                        ),
                        &Env::new(),
                    );
                    let exact = match operation {
                        BinaryOp::Add => Some(left as i128 + right as i128),
                        BinaryOp::Sub => Some(left as i128 - right as i128),
                        BinaryOp::Mul => Some(left as i128 * right as i128),
                        BinaryOp::Div if right == 0 => None,
                        BinaryOp::Div => Some(left as i128 / right as i128),
                        _ => unreachable!(),
                    };
                    match exact.and_then(|value| i64::try_from(value).ok()) {
                        Some(expected) => assert_eq!(actual.unwrap(), Value::Int64(expected)),
                        None if operation == BinaryOp::Div && right == 0 => {
                            let error = actual.unwrap_err();
                            assert_eq!(error.kind(), EvalErrorKind::Runtime);
                            assert_eq!(error.reason(), EvalErrorReason::DivisionByZero);
                        }
                        None => {
                            let error = actual.unwrap_err();
                            assert_eq!(error.kind(), EvalErrorKind::Runtime);
                            assert_eq!(error.reason(), EvalErrorReason::NumericOverflow);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn float_arithmetic_matrix_distinguishes_every_operation_and_zero_rule() {
        let cases: [(BinaryOp, f64, f64, f64); 7] = [
            (BinaryOp::Add, 7.5, 2.0, 9.5),
            (BinaryOp::Sub, 7.5, 2.0, 5.5),
            (BinaryOp::Mul, 7.5, 2.0, 15.0),
            (BinaryOp::Div, 7.5, 2.0, 3.75),
            (BinaryOp::Add, -3.0, 0.0, -3.0),
            (BinaryOp::Sub, -3.0, 0.0, -3.0),
            (BinaryOp::Mul, -3.0, 0.0, -0.0),
        ];
        for (operation, left, right, expected) in cases {
            let actual = evaluate(
                &Expr::binary(
                    operation,
                    literal(Value::Float64(left)),
                    literal(Value::Float64(right)),
                ),
                &Env::new(),
            )
            .unwrap();
            let Value::Float64(actual) = actual else {
                panic!("float arithmetic produced {actual:?}");
            };
            assert_eq!(
                actual.to_bits(),
                expected.to_bits(),
                "{left} {} {right}",
                operation_name(operation)
            );
        }

        for zero in [0.0, -0.0] {
            let error = evaluate(
                &Expr::binary(
                    BinaryOp::Div,
                    literal(Value::Float64(1.0)),
                    literal(Value::Float64(zero)),
                ),
                &Env::new(),
            )
            .unwrap_err();
            assert_eq!(error.kind(), EvalErrorKind::Runtime);
            assert_eq!(error.reason(), EvalErrorReason::DivisionByZero);
        }
    }

    #[test]
    fn float_arithmetic_rejects_non_finite_results() {
        for (operation, left, right) in [
            (BinaryOp::Add, f64::MAX, f64::MAX),
            (BinaryOp::Mul, f64::MAX, 2.0),
        ] {
            let error = evaluate(
                &Expr::binary(
                    operation,
                    literal(Value::Float64(left)),
                    literal(Value::Float64(right)),
                ),
                &Env::new(),
            )
            .unwrap_err();
            assert_eq!(error.kind(), EvalErrorKind::Runtime);
            assert_eq!(error.reason(), EvalErrorReason::NumericOverflow);
        }
    }

    #[test]
    fn mixed_numeric_arithmetic_promotes_to_float_for_every_operation() {
        for (operation, expected) in [
            (BinaryOp::Add, 9.5),
            (BinaryOp::Sub, 5.5),
            (BinaryOp::Mul, 15.0),
            (BinaryOp::Div, 3.75),
        ] {
            assert_eq!(
                evaluate(
                    &Expr::binary(
                        operation,
                        literal(Value::Float64(7.5)),
                        literal(Value::Int64(2)),
                    ),
                    &Env::new(),
                )
                .unwrap(),
                Value::Float64(expected),
            );
        }
    }

    #[test]
    fn numeric_nulls_floats_and_cast_boundaries_match_the_contract() {
        for operation in [BinaryOp::Add, BinaryOp::Sub, BinaryOp::Mul, BinaryOp::Div] {
            for (left, right, expected_type) in [
                (
                    Value::Int64(1),
                    Value::Null(ScalarType::Int64),
                    ScalarType::Int64,
                ),
                (
                    Value::Null(ScalarType::Int64),
                    Value::Int64(1),
                    ScalarType::Int64,
                ),
                (
                    Value::Float64(1.0),
                    Value::Null(ScalarType::Float64),
                    ScalarType::Float64,
                ),
            ] {
                assert_eq!(
                    evaluate(
                        &Expr::binary(operation, literal(left), literal(right)),
                        &Env::new(),
                    )
                    .unwrap(),
                    Value::Null(expected_type)
                );
            }
        }
        assert_eq!(
            evaluate(
                &Expr::binary(
                    BinaryOp::Mul,
                    literal(Value::Int64(2)),
                    literal(Value::Float64(1.5)),
                ),
                &Env::new(),
            )
            .unwrap(),
            Value::Float64(3.0)
        );
        assert_eq!(
            evaluate(
                &Expr::binary(
                    BinaryOp::Div,
                    literal(Value::Float64(1.0)),
                    literal(Value::Float64(-0.0)),
                ),
                &Env::new(),
            )
            .unwrap_err()
            .kind(),
            EvalErrorKind::Runtime
        );

        for (value, expected) in [
            (0.0, 0),
            (0.9, 0),
            (-0.9, 0),
            (3.9, 3),
            (-3.9, -3),
            (9.0e18, 9_000_000_000_000_000_000),
            (i64::MIN as f64, i64::MIN),
        ] {
            assert_eq!(
                evaluate(
                    &Expr::cast(literal(Value::Float64(value)), Kind::Int64),
                    &Env::new(),
                )
                .unwrap(),
                Value::Int64(expected)
            );
        }
        for value in [
            1e19,
            -1e19,
            f64::MAX,
            -f64::MAX,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            i64::MAX as f64,
        ] {
            assert_eq!(
                evaluate(
                    &Expr::cast(literal(Value::Float64(value)), Kind::Int64),
                    &Env::new(),
                )
                .unwrap_err()
                .kind(),
                EvalErrorKind::Runtime
            );
        }
    }

    #[test]
    fn branches_are_ordered_lazy_and_k3_aware() {
        let boom = || {
            Expr::binary(
                BinaryOp::Div,
                literal(Value::Int64(1)),
                literal(Value::Int64(0)),
            )
        };
        let branch = Expr::branch(
            vec![
                BranchArm {
                    when: literal(Value::Bool(false)),
                    then: boom(),
                },
                BranchArm {
                    when: literal(Value::Null(ScalarType::Bool)),
                    then: boom(),
                },
                BranchArm {
                    when: literal(Value::Bool(true)),
                    then: literal(Value::Int64(2)),
                },
                BranchArm {
                    when: boom(),
                    then: literal(Value::Int64(3)),
                },
            ],
            boom(),
        );
        assert_eq!(evaluate(&branch, &Env::new()).unwrap(), Value::Int64(2));

        let selected_error = Expr::branch(
            vec![BranchArm {
                when: literal(Value::Bool(true)),
                then: boom(),
            }],
            literal(Value::Int64(2)),
        );
        assert_eq!(
            evaluate(&selected_error, &Env::new()).unwrap_err().kind(),
            EvalErrorKind::Runtime
        );
    }

    #[test]
    fn crossings_and_invalid_scalar_environments_are_internal_errors() {
        let rows = crate::engine::lir::bound::Relation::rows("r", Vec::new(), Vec::new());
        for crossing in [
            Expr::exists(rows.clone()),
            Expr::first(rows.clone()),
            Expr::array(rows),
        ] {
            assert_eq!(
                evaluate(&crossing, &Env::new()).unwrap_err().kind(),
                EvalErrorKind::Internal
            );
        }
        assert_eq!(
            evaluate(
                &Expr::slot(SlotId(9), "ghost", Type::scalar(Kind::Text, false)),
                &Env::new(),
            )
            .unwrap_err()
            .kind(),
            EvalErrorKind::Internal
        );
        let nested = Expr::slot(SlotId(3), "owner", Type::row(RowType::default(), true));
        let mut environment = Env::new();
        environment.insert(SlotId(3), Datum::Object(Vec::new()));
        assert_eq!(
            evaluate(&nested, &environment).unwrap_err().kind(),
            EvalErrorKind::Internal
        );
    }

    #[test]
    fn text_match_and_canonical_rows_match_the_reference_contract() {
        let expression = Expr::text_match(
            literal(Value::Text("FOO---bar".into())),
            &[
                TextMatchPart::Literal("foo".into()),
                TextMatchPart::AnyMany,
                TextMatchPart::Literal("BAR".into()),
            ],
            TextComparison::UnicodeSimpleFold,
        )
        .unwrap();
        assert_eq!(
            evaluate(&expression, &Env::new()).unwrap(),
            Value::Bool(true)
        );

        let fields = vec![
            Field {
                name: "a".into(),
                slot: SlotId(0),
                value_type: Type::scalar(Kind::Text, true),
            },
            Field {
                name: "b".into(),
                slot: SlotId(1),
                value_type: Type::scalar(Kind::Int64, true),
            },
        ];
        let mut set = CanonicalRowSet::new(fields);
        let mut row = Env::new();
        row.set_scalar(SlotId(0), Value::Text("x".into()));
        row.set_scalar(SlotId(1), Value::Int64(1));
        assert!(set.insert(&row));
        assert!(!set.insert(&row));
        assert!(set.contains(&row));

        let one_field = vec![Field {
            name: "x".into(),
            slot: SlotId(0),
            value_type: Type::scalar(Kind::Text, false),
        }];
        let mut keys = HashSet::new();
        for value in [
            Value::Text("1".into()),
            Value::Int64(1),
            Value::Float64(1.0),
            Value::Bool(true),
        ] {
            let mut row = Env::new();
            row.set_scalar(SlotId(0), value);
            assert!(keys.insert(canonical_row_key(&one_field, &row)));
        }
    }

    #[test]
    fn canonical_rows_match_numeric_and_nested_value_equality() {
        let scalar_field = vec![Field {
            name: "value".into(),
            slot: SlotId(0),
            value_type: Type::scalar(Kind::Float64, false),
        }];
        let mut positive_zero = Env::new();
        positive_zero.set_scalar(SlotId(0), Value::Float64(0.0));
        let mut negative_zero = Env::new();
        negative_zero.set_scalar(SlotId(0), Value::Float64(-0.0));
        assert_eq!(
            canonical_row_key(&scalar_field, &positive_zero),
            canonical_row_key(&scalar_field, &negative_zero)
        );

        let nested_field = vec![Field {
            name: "value".into(),
            slot: SlotId(0),
            value_type: Type::array(Type::scalar(Kind::Int64, false)),
        }];
        let mut one = Env::new();
        one.insert(
            SlotId(0),
            Datum::Array(vec![Datum::Scalar(Value::Int64(1))]),
        );
        let mut two = Env::new();
        two.insert(
            SlotId(0),
            Datum::Array(vec![Datum::Scalar(Value::Int64(2))]),
        );
        assert_ne!(
            canonical_row_key(&nested_field, &one),
            canonical_row_key(&nested_field, &two)
        );

        let mut set = CanonicalRowSet::new(nested_field);
        assert!(set.insert(&one));
        assert!(set.insert(&two));
        assert!(!set.insert(&one));
    }
}
