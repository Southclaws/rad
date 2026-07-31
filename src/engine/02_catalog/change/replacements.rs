use super::admission::TransitionCandidate;
use super::transitions::{require_owner, required_table_by_id, required_transition};
use super::*;
use crate::engine::catalog::identity::{ColumnId, OwnerEpoch, ReclamationId, TransitionId};
use crate::engine::catalog::model::{
    ColumnConversion, ColumnReplacement, ColumnReplacementDef, ColumnReplacementRequest,
    ColumnReplacementWrite, ConstraintKind, ConstraintState, DataPosition, ReclamationKind,
    SchemaTransition, Timestamp, TransitionKind, TransitionState, TransitionWorkState,
};

impl Mutation<'_> {
    pub async fn start_column_replacement(
        &mut self,
        table_id: SchemaId,
        column_id: SchemaId,
        mut definition: ColumnReplacementDef,
    ) -> Result<SchemaTransition> {
        let mut table = self.table_by_schema_id(table_id).await?;
        let source = table
            .columns
            .iter()
            .find(|column| column.schema_id == column_id)
            .cloned()
            .ok_or_else(|| {
                input(format!(
                    "catalog: column schema ID {column_id} does not exist in table {:?}",
                    table.name
                ))
            })?;
        validate_dependencies(&table, &source, definition.nullable)?;
        for candidate in store::list_tables(self.view).await? {
            for foreign_key in &candidate.foreign_keys {
                if foreign_key.ref_table_id == table.id
                    && foreign_key.ref_columns.contains(&source.name)
                {
                    return Err(input(format!(
                        "catalog: cannot replace column {:?} while foreign key {:?} on table {:?} references it",
                        source.name, foreign_key.name, candidate.name
                    )));
                }
            }
        }
        validate_default(
            &source.name,
            definition.scalar_type,
            definition.default.as_ref(),
        )?;
        if definition.conversion != ColumnConversion::StrictBuiltin {
            return Err(input("catalog: unsupported column conversion"));
        }
        let (prerequisites, waiting) = self
            .validate_transition_admission(
                &table,
                TransitionCandidate {
                    kind: TransitionKind::ColumnReplacement,
                    table_id: table.id.clone(),
                    affected_column_ids: vec![source.schema_id],
                    prerequisites: std::mem::take(&mut definition.prerequisites),
                },
            )
            .await?;
        if !waiting
            && source.scalar_type == definition.scalar_type
            && source.nullable == definition.nullable
            && source.format == definition.format
            && source.insert_default == definition.default
        {
            return Err(input(format!(
                "catalog: replacement for column {:?} has identical value semantics",
                source.name
            )));
        }
        let id: TransitionId = store::next_physical_id(self.view, "tr").await?.into();
        let now = self.now();
        let mut transition = SchemaTransition {
            id: id.clone(),
            kind: TransitionKind::ColumnReplacement,
            object_id: format!("column:{}", source.schema_id),
            state: TransitionState::Waiting,
            generation: 1.into(),
            owner_epoch: OwnerEpoch::ZERO,
            source_catalog_version: store::current_revision(self.view).await?.version,
            base_position: DataPosition::default(),
            barrier_position: DataPosition::default(),
            table_id: table.id.clone(),
            table_schema_id: table.schema_id,
            affected_column_ids: vec![source.schema_id],
            index: Index::default(),
            index_request: None,
            column_replacement: None,
            replacement_request: Some(ColumnReplacementRequest {
                column_schema_id: source.schema_id,
                scalar_type: definition.scalar_type,
                nullable: definition.nullable,
                format: definition.format.clone(),
                default: definition.default.clone(),
                conversion: definition.conversion,
            }),
            constraint: None,
            constraint_request: None,
            prerequisites,
            gate_table_ids: vec![table.id.clone()],
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
        if !waiting {
            let target = build_target(self.view, &source, &definition).await?;
            let replacement = ColumnReplacement {
                source,
                target,
                conversion: definition.conversion,
            };
            let mut protocol = store::read_write_protocol(self.view, &table).await?;
            protocol.generation = protocol.generation.next();
            protocol.column_replacements.push(ColumnReplacementWrite {
                transition_id: id,
                replacement: replacement.clone(),
            });
            table.write_protocol_generation = protocol.generation;
            store::save_table(self.view, &mut table).await?;
            self.save_write_protocol(protocol).await?;
            transition.state = TransitionState::Building;
            transition.column_replacement = Some(replacement);
            if let Some(position) = self.view.begin_position() {
                transition.base_position = DataPosition::new(position.as_str());
            }
        }
        store::create_transition(self.view, &transition).await?;
        self.mark_catalog_changed();
        Ok(transition)
    }

    pub async fn begin_column_replacement_validation(
        &mut self,
        id: &TransitionId,
        owner: OwnerEpoch,
    ) -> Result<SchemaTransition> {
        let (mut transition, _, _) = self.replacement_context(id, owner).await?;
        if transition.state != TransitionState::Building {
            return Err(input(format!(
                "catalog: replacement transition {id:?} cannot validate from state {:?}",
                transition.state
            )));
        }
        self.acquire_schema_finalization_gates(&transition).await?;
        transition.state = TransitionState::Validating;
        transition.generation = transition.generation.next();
        transition.updated_at = self.now();
        store::save_transition(self.view, &transition).await?;
        self.mark_catalog_changed();
        Ok(transition)
    }

    pub async fn publish_column_replacement(
        &mut self,
        id: &TransitionId,
        owner: OwnerEpoch,
    ) -> Result<SchemaTransition> {
        let (transition, _, _) = self.replacement_context(id, owner).await?;
        if transition.state != TransitionState::Validating {
            return Err(input(format!(
                "catalog: replacement transition {id:?} cannot publish from state {:?}",
                transition.state
            )));
        }
        if store::first_transition_violation(self.view, id)
            .await?
            .is_some()
        {
            return Err(input(format!(
                "catalog: replacement transition {id:?} still has conversion violations"
            )));
        }
        self.release_schema_finalization_gates(&transition).await?;
        let (mut transition, mut table, mut protocol) = self.replacement_context(id, owner).await?;
        let mut replacement = transition
            .column_replacement
            .clone()
            .expect("context requires replacement");
        let position = table
            .columns
            .iter()
            .position(|column| column.id == replacement.source.id)
            .ok_or_else(|| {
                input(format!(
                    "catalog: replacement transition {id:?} source column is no longer active"
                ))
            })?;
        store::advance_column_value_fence(self.view, &table, &table.columns[position]).await?;
        replacement
            .target
            .name
            .clone_from(&table.columns[position].name);
        replacement.target.schema_id = table.columns[position].schema_id;
        table.columns[position] = replacement.target.clone();
        for constraint in &mut table.constraints {
            if replace_physical_id(
                &mut constraint.column_ids,
                &replacement.source.id,
                &replacement.target.id,
            ) {
                constraint.definition_generation = constraint.definition_generation.next();
            }
        }
        transition.column_replacement = Some(replacement.clone());
        protocol.generation = protocol.generation.next();
        protocol
            .column_replacements
            .retain(|write| write.transition_id != *id);
        table.write_protocol_generation = protocol.generation;
        store::save_table(self.view, &mut table).await?;
        self.save_write_protocol(protocol).await?;
        transition.state = TransitionState::Ready;
        transition.generation = transition.generation.next();
        transition.work_state = TransitionWorkState::Normal;
        transition.updated_at = self.now();
        store::save_transition(self.view, &transition).await?;
        self.retire(
            &transition,
            ReclamationKind::ReplacedColumn,
            store::replaced_column_reclamation_id(id),
            replacement.source.id,
        )
        .await?;
        self.mark_schema_changed();
        Ok(transition)
    }

    pub async fn fail_column_replacement(
        &mut self,
        id: &TransitionId,
        owner: OwnerEpoch,
        cause: impl Into<String>,
    ) -> Result<SchemaTransition> {
        let (transition, _, _) = self.replacement_context(id, owner).await?;
        self.release_schema_finalization_gates(&transition).await?;
        let (mut transition, mut table, mut protocol) = self.replacement_context(id, owner).await?;
        protocol.generation = protocol.generation.next();
        protocol
            .column_replacements
            .retain(|write| write.transition_id != *id);
        table.write_protocol_generation = protocol.generation;
        store::save_table(self.view, &mut table).await?;
        self.save_write_protocol(protocol).await?;
        transition.state = TransitionState::Failed;
        transition.generation = transition.generation.next();
        transition.owner_epoch = transition.owner_epoch.next();
        transition.work_state = TransitionWorkState::Normal;
        transition.last_error = cause.into();
        transition.updated_at = self.now();
        store::save_transition(self.view, &transition).await?;
        let target = transition
            .column_replacement
            .as_ref()
            .expect("context requires replacement")
            .target
            .id
            .clone();
        self.retire(
            &transition,
            ReclamationKind::FailedReplacement,
            store::failed_replacement_reclamation_id(id),
            target,
        )
        .await?;
        self.mark_catalog_changed();
        Ok(transition)
    }

    pub async fn cancel_column_replacement(
        &mut self,
        id: &TransitionId,
    ) -> Result<SchemaTransition> {
        let mut transition = required_transition(self.view, id).await?;
        if transition.kind != TransitionKind::ColumnReplacement {
            return Err(input(format!(
                "catalog: column replacement transition {id:?} does not exist"
            )));
        }
        match transition.state {
            TransitionState::Cancelled => return Ok(transition),
            TransitionState::Ready => {
                return Err(input(format!(
                    "catalog: ready replacement transition {id:?} is not cancellable"
                )));
            }
            TransitionState::Failed => {
                return Err(input(format!(
                    "catalog: failed replacement transition {id:?} is already cleaning up"
                )));
            }
            TransitionState::Waiting => {}
            _ => {
                self.release_schema_finalization_gates(&transition).await?;
                let (_, mut table, mut protocol) =
                    self.replacement_context(id, transition.owner_epoch).await?;
                protocol.generation = protocol.generation.next();
                protocol
                    .column_replacements
                    .retain(|write| write.transition_id != *id);
                table.write_protocol_generation = protocol.generation;
                store::save_table(self.view, &mut table).await?;
                self.save_write_protocol(protocol).await?;
                let target = transition
                    .column_replacement
                    .as_ref()
                    .expect("active replacement has a target")
                    .target
                    .id
                    .clone();
                self.retire(
                    &transition,
                    ReclamationKind::CancelledReplacement,
                    store::cancelled_replacement_reclamation_id(id),
                    target,
                )
                .await?;
            }
        }
        transition.state = TransitionState::Cancelled;
        transition.generation = transition.generation.next();
        transition.owner_epoch = transition.owner_epoch.next();
        transition.updated_at = self.now();
        store::save_transition(self.view, &transition).await?;
        self.mark_catalog_changed();
        Ok(transition)
    }

    async fn replacement_context(
        &mut self,
        id: &TransitionId,
        owner: OwnerEpoch,
    ) -> Result<(SchemaTransition, Table, WriteProtocol)> {
        let transition = required_transition(self.view, id).await?;
        if transition.kind != TransitionKind::ColumnReplacement
            || transition.column_replacement.is_none()
        {
            return Err(input(format!(
                "catalog: column replacement transition {id:?} does not exist"
            )));
        }
        require_owner(&transition, owner)?;
        let table = required_table_by_id(self.view, &transition.table_id).await?;
        let protocol = store::read_write_protocol(self.view, &table).await?;
        Ok((transition, table, protocol))
    }

    async fn retire(
        &mut self,
        transition: &SchemaTransition,
        kind: ReclamationKind,
        id: ReclamationId,
        column_id: ColumnId,
    ) -> Result<()> {
        let mut reclamation = self.pending_reclamation(id, kind).await?;
        reclamation.table_id = transition.table_id.clone();
        reclamation.table_schema_id = Some(transition.table_schema_id);
        reclamation.column_id = column_id;
        reclamation.transition_id = transition.id.clone();
        self.queue_reclamation(reclamation).await
    }
}

pub(super) fn validate_dependencies(
    table: &Table,
    source: &Column,
    target_nullable: bool,
) -> Result<()> {
    if table.primary_key.contains(&source.name) {
        return Err(input(format!(
            "catalog: cannot replace primary-key column {:?} until key-rewrite transitions are supported",
            source.name
        )));
    }
    for index in &table.indexes {
        if index.column_ids.contains(&source.id) || index.columns.contains(&source.name) {
            return Err(input(format!(
                "catalog: cannot replace column {:?} while index {:?} uses its physical representation",
                source.name, index.name
            )));
        }
    }
    for foreign_key in &table.foreign_keys {
        if foreign_key.columns.contains(&source.name)
            || foreign_key.ref_columns.contains(&source.name)
        {
            return Err(input(format!(
                "catalog: cannot replace column {:?} while foreign key {:?} uses it",
                source.name, foreign_key.name
            )));
        }
    }
    if target_nullable
        && table.constraints.iter().any(|constraint| {
            constraint.kind == ConstraintKind::NotNull
                && constraint.state == ConstraintState::Valid
                && constraint.column_ids.contains(&source.id)
        })
    {
        return Err(input(format!(
            "catalog: cannot make column {:?} nullable while a valid constraint requires non-null values",
            source.name
        )));
    }
    Ok(())
}

fn validate_default(
    name: &str,
    scalar_type: ScalarType,
    default: Option<&super::super::model::DefaultValue>,
) -> Result<()> {
    if let Some(function) = default.and_then(|value| value.function)
        && !matches!(
            (function, scalar_type),
            (DefaultFunction::Uuid, ScalarType::Text) | (DefaultFunction::NowMs, ScalarType::Int64)
        )
    {
        return Err(input(format!(
            "catalog: column {name:?}: default function does not support {scalar_type:?}"
        )));
    }
    Ok(())
}

pub(super) async fn build_target(
    view: &mut dyn KvView,
    source: &Column,
    definition: &ColumnReplacementDef,
) -> Result<Column> {
    Ok(Column {
        id: store::next_physical_id(view, "c").await?.into(),
        schema_id: source.schema_id,
        name: source.name.clone(),
        value_generation: ValueGeneration::ZERO,
        scalar_type: definition.scalar_type,
        nullable: definition.nullable,
        format: definition.format.clone(),
        insert_default: definition.default.clone(),
        missing_value: definition
            .default
            .as_ref()
            .filter(|value| value.function.is_none())
            .cloned(),
    })
}

fn replace_physical_id(values: &mut [ColumnId], old: &ColumnId, new: &ColumnId) -> bool {
    let mut changed = false;
    for value in values {
        if value == old {
            value.clone_from(new);
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::engine::catalog::model::{ColumnDraft, ScalarType, TableDraft};
    use crate::engine::kv::slatedb;
    use crate::engine::kv::{IsolationLevel, TransactionView, TransactionalKv};

    use super::*;

    #[tokio::test]
    async fn replacement_dual_writes_then_swaps_physical_column() {
        let database = Arc::new(slatedb::Store::memory("catalog-replacement").await.unwrap());
        let service = Service::new(database.clone());
        let before = service
            .create_table(TableDraft {
                id: Some(SchemaId::new(1).unwrap()),
                name: "events".into(),
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
        let source_id = before.column("value").unwrap().id.clone();
        let mut transaction = database
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        let ready = {
            let mut view = TransactionView(transaction.as_mut());
            let mut mutation = Mutation::new(&mut view);
            let mut transition = mutation
                .start_column_replacement(
                    SchemaId::new(1).unwrap(),
                    SchemaId::new(2).unwrap(),
                    ColumnReplacementDef {
                        scalar_type: ScalarType::Int64,
                        nullable: true,
                        format: String::new(),
                        default: None,
                        conversion: ColumnConversion::StrictBuiltin,
                        prerequisites: Vec::new(),
                    },
                )
                .await
                .unwrap();
            transition.owner_epoch = 1.into();
            store::save_transition(mutation.view, &transition)
                .await
                .unwrap();
            let validating = mutation
                .begin_column_replacement_validation(&transition.id, transition.owner_epoch)
                .await
                .unwrap();
            mutation
                .publish_column_replacement(&transition.id, validating.owner_epoch)
                .await
                .unwrap()
        };
        transaction.commit().await.unwrap();
        assert_eq!(ready.state, TransitionState::Ready);
        let after = service.get_table("events").await.unwrap().unwrap();
        assert_eq!(
            after.column("value").unwrap().scalar_type,
            ScalarType::Int64
        );
        assert_ne!(after.column("value").unwrap().id, source_id);
        database.close().await.unwrap();
    }
}
