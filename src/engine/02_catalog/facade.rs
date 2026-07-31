use std::sync::{Arc, RwLock};

use super::identity::{SchemaId, TableId, TransitionId};
use super::model::{
    ColumnDraft, DefaultValue, Index, IndexDef, Mode, Revision, Schema, SchemaTransition, Table,
    TableDraft,
};
use super::{Result, Service};
use crate::engine::kv::TransactionalKv;
use crate::runtime::{RuntimeEffects, SystemRuntime};

type Observer = Arc<dyn Fn() + Send + Sync>;

/// Numbered catalog-layer facade. Durable state remains in the store and
/// mutations remain in [`Service`]; observers are process-local wake-up hints.
pub struct Catalog {
    changes: Service,
    observers: RwLock<Vec<Observer>>,
}

impl Catalog {
    pub fn new(store: Arc<dyn TransactionalKv>) -> Self {
        Self::with_runtime(store, Arc::new(SystemRuntime))
    }

    pub fn with_runtime(store: Arc<dyn TransactionalKv>, runtime: Arc<dyn RuntimeEffects>) -> Self {
        Self {
            changes: Service::with_runtime(store, runtime),
            observers: RwLock::new(Vec::new()),
        }
    }

    pub fn changes(&self) -> &Service {
        &self.changes
    }

    pub fn on_change(&self, observer: impl Fn() + Send + Sync + 'static) {
        self.observers
            .write()
            .expect("catalog observer lock poisoned")
            .push(Arc::new(observer));
    }

    pub async fn get_table(&self, name: &str) -> Result<Option<Table>> {
        self.changes.get_table(name).await
    }

    pub async fn get_table_by_id(&self, id: &TableId) -> Result<Option<Table>> {
        self.changes.get_table_by_id(id).await
    }

    pub async fn get_table_by_schema_id(&self, id: SchemaId) -> Result<Option<Table>> {
        self.changes.get_table_by_schema_id(id).await
    }

    pub async fn list_tables(&self) -> Result<Vec<Table>> {
        self.changes.list_tables().await
    }

    pub async fn create_table(&self, definition: impl Into<TableDraft>) -> Result<Table> {
        self.notify(self.changes.create_table(definition.into()).await)
    }

    pub async fn delete_table(&self, name: &str) -> Result<()> {
        self.notify(self.changes.delete_table(name).await)
    }

    pub async fn rename_table(&self, from: &str, to: &str) -> Result<()> {
        self.notify(self.changes.rename_table(from, to).await)
    }

    pub async fn create_column(
        &self,
        table: &str,
        definition: impl Into<ColumnDraft>,
    ) -> Result<Table> {
        self.notify(self.changes.create_column(table, definition.into()).await)
    }

    pub async fn delete_column(&self, table: &str, column: &str) -> Result<Table> {
        self.notify(self.changes.delete_column(table, column).await)
    }

    pub async fn rename_column(&self, table: &str, from: &str, to: &str) -> Result<Table> {
        self.notify(self.changes.rename_column(table, from, to).await)
    }

    pub async fn change_column_insert_default(
        &self,
        table: &str,
        column: &str,
        value: Option<DefaultValue>,
    ) -> Result<Table> {
        self.notify(
            self.changes
                .change_column_insert_default(table, column, value)
                .await,
        )
    }

    pub async fn create_index(&self, table: &str, definition: IndexDef) -> Result<Index> {
        self.notify(self.changes.create_index(table, definition).await)
    }

    pub async fn delete_index(&self, table: &str, index: &str) -> Result<()> {
        self.notify(self.changes.delete_index(table, index).await)
    }

    pub async fn get_transition(&self, id: &TransitionId) -> Result<Option<SchemaTransition>> {
        self.changes.get_transition(id).await
    }

    pub async fn list_transitions(&self) -> Result<Vec<SchemaTransition>> {
        self.changes.list_transitions().await
    }

    pub async fn cancel_schema_transition(&self, id: &TransitionId) -> Result<SchemaTransition> {
        self.notify(self.changes.cancel_schema_transition(id).await)
    }

    pub async fn mode(&self) -> Result<Mode> {
        self.changes.mode().await
    }

    pub async fn init_mode(&self, requested: Option<Mode>) -> Result<Mode> {
        self.changes.init_mode(requested).await
    }

    pub async fn revision(&self) -> Result<Revision> {
        self.changes.revision().await
    }

    pub async fn revisions(&self) -> Result<Vec<Revision>> {
        self.changes.revisions().await
    }

    pub async fn schema(&self) -> Result<Schema> {
        self.changes.schema().await
    }

    pub async fn validate_current_schema(&self) -> Result<()> {
        self.changes.validate_current_schema().await
    }

    fn notify<T>(&self, result: Result<T>) -> Result<T> {
        if result.is_ok() {
            let observers = self
                .observers
                .read()
                .expect("catalog observer lock poisoned")
                .clone();
            for observer in observers {
                observer();
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::engine::catalog::model::{ColumnDef, ScalarType, TableDef};
    use crate::engine::kv::slatedb;
    use crate::engine::kv::{IsolationLevel, TransactionView, TransactionalKv};

    use super::*;

    #[tokio::test]
    async fn observers_run_only_after_successful_direct_mutations() {
        let database = Arc::new(slatedb::Store::memory("catalog-facade").await.unwrap());
        let catalog = Catalog::new(database.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        catalog.on_change(move || {
            observed.fetch_add(1, Ordering::Relaxed);
        });
        let definition = TableDef {
            id: SchemaId::new(1).unwrap(),
            name: "users".into(),
            columns: vec![ColumnDef {
                id: SchemaId::new(1).unwrap(),
                name: "id".into(),
                scalar_type: ScalarType::Int64,
                nullable: false,
                format: String::new(),
                default: None,
            }],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        };
        catalog.create_table(definition.clone()).await.unwrap();
        assert!(catalog.create_table(definition).await.is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        catalog.validate_current_schema().await.unwrap();

        let mut transaction = database
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        {
            let mut view = TransactionView(transaction.as_mut());
            let mut table = super::super::store::get_table(&view, "users")
                .await
                .unwrap()
                .unwrap();
            table.name = "physically_drifted".into();
            super::super::store::save_table(&mut view, &mut table)
                .await
                .unwrap();
        }
        transaction.commit().await.unwrap();
        assert_eq!(
            catalog.validate_current_schema().await.unwrap_err().kind(),
            super::super::ErrorKind::CatalogDrift
        );
        database.close().await.unwrap();
    }
}
