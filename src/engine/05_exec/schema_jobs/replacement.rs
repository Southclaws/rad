use bytes::Bytes;

use crate::engine::catalog::Mutation;
use crate::engine::catalog::identity::OwnerEpoch;
use crate::engine::catalog::model::{DataPosition, SchemaTransition, TransitionState};
use crate::engine::catalog::store;
use crate::engine::kv::{IsolationLevel, KvView, TransactionView};

use super::{
    Engine, EngineOperation, Error, ErrorKind, Result, TransitionStep, required_owned_transition,
    transition_table,
};
use crate::engine::exec::{codec, row_store};

impl Engine {
    pub(super) async fn step_column_replacement(
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
        let isolation = if current.state == TransitionState::Validating {
            IsolationLevel::SerializableSnapshot
        } else {
            IsolationLevel::Snapshot
        };
        let transaction = self.store.begin(isolation).await?;
        let result = async {
            let mut view = TransactionView(&*transaction);
            let mut transition = required_owned_transition(&mut view, &current.id, owner).await?;
            if transition.column_replacement.is_none() {
                return Err(Error::message(
                    ErrorKind::CorruptData,
                    format!(
                        "catalog: replacement transition {:?} has no definition",
                        transition.id
                    ),
                ));
            }
            let table = transition_table(&view, &transition).await?;
            let items = match transition.state {
                TransitionState::Building => {
                    scan_batch(
                        self,
                        &mut view,
                        &table,
                        &mut transition,
                        owner,
                        batch_size,
                        &operation,
                    )
                    .await?
                }
                TransitionState::Validating => {
                    validate(self, &mut view, &mut transition, owner, &operation).await?;
                    0
                }
                state => {
                    return Err(Error::message(
                        ErrorKind::InvalidInput,
                        format!(
                            "catalog: replacement transition {:?} cannot run in state {state:?}",
                            transition.id
                        ),
                    ));
                }
            };
            self.stage_transition_checkpoint(&operation, &transition)
                .await;
            Ok(TransitionStep { transition, items })
        }
        .await;
        self.finish_transition_step(operation, transaction, result)
            .await
    }
}

async fn scan_batch(
    engine: &Engine,
    view: &mut dyn KvView,
    table: &crate::engine::catalog::model::Table,
    transition: &mut SchemaTransition,
    owner: OwnerEpoch,
    batch_size: usize,
    operation: &EngineOperation,
) -> Result<usize> {
    let rows = row_store::scan_raw_table_batch(view, table, &transition.cursor, batch_size).await?;
    let replacement = transition
        .column_replacement
        .clone()
        .expect("worker validated replacement definition");
    for row in &rows {
        let source = codec::read_column_value(&row.raw, &replacement.source)?;
        match codec::convert_column_value(&source, &replacement.target, replacement.conversion) {
            Ok(target) => {
                let raw = codec::set_column_value(&row.raw, &replacement.target, &target)?;
                view.put(Bytes::copy_from_slice(&row.key), Bytes::from(raw))
                    .await?;
                store::delete_transition_violation(view, &transition.id, &row.primary_key).await?;
            }
            Err(error) => {
                store::put_transition_violation(
                    view,
                    &transition.id,
                    &row.primary_key,
                    &error.to_string(),
                )
                .await?;
            }
        }
    }
    engine.stage_physical_batch(operation, rows.len()).await;
    let exhausted = transition.advance_scan(
        rows.len(),
        rows.last().map(|row| row.key.as_slice()),
        engine.runtime.now().into(),
    );
    if !exhausted {
        store::save_transition(view, transition).await?;
    } else {
        store::save_transition(view, transition).await?;
        let mut mutation = Mutation::with_runtime(view, engine.runtime.clone());
        *transition = mutation
            .begin_column_replacement_validation(&transition.id, owner)
            .await?;
        mutation.finish().await?;
        engine
            .stage_finalization_gate(view, operation, transition)
            .await?;
    }
    Ok(rows.len())
}

async fn validate(
    engine: &Engine,
    view: &mut dyn KvView,
    transition: &mut SchemaTransition,
    owner: OwnerEpoch,
    operation: &EngineOperation,
) -> Result<()> {
    if let Some((row, cause)) = store::first_transition_violation(view, &transition.id).await? {
        let source = &transition
            .column_replacement
            .as_ref()
            .expect("worker validated replacement definition")
            .source;
        let message = format!(
            "exec: cannot publish replacement for column {:?}: row {row:x?}: {cause}",
            source.name
        );
        store::save_transition(view, transition).await?;
        let mut mutation = Mutation::with_runtime(view, engine.runtime.clone());
        *transition = mutation
            .fail_column_replacement(&transition.id, owner, message)
            .await?;
        mutation.finish().await?;
        engine
            .stage_transition_publication(view, operation, transition, true)
            .await?;
        return Ok(());
    }
    if let Some(position) = view.begin_position() {
        transition.barrier_position = DataPosition::new(position.as_str());
    }
    store::save_transition(view, transition).await?;
    let mut mutation = Mutation::with_runtime(view, engine.runtime.clone());
    *transition = mutation
        .publish_column_replacement(&transition.id, owner)
        .await?;
    mutation.finish().await?;
    engine
        .stage_transition_publication(view, operation, transition, true)
        .await?;
    Ok(())
}
