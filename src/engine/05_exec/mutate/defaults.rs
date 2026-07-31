use crate::engine::catalog::model::{DefaultFunction, Table};
use crate::engine::lir::{Row, Value};
use crate::runtime::RuntimeEffects;

use super::super::{Error, ErrorKind, ErrorReason, Result, codec};

pub(super) fn prepare(table: &Table, row: &Row, runtime: &dyn RuntimeEffects) -> Result<Row> {
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
            None => codec::literal_default_value(column.scalar_type, default)
                .expect("literal default has no generator"),
        };
        with_defaults.insert(column.name.clone(), value);
    }
    normalize(table, &with_defaults)
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
        if value.is_none_or(Value::is_null) {
            if !column.nullable {
                return Err(Error::message(
                    ErrorKind::ConstraintViolation,
                    format!("exec: column {:?} is not nullable", column.name),
                ));
            }
            normalized.insert(column.name.clone(), Value::Null(column.scalar_type));
            continue;
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
        normalized.insert(column.name.clone(), value.clone());
    }
    Ok(normalized)
}
