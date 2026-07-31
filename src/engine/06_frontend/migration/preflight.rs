use std::collections::{HashMap, HashSet};

use crate::engine::catalog::identity::SchemaId;
use crate::engine::catalog::migrate::Step;
use crate::engine::catalog::model::{ColumnReplacementDef, Schema, Table};
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
                let count = table_rows(engine, &mut rows, current)
                    .await?
                    .iter()
                    .filter(|row| {
                        codec::convert_value(
                            &row[&source.name],
                            definition.scalar_type,
                            definition.nullable,
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
            let desired_column = desired.column(name).ok_or_else(|| {
                invalid(format!(
                    "migration preflight: index column {name:?} is missing"
                ))
            })?;
            let mut value = if let Some(current_column) = current_by_id.get(&desired_column.id) {
                row[&current_column.name].clone()
            } else {
                desired_column
                    .default
                    .as_ref()
                    .and_then(|default| {
                        codec::literal_default_value(desired_column.scalar_type, default)
                    })
                    .unwrap_or(Value::Null(desired_column.scalar_type))
            };
            if let Some(replacement) = replacements.get(&(current.schema_id, desired_column.id)) {
                match codec::convert_value(
                    &value,
                    replacement.scalar_type,
                    replacement.nullable,
                    replacement.conversion,
                ) {
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
