use std::collections::HashMap;

use serde_json::{Value, json};

use super::generated::types as wire;
use crate::engine::catalog::identity::SchemaId;
use crate::engine::catalog::migrate::Step;
use crate::engine::catalog::model::{
    ColumnDef, ColumnDraft, DefaultFunction, DefaultValue, ForeignKeyDef, IndexDef, Schema, Table,
    TableDef, TableDraft, TransitionControl, TransitionKind, TransitionState, TransitionWorkState,
};
use crate::engine::exec::{DefaultSpec, Program, Statement};
use crate::engine::frontend::migration::{MigrationState, SchemaFinding};

#[derive(Debug, thiserror::Error)]
pub(super) enum EncodeError {
    #[error("{0} exceeds the HTTP wire format")]
    Integer(&'static str),
    #[error("encode migration program: {0}")]
    Json(#[from] serde_json::Error),
    #[error("migration program contains a relational statement")]
    RelationalMigrationStatement,
}

pub(super) fn table_draft(value: wire::TableDef) -> Result<TableDraft, String> {
    Ok(TableDraft {
        id: optional_schema_id(value.id, "table")?,
        name: value.name,
        columns: value
            .columns
            .into_iter()
            .map(column_draft)
            .collect::<Result<_, _>>()?,
        primary_key: value.primary_key,
        indexes: value
            .indexes
            .unwrap_or_default()
            .into_iter()
            .map(index_def)
            .collect(),
        foreign_keys: value
            .foreign_keys
            .unwrap_or_default()
            .into_iter()
            .map(foreign_key_def)
            .collect(),
    })
}

pub(super) fn column_draft(value: wire::ColumnDef) -> Result<ColumnDraft, String> {
    let scalar_type = scalar_type(&value.r#type)?;
    let format = value.format.unwrap_or_default();
    Ok(ColumnDraft {
        id: optional_schema_id(value.id, "column")?,
        name: value.name.clone(),
        scalar_type,
        nullable: value.nullable.unwrap_or(false),
        default: column_default(&value.name, scalar_type, &format, value.default)?,
        format,
    })
}

pub(super) fn index_def(value: wire::IndexInfo) -> IndexDef {
    IndexDef {
        name: value.name,
        columns: value.columns,
        unique: value.unique.unwrap_or(false),
    }
}

fn foreign_key_def(value: wire::ForeignKeyInfo) -> ForeignKeyDef {
    ForeignKeyDef {
        name: value.name,
        columns: value.columns,
        ref_table: value.ref_table,
        ref_columns: value.ref_columns,
    }
}

fn optional_schema_id(value: Option<i64>, role: &str) -> Result<Option<SchemaId>, String> {
    value
        .map(|value| {
            let value = u32::try_from(value)
                .map_err(|_| format!("{role} schema ID {value} is outside the supported range"))?;
            SchemaId::new(value).map_err(|error| error.to_string())
        })
        .transpose()
}

fn scalar_type(value: &str) -> Result<crate::engine::catalog::model::ScalarType, String> {
    use crate::engine::catalog::model::ScalarType;
    match value {
        "text" => Ok(ScalarType::Text),
        "int64" => Ok(ScalarType::Int64),
        "float64" => Ok(ScalarType::Float64),
        "bool" => Ok(ScalarType::Bool),
        "bytes" => Ok(ScalarType::Bytes),
        _ => Err(format!("unsupported column type {value:?}")),
    }
}

fn column_default(
    column: &str,
    scalar_type: crate::engine::catalog::model::ScalarType,
    format: &str,
    value: Option<wire::ColumnDefault>,
) -> Result<Option<DefaultValue>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    match (value.func, value.value) {
        (Some(_), Some(_)) => Err(format!(
            "column {column:?}: default sets both func and value"
        )),
        (None, None) => Err(format!("column {column:?}: default must set func or value")),
        (Some(function), None) => {
            let function = match function {
                wire::ColumnDefaultFunc::UuidV4 => DefaultFunction::UuidV4,
                wire::ColumnDefaultFunc::UuidV7 => DefaultFunction::UuidV7,
                wire::ColumnDefaultFunc::Ulid => DefaultFunction::Ulid,
                wire::ColumnDefaultFunc::Xid => DefaultFunction::Xid,
                wire::ColumnDefaultFunc::NowMs => DefaultFunction::NowMs,
                wire::ColumnDefaultFunc::Increment => DefaultFunction::Increment,
            };
            Ok(Some(DefaultValue {
                function: Some(function),
                ..DefaultValue::default()
            }))
        }
        (None, Some(value)) => {
            use crate::engine::catalog::model::ScalarType;
            let mut output = DefaultValue::default();
            match scalar_type {
                ScalarType::Text => {
                    output.text = value
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| format!("column {column:?}: default expects a string"))?;
                }
                ScalarType::Int64 => {
                    output.int64 = value
                        .as_i64()
                        .ok_or_else(|| format!("column {column:?}: default expects an integer"))?;
                }
                ScalarType::Float64 => {
                    output.float64 = value
                        .as_f64()
                        .ok_or_else(|| format!("column {column:?}: default expects a number"))?;
                }
                ScalarType::Bool => {
                    output.bool_value = value
                        .as_bool()
                        .ok_or_else(|| format!("column {column:?}: default expects a boolean"))?;
                }
                ScalarType::Bytes => {
                    let text = value.as_str().ok_or_else(|| {
                        format!("column {column:?}: bytes default expects a string")
                    })?;
                    output.bytes =
                        if let Some(format) = crate::identifiers::Format::recognize(format) {
                            crate::identifiers::parse(format, text)
                                .map_err(|error| error.to_string())?
                        } else {
                            crate::identifiers::decode_base64(text)
                                .map_err(|error| error.to_string())?
                        };
                }
            }
            Ok(Some(output))
        }
    }
}

pub(super) fn table_list(tables: &[Table]) -> wire::TableList {
    let names = tables
        .iter()
        .map(|table| (table.id.clone(), table.name.as_str()))
        .collect::<HashMap<_, _>>();
    wire::TableList {
        tables: tables
            .iter()
            .map(|table| table_info(table, &names))
            .collect(),
    }
}

pub(super) fn one_table(table: &Table, tables: &[Table]) -> wire::TableInfo {
    let names = tables
        .iter()
        .map(|table| (table.id.clone(), table.name.as_str()))
        .collect::<HashMap<_, _>>();
    table_info(table, &names)
}

fn table_info(
    table: &Table,
    names: &HashMap<crate::engine::catalog::identity::TableId, &str>,
) -> wire::TableInfo {
    wire::TableInfo {
        columns: table
            .columns
            .iter()
            .map(|column| wire::ColumnInfo {
                default: default_wire(
                    column.scalar_type,
                    &column.format,
                    column.insert_default.as_ref(),
                ),
                format: (!column.format.is_empty()).then(|| column.format.clone()),
                id: i64::from(column.schema_id.get()),
                name: column.name.clone(),
                nullable: column.nullable.then_some(true),
                r#type: scalar_type_wire(column.scalar_type).into(),
            })
            .collect(),
        foreign_keys: (!table.foreign_keys.is_empty()).then(|| {
            table
                .foreign_keys
                .iter()
                .map(|foreign_key| wire::ForeignKeyInfo {
                    columns: foreign_key.columns.clone(),
                    name: foreign_key.name.clone(),
                    ref_columns: foreign_key.ref_columns.clone(),
                    ref_table: names
                        .get(&foreign_key.ref_table_id)
                        .map(|name| (*name).to_owned())
                        .unwrap_or_else(|| foreign_key.ref_table_id.to_string()),
                })
                .collect()
        }),
        id: i64::from(table.schema_id.get()),
        indexes: (!table.indexes.is_empty()).then(|| {
            table
                .indexes
                .iter()
                .map(|index| wire::IndexInfo {
                    columns: index.columns.clone(),
                    name: index.name.clone(),
                    unique: index.unique.then_some(true),
                })
                .collect()
        }),
        name: table.name.clone(),
        primary_key: table.primary_key.clone(),
    }
}

pub(super) fn schema_document(schema: &Schema) -> wire::SchemaDocument {
    wire::SchemaDocument {
        tables: schema.tables.iter().map(table_definition).collect(),
    }
}

fn table_definition(table: &TableDef) -> wire::TableDef {
    wire::TableDef {
        columns: table.columns.iter().map(column_definition).collect(),
        foreign_keys: (!table.foreign_keys.is_empty()).then(|| {
            table
                .foreign_keys
                .iter()
                .map(|foreign_key| wire::ForeignKeyInfo {
                    columns: foreign_key.columns.clone(),
                    name: foreign_key.name.clone(),
                    ref_columns: foreign_key.ref_columns.clone(),
                    ref_table: foreign_key.ref_table.clone(),
                })
                .collect()
        }),
        id: Some(i64::from(table.id.get())),
        indexes: (!table.indexes.is_empty()).then(|| {
            table
                .indexes
                .iter()
                .map(|index| wire::IndexInfo {
                    columns: index.columns.clone(),
                    name: index.name.clone(),
                    unique: index.unique.then_some(true),
                })
                .collect()
        }),
        name: table.name.clone(),
        primary_key: table.primary_key.clone(),
    }
}

fn column_definition(column: &ColumnDef) -> wire::ColumnDef {
    wire::ColumnDef {
        default: default_wire(column.scalar_type, &column.format, column.default.as_ref()),
        format: (!column.format.is_empty()).then(|| column.format.clone()),
        id: Some(i64::from(column.id.get())),
        name: column.name.clone(),
        nullable: column.nullable.then_some(true),
        r#type: scalar_type_wire(column.scalar_type).into(),
    }
}

fn default_wire(
    scalar_type: crate::engine::catalog::model::ScalarType,
    format: &str,
    value: Option<&DefaultValue>,
) -> Option<wire::ColumnDefault> {
    let value = value?;
    if let Some(function) = value.function {
        return Some(wire::ColumnDefault {
            func: Some(match function {
                DefaultFunction::UuidV4 => wire::ColumnDefaultFunc::UuidV4,
                DefaultFunction::UuidV7 => wire::ColumnDefaultFunc::UuidV7,
                DefaultFunction::Ulid => wire::ColumnDefaultFunc::Ulid,
                DefaultFunction::Xid => wire::ColumnDefaultFunc::Xid,
                DefaultFunction::NowMs => wire::ColumnDefaultFunc::NowMs,
                DefaultFunction::Increment => wire::ColumnDefaultFunc::Increment,
            }),
            value: None,
        });
    }
    use crate::engine::catalog::model::ScalarType;
    Some(wire::ColumnDefault {
        func: None,
        value: Some(match scalar_type {
            ScalarType::Text => json!(value.text),
            ScalarType::Int64 => json!(value.int64),
            ScalarType::Float64 => json!(value.float64),
            ScalarType::Bool => json!(value.bool_value),
            ScalarType::Bytes => json!(match crate::identifiers::Format::recognize(format) {
                Some(format) => crate::identifiers::render(format, &value.bytes)
                    .expect("catalog validates formatted bytes defaults"),
                None => crate::identifiers::encode_base64(&value.bytes),
            }),
        }),
    })
}

fn scalar_type_wire(value: crate::engine::catalog::model::ScalarType) -> &'static str {
    use crate::engine::catalog::model::ScalarType;
    match value {
        ScalarType::Text => "text",
        ScalarType::Int64 => "int64",
        ScalarType::Float64 => "float64",
        ScalarType::Bool => "bool",
        ScalarType::Bytes => "bytes",
    }
}

pub(super) fn transition(
    value: &TransitionControl,
) -> Result<wire::TransitionControl, EncodeError> {
    Ok(wire::TransitionControl {
        applied_delta: wire_integer(value.applied_delta, "transition applied delta")?,
        delta_lag: wire_integer(value.delta_lag, "transition delta lag")?,
        generation: wire_integer(value.generation.get(), "transition generation")?,
        kind: wire::TransitionControlKind::Transition,
        last_error: (!value.last_error.is_empty()).then(|| value.last_error.clone()),
        object_id: value.object_id.clone(),
        prerequisites: value
            .prerequisites
            .iter()
            .map(ToString::to_string)
            .collect(),
        retained_work_state: match value.retained_work_state {
            TransitionWorkState::Unspecified | TransitionWorkState::Normal => {
                wire::TransitionWorkState::Normal
            }
            TransitionWorkState::Degraded => wire::TransitionWorkState::Degraded,
            TransitionWorkState::WriteGated => wire::TransitionWorkState::WriteGated,
        },
        rows_scanned: wire_integer(value.rows_scanned, "transition rows scanned")?,
        state: transition_state(value.state),
        transition_id: value.transition_id.to_string(),
        transition_kind: transition_kind(value.transition_kind),
    })
}

pub(super) fn transition_kind(value: TransitionKind) -> wire::TransitionKind {
    match value {
        TransitionKind::IndexBuild => wire::TransitionKind::IndexBuild,
        TransitionKind::ColumnReplacement => wire::TransitionKind::ColumnReplacement,
        TransitionKind::ConstraintValidation => wire::TransitionKind::ConstraintValidation,
    }
}

pub(super) fn transition_state(value: TransitionState) -> wire::TransitionState {
    match value {
        TransitionState::Waiting => wire::TransitionState::Waiting,
        TransitionState::Building => wire::TransitionState::Building,
        TransitionState::CatchingUp => wire::TransitionState::CatchingUp,
        TransitionState::Validating => wire::TransitionState::Validating,
        TransitionState::Ready => wire::TransitionState::Ready,
        TransitionState::Failed => wire::TransitionState::Failed,
        TransitionState::Cancelled => wire::TransitionState::Cancelled,
    }
}

pub(super) fn migration_state(value: MigrationState) -> wire::SchemaMigrateResultState {
    match value {
        MigrationState::Converging => wire::SchemaMigrateResultState::Converging,
        MigrationState::Ready => wire::SchemaMigrateResultState::Ready,
    }
}

pub(super) fn schema_findings(
    values: &[SchemaFinding],
) -> Result<Vec<wire::SchemaFinding>, EncodeError> {
    values
        .iter()
        .map(|value| {
            Ok(wire::SchemaFinding {
                column: (!value.column.is_empty()).then(|| value.column.clone()),
                kind: value.kind.clone(),
                rows: (value.rows > 0)
                    .then(|| wire_integer(value.rows, "schema finding row count"))
                    .transpose()?,
                summary: value.summary.clone(),
                table: (!value.table.is_empty()).then(|| value.table.clone()),
            })
        })
        .collect()
}

pub(super) fn schema_changes(values: &[Step]) -> Vec<wire::SchemaChange> {
    values.iter().map(schema_change).collect()
}

fn schema_change(value: &Step) -> wire::SchemaChange {
    let (kind, table, column) = match value {
        Step::RenameTable { to, .. } => ("rename_table", Some(to.clone()), None),
        Step::CreateTable { definition } => ("create_table", Some(definition.name.clone()), None),
        Step::DeleteTable { table } => ("delete_table", Some(table.clone()), None),
        Step::RenameColumn { table, to, .. } => {
            ("rename_column", Some(table.clone()), Some(to.clone()))
        }
        Step::CreateColumn { table, definition } => (
            "create_column",
            Some(table.clone()),
            Some(definition.name.clone()),
        ),
        Step::ChangeColumnDefault { table, column, .. } => (
            "change_column_default",
            Some(table.clone()),
            Some(column.clone()),
        ),
        Step::DeleteColumn { table, column } => {
            ("delete_column", Some(table.clone()), Some(column.clone()))
        }
        Step::CreateIndex { table, .. } => ("start_index_build", Some(table.clone()), None),
        Step::ReplaceColumn { table, column, .. } => (
            "start_column_replacement",
            Some(table.clone()),
            Some(column.clone()),
        ),
        Step::ValidateNotNull { table, column, .. } => (
            "start_constraint_validation",
            Some(table.clone()),
            Some(column.clone()),
        ),
        Step::DeleteIndex { table, .. } => ("delete_index", Some(table.clone()), None),
    };
    wire::SchemaChange {
        column,
        kind: kind.into(),
        summary: value.to_string(),
        table,
    }
}

pub(super) fn migration_program(value: &Program) -> Result<Value, EncodeError> {
    let statements = value
        .statements
        .iter()
        .map(statement_json)
        .collect::<Result<Vec<_>, _>>()?;
    let output = json!({"statements": statements});
    serde_json::from_value::<crate::protocol::generated::pir::Program>(output.clone())?;
    Ok(output)
}

fn statement_json(value: &Statement) -> Result<Value, EncodeError> {
    let value = match value {
        Statement::CreateTable { name, table } => json!({
            "kind": "create_table", "name": name, "table": table_draft_json(table)
        }),
        Statement::RenameTable { name, table_id, to } => json!({
            "kind": "rename_table", "name": name, "table_id": table_id.get(), "to": to
        }),
        Statement::DeleteTable { name, table_id } => json!({
            "kind": "delete_table", "name": name, "table_id": table_id.get()
        }),
        Statement::CreateColumn {
            name,
            table_id,
            column,
        } => json!({
            "kind": "create_column", "name": name, "table_id": table_id.get(),
            "column": column_draft_json(column)
        }),
        Statement::RenameColumn {
            name,
            table_id,
            column_id,
            to,
        } => json!({
            "kind": "rename_column", "name": name, "table_id": table_id.get(),
            "column_id": column_id.get(), "to": to
        }),
        Statement::ChangeColumnDefault {
            name,
            table_id,
            column_id,
            default,
        } => {
            let mut object = serde_json::Map::from_iter([
                ("kind".into(), json!("change_column_default")),
                ("name".into(), json!(name)),
                ("table_id".into(), json!(table_id.get())),
                ("column_id".into(), json!(column_id.get())),
            ]);
            if let Some(default) = default {
                object.insert("default".into(), default_spec_json(default));
            }
            Value::Object(object)
        }
        Statement::DeleteColumn {
            name,
            table_id,
            column_id,
        } => json!({
            "kind": "delete_column", "name": name, "table_id": table_id.get(),
            "column_id": column_id.get()
        }),
        Statement::CreateIndex {
            name,
            table_id,
            index,
        } => json!({
            "kind": "create_index", "name": name, "table_id": table_id.get(),
            "index": index_json(index)
        }),
        Statement::DeleteIndex {
            name,
            table_id,
            index,
        } => json!({
            "kind": "delete_index", "name": name, "table_id": table_id.get(), "index": index
        }),
        Statement::StartIndexBuild {
            name,
            table_id,
            index,
            prerequisites,
            after,
        } => json!({
            "kind": "start_index_build", "name": name, "table_id": table_id.get(),
            "index": index_json(index),
            "prerequisites": prerequisites.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "after": after
        }),
        Statement::StartColumnReplacement {
            name,
            table_id,
            column_id,
            replacement,
            after,
        } => json!({
            "kind": "start_column_replacement", "name": name, "table_id": table_id.get(),
            "column_id": column_id.get(),
            "replacement": replacement_json(replacement), "after": after
        }),
        Statement::StartConstraintValidation {
            name,
            table_id,
            constraint,
            after,
        } => json!({
            "kind": "start_constraint_validation", "name": name, "table_id": table_id.get(),
            "constraint": {
                "name": constraint.name,
                "kind": constraint.kind,
                "column_id": constraint.column_id.get(),
                "prerequisites": constraint.prerequisites,
            },
            "after": after
        }),
        Statement::Query { .. }
        | Statement::Create { .. }
        | Statement::Update { .. }
        | Statement::Delete { .. } => return Err(EncodeError::RelationalMigrationStatement),
    };
    Ok(value)
}

fn table_draft_json(value: &TableDraft) -> Value {
    let mut output = json!({
        "name": value.name,
        "columns": value.columns.iter().map(column_draft_json).collect::<Vec<_>>(),
        "primary_key": value.primary_key,
        "indexes": value.indexes.iter().map(index_json).collect::<Vec<_>>(),
        "foreign_keys": value.foreign_keys,
    });
    if let Some(id) = value.id {
        output["id"] = json!(id.get());
    }
    output
}

fn column_draft_json(value: &ColumnDraft) -> Value {
    let mut output = json!({
        "name": value.name,
        "type": scalar_type_wire(value.scalar_type),
        "nullable": value.nullable,
    });
    if let Some(id) = value.id {
        output["id"] = json!(id.get());
    }
    if !value.format.is_empty() {
        output["format"] = json!(value.format);
    }
    if let Some(default) = &value.default {
        output["default"] = default_value_json(value.scalar_type, &value.format, default);
    }
    output
}

fn index_json(value: &IndexDef) -> Value {
    json!({"name": value.name, "columns": value.columns, "unique": value.unique})
}

fn replacement_json(value: &crate::engine::catalog::model::ColumnReplacementDef) -> Value {
    let mut output = json!({
        "type": scalar_type_wire(value.scalar_type),
        "nullable": value.nullable,
        "conversion": value.conversion,
        "prerequisites": value.prerequisites,
    });
    if !value.format.is_empty() {
        output["format"] = json!(value.format);
    }
    if let Some(default) = &value.default {
        output["default"] = default_value_json(value.scalar_type, &value.format, default);
    }
    output
}

fn default_spec_json(value: &DefaultSpec) -> Value {
    match value {
        DefaultSpec::Generator(function) => json!({
            "kind": "generator",
            "func": default_function_name(*function)
        }),
        DefaultSpec::Text(value) => json!({"kind": "literal", "value": value}),
        DefaultSpec::Number(value) => {
            let number = serde_json::from_str::<Value>(value).unwrap_or_else(|_| json!(value));
            json!({"kind": "literal", "value": number})
        }
        DefaultSpec::Bool(value) => json!({"kind": "literal", "value": value}),
    }
}

fn default_value_json(
    scalar_type: crate::engine::catalog::model::ScalarType,
    format: &str,
    value: &DefaultValue,
) -> Value {
    if let Some(function) = value.function {
        return json!({
            "kind": "generator",
            "func": default_function_name(function)
        });
    }
    let value = default_wire(scalar_type, format, Some(value))
        .and_then(|value| value.value)
        .unwrap_or(Value::Null);
    json!({"kind": "literal", "value": value})
}

fn default_function_name(function: DefaultFunction) -> &'static str {
    match function {
        DefaultFunction::UuidV4 => "uuid_v4",
        DefaultFunction::UuidV7 => "uuid_v7",
        DefaultFunction::Ulid => "ulid",
        DefaultFunction::Xid => "xid",
        DefaultFunction::NowMs => "now_ms",
        DefaultFunction::Increment => "increment",
    }
}

fn wire_integer(value: u64, role: &'static str) -> Result<i64, EncodeError> {
    i64::try_from(value).map_err(|_| EncodeError::Integer(role))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_column(format: Option<&str>, value: &str) -> wire::ColumnDef {
        wire::ColumnDef {
            default: Some(wire::ColumnDefault {
                func: None,
                value: Some(json!(value)),
            }),
            format: format.map(str::to_owned),
            id: Some(1),
            name: "payload".into(),
            nullable: None,
            r#type: "bytes".into(),
        }
    }

    #[test]
    fn byte_defaults_use_base64_or_identifier_porcelain_at_the_http_boundary() {
        let raw = column_draft(bytes_column(None, "AP8=")).unwrap();
        assert_eq!(raw.default.unwrap().bytes, [0, 0xff]);

        for (format, text, length) in [
            ("uuid", "00000000-0000-0000-0000-000000000000", 16),
            ("ulid", "00000000000000000000000000", 16),
            ("xid", "00000000000000000000", 12),
        ] {
            let column = column_draft(bytes_column(Some(format), text)).unwrap();
            assert_eq!(column.default.unwrap().bytes, vec![0; length]);
        }

        for column in [
            bytes_column(None, "/w"),
            bytes_column(Some("uuid"), "not-a-uuid"),
            bytes_column(Some("ulid"), "80000000000000000000000000"),
            bytes_column(Some("xid"), "0000000000000000000A"),
        ] {
            assert!(column_draft(column).is_err());
        }
    }

    #[test]
    fn byte_defaults_and_generators_round_trip_through_http_models() {
        let literal = ColumnDef {
            id: SchemaId::new(1).unwrap(),
            name: "id".into(),
            scalar_type: crate::engine::catalog::model::ScalarType::Bytes,
            nullable: false,
            format: "uuid".into(),
            default: Some(DefaultValue {
                bytes: vec![0; 16],
                ..DefaultValue::default()
            }),
        };
        assert_eq!(
            column_definition(&literal).default.unwrap().value,
            Some(json!("00000000-0000-0000-0000-000000000000"))
        );

        let generated = ColumnDef {
            default: Some(DefaultValue {
                function: Some(DefaultFunction::UuidV7),
                ..DefaultValue::default()
            }),
            ..literal
        };
        assert_eq!(
            column_definition(&generated).default.unwrap().func,
            Some(wire::ColumnDefaultFunc::UuidV7)
        );
    }
}
