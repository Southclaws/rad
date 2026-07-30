use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    ColumnDef, ForeignKeyDef, IndexDef, Table, TableDef, deserialize_null_default, is_empty,
};
use crate::engine::catalog::identity::{SchemaId, TableId};
use crate::engine::catalog::{Error, ErrorKind, Result};

/// Canonical logical catalog shape. Physical identities and operational state
/// never participate in this value or its hash.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Schema {
    #[serde(
        default,
        deserialize_with = "deserialize_null_default",
        skip_serializing_if = "is_empty"
    )]
    pub tables: Vec<TableDef>,
}

impl Schema {
    pub fn canonical_json(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|error| {
            Error::source(
                ErrorKind::CatalogDrift,
                "catalog: encode canonical schema",
                error,
            )
        })
    }

    pub fn hash(&self) -> Result<String> {
        let digest = Sha256::digest(self.canonical_json()?);
        let mut hash = String::with_capacity(7 + digest.len() * 2);
        hash.push_str("sha256:");
        for byte in digest {
            write!(&mut hash, "{byte:02x}").expect("writing to a String cannot fail");
        }
        Ok(hash)
    }

    pub fn canonical_eq(&self, other: &Self) -> Result<bool> {
        Ok(self.canonical_json()? == other.canonical_json()?)
    }

    /// Canonicalize authored definitions. Table order is stable schema
    /// identity order; index and foreign-key declaration order is immaterial.
    pub fn from_definitions(mut tables: Vec<TableDef>) -> Self {
        for table in &mut tables {
            table.indexes.sort_by(|left, right| {
                left.name
                    .cmp(&right.name)
                    .then_with(|| left.columns.join("\0").cmp(&right.columns.join("\0")))
                    .then_with(|| left.unique.cmp(&right.unique))
            });
            table.foreign_keys.sort_by(|left, right| {
                (
                    &left.name,
                    left.columns.join("\0"),
                    &left.ref_table,
                    left.ref_columns.join("\0"),
                )
                    .cmp(&(
                        &right.name,
                        right.columns.join("\0"),
                        &right.ref_table,
                        right.ref_columns.join("\0"),
                    ))
            });
        }
        tables.sort_by(|left, right| {
            left.id
                .cmp(&right.id)
                .then_with(|| left.name.cmp(&right.name))
        });
        Self { tables }
    }

    /// Reconstruct the canonical logical schema from physical definitions,
    /// validating identity uniqueness and resolving foreign-key table IDs.
    pub fn from_physical(tables: &[Table]) -> Result<Self> {
        let mut names_by_id = HashMap::<TableId, &str>::with_capacity(tables.len());
        let mut seen_names = HashSet::<&str>::with_capacity(tables.len());
        let mut names_by_schema_id = HashMap::<SchemaId, &str>::with_capacity(tables.len());

        for table in tables {
            if let Some(previous) = names_by_id.insert(table.id.clone(), &table.name) {
                return Err(drift(format!(
                    "catalog: tables {previous:?} and {:?} share physical ID {:?}",
                    table.name, table.id
                )));
            }
            if !seen_names.insert(&table.name) {
                return Err(drift(format!(
                    "catalog: duplicate physical table name {:?}",
                    table.name
                )));
            }
            if let Some(previous) = names_by_schema_id.insert(table.schema_id, &table.name) {
                return Err(drift(format!(
                    "catalog: tables {previous:?} and {:?} share schema ID {}",
                    table.name, table.schema_id
                )));
            }
        }

        let mut definitions = Vec::with_capacity(tables.len());
        for table in tables {
            let mut seen_column_ids = HashMap::<SchemaId, &str>::with_capacity(table.columns.len());
            let mut columns = Vec::with_capacity(table.columns.len());
            for column in &table.columns {
                if column
                    .missing_value
                    .as_ref()
                    .is_some_and(|value| value.function.is_some())
                {
                    return Err(drift(format!(
                        "catalog: physical column {:?}.{:?} has a generator as its historical missing value",
                        table.name, column.name
                    )));
                }
                if let Some(previous) = seen_column_ids.insert(column.schema_id, &column.name) {
                    return Err(drift(format!(
                        "catalog: columns {:?}.{previous:?} and {:?}.{:?} share schema ID {}",
                        table.name, table.name, column.name, column.schema_id
                    )));
                }
                columns.push(ColumnDef {
                    id: column.schema_id,
                    name: column.name.clone(),
                    scalar_type: column.scalar_type,
                    nullable: column.nullable,
                    format: column.format.clone(),
                    default: column.insert_default.clone(),
                });
            }

            let indexes = table
                .indexes
                .iter()
                .filter(|index| index.is_ready())
                .map(|index| IndexDef {
                    name: index.name.clone(),
                    columns: index.columns.clone(),
                    unique: index.unique,
                })
                .collect();

            let mut foreign_keys = Vec::with_capacity(table.foreign_keys.len());
            for foreign_key in &table.foreign_keys {
                let Some(ref_table) = names_by_id.get(&foreign_key.ref_table_id) else {
                    return Err(drift(format!(
                        "catalog: foreign key {:?} on table {:?} references missing physical table ID {:?}",
                        foreign_key.name, table.name, foreign_key.ref_table_id
                    )));
                };
                foreign_keys.push(ForeignKeyDef {
                    name: foreign_key.name.clone(),
                    columns: foreign_key.columns.clone(),
                    ref_table: (*ref_table).to_owned(),
                    ref_columns: foreign_key.ref_columns.clone(),
                });
            }

            definitions.push(TableDef {
                id: table.schema_id,
                name: table.name.clone(),
                columns,
                primary_key: table.primary_key.clone(),
                indexes,
                foreign_keys,
            });
        }
        Ok(Self::from_definitions(definitions))
    }
}

fn drift(message: impl Into<String>) -> Error {
    Error::message(ErrorKind::CatalogDrift, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::catalog::identity::{
        DefinitionGeneration, ExistenceGeneration, ValueGeneration, WriteProtocolGeneration,
    };
    use crate::engine::catalog::model::{
        Column, DefaultFunction, DefaultValue, ForeignKey, ScalarType,
    };

    fn sid(value: u32) -> SchemaId {
        SchemaId::new(value).unwrap()
    }

    fn table(id: &str, schema_id: u32, name: &str, column_type: ScalarType) -> Table {
        Table {
            id: id.into(),
            schema_id: sid(schema_id),
            name: name.into(),
            definition_generation: DefinitionGeneration::ZERO,
            existence_generation: ExistenceGeneration::ZERO,
            write_protocol_generation: WriteProtocolGeneration::ZERO,
            columns: vec![Column {
                id: format!("c{schema_id}").into(),
                schema_id: sid(1),
                name: "id".into(),
                value_generation: ValueGeneration::ZERO,
                scalar_type: column_type,
                nullable: false,
                format: String::new(),
                insert_default: None,
                missing_value: None,
            }],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            constraints: Vec::new(),
        }
    }

    #[test]
    fn canonicalization_ignores_table_declaration_order() {
        let left = Schema::from_definitions(vec![
            TableDef {
                id: sid(2),
                name: "orders".into(),
                columns: vec![ColumnDef {
                    id: sid(1),
                    name: "id".into(),
                    scalar_type: ScalarType::Int64,
                    nullable: false,
                    format: String::new(),
                    default: None,
                }],
                primary_key: vec!["id".into()],
                indexes: Vec::new(),
                foreign_keys: Vec::new(),
            },
            TableDef {
                id: sid(1),
                name: "users".into(),
                columns: vec![ColumnDef {
                    id: sid(1),
                    name: "id".into(),
                    scalar_type: ScalarType::Text,
                    nullable: false,
                    format: String::new(),
                    default: None,
                }],
                primary_key: vec!["id".into()],
                indexes: Vec::new(),
                foreign_keys: Vec::new(),
            },
        ]);
        let right = Schema::from_definitions(left.tables.iter().rev().cloned().collect());
        assert!(left.canonical_eq(&right).unwrap());
        assert_eq!(left.tables[0].name, "users");
    }

    #[test]
    fn empty_schema_matches_the_canonical_json_and_hash() {
        let schema = Schema::default();
        assert_eq!(schema.canonical_json().unwrap(), br#"{}"#);
        assert_eq!(
            schema.hash().unwrap(),
            "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
    }

    #[test]
    fn physical_schema_rejects_duplicate_logical_identity() {
        let error = Schema::from_physical(&[
            table("t1", 1, "users", ScalarType::Text),
            table("t2", 1, "accounts", ScalarType::Text),
        ])
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::CatalogDrift);
    }

    #[test]
    fn physical_schema_exposes_insert_default_not_historical_missing_value() {
        let mut physical = table("t1", 1, "items", ScalarType::Text);
        physical.columns[0].insert_default = Some(DefaultValue {
            text: "pending".into(),
            ..DefaultValue::default()
        });
        physical.columns[0].missing_value = Some(DefaultValue {
            text: "active".into(),
            ..DefaultValue::default()
        });
        let schema = Schema::from_physical(&[physical]).unwrap();
        assert_eq!(
            schema.tables[0].columns[0].default.as_ref().unwrap().text,
            "pending"
        );
    }

    #[test]
    fn physical_schema_rejects_generator_historical_missing_value() {
        let mut physical = table("t1", 1, "items", ScalarType::Text);
        physical.columns[0].missing_value = Some(DefaultValue {
            function: Some(DefaultFunction::Uuid),
            ..DefaultValue::default()
        });
        assert!(Schema::from_physical(&[physical]).is_err());
    }

    #[test]
    fn physical_schema_rejects_dangling_foreign_key_targets() {
        let mut physical = table("t1", 1, "items", ScalarType::Text);
        physical.foreign_keys.push(ForeignKey {
            id: "fk1".into(),
            name: "items_owner_fk".into(),
            columns: vec!["id".into()],
            ref_table_id: "missing".into(),
            ref_columns: vec!["id".into()],
        });
        assert_eq!(
            Schema::from_physical(&[physical]).unwrap_err().kind(),
            ErrorKind::CatalogDrift
        );
    }

    #[test]
    fn physical_schema_resolves_foreign_keys_without_leaking_physical_ids() {
        let users = table("physical-users", 1, "users", ScalarType::Text);
        let mut orders = table("physical-orders", 2, "orders", ScalarType::Int64);
        orders.foreign_keys.push(ForeignKey {
            id: "physical-foreign-key".into(),
            name: "orders_user_fk".into(),
            columns: vec!["id".into()],
            ref_table_id: users.id.clone(),
            ref_columns: vec!["id".into()],
        });

        let schema = Schema::from_physical(&[orders, users]).unwrap();
        assert_eq!(
            schema
                .tables
                .iter()
                .map(|table| table.name.as_str())
                .collect::<Vec<_>>(),
            ["users", "orders"]
        );
        assert_eq!(schema.tables[1].foreign_keys[0].ref_table, "users");

        let json = String::from_utf8(schema.canonical_json().unwrap()).unwrap();
        for physical_id in [
            "physical-users",
            "physical-orders",
            "physical-foreign-key",
            "c1",
            "c2",
        ] {
            assert!(
                !json.contains(physical_id),
                "leaked {physical_id:?}: {json}"
            );
        }
    }

    #[test]
    fn canonical_schema_matches_the_shared_fixture() {
        let raw = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/catalog/canonical-schema.json"
        ))
        .trim();
        let schema: Schema = serde_json::from_str(raw).unwrap();
        assert_eq!(
            String::from_utf8(schema.canonical_json().unwrap()).unwrap(),
            raw
        );
    }
}
