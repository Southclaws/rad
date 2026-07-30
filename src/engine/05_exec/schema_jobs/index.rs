use bytes::Bytes;

use crate::engine::catalog::Mutation;
use crate::engine::catalog::identity::{OwnerEpoch, TransitionId};
use crate::engine::catalog::model::{
    DataPosition, IndexDeltaOperation, IndexState, SchemaTransition, TransitionState,
};
use crate::engine::catalog::store;
use crate::engine::kv::{IsolationLevel, KeyRange, KvView, TransactionView};

use super::{
    Engine, EngineEvent, EngineOperation, Error, ErrorKind, GateAction, Result, TransitionStep,
    emit_write_protocol, finish, ownership_changed,
};
use crate::engine::exec::{codec, row_store};

impl Engine {
    pub(super) async fn step_index_build(
        &self,
        current: SchemaTransition,
        owner: OwnerEpoch,
        batch_size: usize,
    ) -> Result<TransitionStep> {
        let batch_size = batch_size.max(1);
        let operation = EngineOperation::StepTransition {
            transition_id: current.id.clone(),
            owner,
            transition_kind: current.kind,
            started_state: current.state,
        };
        let isolation = if current.index.unique || current.state == TransitionState::Validating {
            IsolationLevel::SerializableSnapshot
        } else {
            IsolationLevel::Snapshot
        };
        let transaction = self.store.begin(isolation).await?;
        let result = async {
            let mut view = TransactionView(&*transaction);
            let mut transition = required_owned(&mut view, &current.id, owner).await?;
            let started_state = transition.state;
            let table = store::get_table_by_id(&view, &transition.table_id)
                .await?
                .ok_or_else(|| {
                    Error::message(
                        ErrorKind::CorruptData,
                        format!(
                            "catalog: transition {:?} table no longer exists",
                            transition.id
                        ),
                    )
                })?;
            let previous_applied = transition.applied_delta;
            let items = match started_state {
                TransitionState::Building => {
                    scan_build_batch(
                        &mut view,
                        &table,
                        &mut transition,
                        batch_size,
                        self.runtime.now().into(),
                    )
                    .await?
                }
                TransitionState::CatchingUp => {
                    apply_delta_batch(
                        &mut view,
                        &table,
                        &mut transition,
                        batch_size,
                        false,
                        self.runtime.now().into(),
                    )
                    .await?
                }
                TransitionState::Validating => {
                    apply_delta_batch(
                        &mut view,
                        &table,
                        &mut transition,
                        batch_size,
                        true,
                        self.runtime.now().into(),
                    )
                    .await?
                }
                state => {
                    return Err(Error::message(
                        ErrorKind::InvalidInput,
                        format!(
                            "catalog: transition {:?} cannot run in state {state:?}",
                            transition.id
                        ),
                    ));
                }
            };
            self.events
                .reach(EngineEvent::PhysicalBatchStaged {
                    operation: operation.clone(),
                    items,
                })
                .await;
            if transition.applied_delta != previous_applied {
                store::save_delta_applied(&mut view, &transition.id, transition.applied_delta)
                    .await?;
            }

            let drained = matches!(
                started_state,
                TransitionState::CatchingUp | TransitionState::Validating
            ) && items < batch_size;
            let transition = finish_index_state(
                self,
                &mut view,
                transition,
                owner,
                started_state,
                drained,
                &operation,
            )
            .await?;
            self.events
                .reach(EngineEvent::CheckpointStaged {
                    operation: operation.clone(),
                    generation: transition.generation.get(),
                    batch_id: transition.batch_id,
                    state: transition.state,
                })
                .await;
            Ok(TransitionStep { transition, items })
        }
        .await;
        let step = finish(self, operation, transaction, result).await?;
        Ok(TransitionStep {
            transition: self.inspect_schema_transition(&step.transition.id).await?,
            items: step.items,
        })
    }
}

async fn required_owned(
    view: &mut dyn KvView,
    id: &TransitionId,
    owner: OwnerEpoch,
) -> Result<SchemaTransition> {
    let transition = store::get_transition(view, id).await?.ok_or_else(|| {
        Error::message(
            ErrorKind::InvalidInput,
            format!("catalog: transition {id:?} does not exist"),
        )
    })?;
    if transition.owner_epoch != owner {
        return Err(ownership_changed(id));
    }
    Ok(transition)
}

async fn scan_build_batch(
    view: &mut dyn KvView,
    table: &crate::engine::catalog::model::Table,
    transition: &mut SchemaTransition,
    batch_size: usize,
    now: crate::engine::catalog::model::Timestamp,
) -> Result<usize> {
    let rows = row_store::scan_table_batch(view, table, &transition.cursor, batch_size).await?;
    for row in &rows {
        let tuple = codec::encode_row_tuple(&row.row, &transition.index.columns)?;
        if transition.index.unique
            && transition
                .index
                .columns
                .iter()
                .all(|column| !row.row[column].is_null())
        {
            store::put_unique_claim(view, &transition.id, &tuple, &row.primary_key).await?;
        }
        view.put(
            Bytes::from(codec::index_key(
                table,
                &transition.index.id,
                &tuple,
                &row.primary_key,
            )),
            Bytes::copy_from_slice(&row.primary_key),
        )
        .await?;
    }
    transition.batch_id = transition.batch_id.saturating_add(1);
    transition.generation = transition.generation.next();
    transition.rows_scanned = transition.rows_scanned.saturating_add(rows.len() as u64);
    transition.updated_at = now;
    if let Some(last) = rows.last() {
        transition.cursor.clone_from(&last.key);
    } else {
        transition.state = TransitionState::CatchingUp;
        transition.index.state = IndexState::CatchingUp;
    }
    Ok(rows.len())
}

async fn apply_delta_batch(
    view: &mut dyn KvView,
    table: &crate::engine::catalog::model::Table,
    transition: &mut SchemaTransition,
    batch_size: usize,
    finalizing: bool,
    now: crate::engine::catalog::model::Timestamp,
) -> Result<usize> {
    let (_, end) = store::delta_range(&transition.id);
    let start = store::delta_key(&transition.id, transition.applied_delta.saturating_add(1));
    let mut iterator = view
        .scan(KeyRange::new(Bytes::from(start), Bytes::from(end)))
        .await?;
    let mut deltas = Vec::with_capacity(batch_size);
    while deltas.len() < batch_size {
        let Some(entry) = iterator.next().await? else {
            break;
        };
        deltas.push(store::decode_index_delta(
            &transition.id,
            &entry.key,
            &entry.value,
        )?);
    }
    drop(iterator);
    for delta in &deltas {
        let key = codec::index_key(table, &transition.index.id, &delta.tuple, &delta.pk);
        match delta.operation {
            IndexDeltaOperation::Put => {
                view.put(Bytes::from(key), Bytes::copy_from_slice(&delta.pk))
                    .await?;
            }
            IndexDeltaOperation::Delete => view.delete(&key).await?,
        }
        transition.applied_delta = delta.sequence;
    }
    transition.batch_id = transition.batch_id.saturating_add(1);
    transition.generation = transition.generation.next();
    transition.updated_at = now;
    if deltas.len() < batch_size && !finalizing && !transition.index.unique {
        transition.state = TransitionState::Validating;
        transition.index.state = IndexState::Validating;
    }
    Ok(deltas.len())
}

async fn finish_index_state(
    engine: &Engine,
    view: &mut dyn KvView,
    mut transition: SchemaTransition,
    owner: OwnerEpoch,
    started_state: TransitionState,
    drained: bool,
    operation: &EngineOperation,
) -> Result<SchemaTransition> {
    let high_water = store::delta_high_water(view, &transition.id).await?;
    let caught_up = transition.applied_delta >= high_water;
    if started_state == TransitionState::CatchingUp
        && drained
        && caught_up
        && transition.index.unique
    {
        store::save_transition(view, &transition).await?;
        let mut mutation = Mutation::with_runtime(view, engine.runtime.clone());
        transition = mutation
            .begin_index_validation(&transition.id, owner)
            .await?;
        mutation.finish().await?;
        engine
            .events
            .reach(EngineEvent::FinalizationGateStaged {
                operation: operation.clone(),
                action: GateAction::Acquired,
            })
            .await;
        emit_write_protocol(engine, view, operation, &transition).await?;
        return Ok(transition);
    }
    if started_state == TransitionState::Validating && drained && caught_up {
        if transition.index.unique
            && let Some(tuple) = store::first_unique_violation(view, &transition.id).await?
        {
            store::save_transition(view, &transition).await?;
            let message = format!(
                "exec: cannot publish unique index {:?}: indexed tuple {:x?} is shared by multiple rows",
                transition.index.name, tuple
            );
            let mut mutation = Mutation::with_runtime(view, engine.runtime.clone());
            transition = mutation
                .fail_index_validation(&transition.id, owner, message)
                .await?;
            mutation.finish().await?;
            engine
                .events
                .reach(EngineEvent::FinalizationGateStaged {
                    operation: operation.clone(),
                    action: GateAction::Released,
                })
                .await;
            emit_write_protocol(engine, view, operation, &transition).await?;
            engine
                .events
                .reach(EngineEvent::CatalogPublicationStaged {
                    operation: operation.clone(),
                    state: transition.state,
                })
                .await;
            return Ok(transition);
        }
        if let Some(position) = view.begin_position() {
            transition.barrier_position = DataPosition::new(position.as_str());
        }
        store::save_transition(view, &transition).await?;
        let mut mutation = Mutation::with_runtime(view, engine.runtime.clone());
        transition = mutation.publish_index_ready(&transition.id, owner).await?;
        mutation.finish().await?;
        if transition.index.unique {
            engine
                .events
                .reach(EngineEvent::FinalizationGateStaged {
                    operation: operation.clone(),
                    action: GateAction::Released,
                })
                .await;
        }
        emit_write_protocol(engine, view, operation, &transition).await?;
        engine
            .events
            .reach(EngineEvent::CatalogPublicationStaged {
                operation: operation.clone(),
                state: transition.state,
            })
            .await;
        return Ok(transition);
    }
    transition.refresh_work_state(high_water);
    store::save_transition(view, &transition).await?;
    Ok(transition)
}
