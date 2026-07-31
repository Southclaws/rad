use bytes::Bytes;

use crate::engine::catalog::model::{ForeignKey, Index, Table};
use crate::engine::catalog::store;
use crate::engine::kv::key_encoding::prefix_end;
use crate::engine::kv::{KeyRange, KvView};
use crate::engine::lir::{Row, Value};

use super::super::row_store;
use super::super::{Error, ErrorKind, Result, codec};

pub(super) async fn check_foreign_keys(
    view: &mut dyn KvView,
    table: &Table,
    row: &Row,
) -> Result<()> {
    for foreign_key in &table.foreign_keys {
        check_foreign_key(view, row, foreign_key).await?;
    }
    Ok(())
}

pub(super) async fn check_foreign_keys_for(
    view: &mut dyn KvView,
    table: &Table,
    row: &Row,
    assigned: &Row,
) -> Result<()> {
    for foreign_key in &table.foreign_keys {
        if touches(&foreign_key.columns, assigned) {
            check_foreign_key(view, row, foreign_key).await?;
        }
    }
    Ok(())
}

async fn check_foreign_key(
    view: &mut dyn KvView,
    row: &Row,
    foreign_key: &ForeignKey,
) -> Result<()> {
    let values = foreign_key
        .columns
        .iter()
        .map(|column| {
            row.get(column).cloned().ok_or_else(|| {
                Error::message(
                    ErrorKind::Internal,
                    format!("exec: normalized row lacks foreign-key column {column:?}"),
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if values.iter().any(Value::is_null) {
        return Ok(());
    }
    let tuple = codec::encode_tuple(&values)?;
    let mut key = format!("/rad/data/{}/primary/", foreign_key.ref_table_id).into_bytes();
    key.extend_from_slice(&tuple);
    if view.get(&key).await?.is_none() {
        return Err(Error::message(
            ErrorKind::ConstraintViolation,
            format!(
                "exec: foreign key {:?} violation: referenced row does not exist",
                foreign_key.name
            ),
        ));
    }
    Ok(())
}

pub(super) async fn check_unique_indexes(
    view: &mut dyn KvView,
    table: &Table,
    row: &Row,
    primary_key: &[u8],
) -> Result<()> {
    for index in table.indexes.iter().filter(|index| index.is_ready()) {
        check_unique_index(view, table, row, primary_key, index).await?;
    }
    Ok(())
}

pub(super) async fn check_unique_indexes_for(
    view: &mut dyn KvView,
    table: &Table,
    row: &Row,
    primary_key: &[u8],
    assigned: &Row,
) -> Result<()> {
    for index in table.indexes.iter().filter(|index| index.is_ready()) {
        let columns = index_columns(table, index);
        if touches(&columns, assigned) {
            check_unique_index(view, table, row, primary_key, index).await?;
        }
    }
    Ok(())
}

pub(crate) async fn check_unique_index(
    view: &dyn KvView,
    table: &Table,
    row: &Row,
    primary_key: &[u8],
    index: &Index,
) -> Result<()> {
    if !index.unique {
        return Ok(());
    }
    if codec::index_has_null(table, index, row) {
        return Ok(());
    }
    let tuple = codec::encode_index_tuple(table, index, row)?;
    let mut prefix = codec::index_prefix(table, &index.id);
    prefix.extend_from_slice(&tuple);
    let mut iterator = view
        .scan(KeyRange {
            start: Some(Bytes::copy_from_slice(&prefix)),
            end: prefix_end(&prefix).map(Bytes::from),
        })
        .await?;
    while let Some(entry) = iterator.next().await? {
        if entry.value.as_ref() != primary_key {
            return Err(Error::message(
                ErrorKind::ConstraintViolation,
                format!(
                    "exec: unique index {:?} violation in table {:?}",
                    index.name, table.name
                ),
            ));
        }
    }
    Ok(())
}

pub(super) async fn check_no_references(
    view: &mut dyn KvView,
    table: &Table,
    row: &Row,
) -> Result<()> {
    let tables = store::list_tables(view).await?;
    for child in tables {
        for foreign_key in &child.foreign_keys {
            if foreign_key.ref_table_id != table.id {
                continue;
            }
            let mut wanted = Row::new();
            for (child_column, parent_column) in
                foreign_key.columns.iter().zip(&foreign_key.ref_columns)
            {
                let value = row.get(parent_column).cloned().ok_or_else(|| {
                    Error::message(
                        ErrorKind::CorruptData,
                        format!(
                            "exec: foreign key {:?} references missing parent column {:?}",
                            foreign_key.name, parent_column
                        ),
                    )
                })?;
                wanted.insert(child_column.clone(), value);
            }
            if any_row_matching(view, &child, &foreign_key.columns, &wanted).await? {
                return Err(Error::message(
                    ErrorKind::ConstraintViolation,
                    format!(
                        "exec: cannot delete from {:?}: row is referenced by {:?} via {:?}",
                        table.name, child.name, foreign_key.name
                    ),
                ));
            }
        }
    }
    Ok(())
}

async fn any_row_matching(
    view: &mut dyn KvView,
    table: &Table,
    columns: &[String],
    wanted: &Row,
) -> Result<bool> {
    for index in table.indexes.iter().filter(|index| index.is_ready()) {
        let index_columns = index_columns(table, index);
        if index_columns.len() < columns.len() || index_columns[..columns.len()] != *columns {
            continue;
        }
        let tuple = codec::encode_row_tuple(wanted, columns)?;
        let mut prefix = codec::index_prefix(table, &index.id);
        prefix.extend_from_slice(&tuple);
        let mut iterator = view
            .scan(KeyRange {
                start: Some(Bytes::copy_from_slice(&prefix)),
                end: prefix_end(&prefix).map(Bytes::from),
            })
            .await?;
        return Ok(iterator.next().await?.is_some());
    }

    let mut iterator = row_store::scan_table(view, table, &table.columns).await?;
    while let Some(candidate) = iterator.next().await? {
        if columns
            .iter()
            .all(|column| candidate.get(column) == wanted.get(column))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn index_columns(table: &Table, index: &Index) -> Vec<String> {
    table
        .index_column_names(index)
        .into_iter()
        .map(str::to_owned)
        .collect()
}

fn touches(columns: &[String], assigned: &Row) -> bool {
    columns.iter().any(|column| assigned.contains_key(column))
}
