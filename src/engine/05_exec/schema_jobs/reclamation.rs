use bytes::Bytes;

use crate::engine::catalog::identity::{OwnerEpoch, ReclamationId};
use crate::engine::catalog::model::{
    Reclamation, ReclamationKind, ReclamationState, Timestamp, TransitionKind, TransitionState,
};
use crate::engine::catalog::store;
use crate::engine::kv::key_encoding::prefix_end;
use crate::engine::kv::{IsolationLevel, KeyRange, KvView, TransactionView};

use super::{Engine, EngineEvent, EngineOperation, Error, ErrorKind, Result, finish};
use crate::engine::exec::codec;

impl Engine {
    pub async fn claim_reclamation(&self, id: &ReclamationId) -> Result<Option<OwnerEpoch>> {
        let transaction = self
            .store
            .begin(IsolationLevel::SerializableSnapshot)
            .await?;
        let result = async {
            let mut view = TransactionView(&*transaction);
            let Some(mut reclamation) = store::get_reclamation(&mut view, id).await? else {
                return Ok(None);
            };
            if matches!(
                reclamation.state,
                ReclamationState::Reclaimed | ReclamationState::Failed
            ) {
                return Ok(None);
            }
            reclamation.owner_epoch = reclamation.owner_epoch.next();
            reclamation.generation = reclamation.generation.next();
            reclamation.state = ReclamationState::Reclaiming;
            reclamation.last_error.clear();
            reclamation.updated_at = self.runtime.now().into();
            store::save_reclamation(&mut view, &reclamation).await?;
            Ok(Some(reclamation.owner_epoch))
        }
        .await;
        finish(
            self,
            EngineOperation::ClaimReclamation {
                reclamation_id: id.clone(),
            },
            transaction,
            result,
        )
        .await
    }

    pub async fn step_reclamation(
        &self,
        id: &ReclamationId,
        owner: OwnerEpoch,
        batch_size: usize,
    ) -> Result<(Reclamation, usize)> {
        let batch_size = batch_size.max(1);
        let operation = EngineOperation::StepReclamation {
            reclamation_id: id.clone(),
            owner,
        };
        // Eligibility is a predicate over live catalog objects and the full
        // retention-pin set. A pin or target publication that commits after
        // those reads must conflict with this batch rather than allowing
        // reclamation to delete newly retained bytes.
        let transaction = self
            .store
            .begin(IsolationLevel::SerializableSnapshot)
            .await?;
        let result = async {
            let mut view = TransactionView(&*transaction);
            let mut reclamation =
                store::get_reclamation(&mut view, id)
                    .await?
                    .ok_or_else(|| {
                        Error::message(
                            ErrorKind::InvalidInput,
                            format!("catalog: reclamation {id:?} does not exist"),
                        )
                    })?;
            if reclamation.owner_epoch != owner {
                return Err(Error::message(
                    ErrorKind::Conflict,
                    format!("catalog: reclamation {id:?} ownership changed"),
                ));
            }
            if reclamation.state == ReclamationState::Reclaimed {
                return Ok((reclamation, 0));
            }
            if reclamation.state != ReclamationState::Reclaiming {
                return Err(Error::message(
                    ErrorKind::InvalidInput,
                    format!(
                        "catalog: reclamation {id:?} cannot run in state {:?}",
                        reclamation.state
                    ),
                ));
            }
            if let Some(pin) = store::retention_blocker(&mut view, &reclamation).await? {
                return Err(Error::message(
                    ErrorKind::Conflict,
                    format!(
                        "catalog: reclamation {id:?} is retained by pin {:?}",
                        pin.id
                    ),
                ));
            }
            validate_eligibility(&mut view, &reclamation).await?;
            let (items, done) = apply_batch(&mut view, &mut reclamation, batch_size).await?;
            self.events
                .reach(EngineEvent::PhysicalBatchStaged {
                    operation: operation.clone(),
                    items,
                })
                .await;
            reclamation.batch_id = reclamation.batch_id.saturating_add(1);
            reclamation.items_reclaimed = reclamation.items_reclaimed.saturating_add(items as u64);
            reclamation.generation = reclamation.generation.next();
            reclamation.updated_at = self.runtime.now().into();
            if done {
                reclamation.state = ReclamationState::Reclaimed;
                reclamation.cursor.clear();
                compact_completed(&mut view, &mut reclamation, self.runtime.now().into()).await?;
                self.events
                    .reach(EngineEvent::CompactionStaged {
                        operation: operation.clone(),
                        items: 1,
                    })
                    .await;
            }
            store::save_reclamation(&mut view, &reclamation).await?;
            self.events
                .reach(EngineEvent::ReclamationCheckpointStaged {
                    operation: operation.clone(),
                    reclamation_kind: reclamation.kind,
                    generation: reclamation.generation.get(),
                    batch_id: reclamation.batch_id,
                    state: reclamation.state,
                    phase: reclamation.phase.clone(),
                })
                .await;
            Ok((reclamation, items))
        }
        .await;
        finish(self, operation, transaction, result).await
    }

    pub async fn fail_reclamation(
        &self,
        id: &ReclamationId,
        owner: OwnerEpoch,
        cause: impl Into<String>,
    ) -> Result<Reclamation> {
        let transaction = self
            .store
            .begin(IsolationLevel::SerializableSnapshot)
            .await?;
        let cause = cause.into();
        let result = async {
            let mut view = TransactionView(&*transaction);
            let mut reclamation =
                store::get_reclamation(&mut view, id)
                    .await?
                    .ok_or_else(|| {
                        Error::message(
                            ErrorKind::InvalidInput,
                            format!("catalog: reclamation {id:?} does not exist"),
                        )
                    })?;
            if reclamation.owner_epoch != owner {
                return Err(Error::message(
                    ErrorKind::Conflict,
                    format!("catalog: reclamation {id:?} ownership changed"),
                ));
            }
            if reclamation.state == ReclamationState::Reclaimed {
                return Ok(reclamation);
            }
            reclamation.state = ReclamationState::Failed;
            reclamation.generation = reclamation.generation.next();
            reclamation.last_error = cause;
            reclamation.updated_at = self.runtime.now().into();
            store::save_reclamation(&mut view, &reclamation).await?;
            Ok(reclamation)
        }
        .await;
        finish(
            self,
            EngineOperation::FailReclamation {
                reclamation_id: id.clone(),
            },
            transaction,
            result,
        )
        .await
    }

    pub async fn compact_catalog_history_step(
        &self,
        retain_recent: u64,
        batch_size: usize,
    ) -> Result<(usize, bool)> {
        let transaction = self
            .store
            .begin(IsolationLevel::SerializableSnapshot)
            .await?;
        let operation = EngineOperation::CompactCatalogHistory;
        let result = async {
            let mut view = TransactionView(&*transaction);
            let result =
                store::compact_revision_history_batch(&mut view, retain_recent, batch_size).await?;
            self.events
                .reach(EngineEvent::CompactionStaged {
                    operation: operation.clone(),
                    items: result.0,
                })
                .await;
            Ok(result)
        }
        .await;
        finish(self, operation, transaction, result).await
    }

    pub async fn compact_transition_step(
        &self,
        id: &crate::engine::catalog::identity::TransitionId,
    ) -> Result<bool> {
        let transaction = self
            .store
            .begin(IsolationLevel::SerializableSnapshot)
            .await?;
        let operation = EngineOperation::CompactTransition {
            transition_id: id.clone(),
        };
        let result = async {
            let mut view = TransactionView(&*transaction);
            let transition = store::get_transition(&mut view, id).await?.ok_or_else(|| {
                Error::message(
                    ErrorKind::InvalidInput,
                    format!("catalog: transition {id:?} does not exist"),
                )
            })?;
            if !store::transition_compaction_eligible(&mut view, &transition).await? {
                return Ok(false);
            }
            store::compact_transition(&mut view, id, self.runtime.now().into()).await?;
            self.events
                .reach(EngineEvent::CompactionStaged {
                    operation: operation.clone(),
                    items: 1,
                })
                .await;
            Ok(true)
        }
        .await;
        finish(self, operation, transaction, result).await
    }
}

async fn validate_eligibility(view: &mut dyn KvView, value: &Reclamation) -> Result<()> {
    let table = store::get_table_by_id(view, &value.table_id).await?;
    let invalid = |message: String| Error::message(ErrorKind::CorruptData, message);
    match value.kind {
        ReclamationKind::Table if table.is_some() => {
            return Err(invalid(format!(
                "catalog: reclamation {:?} targets a live table",
                value.id
            )));
        }
        ReclamationKind::Column => {
            if table.as_ref().is_some_and(|table| {
                table
                    .columns
                    .iter()
                    .any(|column| column.id == value.column_id)
            }) {
                return Err(invalid(format!(
                    "catalog: reclamation {:?} targets a live column",
                    value.id
                )));
            }
        }
        ReclamationKind::Index => {
            if table
                .as_ref()
                .is_some_and(|table| table.indexes.iter().any(|index| index.id == value.index_id))
            {
                return Err(invalid(format!(
                    "catalog: reclamation {:?} targets a live index",
                    value.id
                )));
            }
        }
        ReclamationKind::TableDefinition => {
            if store::definition_head(
                view,
                value
                    .table_schema_id
                    .expect("validated reclamation schema ID"),
            )
            .await?
            .is_some_and(|(_, generation)| generation == value.definition_generation)
            {
                return Err(invalid(format!(
                    "catalog: reclamation {:?} targets the current table definition",
                    value.id
                )));
            }
        }
        ReclamationKind::WriteProtocolDefinition => {
            if store::write_protocol_generation(view, &value.table_id).await?
                == Some(value.write_protocol_generation)
            {
                return Err(invalid(format!(
                    "catalog: reclamation {:?} targets the current write protocol",
                    value.id
                )));
            }
        }
        ReclamationKind::TransitionDeltas
        | ReclamationKind::CancelledIndex
        | ReclamationKind::FailedIndex => {
            let transition = terminal_transition(view, value).await?;
            let expected = match value.kind {
                ReclamationKind::TransitionDeltas => TransitionState::Ready,
                ReclamationKind::CancelledIndex => TransitionState::Cancelled,
                ReclamationKind::FailedIndex => TransitionState::Failed,
                _ => unreachable!(),
            };
            if transition.kind != TransitionKind::IndexBuild
                || transition.state != expected
                || transition.table_id != value.table_id
                || transition.index.id != value.index_id
            {
                return Err(invalid(format!(
                    "catalog: reclamation {:?} does not match its index transition",
                    value.id
                )));
            }
        }
        ReclamationKind::ReplacedColumn
        | ReclamationKind::CancelledReplacement
        | ReclamationKind::FailedReplacement => {
            let transition = terminal_transition(view, value).await?;
            let expected = match value.kind {
                ReclamationKind::ReplacedColumn => TransitionState::Ready,
                ReclamationKind::CancelledReplacement => TransitionState::Cancelled,
                ReclamationKind::FailedReplacement => TransitionState::Failed,
                _ => unreachable!(),
            };
            if transition.kind != TransitionKind::ColumnReplacement
                || transition.state != expected
                || transition.table_id != value.table_id
            {
                return Err(invalid(format!(
                    "catalog: reclamation {:?} does not match its replacement transition",
                    value.id
                )));
            }
            let replacement = transition.column_replacement.as_ref().ok_or_else(|| {
                invalid(format!(
                    "catalog: reclamation {:?} replacement definition is missing",
                    value.id
                ))
            })?;
            let expected_column = if value.kind == ReclamationKind::ReplacedColumn {
                &replacement.source.id
            } else {
                &replacement.target.id
            };
            if &value.column_id != expected_column
                || table.as_ref().is_some_and(|table| {
                    table
                        .columns
                        .iter()
                        .any(|column| column.id == value.column_id)
                })
            {
                return Err(invalid(format!(
                    "catalog: reclamation {:?} does not match its retired replacement column",
                    value.id
                )));
            }
        }
        ReclamationKind::ConstraintValidation => {
            let transition = terminal_transition(view, value).await?;
            if transition.kind != TransitionKind::ConstraintValidation {
                return Err(invalid(format!(
                    "catalog: reclamation {:?} does not match its constraint transition",
                    value.id
                )));
            }
        }
        ReclamationKind::Table => {}
    }
    Ok(())
}

async fn terminal_transition(
    view: &mut dyn KvView,
    value: &Reclamation,
) -> Result<crate::engine::catalog::model::SchemaTransition> {
    let transition = store::get_transition(view, &value.transition_id)
        .await?
        .ok_or_else(|| {
            Error::message(
                ErrorKind::CorruptData,
                format!("catalog: reclamation {:?} transition is missing", value.id),
            )
        })?;
    if !transition.state.is_terminal() {
        return Err(Error::message(
            ErrorKind::CorruptData,
            format!(
                "catalog: reclamation {:?} transition is not terminal",
                value.id
            ),
        ));
    }
    Ok(transition)
}

async fn apply_batch(
    view: &mut dyn KvView,
    value: &mut Reclamation,
    limit: usize,
) -> Result<(usize, bool)> {
    match value.kind {
        ReclamationKind::Table => reclaim_table(view, value, limit).await,
        ReclamationKind::Column => reclaim_column(view, value, limit).await,
        ReclamationKind::Index => {
            reclaim_prefix(
                view,
                value,
                codec::index_prefix_for(&value.table_id, &value.index_id),
                limit,
            )
            .await
        }
        ReclamationKind::TableDefinition => {
            reclaim_exact(
                view,
                store::table_definition_key(
                    value
                        .table_schema_id
                        .expect("validated reclamation schema ID"),
                    value.definition_generation,
                ),
            )
            .await
        }
        ReclamationKind::WriteProtocolDefinition => {
            reclaim_exact(
                view,
                store::write_protocol_definition_key(
                    &value.table_id,
                    value.write_protocol_generation,
                ),
            )
            .await
        }
        ReclamationKind::TransitionDeltas => reclaim_transition(view, value, limit, false).await,
        ReclamationKind::CancelledIndex | ReclamationKind::FailedIndex => {
            reclaim_transition(view, value, limit, true).await
        }
        ReclamationKind::ReplacedColumn
        | ReclamationKind::CancelledReplacement
        | ReclamationKind::FailedReplacement => reclaim_replacement(view, value, limit).await,
        ReclamationKind::ConstraintValidation => {
            let (start, end) = store::transition_violation_range(&value.transition_id);
            reclaim_range(view, value, start, Some(end), limit).await
        }
    }
}

async fn reclaim_table(
    view: &mut dyn KvView,
    value: &mut Reclamation,
    limit: usize,
) -> Result<(usize, bool)> {
    if value.phase.is_empty() {
        value.phase = "data".into();
    }
    if value.phase == "data" {
        let result =
            reclaim_prefix(view, value, codec::data_prefix_for(&value.table_id), limit).await?;
        if result.1 {
            value.phase = "index:0".into();
            value.cursor.clear();
        }
        return Ok((result.0, false));
    }
    if let Some(position) = value.phase.strip_prefix("index:") {
        let position = position.parse::<usize>().map_err(|_| {
            Error::message(
                ErrorKind::CorruptData,
                "catalog: invalid reclamation index phase",
            )
        })?;
        if position >= value.index_ids.len() {
            value.phase = "definitions".into();
            value.cursor.clear();
            return Ok((0, false));
        }
        let result = reclaim_prefix(
            view,
            value,
            codec::index_prefix_for(&value.table_id, &value.index_ids[position]),
            limit,
        )
        .await?;
        if result.1 {
            value.phase = format!("index:{}", position + 1);
            value.cursor.clear();
        }
        return Ok((result.0, false));
    }
    if value.phase == "definitions" {
        let (start, end) = store::table_definition_range(
            value
                .table_schema_id
                .expect("validated reclamation schema ID"),
        );
        let result = reclaim_range(view, value, start, Some(end), limit).await?;
        if result.1 {
            value.phase = "write_protocols".into();
            value.cursor.clear();
        }
        return Ok((result.0, false));
    }
    if value.phase == "write_protocols" {
        let (start, end) = store::write_protocol_definition_range(&value.table_id);
        return reclaim_range(view, value, start, Some(end), limit).await;
    }
    Err(Error::message(
        ErrorKind::CorruptData,
        format!("catalog: invalid reclamation phase {:?}", value.phase),
    ))
}

async fn reclaim_column(
    view: &mut dyn KvView,
    value: &mut Reclamation,
    limit: usize,
) -> Result<(usize, bool)> {
    let prefix = codec::data_prefix_for(&value.table_id);
    let entries = scan_batch(view, value, prefix, None, limit).await?;
    for (key, raw) in &entries {
        let (cleaned, changed) = codec::remove_column(raw, &value.column_id)?;
        if changed {
            view.put(Bytes::copy_from_slice(key), Bytes::from(cleaned))
                .await?;
        }
    }
    Ok((entries.len(), entries.len() < limit))
}

async fn reclaim_replacement(
    view: &mut dyn KvView,
    value: &mut Reclamation,
    limit: usize,
) -> Result<(usize, bool)> {
    if value.phase.is_empty() {
        value.phase = "column".into();
    }
    if value.phase == "column" {
        let result = reclaim_column(view, value, limit).await?;
        if result.1 {
            value.phase = "violations".into();
            value.cursor.clear();
        }
        return Ok((result.0, false));
    }
    if value.phase == "violations" {
        let (start, end) = store::transition_violation_range(&value.transition_id);
        return reclaim_range(view, value, start, Some(end), limit).await;
    }
    Err(Error::message(
        ErrorKind::CorruptData,
        "catalog: invalid replacement reclamation phase",
    ))
}

async fn reclaim_transition(
    view: &mut dyn KvView,
    value: &mut Reclamation,
    limit: usize,
    partial_index: bool,
) -> Result<(usize, bool)> {
    if value.phase.is_empty() {
        value.phase = if partial_index { "index" } else { "claims" }.into();
    }
    let (start, end, next) = match value.phase.as_str() {
        "index" => {
            let start = codec::index_prefix_for(&value.table_id, &value.index_id);
            let end = prefix_end(&start);
            (start, end, "claims")
        }
        "claims" => {
            let (start, end) = store::unique_claim_range(&value.transition_id);
            (start, Some(end), "violations")
        }
        "violations" => {
            let (start, end) = store::unique_violation_range(&value.transition_id);
            (start, Some(end), "deltas")
        }
        "deltas" => {
            let (start, end) = store::delta_range(&value.transition_id);
            let result = reclaim_range(view, value, start, Some(end), limit).await?;
            if result.1 {
                store::delete_delta_metadata(view, &value.transition_id).await?;
                return Ok((result.0 + 2, true));
            }
            return Ok((result.0, false));
        }
        _ => {
            return Err(Error::message(
                ErrorKind::CorruptData,
                "catalog: invalid transition reclamation phase",
            ));
        }
    };
    let result = reclaim_range(view, value, start, end, limit).await?;
    if result.1 {
        value.phase = next.into();
        value.cursor.clear();
    }
    Ok((result.0, false))
}

async fn reclaim_prefix(
    view: &mut dyn KvView,
    value: &mut Reclamation,
    prefix: Vec<u8>,
    limit: usize,
) -> Result<(usize, bool)> {
    reclaim_range(view, value, prefix.clone(), prefix_end(&prefix), limit).await
}

async fn reclaim_range(
    view: &mut dyn KvView,
    value: &mut Reclamation,
    start: Vec<u8>,
    end: Option<Vec<u8>>,
    limit: usize,
) -> Result<(usize, bool)> {
    let entries = scan_batch(view, value, start, end, limit).await?;
    for (key, _) in &entries {
        view.delete(key).await?;
    }
    Ok((entries.len(), entries.len() < limit))
}

async fn scan_batch(
    view: &mut dyn KvView,
    value: &mut Reclamation,
    start: Vec<u8>,
    end: Option<Vec<u8>>,
    limit: usize,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut scan_start = start;
    if !value.cursor.is_empty() {
        scan_start = value.cursor.clone();
        scan_start.push(0);
    }
    let mut iterator = view
        .scan(KeyRange {
            start: Some(Bytes::from(scan_start)),
            end: end.map(Bytes::from),
        })
        .await?;
    let mut entries = Vec::with_capacity(limit);
    while entries.len() < limit {
        let Some(entry) = iterator.next().await? else {
            break;
        };
        entries.push((entry.key.to_vec(), entry.value.to_vec()));
    }
    drop(iterator);
    if let Some((key, _)) = entries.last() {
        value.cursor.clone_from(key);
    }
    Ok(entries)
}

async fn reclaim_exact(view: &mut dyn KvView, key: Vec<u8>) -> Result<(usize, bool)> {
    if view.get(&key).await?.is_none() {
        return Ok((0, true));
    }
    view.delete(&key).await?;
    Ok((1, true))
}

async fn compact_completed(
    view: &mut dyn KvView,
    value: &mut Reclamation,
    now: Timestamp,
) -> Result<()> {
    if matches!(
        value.kind,
        ReclamationKind::TransitionDeltas
            | ReclamationKind::CancelledIndex
            | ReclamationKind::FailedIndex
            | ReclamationKind::ReplacedColumn
            | ReclamationKind::CancelledReplacement
            | ReclamationKind::FailedReplacement
            | ReclamationKind::ConstraintValidation
    ) && !store::transition_diagnostics_pinned(view, &value.transition_id).await?
    {
        store::compact_transition(view, &value.transition_id, now).await?;
    }
    value.owner_epoch = OwnerEpoch::ZERO;
    value.phase.clear();
    value.cursor.clear();
    value.batch_id = 0;
    value.index_ids.clear();
    value.last_error.clear();
    value.compacted_at = now;
    Ok(())
}
