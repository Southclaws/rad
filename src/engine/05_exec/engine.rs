//! Snapshot-coherent bind, plan, and execute entry points.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use crate::engine::catalog;
use crate::engine::catalog::model::{Revision, Schema, SchemaTransition, Table};
use crate::engine::kv::{IsolationLevel, KvView, Transaction, TransactionView, TransactionalKv};
use crate::engine::lir::{self, Datum, Row, RowType};
use crate::engine::planner::bind;
use crate::engine::planner::{PlanOptions, plan_query};
use crate::runtime::{RuntimeEffects, SystemRuntime};

use super::{
    CatalogPolicy, EngineEvent, EngineEventHook, EngineOperation, Executor, Limits,
    NoopEngineEventHook, Program, ProgramOptions, ProgramResult, ReferenceExecutor, Result,
};

type CatalogObserver = Arc<dyn Fn() + Send + Sync>;

pub struct Engine {
    pub(super) store: Arc<dyn TransactionalKv>,
    limits: Limits,
    pub(super) runtime: Arc<dyn RuntimeEffects>,
    pub(super) events: Arc<dyn EngineEventHook>,
    catalog_observers: RwLock<Vec<CatalogObserver>>,
}

impl Engine {
    pub fn new(store: Arc<dyn TransactionalKv>) -> Self {
        Self::with_runtime(store, Arc::new(SystemRuntime))
    }

    pub fn with_runtime(store: Arc<dyn TransactionalKv>, runtime: Arc<dyn RuntimeEffects>) -> Self {
        Self {
            store,
            limits: Limits::default(),
            runtime,
            events: Arc::new(NoopEngineEventHook),
            catalog_observers: RwLock::new(Vec::new()),
        }
    }

    pub fn with_limits(store: Arc<dyn TransactionalKv>, limits: Limits) -> Self {
        Self::with_limits_and_runtime(store, limits, Arc::new(SystemRuntime))
    }

    pub fn with_limits_and_runtime(
        store: Arc<dyn TransactionalKv>,
        limits: Limits,
        runtime: Arc<dyn RuntimeEffects>,
    ) -> Self {
        Self {
            store,
            limits,
            runtime,
            events: Arc::new(NoopEngineEventHook),
            catalog_observers: RwLock::new(Vec::new()),
        }
    }

    /// Install a semantic event hook before sharing the engine. Production
    /// uses the no-op hook; deterministic tests may suspend at these points.
    pub fn with_event_hook(mut self, events: Arc<dyn EngineEventHook>) -> Self {
        self.events = events;
        self
    }

    /// Register a process-local latency hint for committed catalog programs.
    /// Durable scheduler discovery remains authoritative.
    pub fn on_catalog_change(&self, observer: impl Fn() + Send + Sync + 'static) {
        self.catalog_observers
            .write()
            .expect("engine catalog observer lock poisoned")
            .push(Arc::new(observer));
    }

    pub(super) fn notify_catalog_change(&self) {
        let observers = self
            .catalog_observers
            .read()
            .expect("engine catalog observer lock poisoned")
            .clone();
        for observer in observers {
            observer();
        }
    }

    /// Read the revision, physical catalog, and durable transition records
    /// through one snapshot for declarative migration planning.
    pub async fn schema_migration_snapshot(
        &self,
    ) -> Result<(Revision, Vec<Table>, Vec<SchemaTransition>)> {
        let transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let result = async {
            let mut view = TransactionView(&*transaction);
            let revision = catalog::store::current_revision(&mut view).await?;
            let tables = catalog::store::list_tables(&mut view).await?;
            let physical = Schema::from_physical(&tables)?;
            if !revision.schema.canonical_eq(&physical)? {
                return Err(super::Error::message(
                    super::ErrorKind::CorruptData,
                    format!(
                        "catalog: stored schema revision {} differs from physical catalog",
                        revision.version
                    ),
                ));
            }
            let mut transitions = catalog::store::list_transitions(&mut view).await?;
            for transition in &mut transitions {
                let high_water =
                    catalog::store::delta_high_water(&mut view, &transition.id).await?;
                transition.refresh_work_state(high_water);
            }
            Ok((revision, tables, transitions))
        }
        .await;
        transaction.rollback();
        result
    }

    pub(crate) async fn scan_table_rows(&self, table: &Table) -> Result<Vec<Row>> {
        let transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let result = {
            let view = TransactionView(&*transaction);
            super::row_store::scan_table_columns(&view, table, &table.columns).await
        };
        transaction.rollback();
        result
    }

    /// Bind and read through one discarded snapshot.
    pub async fn execute(&self, query: lir::Query) -> Result<Datum> {
        self.execute_snapshot(query, PlanOptions::default(), false)
            .await
    }

    /// Conformance oracle: every narrowed access becomes a table scan while
    /// the full residual predicate remains authoritative.
    pub async fn execute_forced(&self, query: lir::Query) -> Result<Datum> {
        self.execute_snapshot(
            query,
            PlanOptions {
                full_scan_only: true,
            },
            false,
        )
        .await
    }

    /// Conformance oracle for correlation: disable distinct-key batching.
    pub async fn execute_nested(&self, query: lir::Query) -> Result<Datum> {
        self.execute_snapshot(query, PlanOptions::default(), true)
            .await
    }

    /// Bind logical LIR and interpret it without a physical plan. This is a
    /// slow semantic oracle for differential and deterministic-scheduler tests.
    pub async fn execute_reference(&self, query: lir::Query) -> Result<Datum> {
        let transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let result = {
            let view = TransactionView(&*transaction);
            match bind::bind(&ViewCatalog { view: &view }, query).await {
                Ok(bound) => {
                    ReferenceExecutor::new(&view, self.limits)
                        .execute(&bound)
                        .await
                }
                Err(error) => Err(error.into()),
            }
        };
        transaction.rollback();
        result
    }

    pub async fn execute_in(
        &self,
        transaction: &mut dyn Transaction,
        query: lir::Query,
    ) -> Result<Datum> {
        let view = TransactionView(&*transaction);
        execute_on_view(&view, query, PlanOptions::default(), false, self.limits).await
    }

    /// Preflight every statement against a rollback-only transaction, then
    /// execute the ordered program atomically in one transaction.
    pub async fn execute_program(
        &self,
        program: Program,
        catalog_policy: CatalogPolicy,
    ) -> Result<ProgramResult> {
        self.execute_program_with_options(
            program,
            ProgramOptions {
                catalog: catalog_policy,
                ..ProgramOptions::default()
            },
        )
        .await
    }

    pub async fn execute_program_with_options(
        &self,
        program: Program,
        options: ProgramOptions,
    ) -> Result<ProgramResult> {
        self.execute_program_path(program, options, false).await
    }

    /// Execute a PIR program with relational statements interpreted from bound
    /// logical LIR. Transaction, catalog, and mutation orchestration stays the
    /// same so differential tests isolate planner/executor semantics.
    pub async fn execute_program_reference_with_options(
        &self,
        program: Program,
        options: ProgramOptions,
    ) -> Result<ProgramResult> {
        self.execute_program_path(program, options, true).await
    }

    async fn execute_program_path(
        &self,
        program: Program,
        options: ProgramOptions,
        reference: bool,
    ) -> Result<ProgramResult> {
        let result_name = super::program::validate(&program, options.catalog)?;
        let catalog_statements = program
            .statements
            .iter()
            .filter(|statement| !statement.relational())
            .map(|statement| statement.name().to_owned())
            .collect::<Vec<_>>();
        let preflight = self
            .store
            .begin(IsolationLevel::SerializableSnapshot)
            .await?;
        let preflight_result = {
            let mut view = TransactionView(&*preflight);
            match super::program::expect_catalog(&mut view, options.expected_catalog.as_ref()).await
            {
                Ok(()) => {
                    super::program::preflight(
                        &mut view,
                        &program,
                        options.catalog,
                        options.collect_plan,
                        !reference,
                        &self.runtime,
                    )
                    .await
                }
                Err(error) => Err(error),
            }
        };
        preflight.rollback();
        let plans = preflight_result?;
        if options.dry_run {
            return Ok(ProgramResult {
                result: Datum::Null,
                statements: Vec::new(),
                plans,
            });
        }

        let effectful = program.statements.iter().any(super::Statement::effectful);
        let isolation = if effectful {
            IsolationLevel::SerializableSnapshot
        } else {
            IsolationLevel::Snapshot
        };
        let transaction = self.store.begin(isolation).await?;
        let execution = {
            let mut view = TransactionView(&*transaction);
            match super::program::expect_catalog(&mut view, options.expected_catalog.as_ref()).await
            {
                Ok(()) => (if reference {
                    super::program::run_reference(
                        &mut view,
                        &program,
                        result_name.as_deref(),
                        options.catalog,
                        self.limits,
                        &self.runtime,
                    )
                    .await
                } else {
                    super::program::run(
                        &mut view,
                        &program,
                        result_name.as_deref(),
                        options.catalog,
                        self.limits,
                        &self.runtime,
                    )
                    .await
                })
                .map(|mut result| {
                    result.plans = plans;
                    result
                }),
                Err(error) => Err(error),
            }
        };
        match execution {
            Ok(result) if effectful => {
                let catalog_operation =
                    (!catalog_statements.is_empty()).then_some(EngineOperation::CatalogProgram {
                        statements: catalog_statements,
                    });
                if let Some(operation) = catalog_operation.clone() {
                    self.events
                        .reach(EngineEvent::CommitStarted { operation })
                        .await;
                }
                transaction.commit().await?;
                if let Some(operation) = catalog_operation {
                    self.events
                        .reach(EngineEvent::CommitSucceeded { operation })
                        .await;
                    self.notify_catalog_change();
                }
                Ok(result)
            }
            Ok(result) => {
                transaction.rollback();
                Ok(result)
            }
            Err(error) => {
                transaction.rollback();
                Err(error)
            }
        }
    }

    pub async fn create(&self, table: &str, row: Row) -> Result<Row> {
        let mut rows = self.create_many(table, vec![row]).await?;
        Ok(rows.pop().expect("one input produces one row"))
    }

    pub async fn create_many(&self, table: &str, rows: Vec<Row>) -> Result<Vec<Row>> {
        let transaction = self
            .store
            .begin(IsolationLevel::SerializableSnapshot)
            .await?;
        let result = {
            let mut view = TransactionView(&*transaction);
            create_on_view(&mut view, table, &rows, self.runtime.as_ref()).await
        };
        finish_write(transaction, result).await
    }

    pub async fn create_many_in(
        &self,
        transaction: &mut dyn Transaction,
        table: &str,
        rows: &[Row],
    ) -> Result<Vec<Row>> {
        let mut view = TransactionView(&*transaction);
        create_on_view(&mut view, table, rows, self.runtime.as_ref()).await
    }

    pub async fn update_many(
        &self,
        table: &str,
        input_type: RowType,
        rows: Vec<Row>,
    ) -> Result<Vec<Row>> {
        let transaction = self
            .store
            .begin(IsolationLevel::SerializableSnapshot)
            .await?;
        let result = {
            let mut view = TransactionView(&*transaction);
            update_on_view(&mut view, table, &input_type, &rows).await
        };
        finish_write(transaction, result).await
    }

    pub async fn update_many_in(
        &self,
        transaction: &mut dyn Transaction,
        table: &str,
        input_type: &RowType,
        rows: &[Row],
    ) -> Result<Vec<Row>> {
        let mut view = TransactionView(&*transaction);
        update_on_view(&mut view, table, input_type, rows).await
    }

    pub async fn delete_many(
        &self,
        table: &str,
        input_type: RowType,
        rows: Vec<Row>,
    ) -> Result<Vec<Row>> {
        let transaction = self
            .store
            .begin(IsolationLevel::SerializableSnapshot)
            .await?;
        let result = {
            let mut view = TransactionView(&*transaction);
            delete_on_view(&mut view, table, &input_type, &rows).await
        };
        finish_write(transaction, result).await
    }

    pub async fn delete_many_in(
        &self,
        transaction: &mut dyn Transaction,
        table: &str,
        input_type: &RowType,
        rows: &[Row],
    ) -> Result<Vec<Row>> {
        let mut view = TransactionView(&*transaction);
        delete_on_view(&mut view, table, input_type, rows).await
    }

    async fn execute_snapshot(
        &self,
        query: lir::Query,
        options: PlanOptions,
        force_nested: bool,
    ) -> Result<Datum> {
        let transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let result = {
            let view = TransactionView(&*transaction);
            execute_on_view(&view, query, options, force_nested, self.limits).await
        };
        transaction.rollback();
        result
    }
}

async fn finish_write<T>(transaction: Box<dyn Transaction>, result: Result<T>) -> Result<T> {
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

async fn write_table(view: &dyn KvView, name: &str) -> Result<Table> {
    catalog::store::get_table(view, name).await?.ok_or_else(|| {
        super::Error::message(
            super::ErrorKind::InvalidInput,
            format!("exec: table {name:?} does not exist"),
        )
    })
}

async fn create_on_view(
    view: &mut dyn KvView,
    table: &str,
    rows: &[Row],
    runtime: &dyn RuntimeEffects,
) -> Result<Vec<Row>> {
    let table = write_table(view, table).await?;
    super::mutate::create(view, &table, rows, runtime).await
}

async fn update_on_view(
    view: &mut dyn KvView,
    table: &str,
    input_type: &RowType,
    rows: &[Row],
) -> Result<Vec<Row>> {
    let table = write_table(view, table).await?;
    super::mutate::update(view, &table, input_type, rows).await
}

async fn delete_on_view(
    view: &mut dyn KvView,
    table: &str,
    input_type: &RowType,
    rows: &[Row],
) -> Result<Vec<Row>> {
    let table = write_table(view, table).await?;
    super::mutate::delete(view, &table, input_type, rows).await
}

async fn execute_on_view(
    view: &dyn KvView,
    query: lir::Query,
    options: PlanOptions,
    force_nested: bool,
    limits: Limits,
) -> Result<Datum> {
    let bound = bind::bind(&ViewCatalog { view }, query).await?;
    let plan = plan_query(&bound, options);
    let mut executor = Executor::new(view, limits);
    executor.set_force_nested(force_nested);
    executor.execute(&plan).await
}

pub(super) struct ViewCatalog<'a> {
    pub(super) view: &'a dyn KvView,
}

#[async_trait]
impl bind::Catalog for ViewCatalog<'_> {
    async fn get_table(&self, name: &str) -> catalog::Result<Option<Table>> {
        catalog::store::get_table(self.view, name).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use bytes::Bytes;
    use chrono::{DateTime, TimeZone, Utc};
    use slatedb::object_store::{ObjectStore, local::LocalFileSystem};
    use tempfile::TempDir;
    use uuid::Uuid;

    use crate::engine::catalog::identity::SchemaId;
    use crate::engine::catalog::model::{
        ColumnConversion, ColumnDef, ColumnReplacementDef, ConstraintDef, ConstraintKind,
        DefaultFunction, DefaultValue, ForeignKeyDef, IndexDef, ScalarType, TableDef,
    };
    use crate::engine::exec::codec;
    use crate::engine::exec::{ErrorKind, ErrorReason};
    use crate::engine::kv::Kv;
    use crate::engine::kv::slatedb::Store;
    use crate::engine::lir::{
        BinaryOp, Expr, Field, Kind, Literal, RawScalar, Relation, RootCardinality, Type, Value,
    };
    use crate::runtime::RuntimeEffects;

    use super::*;

    struct DeterministicRuntime {
        now: DateTime<Utc>,
        uuids: Mutex<VecDeque<Uuid>>,
    }

    impl RuntimeEffects for DeterministicRuntime {
        fn now(&self) -> DateTime<Utc> {
            self.now
        }

        fn new_uuid(&self) -> Uuid {
            self.uuids
                .lock()
                .expect("UUID queue lock poisoned")
                .pop_front()
                .expect("test supplied enough UUIDs")
        }
    }

    #[tokio::test]
    async fn runtime_controls_catalog_time_and_generated_row_defaults() {
        let now = Utc.with_ymd_and_hms(2035, 6, 7, 8, 9, 10).unwrap();
        let first = Uuid::parse_str("018f0000-0000-7000-8000-000000000001").unwrap();
        let second = Uuid::parse_str("018f0000-0000-7000-8000-000000000002").unwrap();
        let runtime = Arc::new(DeterministicRuntime {
            now,
            uuids: Mutex::new(VecDeque::from([first, second])),
        });
        let store = Arc::new(Store::memory("exec-deterministic-runtime").await.unwrap());
        let catalog = catalog::Catalog::with_runtime(store.clone(), runtime.clone());
        catalog
            .create_table(TableDef {
                id: SchemaId::new(1).unwrap(),
                name: "events".into(),
                columns: vec![
                    ColumnDef {
                        id: SchemaId::new(1).unwrap(),
                        name: "id".into(),
                        scalar_type: ScalarType::Text,
                        nullable: false,
                        format: "uuid".into(),
                        default: Some(DefaultValue {
                            function: Some(DefaultFunction::Uuid),
                            ..DefaultValue::default()
                        }),
                    },
                    ColumnDef {
                        id: SchemaId::new(2).unwrap(),
                        name: "created_at".into(),
                        scalar_type: ScalarType::Int64,
                        nullable: false,
                        format: String::new(),
                        default: Some(DefaultValue {
                            function: Some(DefaultFunction::NowMs),
                            ..DefaultValue::default()
                        }),
                    },
                ],
                primary_key: vec!["id".into()],
                indexes: Vec::new(),
                foreign_keys: Vec::new(),
            })
            .await
            .unwrap();
        assert_eq!(
            catalog.revision().await.unwrap().created_at.as_datetime(),
            now
        );

        let rows = Engine::with_runtime(store.clone(), runtime)
            .create_many("events", vec![Row::new(), Row::new()])
            .await
            .unwrap();
        assert_eq!(rows[0]["id"], Value::Text(first.to_string()));
        assert_eq!(rows[1]["id"], Value::Text(second.to_string()));
        for row in rows {
            assert_eq!(row["created_at"], Value::Int64(now.timestamp_millis()));
        }
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn historical_missing_values_survive_default_changes_and_file_reopen() {
        let directory = TempDir::new().unwrap();
        let objects: Arc<dyn ObjectStore> = Arc::new(
            LocalFileSystem::new_with_prefix(directory.path()).expect("local object-store root"),
        );

        {
            let store = Arc::new(
                Store::open("historical-defaults", objects.clone())
                    .await
                    .unwrap(),
            );
            let catalog = catalog::Catalog::new(store.clone());
            catalog
                .create_table(TableDef {
                    id: SchemaId::new(1).unwrap(),
                    name: "items".into(),
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
                })
                .await
                .unwrap();
            Engine::new(store.clone())
                .create("items", Row::from([("id".into(), Value::Int64(1))]))
                .await
                .unwrap();
            catalog
                .create_column(
                    "items",
                    ColumnDef {
                        id: SchemaId::new(2).unwrap(),
                        name: "status".into(),
                        scalar_type: ScalarType::Text,
                        nullable: true,
                        format: String::new(),
                        default: Some(DefaultValue {
                            text: "active".into(),
                            ..DefaultValue::default()
                        }),
                    },
                )
                .await
                .unwrap();
            store.close().await.unwrap();
        }

        {
            let store = Arc::new(
                Store::open("historical-defaults", objects.clone())
                    .await
                    .unwrap(),
            );
            let catalog = catalog::Catalog::new(store.clone());
            let engine = Engine::new(store.clone());
            let table = catalog.get_table("items").await.unwrap().unwrap();
            let status = table.column("status").unwrap();
            assert_eq!(status.missing_value.as_ref().unwrap().text, "active");
            assert_eq!(status.insert_default.as_ref().unwrap().text, "active");
            assert_eq!(
                engine.scan_table_rows(&table).await.unwrap()[0]["status"],
                Value::Text("active".into())
            );
            engine
                .create("items", Row::from([("id".into(), Value::Int64(2))]))
                .await
                .unwrap();
            engine
                .create(
                    "items",
                    Row::from([
                        ("id".into(), Value::Int64(3)),
                        ("status".into(), Value::Null(ScalarType::Text)),
                    ]),
                )
                .await
                .unwrap();
            catalog
                .change_column_insert_default(
                    "items",
                    "status",
                    Some(DefaultValue {
                        text: "pending".into(),
                        ..DefaultValue::default()
                    }),
                )
                .await
                .unwrap();
            store.close().await.unwrap();
        }

        {
            let store = Arc::new(
                Store::open("historical-defaults", objects.clone())
                    .await
                    .unwrap(),
            );
            let catalog = catalog::Catalog::new(store.clone());
            let engine = Engine::new(store.clone());
            let status = catalog
                .get_table("items")
                .await
                .unwrap()
                .unwrap()
                .column("status")
                .unwrap()
                .clone();
            assert_eq!(status.missing_value.as_ref().unwrap().text, "active");
            assert_eq!(status.insert_default.as_ref().unwrap().text, "pending");
            engine
                .create("items", Row::from([("id".into(), Value::Int64(4))]))
                .await
                .unwrap();
            catalog
                .change_column_insert_default("items", "status", None)
                .await
                .unwrap();
            store.close().await.unwrap();
        }

        {
            let store = Arc::new(Store::open("historical-defaults", objects).await.unwrap());
            let catalog = catalog::Catalog::new(store.clone());
            let engine = Engine::new(store.clone());
            let table = catalog.get_table("items").await.unwrap().unwrap();
            let status = table.column("status").unwrap();
            assert!(status.insert_default.is_none());
            assert_eq!(status.missing_value.as_ref().unwrap().text, "active");
            engine
                .create("items", Row::from([("id".into(), Value::Int64(5))]))
                .await
                .unwrap();
            let rows = engine.scan_table_rows(&table).await.unwrap();
            let values = rows
                .into_iter()
                .map(|row| (row["id"].clone(), row["status"].clone()))
                .collect::<Vec<_>>();
            assert_eq!(
                values,
                vec![
                    (Value::Int64(1), Value::Text("active".into())),
                    (Value::Int64(2), Value::Text("active".into())),
                    (Value::Int64(3), Value::Null(ScalarType::Text)),
                    (Value::Int64(4), Value::Text("pending".into())),
                    (Value::Int64(5), Value::Null(ScalarType::Text)),
                ]
            );
            store.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn engine_binds_catalog_and_reads_data_from_one_snapshot() {
        let store = Arc::new(Store::memory("exec-engine-snapshot").await.unwrap());
        let catalog = catalog::Catalog::new(store.clone());
        let table = catalog
            .create_table(TableDef {
                id: SchemaId::new(1).unwrap(),
                name: "tasks".into(),
                columns: vec![
                    ColumnDef {
                        id: SchemaId::new(1).unwrap(),
                        name: "id".into(),
                        scalar_type: ScalarType::Text,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                    ColumnDef {
                        id: SchemaId::new(2).unwrap(),
                        name: "status".into(),
                        scalar_type: ScalarType::Text,
                        nullable: false,
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
        let row = lir::Row::from([
            ("id".into(), Value::Text("t1".into())),
            ("status".into(), Value::Text("open".into())),
        ]);
        let primary_key = codec::encode_row_tuple(&row, &table.primary_key).unwrap();
        Kv::put(
            &*store,
            Bytes::from(codec::data_key(&table, &primary_key)),
            Bytes::from(codec::marshal_row(&table, &row).unwrap()),
        )
        .await
        .unwrap();

        let query = lir::Query {
            root: Relation::Filter {
                input: Box::new(Relation::Scan {
                    table: "tasks".into(),
                    scope: "t".into(),
                }),
                predicate: Expr::Binary {
                    op: BinaryOp::Eq,
                    left: Box::new(Expr::Column {
                        scope: "t".into(),
                        name: "id".into(),
                    }),
                    right: Box::new(Expr::Literal(Literal {
                        raw: RawScalar::Text("t1".into()),
                        kind: None,
                    })),
                },
            },
            cardinality: RootCardinality::First,
            bindings: HashMap::new(),
        };
        let engine = Engine::new(store);
        let selected = engine.execute(query.clone()).await.unwrap();
        let forced = engine.execute_forced(query.clone()).await.unwrap();
        let reference = engine.execute_reference(query).await.unwrap();
        assert_eq!(selected, forced);
        assert_eq!(selected, reference);
        assert!(matches!(
            selected,
            Datum::Object(fields)
                if fields.iter().any(|field| field.name == "status"
                    && field.datum == Datum::scalar(Value::Text("open".into())))
        ));
    }

    fn projected_column(table: &str, column: &str, filter: Option<(&str, &str)>) -> lir::Query {
        let scan = Relation::Scan {
            table: table.into(),
            scope: "s".into(),
        };
        let input = if let Some((filter_column, value)) = filter {
            Relation::Filter {
                input: Box::new(scan),
                predicate: Expr::Binary {
                    op: BinaryOp::Eq,
                    left: Box::new(Expr::Column {
                        scope: "s".into(),
                        name: filter_column.into(),
                    }),
                    right: Box::new(Expr::Literal(Literal {
                        raw: RawScalar::Text(value.into()),
                        kind: None,
                    })),
                },
            }
        } else {
            scan
        };
        let ordered = Relation::Order {
            input: Box::new(input),
            terms: vec![crate::engine::lir::OrderTerm {
                expression: Expr::Column {
                    scope: "s".into(),
                    name: "id".into(),
                },
                descending: false,
            }],
        };
        lir::Query {
            root: Relation::Project {
                input: Box::new(ordered),
                scope: Some("result".into()),
                spread: Vec::new(),
                fields: vec![crate::engine::lir::ProjectField {
                    name: column.into(),
                    expression: Expr::Column {
                        scope: "s".into(),
                        name: column.into(),
                    },
                }],
            },
            cardinality: RootCardinality::Many,
            bindings: HashMap::new(),
        }
    }

    fn projected_scratch(column: &str, filter_on_retiring: bool) -> lir::Query {
        projected_column(
            "scratch",
            column,
            filter_on_retiring.then_some(("retiring", "gone")),
        )
    }

    #[tokio::test]
    async fn catalog_mvcc_conflicts_only_with_observed_column_definitions() {
        for (name, projected, filtered, expect_conflict) in [
            ("unobserved", "value", false, false),
            ("projected", "retiring", false, true),
            ("filtered", "value", true, true),
        ] {
            let store = Arc::new(
                Store::memory(&format!("exec-catalog-mvcc-{name}"))
                    .await
                    .unwrap(),
            );
            let catalog = catalog::Catalog::new(store.clone());
            catalog
                .create_table(TableDef {
                    id: SchemaId::new(1).unwrap(),
                    name: "scratch".into(),
                    columns: vec![
                        ColumnDef {
                            id: SchemaId::new(1).unwrap(),
                            name: "id".into(),
                            scalar_type: ScalarType::Int64,
                            nullable: false,
                            format: String::new(),
                            default: None,
                        },
                        ColumnDef {
                            id: SchemaId::new(2).unwrap(),
                            name: "value".into(),
                            scalar_type: ScalarType::Text,
                            nullable: false,
                            format: String::new(),
                            default: None,
                        },
                        ColumnDef {
                            id: SchemaId::new(3).unwrap(),
                            name: "retiring".into(),
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
            let engine = Engine::new(store.clone());
            engine
                .create(
                    "scratch",
                    Row::from([
                        ("id".into(), Value::Int64(1)),
                        ("value".into(), Value::Text("kept".into())),
                        ("retiring".into(), Value::Text("gone".into())),
                    ]),
                )
                .await
                .unwrap();

            let mut transaction = store
                .begin(IsolationLevel::SerializableSnapshot)
                .await
                .unwrap();
            catalog.delete_column("scratch", "retiring").await.unwrap();
            engine
                .execute_in(transaction.as_mut(), projected_scratch(projected, filtered))
                .await
                .unwrap();
            let committed = transaction.commit().await;
            assert_eq!(
                committed.as_ref().err().map(crate::engine::kv::Error::kind),
                expect_conflict.then_some(crate::engine::kv::ErrorKind::Conflict),
                "case {name}: {committed:?}"
            );
            store.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn catalog_mvcc_allows_stable_identity_renames_and_nullable_additions() {
        let store = Arc::new(Store::memory("exec-catalog-mvcc-compatible").await.unwrap());
        let catalog = catalog::Catalog::new(store.clone());
        catalog
            .create_table(TableDef {
                id: SchemaId::new(1).unwrap(),
                name: "scratch".into(),
                columns: vec![
                    ColumnDef {
                        id: SchemaId::new(1).unwrap(),
                        name: "id".into(),
                        scalar_type: ScalarType::Int64,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                    ColumnDef {
                        id: SchemaId::new(2).unwrap(),
                        name: "value".into(),
                        scalar_type: ScalarType::Text,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                ],
                primary_key: vec!["id".into()],
                indexes: vec![IndexDef {
                    name: "scratch_value_idx".into(),
                    columns: vec!["value".into()],
                    unique: false,
                }],
                foreign_keys: Vec::new(),
            })
            .await
            .unwrap();
        let engine = Engine::new(store.clone());
        engine
            .create(
                "scratch",
                Row::from([
                    ("id".into(), Value::Int64(1)),
                    ("value".into(), Value::Text("first".into())),
                ]),
            )
            .await
            .unwrap();

        let mut pinned = store
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        catalog
            .rename_table("scratch", "renamed_table")
            .await
            .unwrap();
        catalog
            .rename_column("renamed_table", "value", "renamed_value")
            .await
            .unwrap();
        catalog
            .create_column(
                "renamed_table",
                ColumnDef {
                    id: SchemaId::new(3).unwrap(),
                    name: "optional".into(),
                    scalar_type: ScalarType::Text,
                    nullable: true,
                    format: String::new(),
                    default: None,
                },
            )
            .await
            .unwrap();

        let old_result = engine
            .execute_in(
                pinned.as_mut(),
                projected_column("scratch", "value", Some(("value", "first"))),
            )
            .await
            .unwrap();
        assert!(matches!(old_result, Datum::Array(ref values) if values.len() == 1));
        engine
            .create_many_in(
                pinned.as_mut(),
                "scratch",
                &[Row::from([
                    ("id".into(), Value::Int64(2)),
                    ("value".into(), Value::Text("second".into())),
                ])],
            )
            .await
            .unwrap();
        pinned.commit().await.unwrap();

        let result = engine
            .execute(projected_column(
                "renamed_table",
                "renamed_value",
                Some(("renamed_value", "second")),
            ))
            .await
            .unwrap();
        assert!(matches!(
            result,
            Datum::Array(ref values)
                if values.len() == 1
                    && matches!(
                        &values[0],
                        Datum::Object(fields)
                            if fields.iter().any(|field| field.name == "renamed_value"
                                && field.datum == Datum::scalar(Value::Text("second".into())))
                    )
        ));
        let inserted = engine
            .execute(projected_column("renamed_table", "optional", None))
            .await
            .unwrap();
        assert!(matches!(
            inserted,
            Datum::Array(ref values)
                if values.len() == 2
                    && values.iter().all(|value| matches!(
                        value,
                        Datum::Object(fields)
                            if fields.iter().any(|field| field.name == "optional"
                                && field.datum == Datum::scalar(Value::Null(ScalarType::Text)))
                    ))
        ));
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn catalog_metadata_relaxation_keeps_catalog_writers_serialized() {
        let store = Arc::new(
            Store::memory("exec-catalog-writer-serialization")
                .await
                .unwrap(),
        );
        catalog::Catalog::new(store.clone())
            .create_table(TableDef {
                id: SchemaId::new(1).unwrap(),
                name: "scratch".into(),
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
            })
            .await
            .unwrap();

        let mut first = store
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        let mut second = store
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        for (transaction, name) in [(&mut first, "first"), (&mut second, "second")] {
            let mut view = TransactionView(transaction.as_mut());
            let mut mutation = catalog::Mutation::new(&mut view);
            mutation
                .create_column(
                    "scratch",
                    ColumnDef {
                        id: SchemaId::new(2).unwrap(),
                        name: name.into(),
                        scalar_type: ScalarType::Text,
                        nullable: true,
                        format: String::new(),
                        default: None,
                    }
                    .into(),
                )
                .await
                .unwrap();
            mutation.finish().await.unwrap();
        }

        first.commit().await.unwrap();
        assert_eq!(
            second.commit().await.unwrap_err().kind(),
            crate::engine::kv::ErrorKind::Conflict
        );
        let table = catalog::Catalog::new(store.clone())
            .get_table("scratch")
            .await
            .unwrap()
            .unwrap();
        assert!(table.column("first").is_some());
        assert!(table.column("second").is_none());
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn catalog_mvcc_tracks_cell_free_counts_and_selected_indexes_exactly() {
        for (name, selected_index, expect_conflict) in
            [("table-scan", false, false), ("selected-index", true, true)]
        {
            let store = Arc::new(
                Store::memory(&format!("exec-catalog-mvcc-index-{name}"))
                    .await
                    .unwrap(),
            );
            let catalog = catalog::Catalog::new(store.clone());
            catalog
                .create_table(TableDef {
                    id: SchemaId::new(1).unwrap(),
                    name: "scratch".into(),
                    columns: vec![
                        ColumnDef {
                            id: SchemaId::new(1).unwrap(),
                            name: "id".into(),
                            scalar_type: ScalarType::Int64,
                            nullable: false,
                            format: String::new(),
                            default: None,
                        },
                        ColumnDef {
                            id: SchemaId::new(2).unwrap(),
                            name: "value".into(),
                            scalar_type: ScalarType::Text,
                            nullable: false,
                            format: String::new(),
                            default: None,
                        },
                    ],
                    primary_key: vec!["id".into()],
                    indexes: vec![IndexDef {
                        name: "scratch_value_idx".into(),
                        columns: vec!["value".into()],
                        unique: false,
                    }],
                    foreign_keys: Vec::new(),
                })
                .await
                .unwrap();
            let engine = Engine::new(store.clone());
            engine
                .create(
                    "scratch",
                    Row::from([
                        ("id".into(), Value::Int64(1)),
                        ("value".into(), Value::Text("indexed".into())),
                    ]),
                )
                .await
                .unwrap();

            let mut transaction = store
                .begin(IsolationLevel::SerializableSnapshot)
                .await
                .unwrap();
            catalog
                .delete_index("scratch", "scratch_value_idx")
                .await
                .unwrap();
            engine
                .execute_in(
                    transaction.as_mut(),
                    projected_column(
                        "scratch",
                        "value",
                        selected_index.then_some(("value", "indexed")),
                    ),
                )
                .await
                .unwrap();
            let committed = transaction.commit().await;
            assert_eq!(
                committed.as_ref().err().map(crate::engine::kv::Error::kind),
                expect_conflict.then_some(crate::engine::kv::ErrorKind::Conflict),
                "case {name}: {committed:?}"
            );
            store.close().await.unwrap();
        }

        let store = Arc::new(Store::memory("exec-catalog-mvcc-count").await.unwrap());
        let catalog = catalog::Catalog::new(store.clone());
        catalog
            .create_table(TableDef {
                id: SchemaId::new(1).unwrap(),
                name: "scratch".into(),
                columns: vec![
                    ColumnDef {
                        id: SchemaId::new(1).unwrap(),
                        name: "id".into(),
                        scalar_type: ScalarType::Int64,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                    ColumnDef {
                        id: SchemaId::new(2).unwrap(),
                        name: "retiring".into(),
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
        let engine = Engine::new(store.clone());
        engine
            .create(
                "scratch",
                Row::from([
                    ("id".into(), Value::Int64(1)),
                    ("retiring".into(), Value::Text("gone".into())),
                ]),
            )
            .await
            .unwrap();
        let mut transaction = store
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        catalog.delete_column("scratch", "retiring").await.unwrap();
        let count = engine
            .execute_in(
                transaction.as_mut(),
                lir::Query {
                    root: Relation::Aggregate {
                        input: Box::new(Relation::Scan {
                            table: "scratch".into(),
                            scope: "s".into(),
                        }),
                        scope: Some("counted".into()),
                        groups: Vec::new(),
                        terms: vec![lir::AggregateTerm {
                            function: lir::AggregateFunction::Count,
                            argument: None,
                            name: "count".into(),
                        }],
                    },
                    cardinality: RootCardinality::ExactlyOne,
                    bindings: HashMap::new(),
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            count,
            Datum::Object(ref fields)
                if fields.iter().any(|field| field.name == "count"
                    && field.datum == Datum::scalar(Value::Int64(1)))
        ));
        transaction.commit().await.unwrap();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn catalog_mvcc_conflicts_only_with_the_dependent_writer_schema() {
        let store = Arc::new(Store::memory("exec-catalog-mvcc-writers").await.unwrap());
        let catalog = catalog::Catalog::new(store.clone());
        for (id, name) in [(1, "dependent"), (10, "unrelated")] {
            catalog
                .create_table(TableDef {
                    id: SchemaId::new(id).unwrap(),
                    name: name.into(),
                    columns: vec![
                        ColumnDef {
                            id: SchemaId::new(id).unwrap(),
                            name: "id".into(),
                            scalar_type: ScalarType::Int64,
                            nullable: false,
                            format: String::new(),
                            default: None,
                        },
                        ColumnDef {
                            id: SchemaId::new(id + 1).unwrap(),
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
        }
        let engine = Engine::new(store.clone());

        let mut unrelated = store
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        engine
            .create_many_in(
                unrelated.as_mut(),
                "dependent",
                &[Row::from([
                    ("id".into(), Value::Int64(1)),
                    ("value".into(), Value::Text("safe".into())),
                ])],
            )
            .await
            .unwrap();
        catalog
            .create_column(
                "unrelated",
                ColumnDef {
                    id: SchemaId::new(12).unwrap(),
                    name: "added".into(),
                    scalar_type: ScalarType::Text,
                    nullable: true,
                    format: String::new(),
                    default: None,
                },
            )
            .await
            .unwrap();
        unrelated.commit().await.unwrap();

        for (name, delete_table) in [("column", false), ("table", true)] {
            let mut dependent = store
                .begin(IsolationLevel::SerializableSnapshot)
                .await
                .unwrap();
            let row = if delete_table {
                Row::from([("id".into(), Value::Int64(3))])
            } else {
                Row::from([
                    ("id".into(), Value::Int64(2)),
                    ("value".into(), Value::Text(name.into())),
                ])
            };
            engine
                .create_many_in(dependent.as_mut(), "dependent", &[row])
                .await
                .unwrap();
            if delete_table {
                catalog.delete_table("dependent").await.unwrap();
            } else {
                catalog.delete_column("dependent", "value").await.unwrap();
            }
            assert_eq!(
                dependent.commit().await.unwrap_err().kind(),
                crate::engine::kv::ErrorKind::Conflict,
                "case {name}"
            );
        }
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn write_value_type_mismatch_has_a_structured_reason() {
        let store = Arc::new(
            Store::memory("exec-write-value-type-mismatch")
                .await
                .unwrap(),
        );
        catalog::Catalog::new(store.clone())
            .create_table(TableDef {
                id: SchemaId::new(20).unwrap(),
                name: "measurements".into(),
                columns: vec![
                    ColumnDef {
                        id: SchemaId::new(20).unwrap(),
                        name: "id".into(),
                        scalar_type: ScalarType::Text,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                    ColumnDef {
                        id: SchemaId::new(21).unwrap(),
                        name: "value".into(),
                        scalar_type: ScalarType::Int64,
                        nullable: false,
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

        let error = Engine::new(store)
            .create(
                "measurements",
                Row::from([
                    ("id".into(), Value::Text("m1".into())),
                    ("value".into(), Value::Text("42".into())),
                ]),
            )
            .await
            .unwrap_err();

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.reason(), ErrorReason::TypeMismatch);
    }

    #[tokio::test]
    async fn direct_delete_validates_shape_even_for_an_empty_batch() {
        let store = Arc::new(Store::memory("exec-delete-empty-shape").await.unwrap());
        catalog::Catalog::new(store.clone())
            .create_table(TableDef {
                id: SchemaId::new(30).unwrap(),
                name: "records".into(),
                columns: vec![
                    ColumnDef {
                        id: SchemaId::new(30).unwrap(),
                        name: "id".into(),
                        scalar_type: ScalarType::Text,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                    ColumnDef {
                        id: SchemaId::new(31).unwrap(),
                        name: "payload".into(),
                        scalar_type: ScalarType::Text,
                        nullable: false,
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
        let engine = Engine::new(store);

        for input_type in [
            RowType { fields: Vec::new() },
            text_input_type(&["payload"]),
        ] {
            let error = engine
                .delete_many("records", input_type, Vec::new())
                .await
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
            assert_eq!(error.reason(), ErrorReason::MutationShape);
        }
    }

    fn text_input_type(names: &[&str]) -> RowType {
        RowType {
            fields: names
                .iter()
                .enumerate()
                .map(|(index, name)| Field {
                    name: (*name).into(),
                    slot: crate::engine::lir::SlotId(index),
                    value_type: Type::scalar(Kind::Text, false),
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn writes_apply_defaults_maintain_indexes_and_swap_unique_values() {
        let store = Arc::new(Store::memory("exec-engine-writes").await.unwrap());
        let catalog = catalog::Catalog::new(store.clone());
        let table = catalog
            .create_table(TableDef {
                id: SchemaId::new(1).unwrap(),
                name: "users".into(),
                columns: vec![
                    ColumnDef {
                        id: SchemaId::new(1).unwrap(),
                        name: "id".into(),
                        scalar_type: ScalarType::Text,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                    ColumnDef {
                        id: SchemaId::new(2).unwrap(),
                        name: "email".into(),
                        scalar_type: ScalarType::Text,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                    ColumnDef {
                        id: SchemaId::new(3).unwrap(),
                        name: "status".into(),
                        scalar_type: ScalarType::Text,
                        nullable: false,
                        format: String::new(),
                        default: Some(DefaultValue {
                            text: "active".into(),
                            ..DefaultValue::default()
                        }),
                    },
                ],
                primary_key: vec!["id".into()],
                indexes: vec![IndexDef {
                    name: "users_email_key".into(),
                    columns: vec!["email".into()],
                    unique: true,
                }],
                foreign_keys: Vec::new(),
            })
            .await
            .unwrap();
        let engine = Engine::new(store.clone());
        let created = engine
            .create_many(
                "users",
                vec![
                    Row::from([
                        ("id".into(), Value::Text("a".into())),
                        ("email".into(), Value::Text("a@example.com".into())),
                    ]),
                    Row::from([
                        ("id".into(), Value::Text("b".into())),
                        ("email".into(), Value::Text("b@example.com".into())),
                    ]),
                ],
            )
            .await
            .unwrap();
        assert!(
            created
                .iter()
                .all(|row| row["status"] == Value::Text("active".into()))
        );

        let updated = engine
            .update_many(
                "users",
                text_input_type(&["id", "email"]),
                vec![
                    Row::from([
                        ("id".into(), Value::Text("a".into())),
                        ("email".into(), Value::Text("b@example.com".into())),
                    ]),
                    Row::from([
                        ("id".into(), Value::Text("b".into())),
                        ("email".into(), Value::Text("a@example.com".into())),
                    ]),
                ],
            )
            .await
            .unwrap();
        assert_eq!(updated[0]["email"], Value::Text("b@example.com".into()));
        assert_eq!(updated[1]["email"], Value::Text("a@example.com".into()));

        for (id, email) in [("a", "b@example.com"), ("b", "a@example.com")] {
            let key = Row::from([("id".into(), Value::Text(id.into()))]);
            let primary_key = codec::encode_row_tuple(&key, &table.primary_key).unwrap();
            let raw = Kv::get(&*store, &codec::data_key(&table, &primary_key))
                .await
                .unwrap()
                .unwrap();
            let row = codec::unmarshal_row(&table, &raw).unwrap();
            assert_eq!(row["email"], Value::Text(email.into()));
            let tuple = codec::encode_tuple(&[Value::Text(email.into())]).unwrap();
            assert_eq!(
                Kv::get(
                    &*store,
                    &codec::index_key(&table, &table.indexes[0].id, &tuple, &primary_key)
                )
                .await
                .unwrap(),
                Some(Bytes::from(primary_key))
            );
        }
    }

    #[tokio::test]
    async fn failed_batches_and_restrict_deletes_roll_back_every_write() {
        let store = Arc::new(Store::memory("exec-engine-write-rollback").await.unwrap());
        let catalog = catalog::Catalog::new(store.clone());
        let parents = catalog
            .create_table(TableDef {
                id: SchemaId::new(1).unwrap(),
                name: "parents".into(),
                columns: vec![ColumnDef {
                    id: SchemaId::new(1).unwrap(),
                    name: "id".into(),
                    scalar_type: ScalarType::Text,
                    nullable: false,
                    format: String::new(),
                    default: None,
                }],
                primary_key: vec!["id".into()],
                indexes: Vec::new(),
                foreign_keys: Vec::new(),
            })
            .await
            .unwrap();
        catalog
            .create_table(TableDef {
                id: SchemaId::new(2).unwrap(),
                name: "children".into(),
                columns: vec![
                    ColumnDef {
                        id: SchemaId::new(1).unwrap(),
                        name: "id".into(),
                        scalar_type: ScalarType::Text,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                    ColumnDef {
                        id: SchemaId::new(2).unwrap(),
                        name: "parent_id".into(),
                        scalar_type: ScalarType::Text,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                ],
                primary_key: vec!["id".into()],
                indexes: Vec::new(),
                foreign_keys: vec![ForeignKeyDef {
                    name: "children_parent".into(),
                    columns: vec!["parent_id".into()],
                    ref_table: "parents".into(),
                    ref_columns: vec!["id".into()],
                }],
            })
            .await
            .unwrap();
        let engine = Engine::new(store.clone());
        engine
            .create(
                "parents",
                Row::from([("id".into(), Value::Text("p1".into()))]),
            )
            .await
            .unwrap();
        engine
            .create(
                "children",
                Row::from([
                    ("id".into(), Value::Text("c1".into())),
                    ("parent_id".into(), Value::Text("p1".into())),
                ]),
            )
            .await
            .unwrap();

        let error = engine
            .delete_many(
                "parents",
                text_input_type(&["id"]),
                vec![Row::from([("id".into(), Value::Text("p1".into()))])],
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), super::super::ErrorKind::ConstraintViolation);
        let key = codec::encode_tuple(&[Value::Text("p1".into())]).unwrap();
        assert!(
            Kv::get(&*store, &codec::data_key(&parents, &key))
                .await
                .unwrap()
                .is_some()
        );

        let error = engine
            .create_many(
                "parents",
                vec![
                    Row::from([("id".into(), Value::Text("dupe".into()))]),
                    Row::from([("id".into(), Value::Text("dupe".into()))]),
                ],
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), super::super::ErrorKind::ConstraintViolation);
        let key = codec::encode_tuple(&[Value::Text("dupe".into())]).unwrap();
        assert!(
            Kv::get(&*store, &codec::data_key(&parents, &key))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn writes_follow_online_index_and_replacement_protocols() {
        let store = Arc::new(Store::memory("exec-engine-online-writes").await.unwrap());
        let catalog = catalog::Catalog::new(store.clone());
        let indexed = catalog
            .create_table(TableDef {
                id: SchemaId::new(1).unwrap(),
                name: "indexed".into(),
                columns: vec![
                    ColumnDef {
                        id: SchemaId::new(1).unwrap(),
                        name: "id".into(),
                        scalar_type: ScalarType::Text,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                    ColumnDef {
                        id: SchemaId::new(2).unwrap(),
                        name: "status".into(),
                        scalar_type: ScalarType::Text,
                        nullable: false,
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
        let replaced = catalog
            .create_table(TableDef {
                id: SchemaId::new(2).unwrap(),
                name: "replaced".into(),
                columns: vec![
                    ColumnDef {
                        id: SchemaId::new(1).unwrap(),
                        name: "id".into(),
                        scalar_type: ScalarType::Text,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                    ColumnDef {
                        id: SchemaId::new(2).unwrap(),
                        name: "value".into(),
                        scalar_type: ScalarType::Text,
                        nullable: false,
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

        let index_transition = {
            let transaction = store
                .begin(IsolationLevel::SerializableSnapshot)
                .await
                .unwrap();
            let transition = {
                let mut view = TransactionView(&*transaction);
                let mut mutation = catalog::Mutation::new(&mut view);
                let transition = mutation
                    .start_index_build(
                        indexed.schema_id,
                        IndexDef {
                            name: "indexed_status_idx".into(),
                            columns: vec!["status".into()],
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
        let replacement_transition = {
            let transaction = store
                .begin(IsolationLevel::SerializableSnapshot)
                .await
                .unwrap();
            let transition = {
                let mut view = TransactionView(&*transaction);
                let mut mutation = catalog::Mutation::new(&mut view);
                let transition = mutation
                    .start_column_replacement(
                        replaced.schema_id,
                        SchemaId::new(2).unwrap(),
                        ColumnReplacementDef {
                            scalar_type: ScalarType::Int64,
                            nullable: false,
                            format: String::new(),
                            default: None,
                            conversion: ColumnConversion::StrictBuiltin,
                            prerequisites: Vec::new(),
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

        let engine = Engine::new(store.clone());
        engine
            .create(
                "indexed",
                Row::from([
                    ("id".into(), Value::Text("i1".into())),
                    ("status".into(), Value::Text("open".into())),
                ]),
            )
            .await
            .unwrap();
        engine
            .create(
                "replaced",
                Row::from([
                    ("id".into(), Value::Text("r1".into())),
                    ("value".into(), Value::Text("42".into())),
                ]),
            )
            .await
            .unwrap();

        let transaction = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let mut view = TransactionView(&*transaction);
        assert_eq!(
            catalog::store::delta_high_water(&mut view, &index_transition.id)
                .await
                .unwrap(),
            1
        );
        transaction.rollback();

        let current = catalog.get_table("replaced").await.unwrap().unwrap();
        let key = codec::encode_tuple(&[Value::Text("r1".into())]).unwrap();
        let raw = Kv::get(&*store, &codec::data_key(&current, &key))
            .await
            .unwrap()
            .unwrap();
        let target = &replacement_transition
            .column_replacement
            .as_ref()
            .expect("active replacement")
            .target;
        assert_eq!(
            codec::read_column_value(&raw, target).unwrap(),
            Value::Int64(42)
        );
    }

    #[tokio::test]
    async fn writes_enforce_new_write_constraint_protocols() {
        let store = Arc::new(Store::memory("exec-engine-constraint-write").await.unwrap());
        let catalog = catalog::Catalog::new(store.clone());
        let table = catalog
            .create_table(TableDef {
                id: SchemaId::new(1).unwrap(),
                name: "items".into(),
                columns: vec![
                    ColumnDef {
                        id: SchemaId::new(1).unwrap(),
                        name: "id".into(),
                        scalar_type: ScalarType::Text,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                    ColumnDef {
                        id: SchemaId::new(2).unwrap(),
                        name: "label".into(),
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
        let transaction = store
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        {
            let mut view = TransactionView(&*transaction);
            let mut mutation = catalog::Mutation::new(&mut view);
            mutation
                .start_constraint_validation(
                    table.schema_id,
                    ConstraintDef {
                        name: "items_label_not_null".into(),
                        kind: ConstraintKind::NotNull,
                        column_id: SchemaId::new(2).unwrap(),
                        prerequisites: Vec::new(),
                    },
                )
                .await
                .unwrap();
            mutation.finish().await.unwrap();
        }
        transaction.commit().await.unwrap();

        let engine = Engine::new(store.clone());
        let error = engine
            .create(
                "items",
                Row::from([
                    ("id".into(), Value::Text("i1".into())),
                    ("label".into(), Value::Null(ScalarType::Text)),
                ]),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), super::super::ErrorKind::ConstraintViolation);
        let current = catalog.get_table("items").await.unwrap().unwrap();
        let key = codec::encode_tuple(&[Value::Text("i1".into())]).unwrap();
        assert!(
            Kv::get(&*store, &codec::data_key(&current, &key))
                .await
                .unwrap()
                .is_none()
        );
    }
}
