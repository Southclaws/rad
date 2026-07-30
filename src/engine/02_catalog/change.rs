use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::identity::{
    AccessGeneration, DefinitionGeneration, ExistenceGeneration, LogicalIndexId, SchemaId, TableId,
    ValueGeneration, WriteProtocolGeneration,
};
use super::model::{
    Column, DefaultFunction, ForeignKey, Index, IndexDef, IndexState, Mode, Reclamation, Revision,
    ScalarType, Table, TableDraft, WriteProtocol,
};
use super::store;
use super::{Error, ErrorKind, Result};
use crate::engine::kv::{IsolationLevel, KvView, TransactionView, TransactionalKv};
use crate::runtime::{RuntimeEffects, SystemRuntime};

mod admission;
mod constraints;
mod identity;
mod immediate;
mod replacements;
mod transitions;

/// Transaction-scoped catalog mutation surface. Callers such as PIR execution
/// can compose multiple catalog and data effects in one Slate transaction and
/// publish at most one canonical revision with [`Mutation::finish`].
pub struct Mutation<'a> {
    view: &'a mut dyn KvView,
    runtime: Arc<dyn RuntimeEffects>,
    catalog_changed: bool,
    schema_changed: bool,
}

impl<'a> Mutation<'a> {
    pub fn new(view: &'a mut dyn KvView) -> Self {
        Self::with_runtime(view, Arc::new(SystemRuntime))
    }

    pub fn with_runtime(view: &'a mut dyn KvView, runtime: Arc<dyn RuntimeEffects>) -> Self {
        Self {
            view,
            runtime,
            catalog_changed: false,
            schema_changed: false,
        }
    }

    pub(crate) fn now(&self) -> super::model::Timestamp {
        self.runtime.now().into()
    }

    pub(crate) async fn save_write_protocol(&mut self, protocol: WriteProtocol) -> Result<()> {
        let now = self.now();
        store::save_write_protocol(self.view, protocol, now).await
    }

    pub(crate) async fn queue_reclamation(&mut self, reclamation: Reclamation) -> Result<()> {
        let now = self.now();
        store::queue_reclamation(self.view, reclamation, now).await
    }

    pub fn schema_changed(&self) -> bool {
        self.schema_changed
    }

    pub fn catalog_changed(&self) -> bool {
        self.catalog_changed
    }

    pub(crate) fn mark_catalog_changed(&mut self) {
        self.catalog_changed = true;
    }

    pub(crate) fn mark_schema_changed(&mut self) {
        self.catalog_changed = true;
        self.schema_changed = true;
    }

    pub(crate) fn view(&self) -> &dyn KvView {
        self.view
    }

    pub async fn finish(&mut self) -> Result<Revision> {
        if self.catalog_changed {
            store::bump_catalog_generation(self.view).await?;
        }
        if self.schema_changed {
            store::bump_revision(self.view, self.now()).await
        } else {
            store::current_revision(self.view).await
        }
    }

    pub async fn create_table(&mut self, draft: TableDraft) -> Result<Table> {
        let definition = self.assign_table_definition_ids(draft).await?;
        if definition.name.is_empty() {
            return Err(input("catalog: table name is required"));
        }
        let name_key = store::table_name_key(&definition.name);
        if self.view.get(&name_key).await?.is_some() {
            return Err(input(format!(
                "catalog: table {:?} already exists",
                definition.name
            )));
        }

        let table_id: TableId = store::next_physical_id(self.view, "t").await?.into();
        let mut table = Table {
            id: table_id,
            schema_id: definition.id,
            name: definition.name,
            definition_generation: DefinitionGeneration::ZERO,
            existence_generation: ExistenceGeneration::ZERO,
            write_protocol_generation: WriteProtocolGeneration::ZERO,
            columns: Vec::with_capacity(definition.columns.len()),
            primary_key: Vec::new(),
            indexes: Vec::with_capacity(definition.indexes.len()),
            foreign_keys: Vec::with_capacity(definition.foreign_keys.len()),
            constraints: Vec::new(),
        };

        let mut seen = HashSet::new();
        for definition in definition.columns {
            if !seen.insert(definition.name.clone()) {
                return Err(input(format!(
                    "catalog: duplicate column {:?}",
                    definition.name
                )));
            }
            validate_column_definition(&definition)?;
            table
                .columns
                .push(build_column(self.view, definition).await?);
        }

        if definition.primary_key.is_empty() {
            return Err(input(format!(
                "catalog: table {:?} needs a primary key",
                table.name
            )));
        }
        for name in &definition.primary_key {
            let Some(column) = table.column(name) else {
                return Err(input(format!(
                    "catalog: primary key column {name:?} does not exist"
                )));
            };
            if column.nullable {
                return Err(input(format!(
                    "catalog: primary key column {name:?} must not be nullable"
                )));
            }
        }
        table.primary_key = definition.primary_key;

        for index in definition.indexes {
            table
                .indexes
                .push(build_index(self.view, &table, index, IndexState::Ready).await?);
        }

        for foreign_key in definition.foreign_keys {
            let referenced = if foreign_key.ref_table == table.name {
                table.clone()
            } else {
                store::get_table(self.view, &foreign_key.ref_table)
                    .await?
                    .ok_or_else(|| {
                        input(format!(
                            "catalog: foreign key {:?} references unknown table {:?}",
                            foreign_key.name, foreign_key.ref_table
                        ))
                    })?
            };
            if foreign_key.ref_columns != referenced.primary_key {
                return Err(input(format!(
                    "catalog: foreign key {:?} must reference {:?}'s primary key",
                    foreign_key.name, foreign_key.ref_table
                )));
            }
            if foreign_key.columns.len() != foreign_key.ref_columns.len() {
                return Err(input(format!(
                    "catalog: foreign key {:?} column count mismatch",
                    foreign_key.name
                )));
            }
            for (column_name, referenced_name) in
                foreign_key.columns.iter().zip(&foreign_key.ref_columns)
            {
                let column = table.column(column_name).ok_or_else(|| {
                    input(format!(
                        "catalog: foreign key {:?} references unknown column {column_name:?}",
                        foreign_key.name
                    ))
                })?;
                let referenced_column = referenced.column(referenced_name).ok_or_else(|| {
                    Error::message(
                        ErrorKind::CatalogDrift,
                        format!(
                            "catalog: referenced primary-key column {referenced_name:?} is missing"
                        ),
                    )
                })?;
                if column.scalar_type != referenced_column.scalar_type {
                    return Err(input(format!(
                        "catalog: foreign key {:?} type mismatch on {column_name:?}",
                        foreign_key.name
                    )));
                }
            }
            table.foreign_keys.push(ForeignKey {
                id: store::next_physical_id(self.view, "fk").await?,
                name: foreign_key.name,
                columns: foreign_key.columns,
                ref_table_id: referenced.id,
                ref_columns: foreign_key.ref_columns,
            });
        }

        store::save_table(self.view, &mut table).await?;
        store::save_table_name(self.view, &table.name, &table.id).await?;
        self.mark_schema_changed();
        Ok(table)
    }

    async fn assign_table_definition_ids(&mut self, draft: TableDraft) -> Result<AssignedTable> {
        let (used, maximum) = self.used_table_schema_ids().await?;
        let id = match draft.id {
            Some(id) if used.contains(&id) => {
                return Err(input(format!(
                    "catalog: table schema ID {id} has already been used"
                )));
            }
            Some(id) => id,
            None => next_schema_id(maximum)?,
        };

        let mut seen = HashMap::new();
        let mut maximum = 0;
        for column in &draft.columns {
            if let Some(column_id) = column.id {
                if let Some(previous) = seen.insert(column_id, column.name.clone()) {
                    return Err(input(format!(
                        "catalog: columns {previous:?} and {:?} on table {:?} share schema ID {column_id}",
                        column.name, draft.name
                    )));
                }
                maximum = maximum.max(column_id.get());
            }
        }
        let mut columns = Vec::with_capacity(draft.columns.len());
        for column in draft.columns {
            let column_id = match column.id {
                Some(id) => id,
                None => {
                    let id = next_schema_id(maximum)?;
                    maximum = id.get();
                    id
                }
            };
            columns.push(AssignedColumn {
                id: column_id,
                name: column.name,
                scalar_type: column.scalar_type,
                nullable: column.nullable,
                format: column.format,
                default: column.default,
            });
        }
        Ok(AssignedTable {
            id,
            name: draft.name,
            columns,
            primary_key: draft.primary_key,
            indexes: draft.indexes,
            foreign_keys: draft.foreign_keys,
        })
    }

    async fn used_table_schema_ids(&mut self) -> Result<(HashSet<SchemaId>, u32)> {
        let mut used = HashSet::new();
        let mut maximum = 0;
        for revision in store::revisions(self.view).await? {
            for table in revision.schema.tables {
                maximum = maximum.max(table.id.get());
                used.insert(table.id);
            }
        }
        for table in store::list_tables(self.view).await? {
            maximum = maximum.max(table.schema_id.get());
            used.insert(table.schema_id);
        }
        Ok((used, maximum))
    }
}

struct AssignedTable {
    id: SchemaId,
    name: String,
    columns: Vec<AssignedColumn>,
    primary_key: Vec<String>,
    indexes: Vec<IndexDef>,
    foreign_keys: Vec<super::model::ForeignKeyDef>,
}

struct AssignedColumn {
    id: SchemaId,
    name: String,
    scalar_type: ScalarType,
    nullable: bool,
    format: String,
    default: Option<super::model::DefaultValue>,
}

fn validate_column_definition(column: &AssignedColumn) -> Result<()> {
    if let Some(default) = &column.default
        && let Some(function) = default.function
    {
        let valid = matches!(
            (function, column.scalar_type),
            (DefaultFunction::Uuid, ScalarType::Text) | (DefaultFunction::NowMs, ScalarType::Int64)
        );
        if !valid {
            return Err(input(format!(
                "catalog: column {:?}: default function does not support {:?}",
                column.name, column.scalar_type
            )));
        }
    }
    Ok(())
}

async fn build_column(view: &mut dyn KvView, definition: AssignedColumn) -> Result<Column> {
    let missing_value = definition
        .default
        .as_ref()
        .filter(|value| value.function.is_none())
        .cloned();
    Ok(Column {
        id: store::next_physical_id(view, "c").await?.into(),
        schema_id: definition.id,
        name: definition.name,
        value_generation: ValueGeneration::ZERO,
        scalar_type: definition.scalar_type,
        nullable: definition.nullable,
        format: definition.format,
        insert_default: definition.default,
        missing_value,
    })
}

async fn build_index(
    view: &mut dyn KvView,
    table: &Table,
    definition: IndexDef,
    state: IndexState,
) -> Result<Index> {
    if definition.columns.is_empty() {
        return Err(input(format!(
            "catalog: index {:?} has no columns",
            definition.name
        )));
    }
    let mut column_ids = Vec::with_capacity(definition.columns.len());
    for name in &definition.columns {
        column_ids.push(
            table
                .column(name)
                .ok_or_else(|| {
                    input(format!(
                        "catalog: index {:?} references unknown column {name:?}",
                        definition.name
                    ))
                })?
                .id
                .clone(),
        );
    }
    Ok(Index {
        id: store::next_physical_id(view, "i").await?.into(),
        logical_id: LogicalIndexId::from(store::next_physical_id(view, "ix").await?),
        definition_generation: DefinitionGeneration::from(1),
        access_generation: AccessGeneration::ZERO,
        state,
        name: definition.name,
        columns: definition.columns,
        column_ids,
        unique: definition.unique,
    })
}

fn next_schema_id(current: u32) -> Result<SchemaId> {
    SchemaId::new(current.checked_add(1).ok_or_else(|| {
        Error::message(
            ErrorKind::InvalidInput,
            "catalog: schema ID space exhausted",
        )
    })?)
    .map_err(|_| {
        Error::message(
            ErrorKind::InvalidInput,
            "catalog: schema ID space exhausted",
        )
    })
}

fn input(message: impl Into<String>) -> Error {
    Error::message(ErrorKind::InvalidInput, message)
}

async fn complete_transaction<T>(
    transaction: Box<dyn crate::engine::kv::Transaction>,
    result: Result<T>,
) -> Result<T> {
    match result {
        Ok(value) => {
            transaction.commit().await?;
            Ok(value)
        }
        Err(error) => {
            transaction.rollback();
            Err(error)
        }
    }
}

/// Transaction-owning catalog facade. Every read pins a Slate snapshot and
/// every direct mutation uses SerializableSnapshot isolation.
pub struct Service {
    store: Arc<dyn TransactionalKv>,
    runtime: Arc<dyn RuntimeEffects>,
}

impl Service {
    pub fn new(store: Arc<dyn TransactionalKv>) -> Self {
        Self::with_runtime(store, Arc::new(SystemRuntime))
    }

    pub fn with_runtime(store: Arc<dyn TransactionalKv>, runtime: Arc<dyn RuntimeEffects>) -> Self {
        Self { store, runtime }
    }

    pub async fn create_table(&self, draft: TableDraft) -> Result<Table> {
        let mut transaction = self
            .store
            .begin(IsolationLevel::SerializableSnapshot)
            .await?;
        let result = {
            let mut view = TransactionView(transaction.as_mut());
            let mut mutation = Mutation::with_runtime(&mut view, self.runtime.clone());
            match mutation.create_table(draft).await {
                Ok(table) => mutation.finish().await.map(|_| table),
                Err(error) => Err(error),
            }
        };
        match result {
            Ok(table) => {
                transaction.commit().await?;
                Ok(table)
            }
            Err(error) => {
                transaction.rollback();
                Err(error)
            }
        }
    }

    pub async fn get_table(&self, name: &str) -> Result<Option<Table>> {
        let mut transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let result = {
            let view = TransactionView(transaction.as_mut());
            store::get_table(&view, name).await
        };
        transaction.rollback();
        result
    }

    pub async fn get_table_by_id(&self, id: &TableId) -> Result<Option<Table>> {
        let mut transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let result = {
            let view = TransactionView(transaction.as_mut());
            store::get_table_by_id(&view, id).await
        };
        transaction.rollback();
        result
    }

    pub async fn get_table_by_schema_id(&self, id: SchemaId) -> Result<Option<Table>> {
        let mut transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let result = {
            let mut view = TransactionView(transaction.as_mut());
            store::get_table_by_schema_id(&mut view, id).await
        };
        transaction.rollback();
        result
    }

    pub async fn list_tables(&self) -> Result<Vec<Table>> {
        let mut transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let result = {
            let mut view = TransactionView(transaction.as_mut());
            store::list_tables(&mut view).await
        };
        transaction.rollback();
        result
    }

    pub async fn schema(&self) -> Result<super::model::Schema> {
        let mut transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let result = {
            let mut view = TransactionView(transaction.as_mut());
            store::read_schema(&mut view).await
        };
        transaction.rollback();
        result
    }

    pub async fn validate_current_schema(&self) -> Result<()> {
        let mut transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let result = {
            let mut view = TransactionView(transaction.as_mut());
            match store::current_revision(&mut view).await {
                Err(error) => Err(error),
                Ok(revision) => store::read_schema(&mut view).await.and_then(|actual| {
                    if revision.schema.canonical_eq(&actual)? {
                        Ok(())
                    } else {
                        Err(Error::message(
                            ErrorKind::CatalogDrift,
                            format!(
                                "catalog: stored schema at version {} does not match physical catalog metadata",
                                revision.version
                            ),
                        ))
                    }
                }),
            }
        };
        transaction.rollback();
        result
    }

    pub async fn revision(&self) -> Result<Revision> {
        let mut transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let result = {
            let mut view = TransactionView(transaction.as_mut());
            store::current_revision(&mut view).await
        };
        transaction.rollback();
        result
    }

    pub async fn revisions(&self) -> Result<Vec<Revision>> {
        let mut transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let result = {
            let mut view = TransactionView(transaction.as_mut());
            store::revisions(&mut view).await
        };
        transaction.rollback();
        result
    }

    pub async fn mode(&self) -> Result<Mode> {
        let mut transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let result = {
            let mut view = TransactionView(transaction.as_mut());
            store::read_mode(&mut view).await
        };
        transaction.rollback();
        result
    }

    pub async fn init_mode(&self, requested: Option<Mode>) -> Result<Mode> {
        let mut transaction = self
            .store
            .begin(IsolationLevel::SerializableSnapshot)
            .await?;
        let result = {
            let mut view = TransactionView(transaction.as_mut());
            match store::read_stored_mode(&mut view).await {
                Err(error) => Err(error),
                Ok(Some(stored)) if requested.is_some_and(|mode| mode != stored) => {
                    Err(input(format!(
                        "catalog: this database is {stored:?}-managed and its mode is set once at creation"
                    )))
                }
                Ok(Some(stored)) => Ok(stored),
                Ok(None) => {
                    let settled = requested.unwrap_or(Mode::Direct);
                    store::set_mode(&mut view, settled).await.map(|()| settled)
                }
            }
        };
        match result {
            Ok(mode) => {
                transaction.commit().await?;
                Ok(mode)
            }
            Err(error) => {
                transaction.rollback();
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::catalog::model::{ColumnDraft, DefaultValue, ForeignKeyDef, IndexDef};
    use crate::engine::kv::TransactionalKv;
    use crate::engine::kv::slatedb;

    use super::*;

    fn users() -> TableDraft {
        TableDraft {
            id: None,
            name: "users".into(),
            columns: vec![ColumnDraft {
                id: None,
                name: "id".into(),
                scalar_type: ScalarType::Text,
                nullable: false,
                format: "uuid".into(),
                default: Some(DefaultValue {
                    function: Some(DefaultFunction::Uuid),
                    ..DefaultValue::default()
                }),
            }],
            primary_key: vec!["id".into()],
            indexes: vec![IndexDef {
                name: "users_id_uq".into(),
                columns: vec!["id".into()],
                unique: true,
            }],
            foreign_keys: Vec::new(),
        }
    }

    #[tokio::test]
    async fn direct_create_is_atomic_and_records_exactly_one_revision() {
        let database = Arc::new(
            slatedb::Store::memory("catalog-service-create")
                .await
                .unwrap(),
        );
        let service = Service::new(database.clone());
        let table = service.create_table(users()).await.unwrap();
        assert_eq!(table.schema_id, SchemaId::new(1).unwrap());
        assert_eq!(table.columns[0].schema_id, SchemaId::new(1).unwrap());
        assert_eq!(service.get_table("users").await.unwrap(), Some(table));
        assert_eq!(service.revision().await.unwrap().version.get(), 1);
        assert_eq!(service.revisions().await.unwrap().len(), 1);
        assert_eq!(
            service.create_table(users()).await.unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
        assert_eq!(service.revision().await.unwrap().version.get(), 1);
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn create_table_validation_failures_are_atomic() {
        let database = Arc::new(
            slatedb::Store::memory("catalog-service-create-validation")
                .await
                .unwrap(),
        );
        let service = Service::new(database.clone());
        service.create_table(users()).await.unwrap();

        let base = TableDraft {
            id: None,
            name: "x".into(),
            columns: vec![ColumnDraft {
                id: None,
                name: "id".into(),
                scalar_type: ScalarType::Text,
                nullable: false,
                format: String::new(),
                default: None,
            }],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        };
        let mut cases = Vec::new();

        let mut value = base.clone();
        value.name.clear();
        cases.push((value, "name is required"));

        let mut value = base.clone();
        value.columns.push(value.columns[0].clone());
        cases.push((value, "duplicate column"));

        let mut value = base.clone();
        value.primary_key.clear();
        cases.push((value, "needs a primary key"));

        let mut value = base.clone();
        value.primary_key[0] = "missing".into();
        cases.push((value, "does not exist"));

        let mut value = base.clone();
        value.columns[0].nullable = true;
        cases.push((value, "must not be nullable"));

        let mut value = base.clone();
        value.indexes.push(IndexDef {
            name: "empty".into(),
            columns: Vec::new(),
            unique: false,
        });
        cases.push((value, "has no columns"));

        let mut value = base.clone();
        value.indexes.push(IndexDef {
            name: "bad".into(),
            columns: vec!["missing".into()],
            unique: false,
        });
        cases.push((value, "unknown column"));

        let mut value = base.clone();
        value.foreign_keys.push(ForeignKeyDef {
            name: "fk".into(),
            columns: vec!["id".into()],
            ref_table: "ghost".into(),
            ref_columns: vec!["id".into()],
        });
        cases.push((value, "unknown table"));

        let mut value = base.clone();
        value.foreign_keys.push(ForeignKeyDef {
            name: "fk".into(),
            columns: vec!["id".into()],
            ref_table: "users".into(),
            ref_columns: vec!["missing".into()],
        });
        cases.push((value, "primary key"));

        let mut value = base.clone();
        value.columns.push(ColumnDraft {
            id: None,
            name: "user_id".into(),
            scalar_type: ScalarType::Int64,
            nullable: false,
            format: String::new(),
            default: None,
        });
        value.foreign_keys.push(ForeignKeyDef {
            name: "fk".into(),
            columns: vec!["user_id".into()],
            ref_table: "users".into(),
            ref_columns: vec!["id".into()],
        });
        cases.push((value, "type mismatch"));

        for (draft, expected) in cases {
            let name = draft.name.clone();
            let error = service.create_table(draft).await.unwrap_err();
            assert!(
                error.to_string().contains(expected),
                "{error} did not contain {expected:?}"
            );
            if !name.is_empty() {
                assert!(service.get_table(&name).await.unwrap().is_none());
            }
            assert_eq!(service.revision().await.unwrap().version.get(), 1);
        }
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn self_referencing_foreign_key_resolves_the_new_physical_table() {
        let database = Arc::new(
            slatedb::Store::memory("catalog-service-self-fk")
                .await
                .unwrap(),
        );
        let service = Service::new(database.clone());
        let mut draft = users();
        draft.columns.push(ColumnDraft {
            id: None,
            name: "parent_id".into(),
            scalar_type: ScalarType::Text,
            nullable: true,
            format: String::new(),
            default: None,
        });
        draft.foreign_keys.push(ForeignKeyDef {
            name: "users_parent_fk".into(),
            columns: vec!["parent_id".into()],
            ref_table: "users".into(),
            ref_columns: vec!["id".into()],
        });
        let table = service.create_table(draft).await.unwrap();
        assert_eq!(table.foreign_keys[0].ref_table_id, table.id);
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn catalog_mode_is_set_once() {
        let database = Arc::new(
            slatedb::Store::memory("catalog-service-mode")
                .await
                .unwrap(),
        );
        let service = Service::new(database.clone());
        assert_eq!(
            service.init_mode(Some(Mode::Schema)).await.unwrap(),
            Mode::Schema
        );
        assert_eq!(service.init_mode(None).await.unwrap(), Mode::Schema);
        assert_eq!(
            service
                .init_mode(Some(Mode::Direct))
                .await
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidInput
        );
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn table_schema_ids_are_historical_and_duplicate_column_ids_are_rejected() {
        let database = Arc::new(
            slatedb::Store::memory("catalog-service-schema-identities")
                .await
                .unwrap(),
        );
        let service = Service::new(database.clone());
        let first = service.create_table(users()).await.unwrap();
        service.delete_table("users").await.unwrap();
        let mut replacement = users();
        replacement.name = "people".into();
        let second = service.create_table(replacement).await.unwrap();
        assert!(second.schema_id > first.schema_id);

        let mut duplicate = users();
        duplicate.name = "duplicates".into();
        duplicate.columns.push(ColumnDraft {
            id: Some(SchemaId::new(1).unwrap()),
            name: "other".into(),
            scalar_type: ScalarType::Text,
            nullable: true,
            format: String::new(),
            default: None,
        });
        duplicate.columns[0].id = Some(SchemaId::new(1).unwrap());
        assert_eq!(
            service.create_table(duplicate).await.unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
        database.close().await.unwrap();
    }
}
