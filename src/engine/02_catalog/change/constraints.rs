use super::admission::TransitionCandidate;
use super::transitions::{require_owner, required_table_by_id, required_transition};
use super::*;
use crate::engine::catalog::identity::{ConstraintId, OwnerEpoch, TransitionId};
use crate::engine::catalog::model::{
    Constraint, ConstraintCheck, ConstraintDef, ConstraintKind, ConstraintState,
    ConstraintValidationRequest, DataPosition, ReclamationKind, SchemaTransition, Timestamp,
    TransitionKind, TransitionState, TransitionWorkState, WriteProtocol,
};

impl Mutation<'_> {
    pub async fn start_constraint_validation(
        &mut self,
        table_id: SchemaId,
        definition: ConstraintDef,
    ) -> Result<SchemaTransition> {
        let (mut table, mut constraint, column) = self
            .declare_or_reset_constraint(table_id, &definition)
            .await?;
        let (prerequisites, waiting) = self
            .validate_transition_admission(
                &table,
                TransitionCandidate {
                    kind: TransitionKind::ConstraintValidation,
                    table_id: table.id.clone(),
                    affected_column_ids: vec![column.schema_id],
                    prerequisites: definition.prerequisites,
                },
            )
            .await?;
        let id: TransitionId = store::next_physical_id(self.view, "tr").await?.into();
        if !waiting {
            constraint.state = ConstraintState::EnforcingNewWrites;
            constraint.definition_generation = constraint.definition_generation.next();
            replace_constraint(&mut table.constraints, constraint.clone())?;
            let mut protocol = store::read_write_protocol(self.view, &table).await?;
            protocol.generation = protocol.generation.next();
            protocol.constraint_checks.push(ConstraintCheck {
                transition_id: id.clone(),
                constraint: constraint.clone(),
            });
            table.write_protocol_generation = protocol.generation;
            store::save_table(self.view, &mut table).await?;
            self.save_write_protocol(protocol).await?;
        }
        let now = self.now();
        let transition = SchemaTransition {
            id,
            kind: TransitionKind::ConstraintValidation,
            object_id: constraint.id.as_str().to_owned(),
            state: if waiting {
                TransitionState::Waiting
            } else {
                TransitionState::Building
            },
            generation: 1.into(),
            owner_epoch: OwnerEpoch::ZERO,
            source_catalog_version: store::current_revision(self.view).await?.version,
            base_position: if waiting {
                DataPosition::default()
            } else {
                self.view
                    .begin_position()
                    .map_or_else(DataPosition::default, |position| {
                        DataPosition::new(position.as_str())
                    })
            },
            barrier_position: DataPosition::default(),
            table_id: table.id.clone(),
            table_schema_id: table.schema_id,
            affected_column_ids: vec![column.schema_id],
            index: Index::default(),
            index_request: None,
            column_replacement: None,
            replacement_request: None,
            constraint: Some(constraint.clone()),
            constraint_request: Some(ConstraintValidationRequest {
                constraint_id: constraint.id,
                column_schema_id: column.schema_id,
            }),
            prerequisites,
            gate_table_ids: vec![table.id],
            cursor: Vec::new(),
            batch_id: 0,
            applied_delta: 0,
            delta_high_water: 0,
            delta_soft_limit: 0,
            delta_hard_limit: 0,
            work_state: TransitionWorkState::Normal,
            rows_scanned: 0,
            last_error: String::new(),
            created_at: now,
            updated_at: now,
            compacted_at: Timestamp::default(),
        };
        store::create_transition(self.view, &transition).await?;
        self.mark_catalog_changed();
        Ok(transition)
    }

    pub async fn begin_constraint_historical_validation(
        &mut self,
        id: &TransitionId,
        owner: OwnerEpoch,
    ) -> Result<SchemaTransition> {
        let (mut transition, mut table, protocol, position) =
            self.constraint_context(id, owner).await?;
        let mut constraint = table.constraints[position].clone();
        if transition.state != TransitionState::Building
            || constraint.state != ConstraintState::EnforcingNewWrites
        {
            return Err(input(format!(
                "catalog: constraint {:?} cannot scan from transition state {:?} and constraint state {:?}",
                constraint.name, transition.state, constraint.state
            )));
        }
        require_check(&protocol, id)?;
        constraint.state = ConstraintState::ValidatingExisting;
        constraint.definition_generation = constraint.definition_generation.next();
        table.constraints[position] = constraint.clone();
        transition.constraint = Some(constraint);
        transition.generation = transition.generation.next();
        transition.updated_at = self.now();
        store::save_table(self.view, &mut table).await?;
        store::save_transition(self.view, &transition).await?;
        self.mark_catalog_changed();
        Ok(transition)
    }

    pub async fn begin_constraint_finalization(
        &mut self,
        id: &TransitionId,
        owner: OwnerEpoch,
    ) -> Result<SchemaTransition> {
        let (mut transition, table, protocol, position) =
            self.constraint_context(id, owner).await?;
        let constraint = &table.constraints[position];
        if transition.state != TransitionState::Building
            || constraint.state != ConstraintState::ValidatingExisting
        {
            return Err(input(format!(
                "catalog: constraint {:?} cannot finalize from transition state {:?} and constraint state {:?}",
                constraint.name, transition.state, constraint.state
            )));
        }
        require_check(&protocol, id)?;
        self.acquire_schema_finalization_gates(&transition).await?;
        transition.state = TransitionState::Validating;
        transition.generation = transition.generation.next();
        transition.updated_at = self.now();
        store::save_transition(self.view, &transition).await?;
        self.mark_catalog_changed();
        Ok(transition)
    }

    pub async fn publish_constraint(
        &mut self,
        id: &TransitionId,
        owner: OwnerEpoch,
    ) -> Result<SchemaTransition> {
        let (transition, _, _, _) = self.constraint_context(id, owner).await?;
        if transition.state != TransitionState::Validating {
            return Err(input(format!(
                "catalog: constraint transition {id:?} cannot publish from state {:?}",
                transition.state
            )));
        }
        if store::first_transition_violation(self.view, id)
            .await?
            .is_some()
        {
            return Err(input(format!(
                "catalog: constraint transition {id:?} still has violations"
            )));
        }
        self.release_schema_finalization_gates(&transition).await?;
        let (mut transition, mut table, mut protocol, position) =
            self.constraint_context(id, owner).await?;
        let mut constraint = table.constraints[position].clone();
        if constraint.kind != ConstraintKind::NotNull || constraint.column_ids.len() != 1 {
            return Err(Error::message(
                ErrorKind::CatalogDrift,
                format!(
                    "catalog: constraint {:?} has invalid not-null definition",
                    constraint.name
                ),
            ));
        }
        let column_position = table
            .columns
            .iter()
            .position(|column| column.id == constraint.column_ids[0])
            .ok_or_else(|| {
                Error::message(
                    ErrorKind::CatalogDrift,
                    format!(
                        "catalog: constraint {:?} physical column is missing",
                        constraint.name
                    ),
                )
            })?;
        store::advance_column_value_fence(self.view, &table, &table.columns[column_position])
            .await?;
        table.columns[column_position].nullable = false;
        table.columns[column_position].value_generation =
            table.columns[column_position].value_generation.next();
        constraint.state = ConstraintState::Valid;
        constraint.definition_generation = constraint.definition_generation.next();
        table.constraints[position] = constraint.clone();
        protocol.generation = protocol.generation.next();
        protocol
            .constraint_checks
            .retain(|check| check.transition_id != *id);
        table.write_protocol_generation = protocol.generation;
        transition.constraint = Some(constraint);
        transition.state = TransitionState::Ready;
        transition.generation = transition.generation.next();
        transition.work_state = TransitionWorkState::Normal;
        transition.updated_at = self.now();
        store::save_table(self.view, &mut table).await?;
        self.save_write_protocol(protocol).await?;
        store::save_transition(self.view, &transition).await?;
        self.retire_constraint(&transition).await?;
        self.mark_schema_changed();
        Ok(transition)
    }

    pub async fn fail_constraint_validation(
        &mut self,
        id: &TransitionId,
        owner: OwnerEpoch,
        cause: impl Into<String>,
    ) -> Result<SchemaTransition> {
        let (transition, _, _, _) = self.constraint_context(id, owner).await?;
        self.release_schema_finalization_gates(&transition).await?;
        let (mut transition, mut table, mut protocol, position) =
            self.constraint_context(id, owner).await?;
        let mut constraint = table.constraints[position].clone();
        constraint.state = ConstraintState::Failed;
        constraint.definition_generation = constraint.definition_generation.next();
        table.constraints[position] = constraint.clone();
        protocol.generation = protocol.generation.next();
        protocol
            .constraint_checks
            .retain(|check| check.transition_id != *id);
        table.write_protocol_generation = protocol.generation;
        transition.constraint = Some(constraint);
        transition.state = TransitionState::Failed;
        transition.generation = transition.generation.next();
        transition.owner_epoch = transition.owner_epoch.next();
        transition.work_state = TransitionWorkState::Normal;
        transition.last_error = cause.into();
        transition.updated_at = self.now();
        store::save_table(self.view, &mut table).await?;
        self.save_write_protocol(protocol).await?;
        store::save_transition(self.view, &transition).await?;
        self.retire_constraint(&transition).await?;
        self.mark_catalog_changed();
        Ok(transition)
    }

    pub async fn cancel_constraint_validation(
        &mut self,
        id: &TransitionId,
    ) -> Result<SchemaTransition> {
        let mut transition = required_transition(self.view, id).await?;
        if transition.kind != TransitionKind::ConstraintValidation {
            return Err(input(format!(
                "catalog: constraint validation transition {id:?} does not exist"
            )));
        }
        match transition.state {
            TransitionState::Cancelled => return Ok(transition),
            TransitionState::Ready => {
                return Err(input(format!(
                    "catalog: valid constraint transition {id:?} is not cancellable"
                )));
            }
            TransitionState::Failed => {
                return Err(input(format!(
                    "catalog: failed constraint transition {id:?} is already cleaning up"
                )));
            }
            _ => {}
        }
        if transition.state != TransitionState::Waiting {
            self.release_schema_finalization_gates(&transition).await?;
        }
        let mut table = required_table_by_id(self.view, &transition.table_id).await?;
        let constraint_id = transition
            .constraint
            .as_ref()
            .ok_or_else(|| {
                Error::message(
                    ErrorKind::CatalogDrift,
                    "catalog: constraint transition lacks definition",
                )
            })?
            .id
            .clone();
        let position = table
            .constraints
            .iter()
            .position(|constraint| constraint.id == constraint_id)
            .ok_or_else(|| {
                Error::message(
                    ErrorKind::CatalogDrift,
                    "catalog: constraint transition definition is missing",
                )
            })?;
        let mut constraint = table.constraints[position].clone();
        constraint.state = ConstraintState::Cancelled;
        constraint.definition_generation = constraint.definition_generation.next();
        table.constraints[position] = constraint.clone();
        if transition.state != TransitionState::Waiting {
            let mut protocol = store::read_write_protocol(self.view, &table).await?;
            protocol.generation = protocol.generation.next();
            protocol
                .constraint_checks
                .retain(|check| check.transition_id != *id);
            table.write_protocol_generation = protocol.generation;
            self.save_write_protocol(protocol).await?;
        }
        transition.constraint = Some(constraint);
        transition.state = TransitionState::Cancelled;
        transition.generation = transition.generation.next();
        transition.owner_epoch = transition.owner_epoch.next();
        transition.updated_at = self.now();
        store::save_table(self.view, &mut table).await?;
        store::save_transition(self.view, &transition).await?;
        self.retire_constraint(&transition).await?;
        self.mark_catalog_changed();
        Ok(transition)
    }

    async fn declare_or_reset_constraint(
        &mut self,
        table_id: SchemaId,
        definition: &ConstraintDef,
    ) -> Result<(Table, Constraint, Column)> {
        let mut table = self.table_by_schema_id(table_id).await?;
        if definition.name.is_empty() {
            return Err(input("catalog: constraint name is required"));
        }
        if definition.kind != ConstraintKind::NotNull {
            return Err(input("catalog: unsupported constraint kind"));
        }
        let column = table
            .columns
            .iter()
            .find(|column| column.schema_id == definition.column_id)
            .cloned()
            .ok_or_else(|| {
                input(format!(
                    "catalog: column schema ID {} does not exist in table {:?}",
                    definition.column_id, table.name
                ))
            })?;
        if !column.nullable {
            return Err(input(format!(
                "catalog: column {:?} on table {:?} is already not nullable",
                column.name, table.name
            )));
        }
        let constraint = if let Some(position) = table
            .constraints
            .iter()
            .position(|constraint| constraint.name == definition.name)
        {
            let mut constraint = table.constraints[position].clone();
            if constraint.kind != definition.kind
                || !matches!(
                    constraint.state,
                    ConstraintState::Failed | ConstraintState::Cancelled
                )
            {
                return Err(input(format!(
                    "catalog: constraint {:?} already exists on table {:?}",
                    definition.name, table.name
                )));
            }
            constraint.state = ConstraintState::Declared;
            constraint.definition_generation = constraint.definition_generation.next();
            constraint.column_ids = vec![column.id.clone()];
            table.constraints[position] = constraint.clone();
            constraint
        } else {
            let constraint = Constraint {
                id: ConstraintId::from(store::next_physical_id(self.view, "ct").await?),
                definition_generation: 1.into(),
                name: definition.name.clone(),
                kind: definition.kind,
                state: ConstraintState::Declared,
                column_ids: vec![column.id.clone()],
            };
            table.constraints.push(constraint.clone());
            constraint
        };
        store::save_table(self.view, &mut table).await?;
        self.mark_catalog_changed();
        Ok((table, constraint, column))
    }

    async fn constraint_context(
        &mut self,
        id: &TransitionId,
        owner: OwnerEpoch,
    ) -> Result<(SchemaTransition, Table, WriteProtocol, usize)> {
        let transition = required_transition(self.view, id).await?;
        if transition.kind != TransitionKind::ConstraintValidation
            || transition.constraint.is_none()
        {
            return Err(input(format!(
                "catalog: constraint validation transition {id:?} does not exist"
            )));
        }
        require_owner(&transition, owner)?;
        let table = required_table_by_id(self.view, &transition.table_id).await?;
        let constraint_id = &transition.constraint.as_ref().expect("checked").id;
        let position = table
            .constraints
            .iter()
            .position(|constraint| &constraint.id == constraint_id)
            .ok_or_else(|| {
                Error::message(
                    ErrorKind::CatalogDrift,
                    format!("catalog: constraint transition {id:?} definition is missing"),
                )
            })?;
        let protocol = store::read_write_protocol(self.view, &table).await?;
        Ok((transition, table, protocol, position))
    }

    pub(super) async fn retire_constraint(&mut self, transition: &SchemaTransition) -> Result<()> {
        let mut reclamation = self
            .pending_reclamation(
                store::constraint_validation_reclamation_id(&transition.id),
                ReclamationKind::ConstraintValidation,
            )
            .await?;
        reclamation.table_id = transition.table_id.clone();
        reclamation.table_schema_id = Some(transition.table_schema_id);
        reclamation.transition_id = transition.id.clone();
        self.queue_reclamation(reclamation).await
    }
}

fn replace_constraint(constraints: &mut [Constraint], replacement: Constraint) -> Result<()> {
    let current = constraints
        .iter_mut()
        .find(|constraint| constraint.id == replacement.id)
        .ok_or_else(|| {
            Error::message(
                ErrorKind::CatalogDrift,
                "catalog: declared constraint disappeared",
            )
        })?;
    *current = replacement;
    Ok(())
}

fn require_check(protocol: &WriteProtocol, id: &TransitionId) -> Result<()> {
    if protocol
        .constraint_checks
        .iter()
        .any(|check| check.transition_id == *id)
    {
        Ok(())
    } else {
        Err(Error::message(
            ErrorKind::CatalogDrift,
            format!("catalog: constraint transition {id:?} has no foreground write obligation"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::catalog::model::{ColumnDraft, ScalarType, TableDraft};
    use crate::engine::kv::slatedb;
    use crate::engine::kv::{IsolationLevel, TransactionView, TransactionalKv};
    use std::sync::Arc;

    #[tokio::test]
    async fn not_null_enforces_writes_before_scan_and_publishes_after_gate() {
        let database = Arc::new(slatedb::Store::memory("catalog-constraint").await.unwrap());
        let service = Service::new(database.clone());
        service
            .create_table(TableDraft {
                id: Some(SchemaId::new(1).unwrap()),
                name: "items".into(),
                columns: vec![
                    ColumnDraft {
                        id: Some(SchemaId::new(1).unwrap()),
                        name: "id".into(),
                        scalar_type: ScalarType::Int64,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                    ColumnDraft {
                        id: Some(SchemaId::new(2).unwrap()),
                        name: "value".into(),
                        scalar_type: ScalarType::Text,
                        nullable: true,
                        format: String::new(),
                        default: None,
                    },
                ],
                primary_key: vec!["id".into()],
                indexes: Vec::new(),
                foreign_keys: Vec::new(),
            })
            .await
            .unwrap();
        let mut transaction = database
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        let ready = {
            let mut view = TransactionView(transaction.as_mut());
            let mut mutation = Mutation::new(&mut view);
            let mut transition = mutation
                .start_constraint_validation(
                    SchemaId::new(1).unwrap(),
                    ConstraintDef {
                        name: "items_value_not_null".into(),
                        kind: ConstraintKind::NotNull,
                        column_id: SchemaId::new(2).unwrap(),
                        prerequisites: Vec::new(),
                    },
                )
                .await
                .unwrap();
            transition.owner_epoch = 1.into();
            store::save_transition(mutation.view, &transition)
                .await
                .unwrap();
            mutation
                .begin_constraint_historical_validation(&transition.id, transition.owner_epoch)
                .await
                .unwrap();
            mutation
                .begin_constraint_finalization(&transition.id, transition.owner_epoch)
                .await
                .unwrap();
            mutation
                .publish_constraint(&transition.id, transition.owner_epoch)
                .await
                .unwrap()
        };
        transaction.commit().await.unwrap();
        assert_eq!(ready.state, TransitionState::Ready);
        assert!(
            !service
                .get_table("items")
                .await
                .unwrap()
                .unwrap()
                .column("value")
                .unwrap()
                .nullable
        );
        database.close().await.unwrap();
    }
}
