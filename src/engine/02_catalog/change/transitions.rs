use super::admission::{TransitionCandidate, index_column_schema_ids};
use super::*;
use crate::engine::catalog::identity::{OwnerEpoch, TransitionId};
use crate::engine::catalog::model::{
    DEFAULT_DELTA_HARD_LIMIT, DEFAULT_DELTA_SOFT_LIMIT, DataPosition, IndexBuildRequest,
    IndexDeltaSink, ReclamationKind, SchemaFinalizationGate, SchemaTransition, Timestamp,
    TransitionKind, TransitionState, TransitionWorkState,
};

impl Mutation<'_> {
    pub(super) async fn acquire_schema_finalization_gates(
        &mut self,
        transition: &SchemaTransition,
    ) -> Result<()> {
        let mut table_ids = transition.gate_table_ids.clone();
        table_ids.sort();
        table_ids.dedup();
        for table_id in table_ids {
            let mut table = required_table_by_id(self.view, &table_id).await?;
            let mut protocol = store::read_write_protocol(self.view, &table).await?;
            let already_owned = protocol.finalization_gate.as_ref() == Some(&gate(transition));
            acquire_gate(&mut protocol, transition)?;
            if already_owned {
                continue;
            }
            table.write_protocol_generation = protocol.generation;
            store::save_table(self.view, &mut table).await?;
            self.save_write_protocol(protocol).await?;
        }
        Ok(())
    }

    pub(super) async fn release_schema_finalization_gates(
        &mut self,
        transition: &SchemaTransition,
    ) -> Result<()> {
        let mut table_ids = transition.gate_table_ids.clone();
        table_ids.sort();
        table_ids.dedup();
        for table_id in table_ids {
            let mut table = required_table_by_id(self.view, &table_id).await?;
            let mut protocol = store::read_write_protocol(self.view, &table).await?;
            let owned = protocol.finalization_gate.as_ref() == Some(&gate(transition));
            remove_owned_gate(&mut protocol, transition)?;
            if owned {
                protocol.generation = protocol.generation.next();
                table.write_protocol_generation = protocol.generation;
                store::save_table(self.view, &mut table).await?;
                self.save_write_protocol(protocol).await?;
            }
        }
        Ok(())
    }

    pub async fn get_transition(&mut self, id: &TransitionId) -> Result<SchemaTransition> {
        let mut transition = store::get_transition(self.view, id).await?.ok_or_else(|| {
            Error::message(
                ErrorKind::NotFound,
                format!("catalog: transition {id:?} does not exist"),
            )
        })?;
        let high_water = store::delta_high_water(self.view, id).await?;
        transition.refresh_work_state(high_water);
        Ok(transition)
    }

    pub async fn start_index_build(
        &mut self,
        table_id: SchemaId,
        definition: IndexDef,
    ) -> Result<SchemaTransition> {
        self.start_index_build_with_limits_and_prerequisites(
            table_id,
            definition,
            Vec::new(),
            DEFAULT_DELTA_SOFT_LIMIT,
            DEFAULT_DELTA_HARD_LIMIT,
        )
        .await
    }

    pub async fn start_index_build_with_prerequisites(
        &mut self,
        table_id: SchemaId,
        definition: IndexDef,
        prerequisites: Vec<TransitionId>,
    ) -> Result<SchemaTransition> {
        self.start_index_build_with_limits_and_prerequisites(
            table_id,
            definition,
            prerequisites,
            DEFAULT_DELTA_SOFT_LIMIT,
            DEFAULT_DELTA_HARD_LIMIT,
        )
        .await
    }

    async fn start_index_build_with_limits_and_prerequisites(
        &mut self,
        table_id: SchemaId,
        definition: IndexDef,
        prerequisites: Vec<TransitionId>,
        soft_limit: u64,
        hard_limit: u64,
    ) -> Result<SchemaTransition> {
        if hard_limit > 0 && soft_limit > hard_limit {
            return Err(input(format!(
                "catalog: delta soft limit {soft_limit} exceeds hard limit {hard_limit}"
            )));
        }
        let mut table = self.table_by_schema_id(table_id).await?;
        self.ensure_index_name_available(&table, &definition.name)
            .await?;
        let affected_column_ids = index_column_schema_ids(
            &table,
            &Index {
                name: definition.name.clone(),
                columns: definition.columns.clone(),
                unique: definition.unique,
                ..Index::default()
            },
        )?;
        let (prerequisites, waiting) = self
            .validate_transition_admission(
                &table,
                TransitionCandidate {
                    kind: TransitionKind::IndexBuild,
                    table_id: table.id.clone(),
                    affected_column_ids: affected_column_ids.clone(),
                    prerequisites,
                },
            )
            .await?;
        let request = new_index_build_request(self.view, &table, definition).await?;
        let transition_id: TransitionId = store::next_physical_id(self.view, "tr").await?.into();
        let revision = store::current_revision(self.view).await?;
        let now = self.now();
        let mut index = Index {
            id: request.physical_id.clone(),
            logical_id: request.logical_id.clone(),
            definition_generation: DefinitionGeneration::from(1),
            access_generation: AccessGeneration::ZERO,
            state: IndexState::Building,
            name: request.name.clone(),
            columns: Vec::new(),
            column_ids: Vec::new(),
            unique: request.unique,
        };
        let mut transition = SchemaTransition {
            id: transition_id.clone(),
            kind: TransitionKind::IndexBuild,
            object_id: index.logical_id.as_str().to_owned(),
            state: TransitionState::Waiting,
            generation: 1.into(),
            owner_epoch: OwnerEpoch::ZERO,
            source_catalog_version: revision.version,
            base_position: DataPosition::default(),
            barrier_position: DataPosition::default(),
            table_id: table.id.clone(),
            table_schema_id: table.schema_id,
            affected_column_ids,
            index: index.clone(),
            index_request: Some(request.clone()),
            column_replacement: None,
            replacement_request: None,
            constraint: None,
            constraint_request: None,
            prerequisites,
            gate_table_ids: vec![table.id.clone()],
            cursor: Vec::new(),
            batch_id: 0,
            applied_delta: 0,
            delta_high_water: 0,
            delta_soft_limit: soft_limit,
            delta_hard_limit: hard_limit,
            work_state: TransitionWorkState::Normal,
            rows_scanned: 0,
            last_error: String::new(),
            created_at: now,
            updated_at: now,
            compacted_at: Timestamp::default(),
        };
        if !waiting {
            index = materialize_index_build(&table, &request, IndexState::Building)?;
            let mut protocol = store::read_write_protocol(self.view, &table).await?;
            protocol.generation = protocol.generation.next();
            protocol.delta_sinks.push(IndexDeltaSink {
                transition_id: transition_id.clone(),
                index: index.clone(),
                columns: index.columns.clone(),
                delta_hard_limit: hard_limit,
            });
            table.write_protocol_generation = protocol.generation;
            table.indexes.push(index.clone());
            store::save_table(self.view, &mut table).await?;
            self.save_write_protocol(protocol).await?;
            transition.state = TransitionState::Building;
            transition.index = index;
            if let Some(position) = self.view.begin_position() {
                transition.base_position = DataPosition::new(position.as_str());
            }
        }
        store::create_transition(self.view, &transition).await?;
        self.mark_catalog_changed();
        Ok(transition)
    }

    pub async fn begin_index_validation(
        &mut self,
        id: &TransitionId,
        owner_epoch: OwnerEpoch,
    ) -> Result<SchemaTransition> {
        let mut transition = required_transition(self.view, id).await?;
        require_owner(&transition, owner_epoch)?;
        if !transition.index.unique || transition.state != TransitionState::CatchingUp {
            return Err(input(format!(
                "catalog: transition {id:?} cannot begin unique validation from state {:?}",
                transition.state
            )));
        }
        self.acquire_schema_finalization_gates(&transition).await?;
        let mut table = required_table_by_id(self.view, &transition.table_id).await?;
        let position = table
            .indexes
            .iter()
            .position(|index| index.logical_id == transition.index.logical_id)
            .ok_or_else(|| {
                Error::message(
                    ErrorKind::CatalogDrift,
                    format!("catalog: transition {id:?} index definition is missing"),
                )
            })?;
        table.indexes[position].state = IndexState::Validating;
        table.indexes[position].definition_generation =
            table.indexes[position].definition_generation.next();
        transition.index = table.indexes[position].clone();
        transition.state = TransitionState::Validating;
        transition.generation = transition.generation.next();
        transition.updated_at = self.now();
        store::save_table(self.view, &mut table).await?;
        store::save_transition(self.view, &transition).await?;
        self.mark_catalog_changed();
        Ok(transition)
    }

    pub async fn publish_index_ready(
        &mut self,
        id: &TransitionId,
        owner_epoch: OwnerEpoch,
    ) -> Result<SchemaTransition> {
        let mut transition = required_transition(self.view, id).await?;
        require_owner(&transition, owner_epoch)?;
        if transition.state != TransitionState::Validating {
            return Err(Error::message(
                ErrorKind::CatalogDrift,
                format!("catalog: transition {id:?} cannot publish ready before validation"),
            ));
        }
        self.release_schema_finalization_gates(&transition).await?;
        transition = required_transition(self.view, id).await?;
        let mut table = required_table_by_id(self.view, &transition.table_id).await?;
        let position = table
            .indexes
            .iter()
            .position(|index| index.logical_id == transition.index.logical_id)
            .ok_or_else(|| {
                input(format!(
                    "catalog: transition {id:?} index definition is missing"
                ))
            })?;
        table.indexes[position].state = IndexState::Ready;
        table.indexes[position].definition_generation =
            table.indexes[position].definition_generation.next();
        transition.index = table.indexes[position].clone();
        let mut protocol = store::read_write_protocol(self.view, &table).await?;
        protocol.generation = protocol.generation.next();
        protocol.ready_indexes.push(transition.index.clone());
        protocol
            .delta_sinks
            .retain(|sink| sink.transition_id != *id);
        table.write_protocol_generation = protocol.generation;
        store::save_table(self.view, &mut table).await?;
        self.save_write_protocol(protocol).await?;
        transition.state = TransitionState::Ready;
        transition.work_state = TransitionWorkState::Normal;
        transition.generation = transition.generation.next();
        transition.updated_at = self.now();
        store::save_transition(self.view, &transition).await?;
        self.retire_index_transition(
            &transition,
            ReclamationKind::TransitionDeltas,
            store::transition_delta_reclamation_id(id),
        )
        .await?;
        self.mark_schema_changed();
        Ok(transition)
    }

    pub async fn fail_index_validation(
        &mut self,
        id: &TransitionId,
        owner_epoch: OwnerEpoch,
        cause: impl Into<String>,
    ) -> Result<SchemaTransition> {
        let mut transition = required_transition(self.view, id).await?;
        require_owner(&transition, owner_epoch)?;
        if transition.state != TransitionState::Validating {
            return Err(input(format!(
                "catalog: transition {id:?} cannot fail validation from state {:?}",
                transition.state
            )));
        }
        self.release_schema_finalization_gates(&transition).await?;
        transition = required_transition(self.view, id).await?;
        let mut table = required_table_by_id(self.view, &transition.table_id).await?;
        let mut protocol = store::read_write_protocol(self.view, &table).await?;
        protocol.generation = protocol.generation.next();
        protocol
            .delta_sinks
            .retain(|sink| sink.transition_id != *id);
        table
            .indexes
            .retain(|index| index.logical_id != transition.index.logical_id);
        table.write_protocol_generation = protocol.generation;
        store::save_table(self.view, &mut table).await?;
        self.save_write_protocol(protocol).await?;
        transition.index.state = IndexState::Failed;
        transition.index.definition_generation = transition.index.definition_generation.next();
        transition.state = TransitionState::Failed;
        transition.work_state = TransitionWorkState::Normal;
        transition.last_error = cause.into();
        transition.owner_epoch = transition.owner_epoch.next();
        transition.generation = transition.generation.next();
        transition.updated_at = self.now();
        store::save_transition(self.view, &transition).await?;
        self.retire_index_transition(
            &transition,
            ReclamationKind::FailedIndex,
            store::failed_index_reclamation_id(id),
        )
        .await?;
        self.mark_catalog_changed();
        Ok(transition)
    }

    pub async fn cancel_schema_transition(
        &mut self,
        id: &TransitionId,
    ) -> Result<SchemaTransition> {
        let mut transition = required_transition(self.view, id).await?;
        if transition.kind == TransitionKind::ColumnReplacement {
            return self.cancel_column_replacement(id).await;
        }
        if transition.kind == TransitionKind::ConstraintValidation {
            return self.cancel_constraint_validation(id).await;
        }
        if transition.kind != TransitionKind::IndexBuild {
            return Err(input(format!(
                "catalog: transition {id:?} of kind {:?} does not support cancellation yet",
                transition.kind
            )));
        }
        match transition.state {
            TransitionState::Ready => {
                return Err(input(format!(
                    "catalog: ready transition {id:?} is not cancellable; delete its index"
                )));
            }
            TransitionState::Cancelled => return Ok(transition),
            TransitionState::Failed => {
                return Err(input(format!(
                    "catalog: failed transition {id:?} requires cleanup, not cancellation"
                )));
            }
            TransitionState::Waiting => {
                transition.state = TransitionState::Cancelled;
                transition.index.state = IndexState::Cancelled;
            }
            _ => {
                self.release_schema_finalization_gates(&transition).await?;
                transition = required_transition(self.view, id).await?;
                let mut table = required_table_by_id(self.view, &transition.table_id).await?;
                let mut protocol = store::read_write_protocol(self.view, &table).await?;
                protocol.generation = protocol.generation.next();
                protocol
                    .delta_sinks
                    .retain(|sink| sink.transition_id != *id);
                table
                    .indexes
                    .retain(|index| index.logical_id != transition.index.logical_id);
                table.write_protocol_generation = protocol.generation;
                store::save_table(self.view, &mut table).await?;
                self.save_write_protocol(protocol).await?;
                transition.state = TransitionState::Cancelled;
                transition.index.state = IndexState::Cancelled;
                self.retire_index_transition(
                    &transition,
                    ReclamationKind::CancelledIndex,
                    store::cancelled_index_reclamation_id(id),
                )
                .await?;
            }
        }
        transition.generation = transition.generation.next();
        transition.owner_epoch = transition.owner_epoch.next();
        transition.updated_at = self.now();
        store::save_transition(self.view, &transition).await?;
        self.mark_catalog_changed();
        Ok(transition)
    }

    pub async fn activate_waiting_transition(
        &mut self,
        id: &TransitionId,
    ) -> Result<SchemaTransition> {
        let mut transition = required_transition(self.view, id).await?;
        if transition.state != TransitionState::Waiting {
            return Ok(transition);
        }
        for prerequisite_id in transition.prerequisites.clone() {
            let Some(prerequisite) = store::get_transition(self.view, &prerequisite_id).await?
            else {
                return self
                    .fail_waiting_transition(
                        transition,
                        format!("prerequisite transition {prerequisite_id:?} is missing"),
                    )
                    .await;
            };
            match prerequisite.state {
                TransitionState::Ready => {}
                TransitionState::Failed | TransitionState::Cancelled => {
                    return self
                        .fail_waiting_transition(
                            transition,
                            format!(
                                "prerequisite transition {prerequisite_id:?} completed in state {:?}",
                                prerequisite.state
                            ),
                        )
                        .await;
                }
                _ => return Ok(transition),
            }
        }
        let mut table = match store::get_table_by_id(self.view, &transition.table_id).await? {
            Some(table) => table,
            None => {
                return self
                    .fail_waiting_transition(transition, "target table was deleted".into())
                    .await;
            }
        };
        match transition.kind {
            TransitionKind::IndexBuild => {
                let request = transition.index_request.clone().ok_or_else(|| {
                    Error::message(
                        ErrorKind::CatalogDrift,
                        format!("catalog: waiting index transition {id:?} has no logical request"),
                    )
                })?;
                if table.index(&request.name).is_some() {
                    return self
                        .fail_waiting_transition(
                            transition,
                            format!("index name {:?} was claimed", request.name),
                        )
                        .await;
                }
                let index = match materialize_index_build(&table, &request, IndexState::Building) {
                    Ok(index) => index,
                    Err(error) => {
                        return self
                            .fail_waiting_transition(transition, error.to_string())
                            .await;
                    }
                };
                let mut protocol = store::read_write_protocol(self.view, &table).await?;
                protocol.generation = protocol.generation.next();
                protocol.delta_sinks.push(IndexDeltaSink {
                    transition_id: id.clone(),
                    index: index.clone(),
                    columns: index.columns.clone(),
                    delta_hard_limit: transition.delta_hard_limit,
                });
                table.write_protocol_generation = protocol.generation;
                table.indexes.push(index.clone());
                store::save_table(self.view, &mut table).await?;
                self.save_write_protocol(protocol).await?;
                transition.index = index;
            }
            TransitionKind::ColumnReplacement => {
                let request = transition.replacement_request.clone().ok_or_else(|| {
                    Error::message(
                        ErrorKind::CatalogDrift,
                        format!(
                            "catalog: waiting replacement transition {id:?} has no logical request"
                        ),
                    )
                })?;
                let Some(source) = table
                    .columns
                    .iter()
                    .find(|column| column.schema_id == request.column_schema_id)
                    .cloned()
                else {
                    return self
                        .fail_waiting_transition(
                            transition,
                            format!("logical column {} was deleted", request.column_schema_id),
                        )
                        .await;
                };
                if let Err(error) =
                    replacements::validate_dependencies(&table, &source, request.nullable)
                {
                    return self
                        .fail_waiting_transition(transition, error.to_string())
                        .await;
                }
                if source.scalar_type == request.scalar_type
                    && source.nullable == request.nullable
                    && source.format == request.format
                    && source.insert_default == request.default
                {
                    transition.state = TransitionState::Ready;
                    transition.generation = transition.generation.next();
                    transition.updated_at = self.now();
                    store::save_transition(self.view, &transition).await?;
                    self.mark_catalog_changed();
                    return Ok(transition);
                }
                let definition = crate::engine::catalog::model::ColumnReplacementDef {
                    scalar_type: request.scalar_type,
                    nullable: request.nullable,
                    format: request.format,
                    default: request.default,
                    conversion: request.conversion,
                    prerequisites: Vec::new(),
                };
                let target = replacements::build_target(self.view, &source, &definition).await?;
                let replacement = crate::engine::catalog::model::ColumnReplacement {
                    source,
                    target,
                    conversion: definition.conversion,
                };
                let mut protocol = store::read_write_protocol(self.view, &table).await?;
                protocol.generation = protocol.generation.next();
                protocol.column_replacements.push(
                    crate::engine::catalog::model::ColumnReplacementWrite {
                        transition_id: id.clone(),
                        replacement: replacement.clone(),
                    },
                );
                table.write_protocol_generation = protocol.generation;
                store::save_table(self.view, &mut table).await?;
                self.save_write_protocol(protocol).await?;
                transition.column_replacement = Some(replacement);
            }
            TransitionKind::ConstraintValidation => {
                let request = transition.constraint_request.clone().ok_or_else(|| {
                    Error::message(
                        ErrorKind::CatalogDrift,
                        format!(
                            "catalog: waiting constraint transition {id:?} has no logical request"
                        ),
                    )
                })?;
                let Some(position) = table
                    .constraints
                    .iter()
                    .position(|constraint| constraint.id == request.constraint_id)
                else {
                    return self
                        .fail_waiting_transition(
                            transition,
                            "declared constraint was deleted".into(),
                        )
                        .await;
                };
                let Some(column) = table
                    .columns
                    .iter()
                    .find(|column| column.schema_id == request.column_schema_id)
                    .cloned()
                else {
                    return self
                        .fail_waiting_transition(
                            transition,
                            format!("logical column {} was deleted", request.column_schema_id),
                        )
                        .await;
                };
                let mut constraint = table.constraints[position].clone();
                constraint.column_ids = vec![column.id];
                constraint.definition_generation = constraint.definition_generation.next();
                if !column.nullable {
                    constraint.state = crate::engine::catalog::model::ConstraintState::Valid;
                    table.constraints[position] = constraint.clone();
                    transition.constraint = Some(constraint);
                    transition.state = TransitionState::Ready;
                    transition.generation = transition.generation.next();
                    transition.updated_at = self.now();
                    store::save_table(self.view, &mut table).await?;
                    store::save_transition(self.view, &transition).await?;
                    self.retire_constraint(&transition).await?;
                    self.mark_catalog_changed();
                    return Ok(transition);
                }
                constraint.state =
                    crate::engine::catalog::model::ConstraintState::EnforcingNewWrites;
                table.constraints[position] = constraint.clone();
                let mut protocol = store::read_write_protocol(self.view, &table).await?;
                protocol.generation = protocol.generation.next();
                protocol
                    .constraint_checks
                    .push(crate::engine::catalog::model::ConstraintCheck {
                        transition_id: id.clone(),
                        constraint: constraint.clone(),
                    });
                table.write_protocol_generation = protocol.generation;
                store::save_table(self.view, &mut table).await?;
                self.save_write_protocol(protocol).await?;
                transition.constraint = Some(constraint);
            }
        }
        transition.source_catalog_version = store::current_revision(self.view).await?.version;
        if let Some(position) = self.view.begin_position() {
            transition.base_position = DataPosition::new(position.as_str());
        }
        transition.state = TransitionState::Building;
        transition.generation = transition.generation.next();
        transition.updated_at = self.now();
        store::save_transition(self.view, &transition).await?;
        self.mark_catalog_changed();
        Ok(transition)
    }

    async fn fail_waiting_transition(
        &mut self,
        mut transition: SchemaTransition,
        cause: String,
    ) -> Result<SchemaTransition> {
        transition.state = TransitionState::Failed;
        if transition.kind == TransitionKind::IndexBuild {
            transition.index.state = IndexState::Failed;
        }
        if transition.kind == TransitionKind::ConstraintValidation
            && let Some(constraint) = &mut transition.constraint
        {
            constraint.state = crate::engine::catalog::model::ConstraintState::Failed;
            constraint.definition_generation = constraint.definition_generation.next();
            if let Some(mut table) = store::get_table_by_id(self.view, &transition.table_id).await?
                && let Some(position) = table
                    .constraints
                    .iter()
                    .position(|candidate| candidate.id == constraint.id)
            {
                table.constraints[position] = constraint.clone();
                store::save_table(self.view, &mut table).await?;
            }
        }
        transition.generation = transition.generation.next();
        transition.owner_epoch = transition.owner_epoch.next();
        transition.last_error = cause;
        transition.updated_at = self.now();
        store::save_transition(self.view, &transition).await?;
        if transition.kind == TransitionKind::ConstraintValidation {
            self.retire_constraint(&transition).await?;
        }
        self.mark_catalog_changed();
        Ok(transition)
    }

    async fn retire_index_transition(
        &mut self,
        transition: &SchemaTransition,
        kind: ReclamationKind,
        reclamation_id: ReclamationId,
    ) -> Result<()> {
        let mut reclamation = self.pending_reclamation(reclamation_id, kind).await?;
        reclamation.table_id = transition.table_id.clone();
        reclamation.table_schema_id = Some(transition.table_schema_id);
        reclamation.index_id = transition.index.id.clone();
        reclamation.transition_id = transition.id.clone();
        self.queue_reclamation(reclamation).await
    }
}

impl Service {
    pub async fn get_transition(&self, id: &TransitionId) -> Result<Option<SchemaTransition>> {
        read_snapshot!(self, |mut view| async {
            match store::get_transition(&mut view, id).await? {
                Some(mut transition) => {
                    let high_water = store::delta_high_water(&mut view, id).await?;
                    transition.refresh_work_state(high_water);
                    Ok(Some(transition))
                }
                None => Ok(None),
            }
        })
    }

    pub async fn list_transitions(&self) -> Result<Vec<SchemaTransition>> {
        read_snapshot!(self, |mut view| async {
            let mut transitions = store::list_transitions(&mut view).await?;
            for transition in &mut transitions {
                let high_water = store::delta_high_water(&mut view, &transition.id).await?;
                transition.refresh_work_state(high_water);
            }
            Ok(transitions)
        })
    }

    pub async fn cancel_schema_transition(&self, id: &TransitionId) -> Result<SchemaTransition> {
        run_mutation!(self, |mutation| mutation.cancel_schema_transition(id))
    }
}

async fn new_index_build_request(
    view: &mut dyn KvView,
    table: &Table,
    definition: IndexDef,
) -> Result<IndexBuildRequest> {
    if definition.columns.is_empty() {
        return Err(input(format!(
            "catalog: index {:?} has no columns",
            definition.name
        )));
    }
    let mut column_schema_ids = Vec::with_capacity(definition.columns.len());
    for name in &definition.columns {
        column_schema_ids.push(
            table
                .column(name)
                .ok_or_else(|| {
                    input(format!(
                        "catalog: index {:?} references unknown column {name:?}",
                        definition.name
                    ))
                })?
                .schema_id,
        );
    }
    Ok(IndexBuildRequest {
        physical_id: store::next_physical_id(view, "i").await?.into(),
        logical_id: store::next_physical_id(view, "ix").await?.into(),
        name: definition.name,
        column_schema_ids,
        unique: definition.unique,
    })
}

fn materialize_index_build(
    table: &Table,
    request: &IndexBuildRequest,
    state: IndexState,
) -> Result<Index> {
    let mut columns = Vec::with_capacity(request.column_schema_ids.len());
    let mut column_ids = Vec::with_capacity(request.column_schema_ids.len());
    for schema_id in &request.column_schema_ids {
        let column = table
            .columns
            .iter()
            .find(|column| column.schema_id == *schema_id)
            .ok_or_else(|| {
                input(format!(
                    "catalog: index {:?} logical column {schema_id} no longer exists on table {:?}",
                    request.name, table.name
                ))
            })?;
        columns.push(column.name.clone());
        column_ids.push(column.id.clone());
    }
    Ok(Index {
        id: request.physical_id.clone(),
        logical_id: request.logical_id.clone(),
        definition_generation: 1.into(),
        access_generation: AccessGeneration::ZERO,
        state,
        name: request.name.clone(),
        columns,
        column_ids,
        unique: request.unique,
    })
}

pub(super) async fn required_transition(
    view: &mut dyn KvView,
    id: &TransitionId,
) -> Result<SchemaTransition> {
    store::get_transition(view, id)
        .await?
        .ok_or_else(|| input(format!("catalog: transition {id:?} does not exist")))
}

pub(super) async fn required_table_by_id(view: &mut dyn KvView, id: &TableId) -> Result<Table> {
    store::get_table_by_id(view, id)
        .await?
        .ok_or_else(|| input(format!("catalog: transition table {id:?} no longer exists")))
}

pub(super) fn require_owner(transition: &SchemaTransition, owner: OwnerEpoch) -> Result<()> {
    if transition.owner_epoch == owner {
        Ok(())
    } else {
        Err(Error::message(
            ErrorKind::Conflict,
            format!("catalog: transition {:?} ownership changed", transition.id),
        ))
    }
}

fn gate(transition: &SchemaTransition) -> SchemaFinalizationGate {
    SchemaFinalizationGate {
        transition_id: transition.id.clone(),
        object_id: transition.object_id.clone(),
        kind: transition.kind,
    }
}

pub(super) fn acquire_gate(
    protocol: &mut WriteProtocol,
    transition: &SchemaTransition,
) -> Result<()> {
    let expected = gate(transition);
    if let Some(existing) = &protocol.finalization_gate {
        if existing == &expected {
            return Ok(());
        }
        if existing.transition_id == transition.id {
            return Err(Error::message(
                ErrorKind::CatalogDrift,
                format!(
                    "catalog: finalization gate for transition {:?} has different contents",
                    transition.id
                ),
            ));
        }
        return Err(Error::message(
            ErrorKind::Conflict,
            format!(
                "catalog: table is already gated by transition {:?}",
                existing.transition_id
            ),
        ));
    }
    protocol.generation = protocol.generation.next();
    protocol.finalization_gate = Some(expected);
    Ok(())
}

pub(super) fn remove_owned_gate(
    protocol: &mut WriteProtocol,
    transition: &SchemaTransition,
) -> Result<()> {
    match &protocol.finalization_gate {
        Some(existing) if existing == &gate(transition) => protocol.finalization_gate = None,
        Some(existing) if existing.transition_id == transition.id => {
            return Err(Error::message(
                ErrorKind::CatalogDrift,
                format!(
                    "catalog: finalization gate for transition {:?} has different contents",
                    transition.id
                ),
            ));
        }
        _ if transition.state == TransitionState::Validating
            && (transition.kind != TransitionKind::IndexBuild || transition.index.unique) =>
        {
            return Err(Error::message(
                ErrorKind::CatalogDrift,
                format!(
                    "catalog: validating transition {:?} has no finalization gate",
                    transition.id
                ),
            ));
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::catalog::model::{ColumnDraft, ScalarType, TableDraft};
    use crate::engine::kv::slatedb;
    use crate::engine::kv::{IsolationLevel, TransactionView, TransactionalKv};
    use std::sync::Arc;

    fn table() -> TableDraft {
        TableDraft {
            id: Some(SchemaId::new(1).unwrap()),
            name: "users".into(),
            columns: vec![ColumnDraft {
                id: Some(SchemaId::new(1).unwrap()),
                name: "id".into(),
                scalar_type: ScalarType::Int64,
                nullable: false,
                format: String::new(),
                default: None,
            }],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        }
    }

    #[tokio::test]
    async fn index_build_publishes_sink_and_cancellation_retires_work() {
        let database = Arc::new(
            slatedb::Store::memory("catalog-index-transition")
                .await
                .unwrap(),
        );
        let service = Service::new(database.clone());
        service.create_table(table()).await.unwrap();
        let transition = {
            let mut transaction = database
                .begin(IsolationLevel::SerializableSnapshot)
                .await
                .unwrap();
            let transition = {
                let mut view = TransactionView(transaction.as_mut());
                let mut mutation = Mutation::new(&mut view);
                let transition = mutation
                    .start_index_build(
                        SchemaId::new(1).unwrap(),
                        IndexDef {
                            name: "users_id_idx".into(),
                            columns: vec!["id".into()],
                            unique: false,
                        },
                    )
                    .await
                    .unwrap();
                mutation.finish().await.unwrap();
                transition
            };
            transaction.commit().await.unwrap();
            transition
        };
        assert_eq!(transition.state, TransitionState::Building);
        assert_eq!(
            service.get_table("users").await.unwrap().unwrap().indexes[0].state,
            IndexState::Building
        );
        let cancelled = service
            .cancel_schema_transition(&transition.id)
            .await
            .unwrap();
        assert_eq!(cancelled.state, TransitionState::Cancelled);
        assert!(
            service
                .get_table("users")
                .await
                .unwrap()
                .unwrap()
                .indexes
                .is_empty()
        );
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn prerequisites_keep_index_logical_until_activation() {
        let database = Arc::new(
            slatedb::Store::memory("catalog-index-waiting")
                .await
                .unwrap(),
        );
        let service = Service::new(database.clone());
        service.create_table(table()).await.unwrap();
        let mut transaction = database
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        let mut view = TransactionView(transaction.as_mut());
        let mut mutation = Mutation::new(&mut view);
        let first = mutation
            .start_index_build(
                SchemaId::new(1).unwrap(),
                IndexDef {
                    name: "one".into(),
                    columns: vec!["id".into()],
                    unique: false,
                },
            )
            .await
            .unwrap();
        let second = mutation
            .start_index_build_with_prerequisites(
                SchemaId::new(1).unwrap(),
                IndexDef {
                    name: "two".into(),
                    columns: vec!["id".into()],
                    unique: false,
                },
                vec![first.id],
            )
            .await
            .unwrap();
        assert_eq!(second.state, TransitionState::Waiting);
        assert_eq!(
            store::get_table(&*mutation.view, "users")
                .await
                .unwrap()
                .unwrap()
                .indexes
                .len(),
            1
        );
        transaction.rollback();
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn finalization_gates_all_tables_atomically_and_releases_them() {
        let database = Arc::new(
            slatedb::Store::memory("catalog-index-multi-gate")
                .await
                .unwrap(),
        );
        let service = Service::new(database.clone());
        service.create_table(table()).await.unwrap();
        let mut second = table();
        second.id = Some(SchemaId::new(2).unwrap());
        second.name = "organizations".into();
        let second = service.create_table(second).await.unwrap();
        let first = service.get_table("users").await.unwrap().unwrap();

        let mut transaction = database
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        {
            let mut view = TransactionView(transaction.as_mut());
            let mut mutation = Mutation::new(&mut view);
            let mut transition = mutation
                .start_index_build(
                    first.schema_id,
                    IndexDef {
                        name: "users_id_uq".into(),
                        columns: vec!["id".into()],
                        unique: true,
                    },
                )
                .await
                .unwrap();
            transition.state = TransitionState::CatchingUp;
            transition.owner_epoch = 1.into();
            transition.gate_table_ids =
                vec![second.id.clone(), first.id.clone(), second.id.clone()];
            store::save_transition(mutation.view, &transition)
                .await
                .unwrap();
            assert_eq!(
                mutation
                    .publish_index_ready(&transition.id, transition.owner_epoch)
                    .await
                    .unwrap_err()
                    .kind(),
                ErrorKind::CatalogDrift
            );
            let validating = mutation
                .begin_index_validation(&transition.id, transition.owner_epoch)
                .await
                .unwrap();
            let first_table = store::get_table_by_id(mutation.view, &first.id)
                .await
                .unwrap()
                .unwrap();
            let first_generation = store::read_write_protocol(mutation.view, &first_table)
                .await
                .unwrap()
                .generation;
            let second_table = store::get_table_by_id(mutation.view, &second.id)
                .await
                .unwrap()
                .unwrap();
            let second_generation = store::read_write_protocol(mutation.view, &second_table)
                .await
                .unwrap()
                .generation;
            mutation
                .acquire_schema_finalization_gates(&validating)
                .await
                .unwrap();
            for (table_id, generation) in [
                (&first.id, first_generation),
                (&second.id, second_generation),
            ] {
                let table = store::get_table_by_id(mutation.view, table_id)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    store::read_write_protocol(mutation.view, &table)
                        .await
                        .unwrap()
                        .generation,
                    generation,
                    "reacquiring an exact gate must not publish a protocol"
                );
            }
            for table_id in [&first.id, &second.id] {
                let table = store::get_table_by_id(mutation.view, table_id)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    store::read_write_protocol(mutation.view, &table)
                        .await
                        .unwrap()
                        .finalization_gate
                        .unwrap()
                        .transition_id,
                    transition.id
                );
            }

            let first_table = store::get_table_by_id(mutation.view, &first.id)
                .await
                .unwrap()
                .unwrap();
            let exact_protocol = store::read_write_protocol(mutation.view, &first_table)
                .await
                .unwrap();
            let mut drifted_transition = validating.clone();
            drifted_transition.object_id.push_str(":drifted");
            let mut drifted_protocol = exact_protocol.clone();
            assert_eq!(
                acquire_gate(&mut drifted_protocol, &drifted_transition)
                    .unwrap_err()
                    .kind(),
                ErrorKind::CatalogDrift
            );

            let mut missing_protocol = exact_protocol.clone();
            missing_protocol.finalization_gate = None;
            assert_eq!(
                remove_owned_gate(&mut missing_protocol, &validating)
                    .unwrap_err()
                    .kind(),
                ErrorKind::CatalogDrift
            );
            let mut wrong_owner_protocol = exact_protocol.clone();
            wrong_owner_protocol
                .finalization_gate
                .as_mut()
                .unwrap()
                .transition_id = TransitionId::new("tr-other");
            assert_eq!(
                remove_owned_gate(&mut wrong_owner_protocol, &validating)
                    .unwrap_err()
                    .kind(),
                ErrorKind::CatalogDrift
            );
            let mut non_unique = validating.clone();
            non_unique.kind = TransitionKind::IndexBuild;
            non_unique.index.unique = false;
            remove_owned_gate(&mut missing_protocol, &non_unique).unwrap();

            mutation
                .publish_index_ready(&transition.id, validating.owner_epoch)
                .await
                .unwrap();
            for table_id in [&first.id, &second.id] {
                let table = store::get_table_by_id(mutation.view, table_id)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(
                    store::read_write_protocol(mutation.view, &table)
                        .await
                        .unwrap()
                        .finalization_gate
                        .is_none()
                );
            }
        }
        transaction.commit().await.unwrap();
        database.close().await.unwrap();
    }
}
