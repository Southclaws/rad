use crate::engine::catalog::Mutation;
use crate::engine::catalog::identity::OwnerEpoch;
use crate::engine::catalog::model::{
    ConstraintKind, ConstraintState, DataPosition, SchemaTransition, TransitionState,
};
use crate::engine::catalog::store;
use crate::engine::kv::{IsolationLevel, KvView, TransactionView};
use crate::engine::lir::Value;

use super::{
    Engine, EngineOperation, Error, ErrorKind, Result, TransitionStep, emit_write_protocol,
    required_owned_transition, transition_table,
};
use crate::engine::exec::{codec, row_store};

impl Engine {
    pub(super) async fn step_constraint_validation(
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
        let transaction = self
            .store
            .begin(IsolationLevel::SerializableSnapshot)
            .await?;
        let result = async {
            let mut view = TransactionView(&*transaction);
            let mut transition =
                required_owned_transition(&mut view, &current.id, owner).await?;
            if transition.constraint.is_none() {
                return Err(Error::message(
                    ErrorKind::CorruptData,
                    format!(
                        "catalog: constraint transition {:?} has no definition",
                        transition.id
                    ),
                ));
            }
            let table = transition_table(&view, &transition).await?;
            let constraint = transition
                .constraint
                .clone()
                .expect("worker validated constraint definition");
            let items = match (transition.state, constraint.state) {
                (TransitionState::Building, ConstraintState::EnforcingNewWrites) => {
                    let id = transition.id.clone();
                    let mut mutation = Mutation::with_runtime(&mut view, self.runtime.clone());
                    transition = mutation
                        .begin_constraint_historical_validation(&id, owner)
                        .await?;
                    mutation.finish().await?;
                    emit_write_protocol(self, &mut view, &operation, &transition).await?;
                    0
                }
                (TransitionState::Building, ConstraintState::ValidatingExisting) => {
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
                (TransitionState::Validating, _) => {
                    validate(self, &mut view, &mut transition, owner, &operation).await?;
                    0
                }
                (state, constraint_state) => {
                    return Err(Error::message(
                        ErrorKind::InvalidInput,
                        format!(
                            "catalog: constraint transition {:?} cannot run in states {state:?}/{constraint_state:?}",
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
    let constraint = transition
        .constraint
        .as_ref()
        .expect("worker validated constraint definition");
    if constraint.kind != ConstraintKind::NotNull || constraint.column_ids.len() != 1 {
        return Err(Error::message(
            ErrorKind::CorruptData,
            format!(
                "catalog: constraint {:?} has invalid definition",
                constraint.name
            ),
        ));
    }
    let column = table
        .columns
        .iter()
        .find(|column| column.id == constraint.column_ids[0])
        .cloned()
        .ok_or_else(|| {
            Error::message(
                ErrorKind::CorruptData,
                format!(
                    "catalog: constraint {:?} physical column is missing",
                    constraint.name
                ),
            )
        })?;
    let rows = row_store::scan_raw_table_batch(view, table, &transition.cursor, batch_size).await?;
    for row in &rows {
        let value = codec::read_column_value(&row.raw, &column)?;
        if matches!(value, Value::Null(_)) {
            store::put_transition_violation(
                view,
                &transition.id,
                &row.primary_key,
                &format!("column {:?} is NULL", column.name),
            )
            .await?;
        } else {
            store::delete_transition_violation(view, &transition.id, &row.primary_key).await?;
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
        let id = transition.id.clone();
        let mut mutation = Mutation::with_runtime(view, engine.runtime.clone());
        *transition = mutation.begin_constraint_finalization(&id, owner).await?;
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
    let id = transition.id.clone();
    let name = transition
        .constraint
        .as_ref()
        .expect("worker validated constraint definition")
        .name
        .clone();
    let violation = store::first_transition_violation(view, &id).await?;
    if violation.is_none()
        && let Some(position) = view.begin_position()
    {
        transition.barrier_position = DataPosition::new(position.as_str());
    }
    store::save_transition(view, transition).await?;
    let mut mutation = Mutation::with_runtime(view, engine.runtime.clone());
    *transition = if let Some((row, cause)) = violation {
        mutation
            .fail_constraint_validation(
                &id,
                owner,
                format!("exec: cannot validate constraint {name:?}: row {row:x?}: {cause}"),
            )
            .await?
    } else {
        mutation.publish_constraint(&id, owner).await?
    };
    mutation.finish().await?;
    engine
        .stage_transition_publication(view, operation, transition, true)
        .await?;
    Ok(())
}
