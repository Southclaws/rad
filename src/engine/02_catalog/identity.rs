use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! string_identity {
    ($name:ident) => {
        #[derive(
            Clone, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
        )]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }

            /// The numeric part of a `<kind><decimal>` physical identity.
            /// Storage keys carry only this number; the keyspace tag implies
            /// the kind. Only the canonical decimal form is accepted: a
            /// non-canonical spelling ("t01", "t+1") must not alias the
            /// canonical identity's storage key. Returns `None` when the
            /// identity does not have the given kind prefix followed by a
            /// canonical positive decimal.
            pub fn physical_number(&self, kind: &str) -> Option<u64> {
                let digits = self.0.strip_prefix(kind)?;
                let value = digits.parse::<u64>().ok().filter(|value| *value > 0)?;
                (value.to_string() == digits).then_some(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

macro_rules! generation {
    ($name:ident) => {
        #[derive(
            Clone,
            Copy,
            Debug,
            Default,
            Deserialize,
            Eq,
            Hash,
            Ord,
            PartialEq,
            PartialOrd,
            Serialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub u64);

        impl $name {
            pub const ZERO: Self = Self(0);

            pub fn get(self) -> u64 {
                self.0
            }

            pub fn is_zero(&self) -> bool {
                self.0 == 0
            }

            pub fn next(self) -> Self {
                Self(self.0.checked_add(1).expect("catalog generation overflow"))
            }
        }

        impl From<u64> for $name {
            fn from(value: u64) -> Self {
                Self(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

string_identity!(TableId);
string_identity!(ColumnId);
string_identity!(IndexId);
string_identity!(LogicalIndexId);
string_identity!(ConstraintId);
string_identity!(TransitionId);
string_identity!(ReclamationId);
string_identity!(RetentionPinId);

generation!(CatalogVersion);
generation!(DefinitionGeneration);
generation!(ExistenceGeneration);
generation!(WriteProtocolGeneration);
generation!(ValueGeneration);
generation!(AccessGeneration);
generation!(TransitionGeneration);
generation!(OwnerEpoch);

/// Physical storage generation of a table's authoritative rows. Data keys
/// embed it between table identity and row identity, so a replacement
/// physical representation is built beside the authoritative one and
/// activated with one catalog switch. Every table starts at generation 1.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct StorageGeneration(pub u64);

impl StorageGeneration {
    pub const INITIAL: Self = Self(1);

    pub fn get(self) -> u64 {
        self.0
    }

    pub fn is_initial(&self) -> bool {
        *self == Self::INITIAL
    }

    pub fn next(self) -> Self {
        Self(self.0.checked_add(1).expect("storage generation overflow"))
    }
}

impl Default for StorageGeneration {
    fn default() -> Self {
        Self::INITIAL
    }
}

impl From<u64> for StorageGeneration {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl fmt::Display for StorageGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

pub const MAX_SCHEMA_ID: u32 = (1 << 31) - 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct SchemaId(u32);

impl SchemaId {
    pub fn new(value: u32) -> Result<Self, InvalidSchemaId> {
        if value == 0 || value > MAX_SCHEMA_ID {
            return Err(InvalidSchemaId(value));
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for SchemaId {
    type Error = InvalidSchemaId;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<SchemaId> for u32 {
    fn from(value: SchemaId) -> Self {
        value.0
    }
}

impl fmt::Display for SchemaId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("schema ID {0} is outside 1..={MAX_SCHEMA_ID}")]
pub struct InvalidSchemaId(u32);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_ids_enforce_the_authored_range() {
        assert!(SchemaId::new(0).is_err());
        assert!(SchemaId::new(MAX_SCHEMA_ID).is_ok());
        assert!(SchemaId::new(MAX_SCHEMA_ID + 1).is_err());
        assert!(serde_json::from_str::<SchemaId>("0").is_err());
    }

    #[test]
    fn identity_newtypes_keep_the_durable_scalar_shape() {
        assert_eq!(
            serde_json::to_string(&TableId::from("t1")).unwrap(),
            "\"t1\""
        );
        assert_eq!(
            serde_json::to_string(&CatalogVersion::from(7)).unwrap(),
            "7"
        );
    }

    #[test]
    fn physical_numbers_accept_only_the_canonical_decimal_form() {
        assert_eq!(TableId::from("t1").physical_number("t"), Some(1));
        assert_eq!(
            TableId::from("t18446744073709551615").physical_number("t"),
            Some(u64::MAX)
        );
        for alias in ["t01", "t+1", "t 1", "t0", "t", "x1", "t1x", "t-1"] {
            assert_eq!(
                TableId::from(alias).physical_number("t"),
                None,
                "{alias:?} must not alias a canonical identity"
            );
        }
    }
}
