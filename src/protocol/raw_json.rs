use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

/// An owned JSON value that preserves its original lexical representation.
#[derive(Clone)]
pub struct RawJson(Box<RawValue>);

impl RawJson {
    pub fn as_str(&self) -> &str {
        self.0.get()
    }

    pub fn into_inner(self) -> Box<RawValue> {
        self.0
    }
}

impl fmt::Debug for RawJson {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("RawJson")
            .field(&self.as_str())
            .finish()
    }
}

impl PartialEq for RawJson {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for RawJson {}

impl Serialize for RawJson {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RawJson {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Box::<RawValue>::deserialize(deserializer).map(Self)
    }
}

impl From<Box<RawValue>> for RawJson {
    fn from(value: Box<RawValue>) -> Self {
        Self(value)
    }
}
