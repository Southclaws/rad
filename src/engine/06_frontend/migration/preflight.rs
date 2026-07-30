use std::collections::{HashMap, HashSet};

use crate::engine::catalog::identity::SchemaId;
use crate::engine::catalog::migrate::Step;
use crate::engine::catalog::model::{
    Column, ColumnReplacementDef, DefaultValue, ScalarType, Schema, Table,
};
use crate::engine::exec::{Engine, Error, ErrorKind, Result, codec};
use crate::engine::lir::{Row, Value};

use super::SchemaFinding;

pub(super) async fn inspect(
    engine: &Engine,
    current: &[Table],
    desired: &Schema,
    steps: &[Step],
) -> Result<(Vec<SchemaFinding>, Vec<SchemaFinding>)> {
    let current_by_id = current
        .iter()
        .map(|table| (table.schema_id, table))
        .collect::<HashMap<_, _>>();
    let desired_by_name = desired
        .tables
        .iter()
        .map(|table| (table.name.as_str(), table))
        .collect::<HashMap<_, _>>();
    let replacements = steps
        .iter()
        .filter_map(|step| match step {
            Step::ReplaceColumn {
                table,
                column_id,
                definition,
                ..
            } => desired_by_name
                .get(table.as_str())
                .map(|table| ((table.id, *column_id), definition)),
            _ => None,
        })
        .collect::<HashMap<_, _>>();
    let mut rows = HashMap::<SchemaId, Vec<Row>>::new();
    let mut destructive = Vec::new();
    let mut blocking = Vec::new();
    for step in steps {
        match step {
            Step::DeleteTable { table } => {
                let current = current
                    .iter()
                    .find(|candidate| candidate.name == *table)
                    .ok_or_else(|| {
                        invalid(format!("migration preflight: table {table:?} is missing"))
                    })?;
                let count = table_rows(engine, &mut rows, current).await?.len() as u64;
                if count > 0 {
                    destructive.push(finding(
                        "delete_table",
                        table,
                        "",
                        count,
                        format!("table {table} will be deleted ({count} rows)"),
                    ));
                }
            }
            Step::DeleteColumn { table, column } => {
                let current = desired_current(table, &desired_by_name, &current_by_id)?;
                let count = table_rows(engine, &mut rows, current)
                    .await?
                    .iter()
                    .filter(|row| row.get(column).is_some_and(|value| !value.is_null()))
                    .count() as u64;
                if count > 0 {
                    destructive.push(finding(
                        "delete_column",
                        table,
                        column,
                        count,
                        format!(
                            "column {table}.{column} will be deleted ({count} rows contain a value)"
                        ),
                    ));
                }
            }
            Step::ReplaceColumn {
                table,
                column,
                column_id,
                definition,
            } => {
                let current = desired_current(table, &desired_by_name, &current_by_id)?;
                let source = current
                    .columns
                    .iter()
                    .find(|candidate| candidate.schema_id == *column_id)
                    .ok_or_else(|| invalid("migration preflight: replacement source is missing"))?;
                let target = replacement_target(source, definition);
                let count = table_rows(engine, &mut rows, current)
                    .await?
                    .iter()
                    .filter(|row| {
                        codec::convert_column_value(
                            &row[&source.name],
                            &target,
                            definition.conversion,
                        )
                        .is_err()
                    })
                    .count() as u64;
                if count > 0 {
                    blocking.push(finding(
                        "column_conversion",
                        table,
                        column,
                        count,
                        format!(
                            "column {table}.{column} has {count} values that cannot be converted to {:?}",
                            definition.scalar_type
                        ),
                    ));
                }
            }
            Step::ValidateNotNull {
                table,
                column,
                definition,
            } => {
                let current = desired_current(table, &desired_by_name, &current_by_id)?;
                let source = current
                    .columns
                    .iter()
                    .find(|candidate| candidate.schema_id == definition.column_id)
                    .ok_or_else(|| invalid("migration preflight: constraint column is missing"))?;
                let count = table_rows(engine, &mut rows, current)
                    .await?
                    .iter()
                    .filter(|row| row[&source.name].is_null())
                    .count() as u64;
                if count > 0 {
                    blocking.push(finding(
                        "not_null_existing_nulls",
                        table,
                        column,
                        count,
                        format!("column {table}.{column} contains {count} NULL values"),
                    ));
                }
            }
            Step::CreateIndex { table, definition } if definition.unique => {
                let desired_table = desired_by_name
                    .get(table.as_str())
                    .ok_or_else(|| invalid("migration preflight: desired table is missing"))?;
                let Some(current) = current_by_id.get(&desired_table.id).copied() else {
                    continue;
                };
                let count = duplicate_keys(
                    table_rows(engine, &mut rows, current).await?,
                    current,
                    desired_table,
                    &definition.columns,
                    &replacements,
                )?;
                if count > 0 {
                    blocking.push(finding(
                        "unique_index_duplicates",
                        table,
                        "",
                        count,
                        format!(
                            "index {} cannot become unique because {count} duplicate keys exist",
                            definition.name
                        ),
                    ));
                }
            }
            _ => {}
        }
    }
    Ok((destructive, blocking))
}

async fn table_rows<'a>(
    engine: &Engine,
    rows: &'a mut HashMap<SchemaId, Vec<Row>>,
    table: &Table,
) -> Result<&'a [Row]> {
    if let std::collections::hash_map::Entry::Vacant(entry) = rows.entry(table.schema_id) {
        entry.insert(engine.scan_table_rows(table).await?);
    }
    Ok(&rows[&table.schema_id])
}

fn desired_current<'a>(
    table: &str,
    desired: &HashMap<&str, &crate::engine::catalog::model::TableDef>,
    current: &HashMap<SchemaId, &'a Table>,
) -> Result<&'a Table> {
    let definition = desired.get(table).ok_or_else(|| {
        invalid(format!(
            "migration preflight: desired table {table:?} is missing"
        ))
    })?;
    current.get(&definition.id).copied().ok_or_else(|| {
        invalid(format!(
            "migration preflight: current table {} is missing",
            definition.id
        ))
    })
}

fn duplicate_keys(
    rows: &[Row],
    current: &Table,
    desired: &crate::engine::catalog::model::TableDef,
    names: &[String],
    replacements: &HashMap<(SchemaId, SchemaId), &ColumnReplacementDef>,
) -> Result<u64> {
    let desired_by_name = desired
        .columns
        .iter()
        .map(|column| (column.name.as_str(), column))
        .collect::<HashMap<_, _>>();
    let current_by_id = current
        .columns
        .iter()
        .map(|column| (column.schema_id, column))
        .collect::<HashMap<_, _>>();
    let mut seen = HashSet::<Vec<u8>>::new();
    let mut duplicates = 0;
    for row in rows {
        let mut values = Vec::with_capacity(names.len());
        let mut conversion_invalid = false;
        for name in names {
            let desired_column = desired_by_name.get(name.as_str()).ok_or_else(|| {
                invalid(format!(
                    "migration preflight: index column {name:?} is missing"
                ))
            })?;
            let mut value = if let Some(current_column) = current_by_id.get(&desired_column.id) {
                row[&current_column.name].clone()
            } else {
                historical_value(desired_column.scalar_type, desired_column.default.as_ref())
            };
            if let Some(replacement) = replacements.get(&(current.schema_id, desired_column.id)) {
                let source = current_by_id
                    .get(&desired_column.id)
                    .expect("replacement source was validated by migration diff");
                let target = replacement_target(source, replacement);
                match codec::convert_column_value(&value, &target, replacement.conversion) {
                    Ok(converted) => value = converted,
                    Err(_) => {
                        conversion_invalid = true;
                        break;
                    }
                }
            }
            values.push(value);
        }
        if conversion_invalid || values.iter().any(Value::is_null) {
            continue;
        }
        if !seen.insert(codec::encode_tuple(&values)?) {
            duplicates += 1;
        }
    }
    Ok(duplicates)
}

fn replacement_target(source: &Column, replacement: &ColumnReplacementDef) -> Column {
    let mut target = source.clone();
    target.scalar_type = replacement.scalar_type;
    target.nullable = replacement.nullable;
    target.format.clone_from(&replacement.format);
    target
}

fn historical_value(scalar_type: ScalarType, default: Option<&DefaultValue>) -> Value {
    let Some(default) = default.filter(|default| default.function.is_none()) else {
        return Value::Null(scalar_type);
    };
    match scalar_type {
        ScalarType::Text => Value::Text(default.text.clone()),
        ScalarType::Int64 => Value::Int64(default.int64),
        ScalarType::Float64 => Value::Float64(default.float64),
        ScalarType::Bool => Value::Bool(default.bool_value),
    }
}

fn finding(kind: &str, table: &str, column: &str, rows: u64, summary: String) -> SchemaFinding {
    SchemaFinding {
        kind: kind.into(),
        summary,
        table: table.into(),
        column: column.into(),
        rows,
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::message(ErrorKind::Internal, message)
}
