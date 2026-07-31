//! Bounded, resumable durable schema-work kernels.
//!
//! This module owns no background tasks. Callers choose which job advances and
//! when; each step commits at most one bounded unit so the same surface can be
//! driven by a production scheduler or deterministic simulation.

mod constraint;
mod index;
mod metrics;
mod reclamation;
mod replacement;
#[cfg(test)]
mod tests;

use crate::engine::catalog::Mutation;
use crate::engine::catalog::identity::{OwnerEpoch, ReclamationId, TransitionId};
use crate::engine::catalog::model::{SchemaTransition, Table, TransitionKind, TransitionState};
use crate::engine::catalog::store;
use crate::engine::kv::{IsolationLevel, TransactionView};

use super::{
    Engine, EngineEvent, EngineOperation, Error, ErrorKind, ErrorReason, GateAction, Result,
};

pub use metrics::SchemaStorageMetrics;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SchemaJob {
    Activation(TransitionId),
    Transition(TransitionId),
    Reclamation(ReclamationId),
    TransitionCompaction(TransitionId),
    CatalogHistory,
}

#[derive(Clone, Debug)]
pub struct TransitionStep {
    pub transition: SchemaTransition,
    pub items: usize,
}

impl Engine {
    pub async fn discover_schema_jobs(&self, retain_recent: u64) -> Result<Vec<SchemaJob>> {
        let transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let result = async {
            let mut view = TransactionView(&*transaction);
            let transitions = store::list_transitions(&mut view).await?;
            let reclamations = store::list_reclamations(&mut view).await?;
            let mut jobs = Vec::new();
            for transition in transitions {
                if transition.state == TransitionState::Waiting {
                    jobs.push(SchemaJob::Activation(transition.id));
                } else if transition.state.is_terminal() {
                    if store::transition_compaction_eligible(&mut view, &transition).await? {
                        jobs.push(SchemaJob::TransitionCompaction(transition.id));
                    }
                } else {
                    jobs.push(SchemaJob::Transition(transition.id));
                }
            }
            for reclamation in reclamations {
                if matches!(
                    reclamation.state,
                    crate::engine::catalog::model::ReclamationState::Pending
                        | crate::engine::catalog::model::ReclamationState::Reclaiming
                ) {
                    jobs.push(SchemaJob::Reclamation(reclamation.id));
                }
            }
            if store::revision_compaction_needed(&mut view, retain_recent).await? {
                jobs.push(SchemaJob::CatalogHistory);
            }
            jobs.sort();
            Ok(jobs)
        }
        .await;
        transaction.rollback();
        result
    }

    pub async fn inspect_schema_transition(&self, id: &TransitionId) -> Result<SchemaTransition> {
        let transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let result = async {
            let mut view = TransactionView(&*transaction);
            load_transition(&mut view, id).await
        }
        .await;
        transaction.rollback();
        result
    }

    pub async fn list_schema_transitions(&self) -> Result<Vec<SchemaTransition>> {
        let transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let result = async {
            let mut view = TransactionView(&*transaction);
            let mut transitions = store::list_transitions(&mut view).await?;
            for transition in &mut transitions {
                let high_water = store::delta_high_water(&mut view, &transition.id).await?;
                transition.refresh_work_state(high_water);
            }
            Ok(transitions)
        }
        .await;
        transaction.rollback();
        result
    }

    pub async fn cancel_schema_transition(&self, id: &TransitionId) -> Result<SchemaTransition> {
        let transaction = self
            .store
            .begin(IsolationLevel::SerializableSnapshot)
            .await?;
        let result = async {
            let mut view = TransactionView(&*transaction);
            let mut mutation = Mutation::with_runtime(&mut view, self.runtime.clone());
            let transition = mutation
                .cancel_schema_transition(id)
                .await
                .map_err(|error| {
                    if error.kind() == crate::engine::catalog::ErrorKind::NotFound {
                        Error::source_with_reason(
                            ErrorKind::InvalidInput,
                            ErrorReason::SchemaTransitionNotFound,
                            format!("catalog: transition {id:?} does not exist"),
                            error,
                        )
                    } else {
                        Error::from(error)
                    }
                })?;
            mutation.finish().await?;
            Ok(transition)
        }
        .await;
        let value = self
            .finish_transaction(
                transaction,
                result,
                Some(EngineOperation::CancelTransition {
                    transition_id: id.clone(),
                }),
            )
            .await?;
        self.notify_catalog_change();
        Ok(value)
    }

    /// Claim active transition work by advancing its durable owner epoch.
    pub async fn claim_schema_transition(&self, id: &TransitionId) -> Result<OwnerEpoch> {
        let transaction = self
            .store
            .begin(IsolationLevel::SerializableSnapshot)
            .await?;
        let result = async {
            let mut view = TransactionView(&*transaction);
            let mut transition = load_transition(&mut view, id).await?;
            match transition.state {
                TransitionState::Building
                | TransitionState::CatchingUp
                | TransitionState::Validating => {}
                TransitionState::Ready => return Ok(transition.owner_epoch),
                state => {
                    return Err(Error::message(
                        ErrorKind::InvalidInput,
                        format!("catalog: transition {id:?} cannot be claimed in state {state:?}"),
                    ));
                }
            }
            transition.owner_epoch = transition.owner_epoch.next();
            transition.generation = transition.generation.next();
            transition.last_error.clear();
            transition.updated_at = self.runtime.now().into();
            store::save_transition(&mut view, &transition).await?;
            Ok(transition.owner_epoch)
        }
        .await;
        self.finish_transaction(
            transaction,
            result,
            Some(EngineOperation::ClaimTransition {
                transition_id: id.clone(),
            }),
        )
        .await
    }

    pub async fn step_schema_transition(
        &self,
        id: &TransitionId,
        owner: OwnerEpoch,
        batch_size: usize,
    ) -> Result<TransitionStep> {
        let current = self.inspect_schema_transition(id).await?;
        if current.owner_epoch != owner {
            return Err(ownership_changed(id));
        }
        if current.state.is_terminal() {
            return Ok(TransitionStep {
                transition: current,
                items: 0,
            });
        }
        match current.kind {
            TransitionKind::IndexBuild => self.step_index_build(current, owner, batch_size).await,
            TransitionKind::ColumnReplacement => {
                self.step_column_replacement(current, owner, batch_size)
                    .await
            }
            TransitionKind::ConstraintValidation => {
                self.step_constraint_validation(current, owner, batch_size)
                    .await
            }
        }
    }

    pub async fn activate_waiting_schema_transition(
        &self,
        id: &TransitionId,
    ) -> Result<SchemaTransition> {
        let transaction = self
            .store
            .begin(IsolationLevel::SerializableSnapshot)
            .await?;
        let result = async {
            let mut view = TransactionView(&*transaction);
            let mut mutation = Mutation::with_runtime(&mut view, self.runtime.clone());
            let transition = mutation.activate_waiting_transition(id).await?;
            mutation.finish().await?;
            Ok(transition)
        }
        .await;
        self.finish_transaction(
            transaction,
            result,
            Some(EngineOperation::ActivateTransition {
                transition_id: id.clone(),
            }),
        )
        .await
    }

    /// Persist a worker failure without disguising it as successful progress.
    ///
    /// Unique-index validation is the one terminal exception: once validation
    /// has begun, an execution failure must release the finalization gate and
    /// retire the partial index through the catalog state machine.
    pub async fn record_schema_transition_error(
        &self,
        id: &TransitionId,
        owner: OwnerEpoch,
        cause: impl Into<String>,
    ) -> Result<SchemaTransition> {
        let transaction = self
            .store
            .begin(IsolationLevel::SerializableSnapshot)
            .await?;
        let cause = cause.into();
        let result = async {
            let mut view = TransactionView(&*transaction);
            let mut transition = load_transition(&mut view, id).await?;
            if transition.owner_epoch != owner {
                return Err(ownership_changed(id));
            }
            if transition.state.is_terminal() {
                return Ok(transition);
            }
            if transition.kind == TransitionKind::IndexBuild
                && transition.index.unique
                && transition.state == TransitionState::Validating
            {
                let mut mutation = Mutation::with_runtime(&mut view, self.runtime.clone());
                transition = mutation.fail_index_validation(id, owner, cause).await?;
                mutation.finish().await?;
                return Ok(transition);
            }
            transition.last_error = cause;
            transition.generation = transition.generation.next();
            transition.updated_at = self.runtime.now().into();
            store::save_transition(&mut view, &transition).await?;
            Ok(transition)
        }
        .await;
        self.finish_transaction(
            transaction,
            result,
            Some(EngineOperation::RecordTransitionError {
                transition_id: id.clone(),
            }),
        )
        .await
    }

    async fn finish_transition_step(
        &self,
        operation: EngineOperation,
        transaction: Box<dyn crate::engine::kv::Transaction>,
        result: Result<TransitionStep>,
    ) -> Result<TransitionStep> {
        let step = self
            .finish_transaction(transaction, result, Some(operation))
            .await?;
        Ok(TransitionStep {
            transition: self.inspect_schema_transition(&step.transition.id).await?,
            items: step.items,
        })
    }

    async fn stage_transition_checkpoint(
        &self,
        operation: &EngineOperation,
        transition: &SchemaTransition,
    ) {
        self.events
            .reach(EngineEvent::CheckpointStaged {
                operation: operation.clone(),
                generation: transition.generation.get(),
                batch_id: transition.batch_id,
                state: transition.state,
            })
            .await;
    }

    async fn stage_physical_batch(&self, operation: &EngineOperation, items: usize) {
        self.events
            .reach(EngineEvent::PhysicalBatchStaged {
                operation: operation.clone(),
                items,
            })
            .await;
    }

    async fn stage_finalization_gate(
        &self,
        view: &mut dyn crate::engine::kv::KvView,
        operation: &EngineOperation,
        transition: &SchemaTransition,
    ) -> Result<()> {
        self.events
            .reach(EngineEvent::FinalizationGateStaged {
                operation: operation.clone(),
                action: GateAction::Acquired,
            })
            .await;
        emit_write_protocol(self, view, operation, transition).await
    }

    async fn stage_transition_publication(
        &self,
        view: &mut dyn crate::engine::kv::KvView,
        operation: &EngineOperation,
        transition: &SchemaTransition,
        release_gate: bool,
    ) -> Result<()> {
        if release_gate {
            self.events
                .reach(EngineEvent::FinalizationGateStaged {
                    operation: operation.clone(),
                    action: GateAction::Released,
                })
                .await;
        }
        emit_write_protocol(self, view, operation, transition).await?;
        self.events
            .reach(EngineEvent::CatalogPublicationStaged {
                operation: operation.clone(),
                state: transition.state,
            })
            .await;
        Ok(())
    }
}

async fn load_transition(
    view: &mut dyn crate::engine::kv::KvView,
    id: &TransitionId,
) -> Result<SchemaTransition> {
    let mut transition = store::get_transition(view, id).await?.ok_or_else(|| {
        Error::with_reason(
            ErrorKind::InvalidInput,
            ErrorReason::SchemaTransitionNotFound,
            format!("catalog: transition {id:?} does not exist"),
        )
    })?;
    let high_water = store::delta_high_water(view, id).await?;
    transition.refresh_work_state(high_water);
    Ok(transition)
}

async fn emit_write_protocol(
    engine: &Engine,
    view: &mut dyn crate::engine::kv::KvView,
    operation: &EngineOperation,
    transition: &SchemaTransition,
) -> Result<()> {
    let table = transition_table(view, transition).await?;
    engine
        .events
        .reach(EngineEvent::WriteProtocolStaged {
            operation: operation.clone(),
            table_id: table.id,
            generation: table.write_protocol_generation.get(),
        })
        .await;
    Ok(())
}

async fn required_owned_transition(
    view: &mut dyn crate::engine::kv::KvView,
    id: &TransitionId,
    owner: OwnerEpoch,
) -> Result<SchemaTransition> {
    let transition = store::get_transition(view, id).await?.ok_or_else(|| {
        Error::with_reason(
            ErrorKind::InvalidInput,
            ErrorReason::SchemaTransitionNotFound,
            format!("catalog: transition {id:?} does not exist"),
        )
    })?;
    if transition.owner_epoch != owner {
        return Err(ownership_changed(id));
    }
    Ok(transition)
}

async fn transition_table(
    view: &dyn crate::engine::kv::KvView,
    transition: &SchemaTransition,
) -> Result<Table> {
    store::get_table_by_id(view, &transition.table_id)
        .await?
        .ok_or_else(|| {
            Error::message(
                ErrorKind::CorruptData,
                format!(
                    "catalog: transition {:?} table {:?} no longer exists",
                    transition.id, transition.table_id
                ),
            )
        })
}

fn ownership_changed(id: &TransitionId) -> Error {
    Error::message(
        ErrorKind::Conflict,
        format!("catalog: transition {id:?} ownership changed"),
    )
}
