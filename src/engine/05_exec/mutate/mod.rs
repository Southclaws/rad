//! Statement-atomic logical row mutation.

pub(super) mod constraints;
mod defaults;

use std::collections::{HashMap, HashSet};

use crate::engine::catalog::model::Table;
use crate::engine::kv::KvView;
use crate::engine::lir::{Row, RowType};
use crate::runtime::RuntimeEffects;

use super::row_store;
use super::{Error, ErrorKind, ErrorReason, Result, codec, write};

pub async fn create(
    view: &mut dyn KvView,
    table: &Table,
    input: &[Row],
    runtime: &dyn RuntimeEffects,
) -> Result<Vec<Row>> {
    let mut stored = Vec::with_capacity(input.len());
    let mut primary_keys = Vec::with_capacity(input.len());
    let mut seen = HashSet::new();

    for input in input {
        let row = defaults::prepare(table, input, runtime)?;
        let primary_key = codec::encode_row_tuple(&row, &table.primary_key)?;
        if !seen.insert(primary_key.clone()) {
            return Err(Error::message(
                ErrorKind::ConstraintViolation,
                format!(
                    "exec: create into {:?} has two rows with the same primary key",
                    table.name
                ),
            ));
        }
        if view
            .get(&codec::data_key(table, &primary_key))
            .await?
            .is_some()
        {
            return Err(Error::message(
                ErrorKind::ConstraintViolation,
                format!("exec: duplicate primary key in table {:?}", table.name),
            ));
        }
        write::insert(view, table, &row, &primary_key).await?;
        stored.push(row);
        primary_keys.push(primary_key);
    }

    for (row, primary_key) in stored.iter().zip(&primary_keys) {
        constraints::check_foreign_keys(view, table, row).await?;
        constraints::check_unique_indexes(view, table, row, primary_key).await?;
    }
    Ok(stored)
}

pub async fn update(
    view: &mut dyn KvView,
    table: &Table,
    input_type: &RowType,
    input: &[Row],
) -> Result<Vec<Row>> {
    let assigned_columns = update_columns(table, input_type)?;
    struct Pending {
        before: Row,
        after: Row,
        assigned: Row,
        primary_key: Vec<u8>,
    }
    let mut pending = Vec::with_capacity(input.len());
    let mut seen = HashSet::new();

    for input in input {
        let key = select_columns(input, &table.primary_key)?;
        let primary_key = codec::encode_row_tuple(&key, &table.primary_key)?;
        let Some(before) = row_store::get_columns(view, table, &key, &table.columns).await? else {
            return Err(Error::message(
                ErrorKind::MutationNotFound,
                format!("exec: update target not found in {:?}", table.name),
            ));
        };
        if !seen.insert(primary_key.clone()) {
            return Err(Error::message(
                ErrorKind::MutationAmbiguous,
                format!(
                    "exec: update of {:?} identifies the same row twice",
                    table.name
                ),
            ));
        }
        let mut after = before.clone();
        let mut assigned = Row::new();
        for column in &assigned_columns {
            let value = input.get(column).cloned().ok_or_else(|| {
                Error::message(
                    ErrorKind::Internal,
                    format!("exec: update input lacks assigned column {column:?}"),
                )
            })?;
            after.insert(column.clone(), value.clone());
            assigned.insert(column.clone(), value);
        }
        let after = defaults::normalize(table, &after)?;
        pending.push(Pending {
            before,
            after,
            assigned,
            primary_key,
        });
    }

    for mutation in &pending {
        write::replace(
            view,
            table,
            &mutation.before,
            &mutation.after,
            &mutation.primary_key,
        )
        .await?;
    }
    for mutation in &pending {
        constraints::check_foreign_keys_for(view, table, &mutation.after, &mutation.assigned)
            .await?;
        constraints::check_unique_indexes_for(
            view,
            table,
            &mutation.after,
            &mutation.primary_key,
            &mutation.assigned,
        )
        .await?;
    }
    Ok(pending.into_iter().map(|mutation| mutation.after).collect())
}

pub async fn delete(
    view: &mut dyn KvView,
    table: &Table,
    input_type: &RowType,
    input: &[Row],
) -> Result<Vec<Row>> {
    validate_delete_columns(table, input_type)?;
    let mut pending = Vec::with_capacity(input.len());
    let mut seen = HashSet::new();
    for input in input {
        let key = select_columns(input, &table.primary_key)?;
        let primary_key = codec::encode_row_tuple(&key, &table.primary_key)?;
        let Some(before) = row_store::get_columns(view, table, &key, &table.columns).await? else {
            return Err(Error::message(
                ErrorKind::MutationNotFound,
                format!("exec: delete target not found in {:?}", table.name),
            ));
        };
        if !seen.insert(primary_key.clone()) {
            return Err(Error::message(
                ErrorKind::MutationAmbiguous,
                format!(
                    "exec: delete of {:?} identifies the same row twice",
                    table.name
                ),
            ));
        }
        pending.push((before, primary_key));
    }
    for (before, primary_key) in &pending {
        write::delete(view, table, before, primary_key).await?;
    }
    for (before, _) in &pending {
        constraints::check_no_references(view, table, before).await?;
    }
    Ok(pending.into_iter().map(|(before, _)| before).collect())
}

fn update_columns(table: &Table, input: &RowType) -> Result<Vec<String>> {
    let mut present = HashMap::new();
    for field in &input.fields {
        if table.column(&field.name).is_none() {
            return Err(Error::message(
                ErrorKind::InvalidInput,
                format!(
                    "exec: update of {:?} references unknown column {:?}",
                    table.name, field.name
                ),
            ));
        }
        if present.insert(field.name.clone(), ()).is_some() {
            return Err(Error::message(
                ErrorKind::InvalidInput,
                format!("exec: update input duplicates column {:?}", field.name),
            ));
        }
    }
    for primary_key in &table.primary_key {
        if !present.contains_key(primary_key) {
            return Err(Error::message(
                ErrorKind::InvalidInput,
                format!(
                    "exec: update of {:?} must include primary-key column {:?}",
                    table.name, primary_key
                ),
            ));
        }
    }
    let assigned = input
        .fields
        .iter()
        .filter(|field| !table.primary_key.contains(&field.name))
        .map(|field| field.name.clone())
        .collect::<Vec<_>>();
    if assigned.is_empty() {
        return Err(Error::message(
            ErrorKind::InvalidInput,
            format!("exec: update of {:?} assigns no columns", table.name),
        ));
    }
    Ok(assigned)
}

fn validate_delete_columns(table: &Table, input: &RowType) -> Result<()> {
    if input.fields.len() != table.primary_key.len()
        || input
            .fields
            .iter()
            .any(|field| !table.primary_key.contains(&field.name))
    {
        return Err(Error::with_reason(
            ErrorKind::InvalidInput,
            ErrorReason::MutationShape,
            format!(
                "exec: delete of {:?} needs exactly the primary-key columns",
                table.name
            ),
        ));
    }
    Ok(())
}

fn select_columns(row: &Row, columns: &[String]) -> Result<Row> {
    columns
        .iter()
        .map(|column| {
            row.get(column)
                .cloned()
                .map(|value| (column.clone(), value))
                .ok_or_else(|| {
                    Error::message(
                        ErrorKind::InvalidInput,
                        format!("exec: mutation input lacks column {column:?}"),
                    )
                })
        })
        .collect()
}
