//! Physical row writes and catalog write-protocol obligations.

use bytes::Bytes;

use crate::engine::catalog::model::{
    ConstraintKind, Index, IndexDelta, IndexDeltaOperation, Table, WriteProtocol,
};
use crate::engine::catalog::store;
use crate::engine::kv::KvView;
use crate::engine::lir::{Row, Value};

use super::codec;
use super::{Error, ErrorKind, Result};

/// Populate one newly registered ready index from a transaction-visible row.
/// The caller scans the table before any later statement can observe the index.
pub async fn backfill_index_entry(
    view: &dyn KvView,
    table: &Table,
    index: &Index,
    row: &Row,
) -> Result<()> {
    let primary_key = codec::encode_row_tuple(row, &table.primary_key)?;
    if index.unique {
        super::mutate::constraints::check_unique_index(view, table, row, &primary_key, index)
            .await?;
    }
    let tuple = index_tuple(table, index, row)?;
    view.put(
        Bytes::from(codec::index_key(table, &index.id, &tuple, &primary_key)),
        Bytes::from(primary_key),
    )
    .await?;
    Ok(())
}

pub async fn insert(
    view: &mut dyn KvView,
    table: &Table,
    row: &Row,
    primary_key: &[u8],
) -> Result<()> {
    let protocol = admit(view, table).await?;
    let mut raw = codec::marshal_row(table, row)?;
    apply_constraint_checks(view, table, &protocol, row, primary_key).await?;
    raw = apply_column_replacements(view, table, &protocol, row, primary_key, raw).await?;
    view.put(
        Bytes::from(codec::data_key(table, primary_key)),
        Bytes::from(raw),
    )
    .await?;
    for index in &protocol.ready_indexes {
        let tuple = index_tuple(table, index, row)?;
        view.put(
            Bytes::from(codec::index_key(table, &index.id, &tuple, primary_key)),
            Bytes::copy_from_slice(primary_key),
        )
        .await?;
    }
    emit_insert_deltas(view, table, &protocol, row, primary_key).await
}

pub async fn replace(
    view: &mut dyn KvView,
    table: &Table,
    before: &Row,
    after: &Row,
    primary_key: &[u8],
) -> Result<()> {
    let protocol = admit(view, table).await?;
    let mut raw = codec::marshal_row(table, after)?;
    apply_constraint_checks(view, table, &protocol, after, primary_key).await?;
    raw = apply_column_replacements(view, table, &protocol, after, primary_key, raw).await?;
    view.put(
        Bytes::from(codec::data_key(table, primary_key)),
        Bytes::from(raw),
    )
    .await?;
    for index in &protocol.ready_indexes {
        let old_tuple = index_tuple(table, index, before)?;
        let new_tuple = index_tuple(table, index, after)?;
        if old_tuple == new_tuple {
            continue;
        }
        view.delete(&codec::index_key(table, &index.id, &old_tuple, primary_key))
            .await?;
        view.put(
            Bytes::from(codec::index_key(table, &index.id, &new_tuple, primary_key)),
            Bytes::copy_from_slice(primary_key),
        )
        .await?;
    }
    emit_replace_deltas(view, table, &protocol, before, after, primary_key).await
}

pub async fn delete(
    view: &mut dyn KvView,
    table: &Table,
    row: &Row,
    primary_key: &[u8],
) -> Result<()> {
    let protocol = admit(view, table).await?;
    for index in &protocol.ready_indexes {
        let tuple = index_tuple(table, index, row)?;
        view.delete(&codec::index_key(table, &index.id, &tuple, primary_key))
            .await?;
    }
    view.delete(&codec::data_key(table, primary_key)).await?;
    clear_transition_violations(view, &protocol, primary_key).await?;
    emit_delete_deltas(view, table, &protocol, row, primary_key).await
}

async fn admit(view: &mut dyn KvView, table: &Table) -> Result<WriteProtocol> {
    store::read_table_existence_fence(view, table).await?;
    for column in &table.columns {
        store::read_column_value_fence(view, table, column).await?;
    }
    let protocol = store::read_write_protocol(view, table).await?;
    if let Some(gate) = &protocol.finalization_gate {
        return Err(Error::message(
            ErrorKind::TransitionFinalizing,
            format!(
                "catalog: table {:?} is write-gated while transition {:?} finalizes {:?} {:?}",
                table.name, gate.transition_id, gate.kind, gate.object_id
            ),
        ));
    }
    Ok(protocol)
}

async fn apply_constraint_checks(
    view: &mut dyn KvView,
    table: &Table,
    protocol: &WriteProtocol,
    row: &Row,
    primary_key: &[u8],
) -> Result<()> {
    for obligation in &protocol.constraint_checks {
        let constraint = &obligation.constraint;
        match constraint.kind {
            ConstraintKind::NotNull => {
                let [column_id] = constraint.column_ids.as_slice() else {
                    return Err(Error::message(
                        ErrorKind::CorruptData,
                        format!(
                            "exec: not-null constraint {:?} has {} columns",
                            constraint.name,
                            constraint.column_ids.len()
                        ),
                    ));
                };
                let column = table
                    .columns
                    .iter()
                    .find(|column| &column.id == column_id)
                    .ok_or_else(|| {
                        Error::message(
                            ErrorKind::CorruptData,
                            format!(
                                "exec: constraint {:?} references inactive column {:?}",
                                constraint.name, column_id
                            ),
                        )
                    })?;
                if row.get(&column.name).is_none_or(Value::is_null) {
                    return Err(Error::message(
                        ErrorKind::ConstraintViolation,
                        format!(
                            "exec: constraint {:?} rejects NULL in column {:?}",
                            constraint.name, column.name
                        ),
                    ));
                }
            }
        }
        store::delete_transition_violation(view, &obligation.transition_id, primary_key).await?;
    }
    Ok(())
}

async fn apply_column_replacements(
    view: &mut dyn KvView,
    table: &Table,
    protocol: &WriteProtocol,
    row: &Row,
    primary_key: &[u8],
    mut raw: Vec<u8>,
) -> Result<Vec<u8>> {
    for obligation in &protocol.column_replacements {
        let replacement = &obligation.replacement;
        let source = table
            .columns
            .iter()
            .find(|column| column.id == replacement.source.id)
            .ok_or_else(|| {
                Error::message(
                    ErrorKind::CorruptData,
                    format!(
                        "exec: replacement transition {:?} source column {:?} is inactive",
                        obligation.transition_id, replacement.source.id
                    ),
                )
            })?;
        let value = row.get(&source.name).ok_or_else(|| {
            Error::message(
                ErrorKind::Internal,
                format!("exec: normalized row lacks column {:?}", source.name),
            )
        })?;
        let converted =
            codec::convert_column_value(value, &replacement.target, replacement.conversion)?;
        raw = codec::set_column_value(&raw, &replacement.target, &converted)?;
        store::delete_transition_violation(view, &obligation.transition_id, primary_key).await?;
    }
    Ok(raw)
}

async fn clear_transition_violations(
    view: &mut dyn KvView,
    protocol: &WriteProtocol,
    primary_key: &[u8],
) -> Result<()> {
    for replacement in &protocol.column_replacements {
        store::delete_transition_violation(view, &replacement.transition_id, primary_key).await?;
    }
    for constraint in &protocol.constraint_checks {
        store::delete_transition_violation(view, &constraint.transition_id, primary_key).await?;
    }
    Ok(())
}

async fn emit_insert_deltas(
    view: &mut dyn KvView,
    table: &Table,
    protocol: &WriteProtocol,
    row: &Row,
    primary_key: &[u8],
) -> Result<()> {
    for sink in &protocol.delta_sinks {
        let tuple = index_tuple(table, &sink.index, row)?;
        if sink.index.unique && !has_null(table, &sink.index, row) {
            store::put_unique_claim(view, &sink.transition_id, &tuple, primary_key).await?;
        }
        append_delta(
            view,
            &sink.transition_id,
            sink.delta_hard_limit,
            IndexDeltaOperation::Put,
            primary_key,
            tuple,
        )
        .await?;
    }
    Ok(())
}

async fn emit_replace_deltas(
    view: &mut dyn KvView,
    table: &Table,
    protocol: &WriteProtocol,
    before: &Row,
    after: &Row,
    primary_key: &[u8],
) -> Result<()> {
    for sink in &protocol.delta_sinks {
        let old_tuple = index_tuple(table, &sink.index, before)?;
        let new_tuple = index_tuple(table, &sink.index, after)?;
        if old_tuple == new_tuple {
            continue;
        }
        if sink.index.unique {
            if !has_null(table, &sink.index, before) {
                store::delete_unique_claim(view, &sink.transition_id, &old_tuple, primary_key)
                    .await?;
            }
            if !has_null(table, &sink.index, after) {
                store::put_unique_claim(view, &sink.transition_id, &new_tuple, primary_key).await?;
            }
        }
        append_delta(
            view,
            &sink.transition_id,
            sink.delta_hard_limit,
            IndexDeltaOperation::Delete,
            primary_key,
            old_tuple,
        )
        .await?;
        append_delta(
            view,
            &sink.transition_id,
            sink.delta_hard_limit,
            IndexDeltaOperation::Put,
            primary_key,
            new_tuple,
        )
        .await?;
    }
    Ok(())
}

async fn emit_delete_deltas(
    view: &mut dyn KvView,
    table: &Table,
    protocol: &WriteProtocol,
    row: &Row,
    primary_key: &[u8],
) -> Result<()> {
    for sink in &protocol.delta_sinks {
        let tuple = index_tuple(table, &sink.index, row)?;
        if sink.index.unique && !has_null(table, &sink.index, row) {
            store::delete_unique_claim(view, &sink.transition_id, &tuple, primary_key).await?;
        }
        append_delta(
            view,
            &sink.transition_id,
            sink.delta_hard_limit,
            IndexDeltaOperation::Delete,
            primary_key,
            tuple,
        )
        .await?;
    }
    Ok(())
}

async fn append_delta(
    view: &mut dyn KvView,
    transition_id: &crate::engine::catalog::identity::TransitionId,
    hard_limit: u64,
    operation: IndexDeltaOperation,
    primary_key: &[u8],
    tuple: Vec<u8>,
) -> Result<()> {
    store::append_index_delta(
        view,
        transition_id,
        hard_limit,
        IndexDelta {
            id: String::new(),
            sequence: 0,
            operation,
            pk: primary_key.to_vec(),
            tuple,
        },
    )
    .await?;
    Ok(())
}

fn index_tuple(table: &Table, index: &Index, row: &Row) -> Result<Vec<u8>> {
    let columns = table
        .index_column_names(index)
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    codec::encode_row_tuple(row, &columns)
}

fn has_null(table: &Table, index: &Index, row: &Row) -> bool {
    table
        .index_column_names(index)
        .into_iter()
        .any(|column| row.get(column).is_none_or(Value::is_null))
}

#[cfg(test)]
mod tests;
