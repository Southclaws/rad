use std::fmt;

use crate::engine::catalog::model::ScalarType;

/// Dense, query-local identity assigned to one bound relation attribute.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SlotId(pub usize);

impl From<usize> for SlotId {
    fn from(value: usize) -> Self {
        Self(value)
    }
}

/// Static LIR kinds. The four scalar variants mirror catalog scalar types.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Kind {
    Text,
    Int64,
    Float64,
    Bool,
    Row,
    Array,
}

impl Kind {
    pub fn of(value: ScalarType) -> Self {
        match value {
            ScalarType::Text => Self::Text,
            ScalarType::Int64 => Self::Int64,
            ScalarType::Float64 => Self::Float64,
            ScalarType::Bool => Self::Bool,
        }
    }

    pub fn is_scalar(self) -> bool {
        matches!(self, Self::Text | Self::Int64 | Self::Float64 | Self::Bool)
    }

    pub fn is_numeric(self) -> bool {
        matches!(self, Self::Int64 | Self::Float64)
    }

    pub fn catalog_type(self) -> Option<ScalarType> {
        match self {
            Self::Text => Some(ScalarType::Text),
            Self::Int64 => Some(ScalarType::Int64),
            Self::Float64 => Some(ScalarType::Float64),
            Self::Bool => Some(ScalarType::Bool),
            Self::Row | Self::Array => None,
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Text => "text",
            Self::Int64 => "int64",
            Self::Float64 => "float64",
            Self::Bool => "bool",
            Self::Row => "row",
            Self::Array => "array",
        })
    }
}

/// Static expression/attribute type, including nested shape and nullability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Type {
    pub kind: Kind,
    pub nullable: bool,
    pub row: Option<Box<RowType>>,
    pub element: Option<Box<Type>>,
}

impl Type {
    pub const fn scalar(kind: Kind, nullable: bool) -> Self {
        Self {
            kind,
            nullable,
            row: None,
            element: None,
        }
    }

    pub fn catalog_scalar(value: ScalarType, nullable: bool) -> Self {
        Self::scalar(Kind::of(value), nullable)
    }

    pub fn row(row: RowType, nullable: bool) -> Self {
        Self {
            kind: Kind::Row,
            nullable,
            row: Some(Box::new(row)),
            element: None,
        }
    }

    pub fn array(element: Type) -> Self {
        Self {
            kind: Kind::Array,
            nullable: false,
            row: None,
            element: Some(Box::new(element)),
        }
    }

    pub fn with_nullable(mut self, nullable: bool) -> Self {
        self.nullable = nullable;
        self
    }
}

impl fmt::Display for Type {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.kind == Kind::Array
            && let Some(element) = &self.element
        {
            write!(formatter, "array<{element}>")?;
        } else {
            write!(formatter, "{}", self.kind)?;
        }
        if self.nullable {
            formatter.write_str("?")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Field {
    pub name: String,
    pub slot: SlotId,
    pub value_type: Type,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RowType {
    pub fields: Vec<Field>,
}

impl RowType {
    pub fn lookup(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|field| field.name == name)
    }

    pub fn slots(&self) -> Vec<SlotId> {
        self.fields.iter().map(|field| field.slot).collect()
    }
}

pub const UNBOUNDED: i64 = -1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cardinality {
    pub min: i64,
    pub max: i64,
}

impl Cardinality {
    pub const MANY: Self = Self {
        min: 0,
        max: UNBOUNDED,
    };

    pub fn at_most_one(self) -> bool {
        self.max != UNBOUNDED && self.max <= 1
    }
}

impl fmt::Display for Cardinality {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.max == UNBOUNDED {
            write!(formatter, "{}..many", self.min)
        } else {
            write!(formatter, "{}..{}", self.min, self.max)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TriBool {
    False,
    Unknown,
    True,
}

impl TriBool {
    pub fn from_bool(value: bool) -> Self {
        if value { Self::True } else { Self::False }
    }

    pub fn and(self, other: Self) -> Self {
        use TriBool::{False, True, Unknown};
        match (self, other) {
            (False, _) | (_, False) => False,
            (Unknown, _) | (_, Unknown) => Unknown,
            (True, True) => True,
        }
    }

    pub fn or(self, other: Self) -> Self {
        use TriBool::{False, True, Unknown};
        match (self, other) {
            (True, _) | (_, True) => True,
            (Unknown, _) | (_, Unknown) => Unknown,
            (False, False) => False,
        }
    }
}

impl std::ops::Not for TriBool {
    type Output = Self;

    fn not(self) -> Self::Output {
        match self {
            Self::False => Self::True,
            Self::Unknown => Self::Unknown,
            Self::True => Self::False,
        }
    }
}

impl fmt::Display for TriBool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::False => "FALSE",
            Self::Unknown => "UNKNOWN",
            Self::True => "TRUE",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kleene_tables_and_type_shapes_match_the_contract() {
        let values = [TriBool::False, TriBool::Unknown, TriBool::True];
        let and = [
            [TriBool::False, TriBool::False, TriBool::False],
            [TriBool::False, TriBool::Unknown, TriBool::Unknown],
            [TriBool::False, TriBool::Unknown, TriBool::True],
        ];
        let or = [
            [TriBool::False, TriBool::Unknown, TriBool::True],
            [TriBool::Unknown, TriBool::Unknown, TriBool::True],
            [TriBool::True, TriBool::True, TriBool::True],
        ];
        for (left_index, left) in values.into_iter().enumerate() {
            for (right_index, right) in values.into_iter().enumerate() {
                assert_eq!(left.and(right), and[left_index][right_index]);
                assert_eq!(left.or(right), or[left_index][right_index]);
            }
        }
        assert_eq!(!TriBool::Unknown, TriBool::Unknown);
        assert_eq!(Type::scalar(Kind::Int64, true).to_string(), "int64?");
        assert_eq!(
            Type::array(Type::scalar(Kind::Text, false)).to_string(),
            "array<text>"
        );
    }
}
