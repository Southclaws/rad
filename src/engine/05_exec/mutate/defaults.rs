use crate::engine::catalog::model::{DefaultFunction, Table};
use crate::engine::catalog::store;
use crate::engine::kv::KvView;
use crate::engine::lir::{Row, Value};
use crate::runtime::RuntimeEffects;

use super::super::{Error, ErrorKind, ErrorReason, Result, codec};

pub(super) async fn prepare_create(
    view: &mut dyn KvView,
    table: &Table,
    input: &[Row],
    runtime: &dyn RuntimeEffects,
) -> Result<Vec<Row>> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let mut rows = input
        .iter()
        .map(|row| prepare_stateless(table, row, runtime))
        .collect::<Vec<_>>();
    for row in &rows {
        validate_before_increment(table, row)?;
    }

    for column in table.columns.iter().filter(|column| {
        column
            .insert_default
            .as_ref()
            .is_some_and(|value| value.is_increment())
    }) {
        let current = store::read_column_increment(view, &table.id, column.schema_id).await?;
        let mut high_water = current;
        let mut omitted = Vec::new();

        for (index, row) in rows.iter().enumerate() {
            match row.get(&column.name) {
                None => omitted.push((allocation_sort_key(table, row, &column.name)?, index)),
                Some(Value::Int64(value)) => high_water = high_water.max(*value),
                Some(_) => {}
            }
        }
        omitted.sort_unstable_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));

        let generated = i64::try_from(omitted.len()).map_err(|_| increment_exhausted(table))?;
        let final_high_water = high_water
            .checked_add(generated)
            .ok_or_else(|| increment_exhausted(table))?;
        for (offset, (_, index)) in omitted.into_iter().enumerate() {
            let offset = i64::try_from(offset + 1).map_err(|_| increment_exhausted(table))?;
            let value = high_water
                .checked_add(offset)
                .ok_or_else(|| increment_exhausted(table))?;
            rows[index].insert(column.name.clone(), Value::Int64(value));
        }
        if final_high_water > current {
            store::save_column_increment(view, &table.id, column.schema_id, final_high_water)
                .await?;
        }
    }

    rows.iter().map(|row| normalize(table, row)).collect()
}

pub(super) async fn raise_increment_floors(
    view: &mut dyn KvView,
    table: &Table,
    assigned: &[&Row],
) -> Result<()> {
    for column in table.columns.iter().filter(|column| {
        column
            .insert_default
            .as_ref()
            .is_some_and(|value| value.is_increment())
    }) {
        let supplied = assigned
            .iter()
            .filter_map(|row| match row.get(&column.name) {
                Some(Value::Int64(value)) => Some(*value),
                _ => None,
            })
            .max();
        let Some(supplied) = supplied else {
            continue;
        };
        let current = store::read_column_increment(view, &table.id, column.schema_id).await?;
        if supplied > current {
            store::save_column_increment(view, &table.id, column.schema_id, supplied).await?;
        }
    }
    Ok(())
}

fn prepare_stateless(table: &Table, row: &Row, runtime: &dyn RuntimeEffects) -> Row {
    let mut with_defaults = row.clone();
    for column in &table.columns {
        if with_defaults.contains_key(&column.name) {
            continue;
        }
        let Some(default) = &column.insert_default else {
            continue;
        };
        let value = match default.function {
            Some(DefaultFunction::Uuid) => Value::Text(runtime.new_uuid().to_string()),
            Some(DefaultFunction::NowMs) => Value::Int64(runtime.now().timestamp_millis()),
            Some(DefaultFunction::Increment) => continue,
            None => codec::literal_default_value(column.scalar_type, default)
                .expect("literal default has no generator"),
        };
        with_defaults.insert(column.name.clone(), value);
    }
    with_defaults
}

fn validate_before_increment(table: &Table, row: &Row) -> Result<()> {
    for name in row.keys() {
        if table.column(name).is_none() {
            return Err(Error::message(
                ErrorKind::InvalidInput,
                format!("exec: table {:?} has no column {name:?}", table.name),
            ));
        }
    }
    for column in &table.columns {
        let value = row.get(&column.name);
        if value.is_none()
            && column
                .insert_default
                .as_ref()
                .is_some_and(|value| value.is_increment())
        {
            continue;
        }
        validate_column(column, value)?;
    }
    Ok(())
}

fn allocation_sort_key(table: &Table, row: &Row, generated_column: &str) -> Result<Vec<u8>> {
    let values = table
        .columns
        .iter()
        .filter(|column| column.name != generated_column)
        .map(|column| {
            row.get(&column.name)
                .cloned()
                .unwrap_or(Value::Null(column.scalar_type))
        })
        .collect::<Vec<_>>();
    codec::encode_tuple(&values)
}

fn increment_exhausted(table: &Table) -> Error {
    Error::with_reason(
        ErrorKind::Runtime,
        ErrorReason::NumericOverflow,
        format!(
            "exec: increment generator for table {:?} is exhausted",
            table.name
        ),
    )
}

pub(super) fn normalize(table: &Table, row: &Row) -> Result<Row> {
    for name in row.keys() {
        if table.column(name).is_none() {
            return Err(Error::message(
                ErrorKind::InvalidInput,
                format!("exec: table {:?} has no column {name:?}", table.name),
            ));
        }
    }
    let mut normalized = Row::new();
    for column in &table.columns {
        let value = row.get(&column.name);
        validate_column(column, value)?;
        if value.is_none_or(Value::is_null) {
            normalized.insert(column.name.clone(), Value::Null(column.scalar_type));
            continue;
        }
        let value = value.expect("non-null value checked");
        normalized.insert(column.name.clone(), value.clone());
    }
    Ok(normalized)
}

fn validate_column(
    column: &crate::engine::catalog::model::Column,
    value: Option<&Value>,
) -> Result<()> {
    if value.is_none_or(Value::is_null) {
        if !column.nullable {
            return Err(Error::message(
                ErrorKind::ConstraintViolation,
                format!("exec: column {:?} is not nullable", column.name),
            ));
        }
        return Ok(());
    }
    let value = value.expect("non-null value checked");
    if value.scalar_type() != column.scalar_type {
        return Err(Error::with_reason(
            ErrorKind::InvalidInput,
            ErrorReason::TypeMismatch,
            format!(
                "exec: column {:?} expects {:?}, got {:?}",
                column.name,
                column.scalar_type,
                value.scalar_type()
            ),
        ));
    }
    Ok(())
}
