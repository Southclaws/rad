//! Parser and renderer for the declarative `rad.schema.yaml` format.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_yaml::Value;

use super::identity::SchemaId;
use super::model::{
    ColumnDef, DefaultFunction, DefaultValue, ForeignKeyDef, IndexDef, ScalarType,
    Schema as CanonicalSchema, TableDef,
};
use super::{Error, ErrorKind, Result, naming};

#[derive(Clone, Debug, PartialEq)]
pub struct Schema {
    pub tables: Vec<Table>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    pub def: TableDef,
}

impl Schema {
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.iter().find(|table| table.def.name == name)
    }

    pub fn canonical(&self) -> CanonicalSchema {
        CanonicalSchema::from_definitions(
            self.tables.iter().map(|table| table.def.clone()).collect(),
        )
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileSchema {
    tables: Vec<FileTable>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileTable {
    id: SchemaId,
    name: String,
    columns: Vec<FileColumn>,
    primary_key: Option<Vec<String>>,
    #[serde(default)]
    indexes: Vec<FileIndex>,
    #[serde(default)]
    foreign_keys: Vec<FileForeignKey>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileColumn {
    id: SchemaId,
    name: String,
    #[serde(rename = "type")]
    scalar_type: String,
    #[serde(default)]
    nullable: bool,
    #[serde(default)]
    pk: bool,
    #[serde(default)]
    unique: bool,
    #[serde(default)]
    index: bool,
    #[serde(default, rename = "ref")]
    reference: String,
    #[serde(default)]
    format: String,
    default: Option<Value>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FileIndex {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    name: String,
    columns: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    unique: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FileForeignKey {
    name: String,
    columns: Vec<String>,
    ref_table: String,
    ref_columns: Vec<String>,
}

pub fn parse(filename: &str, source: &[u8]) -> Result<Schema> {
    let file: FileSchema = serde_yaml::from_slice(source).map_err(|error| {
        Error::source(
            ErrorKind::InvalidInput,
            format!("{filename}: schema validation failed: {error}"),
            error,
        )
    })?;
    let mut names = HashSet::new();
    let mut ids = HashMap::new();
    let mut tables = Vec::with_capacity(file.tables.len());
    for table in file.tables {
        validate_identifier(filename, "table", &table.name)?;
        if !names.insert(table.name.clone()) {
            return Err(input(filename, format!("duplicate table {:?}", table.name)));
        }
        if let Some(previous) = ids.insert(table.id, table.name.clone()) {
            return Err(input(
                filename,
                format!(
                    "tables {previous:?} and {:?} share ID {}",
                    table.name, table.id
                ),
            ));
        }
        tables.push(build_table(filename, table)?);
    }
    Ok(Schema { tables })
}

fn build_table(filename: &str, file: FileTable) -> Result<Table> {
    if file.columns.is_empty() {
        return Err(input(
            filename,
            format!("table {:?} must contain at least one column", file.name),
        ));
    }
    if file.primary_key.as_ref().is_some_and(Vec::is_empty) {
        return Err(input(filename, "primary_key must not be empty"));
    }
    let mut definition = TableDef {
        id: file.id,
        name: file.name.clone(),
        columns: Vec::with_capacity(file.columns.len()),
        primary_key: file.primary_key.unwrap_or_default(),
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
    };
    let mut column_names = HashSet::new();
    let mut column_ids = HashMap::new();
    let mut column_primary_key = Vec::new();
    for column in file.columns {
        validate_identifier(filename, "column", &column.name)?;
        if !column_names.insert(column.name.clone()) {
            return Err(input(
                filename,
                format!("duplicate column {:?}.{:?}", file.name, column.name),
            ));
        }
        if let Some(previous) = column_ids.insert(column.id, column.name.clone()) {
            return Err(input(
                filename,
                format!(
                    "columns {:?}.{previous:?} and {:?}.{:?} share ID {}",
                    file.name, file.name, column.name, column.id
                ),
            ));
        }
        let scalar_type = match column.scalar_type.as_str() {
            "string" => ScalarType::Text,
            "int64" => ScalarType::Int64,
            "float64" => ScalarType::Float64,
            "bool" => ScalarType::Bool,
            other => {
                return Err(input(filename, format!("unknown column type {other:?}")));
            }
        };
        if !column.format.is_empty() {
            validate_identifier(filename, "format", &column.format)?;
        }
        let default = column
            .default
            .as_ref()
            .map(|value| parse_default(value, scalar_type))
            .transpose()
            .map_err(|message| {
                input(
                    filename,
                    format!("table {:?}, column {:?}: {message}", file.name, column.name),
                )
            })?;
        if column.pk {
            column_primary_key.push(column.name.clone());
        }
        if column.unique || column.index {
            definition.indexes.push(IndexDef {
                name: naming::index(
                    &file.name,
                    std::slice::from_ref(&column.name),
                    column.unique,
                ),
                columns: vec![column.name.clone()],
                unique: column.unique,
            });
        }
        if !column.reference.is_empty() {
            let Some((ref_table, ref_column)) = column.reference.split_once('.') else {
                return Err(input(filename, "foreign-key ref must be table.column"));
            };
            validate_identifier(filename, "referenced table", ref_table)?;
            validate_identifier(filename, "referenced column", ref_column)?;
            definition.foreign_keys.push(ForeignKeyDef {
                name: naming::foreign_key(&file.name, &column.name),
                columns: vec![column.name.clone()],
                ref_table: ref_table.into(),
                ref_columns: vec![ref_column.into()],
            });
        }
        definition.columns.push(ColumnDef {
            id: column.id,
            name: column.name,
            scalar_type,
            nullable: column.nullable,
            format: column.format,
            default,
        });
    }
    if !column_primary_key.is_empty() && !definition.primary_key.is_empty() {
        return Err(input(
            filename,
            format!(
                "table {:?}: both column-level pk and table-level primary_key",
                file.name
            ),
        ));
    }
    if !column_primary_key.is_empty() {
        definition.primary_key = column_primary_key;
    }
    validate_names(filename, "primary-key column", &definition.primary_key)?;
    for index in file.indexes {
        if index.columns.is_empty() {
            return Err(input(filename, "index must contain at least one column"));
        }
        validate_names(filename, "index column", &index.columns)?;
        let name = if index.name.is_empty() {
            naming::index(&file.name, &index.columns, index.unique)
        } else {
            validate_identifier(filename, "index", &index.name)?;
            index.name
        };
        definition.indexes.push(IndexDef {
            name,
            columns: index.columns,
            unique: index.unique,
        });
    }
    for foreign_key in file.foreign_keys {
        validate_identifier(filename, "foreign key", &foreign_key.name)?;
        validate_identifier(filename, "referenced table", &foreign_key.ref_table)?;
        validate_names(filename, "foreign-key column", &foreign_key.columns)?;
        validate_names(filename, "referenced column", &foreign_key.ref_columns)?;
        if foreign_key.columns.is_empty() || foreign_key.ref_columns.is_empty() {
            return Err(input(filename, "foreign key columns must not be empty"));
        }
        definition.foreign_keys.push(ForeignKeyDef {
            name: foreign_key.name,
            columns: foreign_key.columns,
            ref_table: foreign_key.ref_table,
            ref_columns: foreign_key.ref_columns,
        });
    }
    Ok(Table { def: definition })
}

fn parse_default(
    value: &Value,
    scalar_type: ScalarType,
) -> std::result::Result<DefaultValue, String> {
    let mut result = DefaultValue::default();
    match value {
        Value::String(value) if value == "uuid()" => result.function = Some(DefaultFunction::Uuid),
        Value::String(value) if value == "now_ms()" => {
            result.function = Some(DefaultFunction::NowMs)
        }
        Value::String(value) if scalar_type == ScalarType::Text => result.text = value.clone(),
        Value::String(_) => {
            return Err(format!("string default on {scalar_type:?} column").to_lowercase());
        }
        Value::Number(value) if value.as_i64().is_some() && scalar_type == ScalarType::Int64 => {
            result.int64 = value.as_i64().expect("checked")
        }
        Value::Number(value) if scalar_type == ScalarType::Float64 => {
            result.float64 = value
                .as_f64()
                .ok_or_else(|| "invalid float default".to_owned())?
        }
        Value::Number(value) if value.as_i64().is_some() => {
            return Err(format!("integer default on {scalar_type:?} column").to_lowercase());
        }
        Value::Number(_) => {
            return Err(format!("float default on {scalar_type:?} column").to_lowercase());
        }
        Value::Bool(value) if scalar_type == ScalarType::Bool => result.bool_value = *value,
        Value::Bool(_) => {
            return Err(format!("bool default on {scalar_type:?} column").to_lowercase());
        }
        other => return Err(format!("cannot use {other:?} as a default")),
    }
    Ok(result)
}

fn validate_names(filename: &str, kind: &str, names: &[String]) -> Result<()> {
    for name in names {
        validate_identifier(filename, kind, name)?;
    }
    Ok(())
}

fn validate_identifier(filename: &str, kind: &str, value: &str) -> Result<()> {
    let mut characters = value.bytes();
    let valid = characters
        .next()
        .is_some_and(|first| first == b'_' || first.is_ascii_lowercase())
        && characters.all(|character| {
            character == b'_' || character.is_ascii_lowercase() || character.is_ascii_digit()
        });
    if valid {
        Ok(())
    } else {
        Err(input(
            filename,
            format!("invalid {kind} identifier {value:?}"),
        ))
    }
}

fn input(filename: &str, message: impl std::fmt::Display) -> Error {
    Error::message(ErrorKind::InvalidInput, format!("{filename}: {message}"))
}

#[derive(Serialize)]
struct RenderedSchema {
    tables: Vec<RenderedTable>,
}

#[derive(Serialize)]
struct RenderedTable {
    id: SchemaId,
    name: String,
    columns: Vec<RenderedColumn>,
    primary_key: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    indexes: Vec<FileIndex>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    foreign_keys: Vec<FileForeignKey>,
}

#[derive(Serialize)]
struct RenderedColumn {
    id: SchemaId,
    name: String,
    #[serde(rename = "type")]
    scalar_type: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    nullable: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    default: Option<Value>,
}

pub fn render(schema: &CanonicalSchema) -> Result<Vec<u8>> {
    let mut tables = Vec::with_capacity(schema.tables.len());
    for table in &schema.tables {
        let mut columns = Vec::with_capacity(table.columns.len());
        for column in &table.columns {
            columns.push(RenderedColumn {
                id: column.id,
                name: column.name.clone(),
                scalar_type: match column.scalar_type {
                    ScalarType::Text => "string",
                    ScalarType::Int64 => "int64",
                    ScalarType::Float64 => "float64",
                    ScalarType::Bool => "bool",
                }
                .into(),
                nullable: column.nullable,
                format: column.format.clone(),
                default: render_default(column),
            });
        }
        tables.push(RenderedTable {
            id: table.id,
            name: table.name.clone(),
            columns,
            primary_key: table.primary_key.clone(),
            indexes: table
                .indexes
                .iter()
                .map(|index| FileIndex {
                    name: index.name.clone(),
                    columns: index.columns.clone(),
                    unique: index.unique,
                })
                .collect(),
            foreign_keys: table
                .foreign_keys
                .iter()
                .map(|foreign_key| FileForeignKey {
                    name: foreign_key.name.clone(),
                    columns: foreign_key.columns.clone(),
                    ref_table: foreign_key.ref_table.clone(),
                    ref_columns: foreign_key.ref_columns.clone(),
                })
                .collect(),
        });
    }
    serde_yaml::to_string(&RenderedSchema { tables })
        .map(String::into_bytes)
        .map_err(|error| Error::source(ErrorKind::CatalogDrift, "schema: render", error))
}

fn render_default(column: &ColumnDef) -> Option<Value> {
    let default = column.default.as_ref()?;
    if let Some(function) = default.function {
        return Some(Value::String(
            match function {
                DefaultFunction::Uuid => "uuid()",
                DefaultFunction::NowMs => "now_ms()",
            }
            .into(),
        ));
    }
    Some(match column.scalar_type {
        ScalarType::Text => Value::String(default.text.clone()),
        ScalarType::Int64 => Value::Number(default.int64.into()),
        ScalarType::Float64 => serde_yaml::to_value(default.float64).expect("f64 serializes"),
        ScalarType::Bool => Value::Bool(default.bool_value),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shorthands_defaults_and_composite_keys() {
        let source = br#"
tables:
  - id: 1
    name: posts
    columns:
      - { id: 1, name: tenant, type: string, pk: true, default: uuid() }
      - { id: 2, name: id, type: int64, pk: true }
      - { id: 3, name: slug, type: string, unique: true }
      - { id: 4, name: author, type: string, ref: users.id }
      - { id: 5, name: ratio, type: float64, default: 0.5 }
      - { id: 6, name: live, type: bool, default: false }
    indexes:
      - { columns: [author, slug] }
"#;
        let parsed = parse("test.rad", source).unwrap();
        let table = &parsed.tables[0].def;
        assert_eq!(table.primary_key, ["tenant", "id"]);
        assert_eq!(table.indexes[0].name, "posts_slug_uq");
        assert_eq!(table.indexes[1].name, "posts_author_slug_idx");
        assert_eq!(table.foreign_keys[0].name, "posts_author_fk");
        assert_eq!(table.columns[4].default.as_ref().unwrap().float64, 0.5);
    }

    #[test]
    fn rejects_unknown_fields_bad_identifiers_and_duplicate_ids() {
        for source in [
            "tables: [{id: 1, name: Bad, columns: [{id: 1, name: id, type: string}]}]",
            "tables: [{id: 1, name: a, renamed_from: old, columns: [{id: 1, name: id, type: string}]}]",
            "tables: [{id: 1, name: a, columns: [{id: 1, name: id, type: string}, {id: 1, name: title, type: string}]}]",
        ] {
            let error = parse("test.rad", source.as_bytes()).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
            assert!(error.to_string().contains("test.rad"));
        }
    }

    #[test]
    fn canonical_render_round_trips() {
        let source = br#"
tables:
  - id: 1
    name: users
    columns:
      - { id: 1, name: id, type: string, pk: true, default: uuid() }
      - { id: 2, name: email, type: string, nullable: true }
    indexes:
      - { name: users_email_lookup, columns: [email] }
"#;
        let expected = parse("rad.schema.yaml", source).unwrap().canonical();
        let rendered = render(&expected).unwrap();
        let actual = parse("rendered.rad", &rendered).unwrap().canonical();
        assert!(expected.canonical_eq(&actual).unwrap());
    }

    #[test]
    fn parses_demo_schema() {
        let source = include_bytes!("../../../examples/demo/rad.schema.yaml");
        let schema = parse("examples/demo/rad.schema.yaml", source).unwrap();
        assert_eq!(schema.tables.len(), 9);
        assert_eq!(schema.table("tasks").unwrap().def.foreign_keys.len(), 4);
    }
}
