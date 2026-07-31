use std::collections::{HashMap, HashSet};

use crate::engine::catalog::identity::SchemaId;
use crate::engine::catalog::migrate::{self, Step};
use crate::engine::catalog::model::{
    ConstraintKind, ConstraintState, Schema, SchemaTransition, Table, TransitionControl,
    TransitionKind,
};
use crate::engine::catalog::schema;
use crate::engine::exec::{Engine, Result};

use super::{MigrationPlan, SchemaFinding, preflight, program};

pub(super) async fn build(engine: &Engine, desired: &schema::Schema) -> Result<MigrationPlan> {
    let (current, tables, transitions) = engine.schema_migration_snapshot().await?;
    let canonical = desired.canonical();
    let desired_hash = canonical.hash()?;
    let steps = migrate::diff(&tables, desired)?;
    let (program_steps, recovered, transition_findings) =
        recover_transitions(&canonical, &steps, &transitions);
    let program = program::lower(&current.schema, &program_steps)?;
    let mut blocking = structure_findings(&tables, &canonical, &steps);
    let (destructive, data_blocking) =
        preflight::inspect(engine, &tables, &canonical, &steps).await?;
    blocking.extend(data_blocking);
    blocking.extend(transition_findings);
    Ok(MigrationPlan {
        current,
        desired: canonical,
        desired_hash,
        steps,
        program,
        transitions: recovered,
        destructive,
        blocking,
    })
}

fn recover_transitions(
    desired: &Schema,
    steps: &[Step],
    transitions: &[SchemaTransition],
) -> (Vec<Step>, Vec<TransitionControl>, Vec<SchemaFinding>) {
    let desired_tables = desired
        .tables
        .iter()
        .map(|table| (table.name.as_str(), table))
        .collect::<HashMap<_, _>>();
    let mut program = Vec::new();
    let mut recovered = Vec::new();
    let mut blocking = Vec::new();
    for step in steps {
        let mut matched = false;
        let mut conflict = false;
        for transition in transitions
            .iter()
            .filter(|transition| !transition.state.is_terminal())
        {
            match step {
                Step::ReplaceColumn {
                    table,
                    column_id,
                    definition,
                    ..
                } => {
                    let Some(table) = desired_tables.get(table.as_str()) else {
                        continue;
                    };
                    if transition.kind != TransitionKind::ColumnReplacement
                        || transition.table_schema_id != table.id
                        || !transition.affected_column_ids.contains(column_id)
                    {
                        continue;
                    }
                    matched = transition
                        .replacement_request
                        .as_ref()
                        .is_some_and(|request| {
                            request.column_schema_id == *column_id
                                && request.scalar_type == definition.scalar_type
                                && request.nullable == definition.nullable
                                && request.format == definition.format
                                && request.default == definition.default
                                && request.conversion == definition.conversion
                        });
                    conflict = !matched;
                }
                Step::ValidateNotNull {
                    table, definition, ..
                } => {
                    let Some(table) = desired_tables.get(table.as_str()) else {
                        continue;
                    };
                    if transition.kind != TransitionKind::ConstraintValidation
                        || transition.table_schema_id != table.id
                        || !transition
                            .affected_column_ids
                            .contains(&definition.column_id)
                    {
                        continue;
                    }
                    matched = transition.constraint.as_ref().is_some_and(|constraint| {
                        constraint.name == definition.name && constraint.kind == definition.kind
                    }) && transition
                        .constraint_request
                        .as_ref()
                        .is_some_and(|request| request.column_schema_id == definition.column_id);
                    conflict = !matched;
                }
                Step::CreateIndex { table, definition } => {
                    let Some(table) = desired_tables.get(table.as_str()) else {
                        continue;
                    };
                    if transition.kind != TransitionKind::IndexBuild
                        || transition.table_schema_id != table.id
                        || transition
                            .index_request
                            .as_ref()
                            .is_none_or(|request| request.name != definition.name)
                    {
                        continue;
                    }
                    let column_ids = desired_index_column_ids(table, &definition.columns);
                    matched = transition.index_request.as_ref().is_some_and(|request| {
                        column_ids.as_ref() == Some(&request.column_schema_ids)
                            && request.unique == definition.unique
                    });
                    conflict = !matched;
                }
                _ => continue,
            }
            if matched {
                recovered.push(transition.control());
                break;
            }
            if conflict {
                break;
            }
        }
        if matched {
            continue;
        }
        if conflict {
            blocking.push(SchemaFinding::new(
                "active_schema_transition_conflict",
                format!("{step} conflicts with active schema work"),
            ));
        } else {
            program.push(step.clone());
        }
    }
    (program, recovered, blocking)
}

fn desired_index_column_ids(
    table: &crate::engine::catalog::model::TableDef,
    names: &[String],
) -> Option<Vec<SchemaId>> {
    names
        .iter()
        .map(|name| table.column(name).map(|column| column.id))
        .collect()
}

fn structure_findings(current: &[Table], desired: &Schema, steps: &[Step]) -> Vec<SchemaFinding> {
    let desired_by_name = desired
        .tables
        .iter()
        .map(|table| (table.name.as_str(), table))
        .collect::<HashMap<_, _>>();
    let current_by_id = current
        .iter()
        .map(|table| (table.schema_id, table))
        .collect::<HashMap<_, _>>();
    let deleted_indexes = steps
        .iter()
        .filter_map(|step| match step {
            Step::DeleteIndex { table, index } => Some((table.as_str(), index.as_str())),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let mut findings = Vec::new();
    for step in steps {
        let Step::ReplaceColumn {
            table,
            column,
            column_id,
            definition,
        } = step
        else {
            continue;
        };
        let Some(desired_table) = desired_by_name.get(table.as_str()) else {
            continue;
        };
        let Some(current_table) = current_by_id.get(&desired_table.id) else {
            continue;
        };
        let Some(current_column) = current_table
            .columns
            .iter()
            .find(|candidate| candidate.schema_id == *column_id)
        else {
            continue;
        };
        let mut dependency = None;
        if current_table.primary_key.contains(&current_column.name) {
            dependency = Some("the primary key".to_owned());
        }
        if dependency.is_none() {
            dependency = current_table.indexes.iter().find_map(|index| {
                let uses_column = index.column_ids.contains(&current_column.id)
                    || index.columns.contains(&current_column.name);
                (uses_column && !deleted_indexes.contains(&(table.as_str(), index.name.as_str())))
                    .then(|| format!("index {}", index.name))
            });
        }
        if dependency.is_none() && definition.nullable {
            dependency = current_table.constraints.iter().find_map(|constraint| {
                (constraint.kind == ConstraintKind::NotNull
                    && constraint.state == ConstraintState::Valid
                    && constraint.column_ids.contains(&current_column.id))
                .then(|| format!("valid constraint {}", constraint.name))
            });
        }
        if dependency.is_none() {
            dependency = current.iter().find_map(|candidate| {
                candidate.foreign_keys.iter().find_map(|foreign_key| {
                    let local = candidate.id == current_table.id
                        && (foreign_key.columns.contains(&current_column.name)
                            || foreign_key.ref_columns.contains(&current_column.name));
                    let referenced = foreign_key.ref_table_id == current_table.id
                        && foreign_key.ref_columns.contains(&current_column.name);
                    (local || referenced).then(|| format!("foreign key {}", foreign_key.name))
                })
            });
        }
        if let Some(dependency) = dependency {
            let mut finding = SchemaFinding::new(
                "column_replacement_dependency",
                format!(
                    "column {table}.{column} cannot be replaced while {dependency} depends on its physical representation"
                ),
            );
            finding.table.clone_from(table);
            finding.column.clone_from(column);
            findings.push(finding);
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use crate::engine::catalog::identity::{
        CatalogVersion, OwnerEpoch, SchemaId, TransitionGeneration, TransitionId,
    };
    use crate::engine::catalog::migrate::Step;
    use crate::engine::catalog::model::{
        ColumnConversion, ColumnDef, ColumnReplacementDef, ColumnReplacementRequest, DataPosition,
        Index, ScalarType, Schema, SchemaTransition, TableDef, Timestamp, TransitionKind,
        TransitionState, TransitionWorkState,
    };

    use super::recover_transitions;

    fn id(value: u32) -> SchemaId {
        SchemaId::new(value).unwrap()
    }

    fn desired() -> Schema {
        Schema {
            tables: vec![TableDef {
                id: id(7),
                name: "events".into(),
                columns: vec![
                    ColumnDef {
                        id: id(1),
                        name: "id".into(),
                        scalar_type: ScalarType::Int64,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                    ColumnDef {
                        id: id(2),
                        name: "value".into(),
                        scalar_type: ScalarType::Int64,
                        nullable: true,
                        format: String::new(),
                        default: None,
                    },
                ],
                primary_key: Vec::new(),
                indexes: Vec::new(),
                foreign_keys: Vec::new(),
            }],
        }
    }

    fn replacement() -> Step {
        Step::ReplaceColumn {
            table: "events".into(),
            column: "value".into(),
            column_id: id(2),
            definition: ColumnReplacementDef {
                scalar_type: ScalarType::Int64,
                nullable: true,
                format: String::new(),
                default: None,
                conversion: ColumnConversion::StrictBuiltin,
                prerequisites: Vec::new(),
            },
        }
    }

    fn transition(state: TransitionState, scalar_type: ScalarType) -> SchemaTransition {
        SchemaTransition {
            id: TransitionId::from("work"),
            kind: TransitionKind::ColumnReplacement,
            object_id: String::new(),
            state,
            generation: TransitionGeneration::ZERO,
            owner_epoch: OwnerEpoch::ZERO,
            source_catalog_version: CatalogVersion::ZERO,
            base_position: DataPosition::default(),
            barrier_position: DataPosition::default(),
            table_id: "physical-events".into(),
            table_schema_id: id(7),
            affected_column_ids: vec![id(2)],
            index: Index::default(),
            index_request: None,
            column_replacement: None,
            replacement_request: Some(ColumnReplacementRequest {
                column_schema_id: id(2),
                scalar_type,
                nullable: true,
                format: String::new(),
                default: None,
                conversion: ColumnConversion::StrictBuiltin,
            }),
            constraint: None,
            constraint_request: None,
            prerequisites: Vec::new(),
            gate_table_ids: Vec::new(),
            cursor: Vec::new(),
            batch_id: 0,
            applied_delta: 0,
            delta_high_water: 0,
            delta_soft_limit: 0,
            delta_hard_limit: 0,
            work_state: TransitionWorkState::Normal,
            rows_scanned: 0,
            last_error: String::new(),
            created_at: Timestamp::default(),
            updated_at: Timestamp::default(),
            compacted_at: Timestamp::default(),
        }
    }

    #[test]
    fn recovery_reuses_only_an_exact_active_transition_request() {
        let wanted = desired();
        let step = replacement();

        let (program, recovered, blocking) = recover_transitions(
            &wanted,
            std::slice::from_ref(&step),
            &[transition(TransitionState::Building, ScalarType::Int64)],
        );
        assert!(program.is_empty());
        assert_eq!(recovered.len(), 1);
        assert!(blocking.is_empty());

        for state in [
            TransitionState::Ready,
            TransitionState::Failed,
            TransitionState::Cancelled,
        ] {
            let (program, recovered, blocking) = recover_transitions(
                &wanted,
                std::slice::from_ref(&step),
                &[transition(state, ScalarType::Int64)],
            );
            assert_eq!(program.as_slice(), std::slice::from_ref(&step));
            assert!(recovered.is_empty());
            assert!(blocking.is_empty());
        }

        let (program, recovered, blocking) = recover_transitions(
            &wanted,
            &[step],
            &[transition(TransitionState::Building, ScalarType::Bool)],
        );
        assert!(program.is_empty());
        assert!(recovered.is_empty());
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].kind, "active_schema_transition_conflict");
    }
}
