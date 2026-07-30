use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;

use crate::engine::catalog::model::ScalarType;

use super::SlotId;

/// Typed SQL-like scalar. Null retains its catalog scalar type.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Text(String),
    Int64(i64),
    Float64(f64),
    Bool(bool),
    Null(ScalarType),
}

impl Value {
    pub fn scalar_type(&self) -> ScalarType {
        match self {
            Self::Text(_) => ScalarType::Text,
            Self::Int64(_) => ScalarType::Int64,
            Self::Float64(_) => ScalarType::Float64,
            Self::Bool(_) => ScalarType::Bool,
            Self::Null(value_type) => *value_type,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null(_))
    }

    /// Two-valued storage equality: null never equals anything, including null.
    pub fn storage_eq(&self, other: &Self) -> bool {
        !self.is_null() && !other.is_null() && self == other
    }

    /// Key-compatible ordering. Null sorts before every non-null value.
    pub fn compare(&self, other: &Self) -> Result<Ordering, ValueError> {
        match (self, other) {
            (Self::Null(_), Self::Null(_)) => return Ok(Ordering::Equal),
            (Self::Null(_), _) => return Ok(Ordering::Less),
            (_, Self::Null(_)) => return Ok(Ordering::Greater),
            _ => {}
        }
        if self.scalar_type() != other.scalar_type() {
            return Err(ValueError::DifferentTypes(
                self.scalar_type(),
                other.scalar_type(),
            ));
        }
        Ok(match (self, other) {
            (Self::Text(left), Self::Text(right)) => left.cmp(right),
            (Self::Int64(left), Self::Int64(right)) => left.cmp(right),
            (Self::Float64(left), Self::Float64(right)) => match left.partial_cmp(right) {
                Some(ordering) => ordering,
                None if left.is_nan() && right.is_nan() => Ordering::Equal,
                None if left.is_nan() => Ordering::Less,
                None => Ordering::Greater,
            },
            (Self::Bool(left), Self::Bool(right)) => left.cmp(right),
            _ => unreachable!("equal scalar types have matching variants"),
        })
    }
}

impl fmt::Display for Value {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(value) => write!(formatter, "{value:?}"),
            Self::Int64(value) => write!(formatter, "{value}"),
            Self::Float64(value) => write!(formatter, "{value}"),
            Self::Bool(value) => write!(formatter, "{value}"),
            Self::Null(_) => formatter.write_str("NULL"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ValueError {
    #[error("lir: cannot compare {0:?} with {1:?}")]
    DifferentTypes(ScalarType, ScalarType),
}

pub type Row = HashMap<String, Value>;

/// Typed read-result tree. Null has one representation at every depth.
#[derive(Clone, Debug, PartialEq)]
pub enum Datum {
    Null,
    Scalar(Value),
    Object(Vec<ObjectField>),
    Array(Vec<Datum>),
}

impl Datum {
    pub fn scalar(value: Value) -> Self {
        if value.is_null() {
            Self::Null
        } else {
            Self::Scalar(value)
        }
    }

    pub fn array(elements: impl IntoIterator<Item = Datum>) -> Self {
        Self::Array(elements.into_iter().collect())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ObjectField {
    pub name: String,
    pub datum: Datum,
}

pub type SlotRow = HashMap<SlotId, Datum>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_classification_covers_every_scalar_variant() {
        for value in [
            Value::Text(String::new()),
            Value::Int64(0),
            Value::Float64(-0.0),
            Value::Bool(false),
        ] {
            assert!(!value.is_null(), "{value:?} was classified as NULL");
        }

        for scalar_type in [
            ScalarType::Text,
            ScalarType::Int64,
            ScalarType::Float64,
            ScalarType::Bool,
        ] {
            assert!(Value::Null(scalar_type).is_null());
        }
    }

    #[test]
    fn storage_equality_excludes_null_and_compares_non_null_values() {
        let cases = [
            (Value::Text("same".into()), Value::Text("same".into()), true),
            (
                Value::Text("left".into()),
                Value::Text("right".into()),
                false,
            ),
            (Value::Int64(1), Value::Int64(1), true),
            (Value::Int64(1), Value::Int64(2), false),
            (Value::Float64(-0.0), Value::Float64(0.0), true),
            (Value::Float64(f64::NAN), Value::Float64(f64::NAN), false),
            (Value::Bool(false), Value::Bool(false), true),
            (Value::Bool(false), Value::Bool(true), false),
            (
                Value::Null(ScalarType::Text),
                Value::Text("value".into()),
                false,
            ),
            (
                Value::Text("value".into()),
                Value::Null(ScalarType::Text),
                false,
            ),
            (
                Value::Null(ScalarType::Text),
                Value::Null(ScalarType::Text),
                false,
            ),
        ];

        for (left, right, expected) in cases {
            assert_eq!(
                left.storage_eq(&right),
                expected,
                "storage equality for {left:?} and {right:?}"
            );
        }
    }

    #[test]
    fn comparison_defines_the_complete_scalar_order() {
        assert_order(
            &Value::Null(ScalarType::Text),
            &Value::Null(ScalarType::Int64),
            Ordering::Equal,
        );
        assert_order(
            &Value::Null(ScalarType::Text),
            &Value::Int64(1),
            Ordering::Less,
        );
        assert_order(
            &Value::Text("a".into()),
            &Value::Text("b".into()),
            Ordering::Less,
        );
        assert_order(&Value::Int64(-1), &Value::Int64(1), Ordering::Less);
        assert_order(&Value::Float64(-1.0), &Value::Float64(1.0), Ordering::Less);
        assert_order(&Value::Float64(-0.0), &Value::Float64(0.0), Ordering::Equal);
        assert_order(&Value::Bool(false), &Value::Bool(true), Ordering::Less);
        assert_order(
            &Value::Float64(f64::NAN),
            &Value::Float64(f64::NAN),
            Ordering::Equal,
        );
        assert_order(
            &Value::Float64(f64::NAN),
            &Value::Float64(-1.0),
            Ordering::Less,
        );

        assert!(matches!(
            Value::Text("1".into()).compare(&Value::Int64(1)),
            Err(ValueError::DifferentTypes(
                ScalarType::Text,
                ScalarType::Int64
            ))
        ));
    }

    #[test]
    fn display_retains_diagnostic_scalar_shape() {
        let cases = [
            (Value::Text("rad".into()), "\"rad\""),
            (Value::Int64(-7), "-7"),
            (Value::Float64(-0.0), "-0"),
            (Value::Bool(true), "true"),
            (Value::Null(ScalarType::Float64), "NULL"),
        ];

        for (value, expected) in cases {
            assert_eq!(value.to_string(), expected, "formatting {value:?}");
        }
    }

    fn assert_order(left: &Value, right: &Value, expected: Ordering) {
        assert_eq!(
            left.compare(right).unwrap(),
            expected,
            "{left:?} vs {right:?}"
        );
        assert_eq!(
            right.compare(left).unwrap(),
            expected.reverse(),
            "{right:?} vs {left:?}"
        );
    }
}
