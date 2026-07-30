use super::*;
use crate::engine::catalog::identity::{ColumnId, TransitionId};
use crate::engine::catalog::model::{Index, SchemaTransition, TransitionKind, TransitionState};

pub(super) struct TransitionCandidate {
    pub kind: TransitionKind,
    pub table_id: TableId,
    pub affected_column_ids: Vec<SchemaId>,
    pub prerequisites: Vec<TransitionId>,
}

impl Mutation<'_> {
    pub(super) async fn validate_transition_admission(
        &mut self,
        table: &Table,
        mut candidate: TransitionCandidate,
    ) -> Result<(Vec<TransitionId>, bool)> {
        candidate.prerequisites.sort();
        candidate.prerequisites.dedup();
        candidate.affected_column_ids.sort();
        candidate.affected_column_ids.dedup();
        let mut waiting = false;

        for id in &candidate.prerequisites {
            let transition = store::get_transition(self.view, id).await?.ok_or_else(|| {
                input(format!(
                    "catalog: prerequisite transition {id:?} does not exist"
                ))
            })?;
            match transition.state {
                TransitionState::Ready => {}
                TransitionState::Failed | TransitionState::Cancelled => {
                    return Err(input(format!(
                        "catalog: prerequisite transition {id:?} is terminal in state {:?}",
                        transition.state
                    )));
                }
                _ => waiting = true,
            }
        }

        for active in store::list_transitions(self.view).await? {
            if active.table_id != candidate.table_id || active.state.is_terminal() {
                continue;
            }
            let active_columns = affected_column_schema_ids(table, &active)?;
            if candidate.kind == TransitionKind::ColumnReplacement
                && active.kind == TransitionKind::IndexBuild
                && schema_ids_overlap(&candidate.affected_column_ids, &active_columns)
            {
                return Err(input(format!(
                    "catalog: column replacement cannot follow active index transition {:?} on table {:?}; cancel or finish and delete the index first",
                    active.id, table.name
                )));
            }
            if transition_kinds_compatible(
                candidate.kind,
                &candidate.affected_column_ids,
                active.kind,
                &active_columns,
            ) {
                continue;
            }
            if candidate.prerequisites.contains(&active.id) {
                waiting = true;
                continue;
            }
            return Err(input(format!(
                "catalog: {:?} transition conflicts with active {:?} transition {:?} on table {:?}; add it as a prerequisite to wait",
                candidate.kind, active.kind, active.id, table.name
            )));
        }
        Ok((candidate.prerequisites, waiting))
    }

    pub(super) async fn ensure_index_name_available(
        &mut self,
        table: &Table,
        name: &str,
    ) -> Result<()> {
        if table.index(name).is_some() {
            return Err(input(format!(
                "catalog: index {name:?} already exists on table {:?}",
                table.name
            )));
        }
        for transition in store::list_transitions(self.view).await? {
            if transition.table_id == table.id
                && transition.kind == TransitionKind::IndexBuild
                && !transition.state.is_terminal()
                && reserved_index_name(&transition) == name
            {
                return Err(input(format!(
                    "catalog: index name {name:?} is reserved by active transition {:?} on table {:?}",
                    transition.id, table.name
                )));
            }
        }
        Ok(())
    }
}

pub(super) fn reserved_index_name(transition: &SchemaTransition) -> &str {
    transition
        .index_request
        .as_ref()
        .map_or(&transition.index.name, |request| &request.name)
}

pub(super) fn transition_kinds_compatible(
    left_kind: TransitionKind,
    left_columns: &[SchemaId],
    right_kind: TransitionKind,
    right_columns: &[SchemaId],
) -> bool {
    if !schema_ids_overlap(left_columns, right_columns) {
        return true;
    }
    if left_kind == TransitionKind::IndexBuild && right_kind == TransitionKind::IndexBuild {
        return true;
    }
    matches!(
        (left_kind, right_kind),
        (
            TransitionKind::IndexBuild,
            TransitionKind::ConstraintValidation
        ) | (
            TransitionKind::ConstraintValidation,
            TransitionKind::IndexBuild
        )
    )
}

fn schema_ids_overlap(left: &[SchemaId], right: &[SchemaId]) -> bool {
    left.iter().any(|id| right.contains(id))
}

pub(super) fn affected_column_schema_ids(
    table: &Table,
    transition: &SchemaTransition,
) -> Result<Vec<SchemaId>> {
    let mut ids = if !transition.affected_column_ids.is_empty() {
        transition.affected_column_ids.clone()
    } else if let Some(request) = &transition.index_request {
        request.column_schema_ids.clone()
    } else if let Some(request) = &transition.replacement_request {
        vec![request.column_schema_id]
    } else if let Some(request) = &transition.constraint_request {
        vec![request.column_schema_id]
    } else if let Some(replacement) = &transition.column_replacement {
        vec![replacement.source.schema_id]
    } else if transition.kind == TransitionKind::IndexBuild {
        index_column_schema_ids(table, &transition.index)?
    } else if let Some(constraint) = &transition.constraint {
        physical_column_schema_ids(table, &constraint.column_ids)?
    } else {
        return Err(Error::message(
            ErrorKind::CatalogDrift,
            format!(
                "catalog: active transition {:?} has no affected-column identity",
                transition.id
            ),
        ));
    };
    ids.sort();
    ids.dedup();
    Ok(ids)
}

pub(super) fn index_column_schema_ids(table: &Table, index: &Index) -> Result<Vec<SchemaId>> {
    if !index.column_ids.is_empty() {
        return physical_column_schema_ids(table, &index.column_ids);
    }
    let mut ids = Vec::with_capacity(index.columns.len());
    for name in &index.columns {
        ids.push(
            table
                .column(name)
                .ok_or_else(|| {
                    Error::message(
                        ErrorKind::CatalogDrift,
                        format!(
                            "catalog: index {:?} references missing column {name:?} on table {:?}",
                            index.name, table.name
                        ),
                    )
                })?
                .schema_id,
        );
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

fn physical_column_schema_ids(table: &Table, physical_ids: &[ColumnId]) -> Result<Vec<SchemaId>> {
    let mut ids = Vec::with_capacity(physical_ids.len());
    for physical_id in physical_ids {
        ids.push(
            table
                .columns
                .iter()
                .find(|column| &column.id == physical_id)
                .ok_or_else(|| {
                    Error::message(
                        ErrorKind::CatalogDrift,
                        format!(
                            "catalog: table {:?} has no physical column {physical_id:?}",
                            table.name
                        ),
                    )
                })?
                .schema_id,
        );
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transition_compatibility_matrix_matches_the_contract() {
        let one = SchemaId::new(1).unwrap();
        let two = SchemaId::new(2).unwrap();
        assert!(transition_kinds_compatible(
            TransitionKind::ColumnReplacement,
            &[one],
            TransitionKind::ConstraintValidation,
            &[two]
        ));
        assert!(!transition_kinds_compatible(
            TransitionKind::ColumnReplacement,
            &[one],
            TransitionKind::ConstraintValidation,
            &[one]
        ));
        assert!(transition_kinds_compatible(
            TransitionKind::IndexBuild,
            &[one],
            TransitionKind::IndexBuild,
            &[one]
        ));
        assert!(transition_kinds_compatible(
            TransitionKind::IndexBuild,
            &[one],
            TransitionKind::ConstraintValidation,
            &[one]
        ));
    }
}
