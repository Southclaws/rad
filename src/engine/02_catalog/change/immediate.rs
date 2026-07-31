use std::collections::HashSet;

use super::admission::{TransitionCandidate, affected_column_schema_ids, index_column_schema_ids};
use super::*;
use crate::engine::catalog::identity::ColumnId;
use crate::engine::catalog::model::{ColumnDraft, DefaultValue, ReclamationKind, TransitionKind};
use crate::engine::catalog::naming;

impl Mutation<'_> {
    pub async fn rename_table(&mut self, old_name: &str, new_name: &str) -> Result<()> {
        if old_name == new_name {
            return Ok(());
        }
        if new_name.is_empty() {
            return Err(input("catalog: table name is required"));
        }
        let old_key = store::table_name_key(old_name);
        let new_key = store::table_name_key(new_name);
        let Some(raw_id) = self.view.get(&old_key).await? else {
            return Err(input(format!("catalog: table {old_name:?} does not exist")));
        };
        if self.view.get(&new_key).await?.is_some() {
            return Err(input(format!("catalog: table {new_name:?} already exists")));
        }
        let id = TableId::from(std::str::from_utf8(&raw_id).map_err(|error| {
            Error::source(
                ErrorKind::CatalogCorrupt,
                format!("catalog: table name {old_name:?} contains a non-UTF-8 ID"),
                error,
            )
        })?);
        let mut table = store::get_table_by_id(self.view, &id)
            .await?
            .ok_or_else(|| {
                Error::message(
                    ErrorKind::CatalogCorrupt,
                    format!("catalog: table {old_name:?} metadata is missing"),
                )
            })?;
        for index in &mut table.indexes {
            if index.name == naming::index(old_name, &index.columns, index.unique) {
                index.name = naming::index(new_name, &index.columns, index.unique);
            }
        }
        for foreign_key in &mut table.foreign_keys {
            if foreign_key.columns.len() == 1
                && foreign_key.name == naming::foreign_key(old_name, &foreign_key.columns[0])
            {
                foreign_key.name = naming::foreign_key(new_name, &foreign_key.columns[0]);
            }
        }
        table.name = new_name.to_owned();
        store::save_table(self.view, &mut table).await?;
        store::delete_table_name(self.view, old_name).await?;
        store::save_table_name(self.view, new_name, &id).await?;
        self.mark_schema_changed();
        Ok(())
    }

    pub async fn create_column(&mut self, table_name: &str, draft: ColumnDraft) -> Result<Table> {
        let mut table = required_table(self.view, table_name).await?;
        if table.column(&draft.name).is_some() {
            return Err(input(format!(
                "catalog: column {:?} already exists in table {table_name:?}",
                draft.name
            )));
        }
        let id = self.assign_column_definition_id(&table, draft.id).await?;
        let definition = AssignedColumn {
            id,
            name: draft.name,
            scalar_type: draft.scalar_type,
            nullable: draft.nullable,
            format: draft.format,
            default: draft.default,
        };
        validate_column_definition(&definition)?;
        if !definition.nullable
            && definition
                .default
                .as_ref()
                .is_none_or(|value| value.function.is_some())
        {
            return Err(input(format!(
                "catalog: new column {:?} must be nullable or have a literal default (existing rows need a value)",
                definition.name
            )));
        }
        table
            .columns
            .push(build_column(self.view, definition).await?);
        store::save_table(self.view, &mut table).await?;
        self.mark_schema_changed();
        Ok(table)
    }

    async fn assign_column_definition_id(
        &mut self,
        table: &Table,
        requested: Option<SchemaId>,
    ) -> Result<SchemaId> {
        let mut used = HashSet::new();
        let mut maximum = 0;
        for revision in store::revisions(self.view).await? {
            if let Some(historical) = revision
                .schema
                .tables
                .iter()
                .find(|candidate| candidate.id == table.schema_id)
            {
                for column in &historical.columns {
                    used.insert(column.id);
                    maximum = maximum.max(column.id.get());
                }
            }
        }
        for column in &table.columns {
            used.insert(column.schema_id);
            maximum = maximum.max(column.schema_id.get());
        }
        match requested {
            Some(id) if used.contains(&id) => Err(input(format!(
                "catalog: column schema ID {id} on table {:?} has already been used",
                table.name
            ))),
            Some(id) => Ok(id),
            None => next_schema_id(maximum),
        }
    }

    pub async fn change_column_insert_default(
        &mut self,
        table_name: &str,
        column_name: &str,
        value: Option<DefaultValue>,
    ) -> Result<Table> {
        let mut table = required_table(self.view, table_name).await?;
        let column = table.column(column_name).cloned().ok_or_else(|| {
            input(format!(
                "catalog: column {column_name:?} does not exist in table {table_name:?}"
            ))
        })?;
        validate_column_definition(&AssignedColumn {
            id: column.schema_id,
            name: column.name.clone(),
            scalar_type: column.scalar_type,
            nullable: column.nullable,
            format: column.format.clone(),
            default: value.clone(),
        })?;
        if column.insert_default == value {
            return Ok(table);
        }
        for transition in store::list_transitions(self.view).await? {
            if transition.table_id == table.id
                && transition.kind == TransitionKind::ColumnReplacement
                && !transition.state.is_terminal()
                && transition_affects_schema_id(&table, &transition, column.schema_id)?
            {
                return Err(input(format!(
                    "catalog: cannot change insert default for column {column_name:?} during active replacement transition {:?}",
                    transition.id
                )));
            }
        }
        table
            .columns
            .iter_mut()
            .find(|candidate| candidate.id == column.id)
            .expect("column was resolved from this table")
            .insert_default = value;
        let mut protocol = store::read_write_protocol(self.view, &table).await?;
        protocol.generation = protocol.generation.next();
        table.write_protocol_generation = protocol.generation;
        self.save_write_protocol(protocol).await?;
        store::save_table(self.view, &mut table).await?;
        self.mark_schema_changed();
        Ok(table)
    }

    pub async fn rename_column(
        &mut self,
        table_name: &str,
        old_name: &str,
        new_name: &str,
    ) -> Result<Table> {
        let mut table = required_table(self.view, table_name).await?;
        if old_name == new_name {
            return Ok(table);
        }
        if table.column(old_name).is_none() {
            return Err(input(format!(
                "catalog: column {old_name:?} does not exist in table {table_name:?}"
            )));
        }
        if table.column(new_name).is_some() {
            return Err(input(format!(
                "catalog: column {new_name:?} already exists in table {table_name:?}"
            )));
        }
        for index in &mut table.indexes {
            let generated = index.name == naming::index(table_name, &index.columns, index.unique);
            rename_values(&mut index.columns, old_name, new_name);
            if generated {
                index.name = naming::index(table_name, &index.columns, index.unique);
            }
        }
        let table_id = table.id.clone();
        for foreign_key in &mut table.foreign_keys {
            let generated = foreign_key.columns.len() == 1
                && foreign_key.name == naming::foreign_key(table_name, &foreign_key.columns[0]);
            rename_values(&mut foreign_key.columns, old_name, new_name);
            if generated {
                foreign_key.name = naming::foreign_key(table_name, &foreign_key.columns[0]);
            }
            if foreign_key.ref_table_id == table_id {
                rename_values(&mut foreign_key.ref_columns, old_name, new_name);
            }
        }
        rename_values(&mut table.primary_key, old_name, new_name);
        table
            .columns
            .iter_mut()
            .find(|column| column.name == old_name)
            .expect("column existence checked")
            .name = new_name.to_owned();
        store::save_table(self.view, &mut table).await?;

        for mut referencing in store::list_tables(self.view).await? {
            if referencing.id == table.id {
                continue;
            }
            let mut changed = false;
            for foreign_key in &mut referencing.foreign_keys {
                if foreign_key.ref_table_id != table.id {
                    continue;
                }
                let before = foreign_key.ref_columns.clone();
                rename_values(&mut foreign_key.ref_columns, old_name, new_name);
                changed |= before != foreign_key.ref_columns;
            }
            if changed {
                store::save_table(self.view, &mut referencing).await?;
            }
        }
        self.mark_schema_changed();
        Ok(table)
    }

    pub async fn delete_table(&mut self, table_name: &str) -> Result<()> {
        let name_key = store::table_name_key(table_name);
        let Some(raw_id) = self.view.get(&name_key).await? else {
            return Err(input(format!(
                "catalog: table {table_name:?} does not exist"
            )));
        };
        let id = TableId::from(std::str::from_utf8(&raw_id).map_err(|error| {
            Error::source(
                ErrorKind::CatalogCorrupt,
                format!("catalog: table {table_name:?} contains a non-UTF-8 ID"),
                error,
            )
        })?);
        let tables = store::list_tables(self.view).await?;
        let deleting = tables
            .iter()
            .find(|table| table.id == id)
            .cloned()
            .ok_or_else(|| {
                Error::message(
                    ErrorKind::CatalogCorrupt,
                    format!("catalog: table {table_name:?} metadata is missing"),
                )
            })?;
        for table in &tables {
            if table.id == deleting.id {
                continue;
            }
            if let Some(foreign_key) = table
                .foreign_keys
                .iter()
                .find(|foreign_key| foreign_key.ref_table_id == deleting.id)
            {
                return Err(input(format!(
                    "catalog: table {table_name:?} is referenced by foreign key {:?} on table {:?}; delete that table first",
                    foreign_key.name, table.name
                )));
            }
        }
        if let Some(transition) =
            store::list_transitions(self.view)
                .await?
                .into_iter()
                .find(|transition| {
                    transition.table_id == deleting.id && !transition.state.is_terminal()
                })
        {
            return Err(input(format!(
                "catalog: table {table_name:?} has active transition {:?}; cancel or finish it before deleting the table",
                transition.id
            )));
        }
        store::advance_table_existence_fence(self.view, &deleting).await?;
        self.retire_table(&deleting).await?;
        store::delete_table_metadata(self.view, &deleting.id).await?;
        store::delete_table_name(self.view, table_name).await?;
        self.mark_schema_changed();
        Ok(())
    }

    pub async fn delete_column(&mut self, table_name: &str, column_name: &str) -> Result<Table> {
        let mut table = required_table(self.view, table_name).await?;
        let column = table.column(column_name).cloned().ok_or_else(|| {
            input(format!(
                "catalog: column {column_name:?} does not exist in table {table_name:?}"
            ))
        })?;
        for transition in store::list_transitions(self.view).await? {
            if transition.table_id != table.id || transition.state.is_terminal() {
                continue;
            }
            if affected_column_schema_ids(&table, &transition)?.contains(&column.schema_id) {
                return Err(input(format!(
                    "catalog: column {column_name:?} has active {:?} transition {:?}",
                    transition.kind, transition.id
                )));
            }
        }
        if table.primary_key.iter().any(|name| name == column_name) {
            return Err(input(format!(
                "catalog: cannot delete primary key column {column_name:?}"
            )));
        }
        if let Some(index) = table
            .indexes
            .iter()
            .find(|index| index.columns.iter().any(|name| name == column_name))
        {
            return Err(input(format!(
                "catalog: column {column_name:?} is used by index {:?}; delete the index first",
                index.name
            )));
        }
        if let Some(foreign_key) = table
            .foreign_keys
            .iter()
            .find(|foreign_key| foreign_key.columns.iter().any(|name| name == column_name))
        {
            return Err(input(format!(
                "catalog: column {column_name:?} is used by foreign key {:?}; delete the foreign key first",
                foreign_key.name
            )));
        }
        store::advance_column_value_fence(self.view, &table, &column).await?;
        self.retire_column(&table, &column.id).await?;
        table.columns.retain(|candidate| candidate.id != column.id);
        table
            .constraints
            .retain(|constraint| !constraint.column_ids.contains(&column.id));
        store::save_table(self.view, &mut table).await?;
        self.mark_schema_changed();
        Ok(table)
    }

    pub async fn create_index(&mut self, table_name: &str, definition: IndexDef) -> Result<Index> {
        let mut table = required_table(self.view, table_name).await?;
        self.ensure_index_name_available(&table, &definition.name)
            .await?;
        let probe = Index {
            name: definition.name.clone(),
            columns: definition.columns.clone(),
            unique: definition.unique,
            ..Index::default()
        };
        let affected = index_column_schema_ids(&table, &probe)?;
        let (_, waiting) = self
            .validate_transition_admission(
                &table,
                TransitionCandidate {
                    kind: TransitionKind::IndexBuild,
                    table_id: table.id.clone(),
                    affected_column_ids: affected,
                    prerequisites: Vec::new(),
                },
            )
            .await?;
        if waiting {
            return Err(Error::message(
                ErrorKind::CatalogDrift,
                "catalog: synchronous index creation unexpectedly requires waiting",
            ));
        }
        let index = build_index(self.view, &table, definition, IndexState::Ready).await?;
        table.indexes.push(index.clone());
        let mut protocol = store::read_write_protocol(self.view, &table).await?;
        protocol.generation = protocol.generation.next();
        if !protocol
            .ready_indexes
            .iter()
            .any(|candidate| candidate.logical_id == index.logical_id && candidate.id == index.id)
        {
            protocol.ready_indexes.push(index.clone());
        }
        table.write_protocol_generation = protocol.generation;
        store::save_table(self.view, &mut table).await?;
        self.save_write_protocol(protocol).await?;
        self.mark_schema_changed();
        Ok(index)
    }

    pub async fn delete_index(&mut self, table_name: &str, index_name: &str) -> Result<()> {
        let mut table = required_table(self.view, table_name).await?;
        let index = table.index(index_name).cloned().ok_or_else(|| {
            input(format!(
                "catalog: index {index_name:?} does not exist on table {table_name:?}"
            ))
        })?;
        if !index.is_ready() {
            return Err(input(format!(
                "catalog: index {index_name:?} on table {table_name:?} is not ready; cancel its transition instead"
            )));
        }
        store::advance_index_access_fence(self.view, &table, &index).await?;
        self.retire_index(&table, &index).await?;
        let mut protocol = store::read_write_protocol(self.view, &table).await?;
        protocol.generation = protocol.generation.next();
        protocol.ready_indexes.retain(|candidate| {
            if !index.logical_id.is_empty() {
                candidate.logical_id != index.logical_id
            } else {
                candidate.id != index.id
            }
        });
        table.write_protocol_generation = protocol.generation;
        table.indexes.retain(|candidate| candidate.id != index.id);
        self.save_write_protocol(protocol).await?;
        store::save_table(self.view, &mut table).await?;
        self.mark_schema_changed();
        Ok(())
    }

    async fn retire_table(&mut self, table: &Table) -> Result<()> {
        let mut reclamation = self
            .pending_reclamation(
                store::table_reclamation_id(&table.id),
                ReclamationKind::Table,
            )
            .await?;
        reclamation.table_id = table.id.clone();
        reclamation.table_schema_id = Some(table.schema_id);
        reclamation.index_ids = table.indexes.iter().map(|index| index.id.clone()).collect();
        reclamation.index_ids.sort();
        self.queue_reclamation(reclamation).await
    }

    async fn retire_column(&mut self, table: &Table, column_id: &ColumnId) -> Result<()> {
        let mut reclamation = self
            .pending_reclamation(
                store::column_reclamation_id(&table.id, column_id),
                ReclamationKind::Column,
            )
            .await?;
        reclamation.table_id = table.id.clone();
        reclamation.table_schema_id = Some(table.schema_id);
        reclamation.column_id = column_id.clone();
        self.queue_reclamation(reclamation).await
    }

    async fn retire_index(&mut self, table: &Table, index: &Index) -> Result<()> {
        let mut reclamation = self
            .pending_reclamation(
                store::index_reclamation_id(&table.id, &index.id),
                ReclamationKind::Index,
            )
            .await?;
        reclamation.table_id = table.id.clone();
        reclamation.table_schema_id = Some(table.schema_id);
        reclamation.index_id = index.id.clone();
        self.queue_reclamation(reclamation).await
    }
}

impl Service {
    pub async fn rename_table(&self, old_name: &str, new_name: &str) -> Result<()> {
        run_mutation!(self, |mutation| mutation.rename_table(old_name, new_name))
    }

    pub async fn create_column(&self, table: &str, draft: ColumnDraft) -> Result<Table> {
        run_mutation!(self, |mutation| mutation.create_column(table, draft))
    }

    pub async fn change_column_insert_default(
        &self,
        table: &str,
        column: &str,
        value: Option<DefaultValue>,
    ) -> Result<Table> {
        run_mutation!(self, |mutation| mutation
            .change_column_insert_default(table, column, value))
    }

    pub async fn rename_column(
        &self,
        table: &str,
        old_name: &str,
        new_name: &str,
    ) -> Result<Table> {
        run_mutation!(self, |mutation| mutation
            .rename_column(table, old_name, new_name))
    }

    pub async fn delete_table(&self, table: &str) -> Result<()> {
        run_mutation!(self, |mutation| mutation.delete_table(table))
    }

    pub async fn delete_column(&self, table: &str, column: &str) -> Result<Table> {
        run_mutation!(self, |mutation| mutation.delete_column(table, column))
    }

    pub async fn create_index(&self, table: &str, definition: IndexDef) -> Result<Index> {
        run_mutation!(self, |mutation| mutation.create_index(table, definition))
    }

    pub async fn delete_index(&self, table: &str, index: &str) -> Result<()> {
        run_mutation!(self, |mutation| mutation.delete_index(table, index))
    }
}

async fn required_table(view: &mut dyn KvView, name: &str) -> Result<Table> {
    store::get_table(view, name)
        .await?
        .ok_or_else(|| input(format!("catalog: table {name:?} does not exist")))
}

fn rename_values(values: &mut [String], old: &str, new: &str) {
    for value in values {
        if value == old {
            *value = new.to_owned();
        }
    }
}

fn transition_affects_schema_id(
    table: &Table,
    transition: &super::super::model::SchemaTransition,
    schema_id: SchemaId,
) -> Result<bool> {
    if !transition.affected_column_ids.is_empty() {
        return Ok(transition.affected_column_ids.contains(&schema_id));
    }
    if let Some(request) = &transition.replacement_request {
        return Ok(request.column_schema_id == schema_id);
    }
    if let Some(replacement) = &transition.column_replacement {
        return Ok(replacement.source.schema_id == schema_id);
    }
    if transition.kind == TransitionKind::ColumnReplacement {
        return Err(Error::message(
            ErrorKind::CatalogDrift,
            format!(
                "catalog: replacement transition {:?} has no affected-column identity on table {:?}",
                transition.id, table.name
            ),
        ));
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::engine::catalog::model::{
        DefaultFunction, ForeignKeyDef, IndexDef, ScalarType, TableDraft,
    };
    use crate::engine::kv::slatedb;
    use crate::engine::kv::{IsolationLevel, TransactionView, TransactionalKv};

    use super::*;

    fn users() -> TableDraft {
        TableDraft {
            id: None,
            name: "users".into(),
            columns: vec![
                ColumnDraft {
                    id: None,
                    name: "id".into(),
                    scalar_type: ScalarType::Int64,
                    nullable: false,
                    format: String::new(),
                    default: None,
                },
                ColumnDraft {
                    id: None,
                    name: "name".into(),
                    scalar_type: ScalarType::Text,
                    nullable: false,
                    format: String::new(),
                    default: None,
                },
                ColumnDraft {
                    id: None,
                    name: "age".into(),
                    scalar_type: ScalarType::Int64,
                    nullable: true,
                    format: String::new(),
                    default: None,
                },
            ],
            primary_key: vec!["id".into()],
            indexes: vec![IndexDef {
                name: "users_name_idx".into(),
                columns: vec!["name".into()],
                unique: false,
            }],
            foreign_keys: Vec::new(),
        }
    }

    async fn service(path: &str) -> (Arc<slatedb::Store>, Service) {
        let database = Arc::new(slatedb::Store::memory(path).await.unwrap());
        let service = Service::new(database.clone());
        (database, service)
    }

    #[tokio::test]
    async fn table_rename_preserves_identity_releases_name_and_noop_does_not_publish() {
        let (database, service) = service("catalog-immediate-rename-table").await;
        let before = service.create_table(users()).await.unwrap();
        service.rename_table("users", "people").await.unwrap();
        let after = service.get_table("people").await.unwrap().unwrap();
        assert_eq!(after.id, before.id);
        assert_eq!(after.schema_id, before.schema_id);
        assert!(service.get_table("users").await.unwrap().is_none());
        assert_eq!(service.revision().await.unwrap().version.get(), 2);
        service.rename_table("people", "people").await.unwrap();
        assert_eq!(service.revision().await.unwrap().version.get(), 2);
        service.create_table(users()).await.unwrap();
        assert_eq!(service.revision().await.unwrap().version.get(), 3);
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn column_creation_preserves_missing_value_and_never_reuses_schema_identity() {
        let (database, service) = service("catalog-immediate-columns").await;
        let created = service.create_table(users()).await.unwrap();
        let retired = created.column("age").unwrap().schema_id;
        service.delete_column("users", "age").await.unwrap();
        let error = service
            .create_column(
                "users",
                ColumnDraft {
                    id: Some(retired),
                    name: "replacement".into(),
                    scalar_type: ScalarType::Text,
                    nullable: true,
                    format: String::new(),
                    default: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);

        let table = service
            .create_column(
                "users",
                ColumnDraft {
                    id: None,
                    name: "status".into(),
                    scalar_type: ScalarType::Text,
                    nullable: false,
                    format: String::new(),
                    default: Some(DefaultValue {
                        text: "active".into(),
                        ..DefaultValue::default()
                    }),
                },
            )
            .await
            .unwrap();
        let status = table.column("status").unwrap();
        assert_eq!(status.schema_id.get(), retired.get() + 1);
        assert_eq!(status.insert_default.as_ref().unwrap().text, "active");
        assert_eq!(status.missing_value.as_ref().unwrap().text, "active");

        let table = service
            .change_column_insert_default(
                "users",
                "status",
                Some(DefaultValue {
                    text: "pending".into(),
                    ..DefaultValue::default()
                }),
            )
            .await
            .unwrap();
        let status = table.column("status").unwrap();
        assert_eq!(status.insert_default.as_ref().unwrap().text, "pending");
        assert_eq!(status.missing_value.as_ref().unwrap().text, "active");
        assert_eq!(table.write_protocol_generation.get(), 1);

        let version = service.revision().await.unwrap().version;
        service
            .change_column_insert_default(
                "users",
                "status",
                Some(DefaultValue {
                    text: "pending".into(),
                    ..DefaultValue::default()
                }),
            )
            .await
            .unwrap();
        assert_eq!(service.revision().await.unwrap().version, version);
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn added_column_requires_nullable_or_literal_default() {
        let (database, service) = service("catalog-immediate-column-rules").await;
        service.create_table(users()).await.unwrap();
        for draft in [
            ColumnDraft {
                id: None,
                name: "required".into(),
                scalar_type: ScalarType::Text,
                nullable: false,
                format: String::new(),
                default: None,
            },
            ColumnDraft {
                id: None,
                name: "generated".into(),
                scalar_type: ScalarType::Text,
                nullable: false,
                format: String::new(),
                default: Some(DefaultValue {
                    function: Some(DefaultFunction::Uuid),
                    ..DefaultValue::default()
                }),
            },
        ] {
            assert_eq!(
                service
                    .create_column("users", draft)
                    .await
                    .unwrap_err()
                    .kind(),
                ErrorKind::InvalidInput
            );
        }
        assert_eq!(service.revision().await.unwrap().version.get(), 1);
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn column_rename_rewrites_local_and_referencing_metadata() {
        let (database, service) = service("catalog-immediate-rename-column").await;
        let parent = service.create_table(users()).await.unwrap();
        let before = parent.column("id").unwrap().id.clone();
        service
            .create_table(TableDraft {
                id: None,
                name: "orders".into(),
                columns: vec![
                    ColumnDraft {
                        id: None,
                        name: "id".into(),
                        scalar_type: ScalarType::Int64,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                    ColumnDraft {
                        id: None,
                        name: "user_id".into(),
                        scalar_type: ScalarType::Int64,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                ],
                primary_key: vec!["id".into()],
                indexes: Vec::new(),
                foreign_keys: vec![ForeignKeyDef {
                    name: "orders_user_fk".into(),
                    columns: vec!["user_id".into()],
                    ref_table: "users".into(),
                    ref_columns: vec!["id".into()],
                }],
            })
            .await
            .unwrap();
        let renamed = service
            .rename_column("users", "id", "user_key")
            .await
            .unwrap();
        assert_eq!(renamed.column("user_key").unwrap().id, before);
        assert_eq!(renamed.primary_key, vec!["user_key"]);
        let orders = service.get_table("orders").await.unwrap().unwrap();
        assert_eq!(orders.foreign_keys[0].ref_columns, vec!["user_key"]);
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn index_add_delete_updates_protocol_fence_and_reclamation() {
        let (database, service) = service("catalog-immediate-index").await;
        service.create_table(users()).await.unwrap();
        let index = service
            .create_index(
                "users",
                IndexDef {
                    name: "users_age_idx".into(),
                    columns: vec!["age".into()],
                    unique: false,
                },
            )
            .await
            .unwrap();
        let table = service.get_table("users").await.unwrap().unwrap();
        assert_eq!(table.write_protocol_generation.get(), 1);
        assert!(table.index("users_age_idx").is_some());
        service
            .delete_index("users", "users_age_idx")
            .await
            .unwrap();
        let table = service.get_table("users").await.unwrap().unwrap();
        assert_eq!(table.write_protocol_generation.get(), 2);
        assert!(table.index("users_age_idx").is_none());

        let mut transaction = database.begin(IsolationLevel::Snapshot).await.unwrap();
        let reclamations = {
            let mut view = TransactionView(transaction.as_mut());
            store::list_reclamations(&mut view).await.unwrap()
        };
        transaction.rollback();
        assert!(reclamations.iter().any(|reclamation| {
            reclamation.kind == ReclamationKind::Index && reclamation.index_id == index.id
        }));
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn deletion_guards_foreign_keys_primary_keys_and_indexes() {
        let (database, service) = service("catalog-immediate-delete-guards").await;
        service.create_table(users()).await.unwrap();
        assert_eq!(
            service
                .delete_column("users", "id")
                .await
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidInput
        );
        assert_eq!(
            service
                .delete_column("users", "name")
                .await
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidInput
        );
        let table = service.delete_column("users", "age").await.unwrap();
        assert!(table.column("age").is_none());
        service
            .create_table(TableDraft {
                id: None,
                name: "orders".into(),
                columns: vec![
                    ColumnDraft {
                        id: None,
                        name: "id".into(),
                        scalar_type: ScalarType::Int64,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                    ColumnDraft {
                        id: None,
                        name: "user_id".into(),
                        scalar_type: ScalarType::Int64,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                ],
                primary_key: vec!["id".into()],
                indexes: Vec::new(),
                foreign_keys: vec![ForeignKeyDef {
                    name: "orders_user_fk".into(),
                    columns: vec!["user_id".into()],
                    ref_table: "users".into(),
                    ref_columns: vec!["id".into()],
                }],
            })
            .await
            .unwrap();
        assert_eq!(
            service.delete_table("users").await.unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
        service.delete_table("orders").await.unwrap();
        service
            .delete_index("users", "users_name_idx")
            .await
            .unwrap();
        service.delete_table("users").await.unwrap();
        assert!(service.get_table("users").await.unwrap().is_none());

        let self_referencing = TableDraft {
            id: None,
            name: "nodes".into(),
            columns: vec![
                ColumnDraft {
                    id: None,
                    name: "id".into(),
                    scalar_type: ScalarType::Int64,
                    nullable: false,
                    format: String::new(),
                    default: None,
                },
                ColumnDraft {
                    id: None,
                    name: "parent_id".into(),
                    scalar_type: ScalarType::Int64,
                    nullable: true,
                    format: String::new(),
                    default: None,
                },
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: vec![ForeignKeyDef {
                name: "nodes_parent_fk".into(),
                columns: vec!["parent_id".into()],
                ref_table: "nodes".into(),
                ref_columns: vec!["id".into()],
            }],
        };
        service.create_table(self_referencing).await.unwrap();
        service.delete_table("nodes").await.unwrap();
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn caller_owned_mutation_groups_multiple_changes_into_one_revision() {
        let database = slatedb::Store::memory("catalog-immediate-grouped")
            .await
            .unwrap();
        let mut transaction = database
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        let revision = {
            let mut view = TransactionView(transaction.as_mut());
            let mut mutation = Mutation::new(&mut view);
            mutation.create_table(users()).await.unwrap();
            mutation
                .create_column(
                    "users",
                    ColumnDraft {
                        id: None,
                        name: "bio".into(),
                        scalar_type: ScalarType::Text,
                        nullable: true,
                        format: String::new(),
                        default: None,
                    },
                )
                .await
                .unwrap();
            mutation.finish().await.unwrap()
        };
        transaction.commit().await.unwrap();
        assert_eq!(revision.version.get(), 1);
        database.close().await.unwrap();
    }
}
