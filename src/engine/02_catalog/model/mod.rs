mod dependencies;
mod reclamations;
mod retention;
mod schema;
mod transitions;

use std::str::FromStr;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Deserializer, Serialize};

use super::identity::{
    AccessGeneration, CatalogVersion, ColumnId, DefinitionGeneration, ExistenceGeneration, IndexId,
    LogicalIndexId, SchemaId, TableId, ValueGeneration, WriteProtocolGeneration,
};
use super::{Error, ErrorKind, Result};

pub use dependencies::*;
pub use reclamations::*;
pub use retention::*;
pub use schema::*;
pub use transitions::*;

pub(crate) fn is_false(value: &bool) -> bool {
    !value
}

pub(crate) fn is_zero(value: &u64) -> bool {
    *value == 0
}

pub(crate) fn is_empty<T>(value: &[T]) -> bool {
    value.is_empty()
}

pub(crate) fn deserialize_null_default<'de, D, T>(
    deserializer: D,
) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

/// A UTC timestamp with Rad's RFC 3339 durable representation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Timestamp(DateTime<Utc>);

impl Timestamp {
    #[cfg(test)]
    pub(crate) fn test_value() -> Self {
        Self(DateTime::from_timestamp(1_700_000_000, 0).expect("test timestamp is valid"))
    }

    pub fn as_datetime(self) -> DateTime<Utc> {
        self.0
    }

    pub fn is_zero(&self) -> bool {
        *self == Self::default()
    }
}

impl From<DateTime<Utc>> for Timestamp {
    fn from(value: DateTime<Utc>) -> Self {
        Self(value)
    }
}

impl Default for Timestamp {
    fn default() -> Self {
        let date = NaiveDate::from_ymd_opt(1, 1, 1).expect("year one is a valid date");
        Self(DateTime::from_naive_utc_and_offset(
            date.and_hms_opt(0, 0, 0).expect("midnight is valid"),
            Utc,
        ))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScalarType {
    Text,
    Int64,
    Float64,
    Bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultFunction {
    Uuid,
    NowMs,
}

/// The durable/catalog representation of an authored column default.
/// Exactly one value member is meaningful, selected by the column type.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultValue {
    #[serde(rename = "func", skip_serializing_if = "Option::is_none")]
    pub function: Option<DefaultFunction>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "is_i64_zero")]
    pub int64: i64,
    #[serde(default, skip_serializing_if = "is_f64_zero")]
    pub float64: f64,
    #[serde(default, rename = "bool", skip_serializing_if = "is_false")]
    pub bool_value: bool,
}

fn is_i64_zero(value: &i64) -> bool {
    *value == 0
}

fn is_f64_zero(value: &f64) -> bool {
    *value == 0.0
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Table {
    pub id: TableId,
    pub schema_id: SchemaId,
    pub name: String,
    #[serde(default, skip_serializing_if = "DefinitionGeneration::is_zero")]
    pub definition_generation: DefinitionGeneration,
    #[serde(default, skip_serializing_if = "ExistenceGeneration::is_zero")]
    pub existence_generation: ExistenceGeneration,
    #[serde(default, skip_serializing_if = "WriteProtocolGeneration::is_zero")]
    pub write_protocol_generation: WriteProtocolGeneration,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub columns: Vec<Column>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub primary_key: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub indexes: Vec<Index>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub foreign_keys: Vec<ForeignKey>,
    #[serde(
        default,
        deserialize_with = "deserialize_null_default",
        skip_serializing_if = "is_empty"
    )]
    pub constraints: Vec<Constraint>,
}

impl Table {
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|column| column.name == name)
    }

    pub fn index(&self, name: &str) -> Option<&Index> {
        self.indexes.iter().find(|index| index.name == name)
    }

    pub fn index_column_names<'a>(&'a self, index: &'a Index) -> Vec<&'a str> {
        if index.column_ids.len() != index.columns.len() {
            return index.columns.iter().map(String::as_str).collect();
        }
        let mut names = Vec::with_capacity(index.column_ids.len());
        for id in &index.column_ids {
            let Some(column) = self.columns.iter().find(|column| &column.id == id) else {
                return index.columns.iter().map(String::as_str).collect();
            };
            names.push(column.name.as_str());
        }
        names
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Column {
    pub id: ColumnId,
    pub schema_id: SchemaId,
    pub name: String,
    #[serde(default, skip_serializing_if = "ValueGeneration::is_zero")]
    pub value_generation: ValueGeneration,
    #[serde(rename = "type")]
    pub scalar_type: ScalarType,
    pub nullable: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insert_default: Option<DefaultValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing_value: Option<DefaultValue>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Index {
    pub id: IndexId,
    #[serde(default, skip_serializing_if = "LogicalIndexId::is_empty")]
    pub logical_id: LogicalIndexId,
    #[serde(default, skip_serializing_if = "DefinitionGeneration::is_zero")]
    pub definition_generation: DefinitionGeneration,
    #[serde(default, skip_serializing_if = "AccessGeneration::is_zero")]
    pub access_generation: AccessGeneration,
    #[serde(default, skip_serializing_if = "IndexState::is_legacy_ready")]
    pub state: IndexState,
    pub name: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub columns: Vec<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_null_default",
        skip_serializing_if = "is_empty"
    )]
    pub column_ids: Vec<ColumnId>,
    pub unique: bool,
}

impl Index {
    pub fn is_ready(&self) -> bool {
        matches!(self.state, IndexState::LegacyReady | IndexState::Ready)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignKey {
    pub id: String,
    pub name: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub columns: Vec<String>,
    pub ref_table_id: TableId,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub ref_columns: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TableDef {
    pub id: SchemaId,
    pub name: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub columns: Vec<ColumnDef>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub primary_key: Vec<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_null_default",
        skip_serializing_if = "is_empty"
    )]
    pub indexes: Vec<IndexDef>,
    #[serde(
        default,
        deserialize_with = "deserialize_null_default",
        skip_serializing_if = "is_empty"
    )]
    pub foreign_keys: Vec<ForeignKeyDef>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnDef {
    pub id: SchemaId,
    pub name: String,
    #[serde(rename = "type")]
    pub scalar_type: ScalarType,
    #[serde(default, skip_serializing_if = "is_false")]
    pub nullable: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<DefaultValue>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IndexDef {
    pub name: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub columns: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub unique: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignKeyDef {
    pub name: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub columns: Vec<String>,
    pub ref_table: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub ref_columns: Vec<String>,
}

/// A table definition entering Direct-mode allocation. Durable canonical
/// definitions use [`TableDef`] and therefore always contain valid IDs.
#[derive(Clone, Debug, PartialEq)]
pub struct TableDraft {
    pub id: Option<SchemaId>,
    pub name: String,
    pub columns: Vec<ColumnDraft>,
    pub primary_key: Vec<String>,
    pub indexes: Vec<IndexDef>,
    pub foreign_keys: Vec<ForeignKeyDef>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnDraft {
    pub id: Option<SchemaId>,
    pub name: String,
    pub scalar_type: ScalarType,
    pub nullable: bool,
    pub format: String,
    pub default: Option<DefaultValue>,
}

impl From<TableDef> for TableDraft {
    fn from(value: TableDef) -> Self {
        Self {
            id: Some(value.id),
            name: value.name,
            columns: value.columns.into_iter().map(Into::into).collect(),
            primary_key: value.primary_key,
            indexes: value.indexes,
            foreign_keys: value.foreign_keys,
        }
    }
}

impl From<ColumnDef> for ColumnDraft {
    fn from(value: ColumnDef) -> Self {
        Self {
            id: Some(value.id),
            name: value.name,
            scalar_type: value.scalar_type,
            nullable: value.nullable,
            format: value.format,
            default: value.default,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Revision {
    pub version: CatalogVersion,
    pub created_at: Timestamp,
    pub hash: String,
    pub schema: Schema,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Direct,
    Schema,
}

impl FromStr for Mode {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "direct" => Ok(Self::Direct),
            "schema" => Ok(Self::Schema),
            _ => Err(Error::message(
                ErrorKind::InvalidInput,
                format!("catalog: unknown catalog mode {value:?} (direct or schema)"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(value: u32) -> SchemaId {
        SchemaId::new(value).unwrap()
    }

    #[test]
    fn table_helpers_resolve_stable_index_columns() {
        let table = Table {
            id: "t1".into(),
            schema_id: sid(1),
            name: "users".into(),
            definition_generation: 1.into(),
            existence_generation: 1.into(),
            write_protocol_generation: 1.into(),
            columns: vec![Column {
                id: "c1".into(),
                schema_id: sid(1),
                name: "renamed".into(),
                value_generation: 1.into(),
                scalar_type: ScalarType::Text,
                nullable: false,
                format: String::new(),
                insert_default: None,
                missing_value: None,
            }],
            primary_key: vec!["renamed".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            constraints: Vec::new(),
        };
        let index = Index {
            id: "i1".into(),
            logical_id: "li1".into(),
            name: "by_old_name".into(),
            columns: vec!["old_name".into()],
            column_ids: vec!["c1".into()],
            unique: false,
            ..Index::default()
        };
        assert_eq!(table.index_column_names(&index), vec!["renamed"]);
    }

    #[test]
    fn mode_parsing_is_closed() {
        assert_eq!("direct".parse::<Mode>().unwrap(), Mode::Direct);
        assert_eq!("schema".parse::<Mode>().unwrap(), Mode::Schema);
        assert!("other".parse::<Mode>().is_err());
    }

    #[test]
    fn physical_table_matches_the_shared_fixture() {
        let raw = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/catalog/physical-table.json"
        ))
        .trim();
        let table: Table = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_string(&table).unwrap(), raw);
    }
}
