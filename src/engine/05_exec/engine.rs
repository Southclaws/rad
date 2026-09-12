//! Snapshot-coherent bind, plan, and execute entry points.

use std::sync::{Arc, RwLock};
use std::time::Instant;

use async_trait::async_trait;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};
use tracing::Instrument as _;

use crate::engine::catalog;
use crate::engine::catalog::model::{Revision, Schema, SchemaTransition, Table};
use crate::engine::kv::{IsolationLevel, KvView, Transaction, TransactionView, TransactionalKv};
use crate::engine::lir::{self, Datum, Row, RowType};
use crate::engine::planner::bind;
use crate::engine::planner::bind::BoundStatement;
use crate::engine::planner::{PlanOptions, PlannerMode, PlanningContext, plan_query_with_context};
use crate::runtime::{RuntimeEffects, SystemRuntime};

use super::parallel::ExecutionScheduler;
use super::relation_cache::{CachedWork, DependencyValidation, PreparedReadResult, RelationCache};
use super::{
    CatalogPolicy, ConditionalQueryResult, EngineEvent, EngineEventHook, EngineOperation, Executor,
    Limits, NoopEngineEventHook, Program, ProgramOptions, ProgramResult, ReferenceExecutor, Result,
};

type CatalogObserver = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, Copy, Eq, PartialEq)]
enum CacheAccess {
    Enabled,
    Disabled,
}

pub struct Engine {
    pub(super) store: Arc<dyn TransactionalKv>,
    read_only: bool,
    limits: Limits,
    pub(super) runtime: Arc<dyn RuntimeEffects>,
    pub(super) events: Arc<dyn EngineEventHook>,
    cache_events_enabled: bool,
    pub(super) observer: Option<Arc<dyn super::observe::ExecutionObserver>>,
    pub(super) statistics: Option<Arc<dyn crate::engine::planner::estimator::StatisticsProvider>>,
    planner_mode: PlannerMode,
    execution_admission: Option<(Arc<Semaphore>, usize)>,
    execution_scheduler: Arc<ExecutionScheduler>,
    relation_cache: RelationCache,
    catalog_observers: RwLock<Vec<CatalogObserver>>,
}

struct ExecutionPermit {
    _permit: OwnedSemaphorePermit,
}

impl ExecutionPermit {
    fn new(permit: OwnedSemaphorePermit) -> Self {
        crate::telemetry::program_admitted_started();
        Self { _permit: permit }
    }
}

impl Drop for ExecutionPermit {
    fn drop(&mut self) {
        crate::telemetry::program_admitted_finished();
    }
}

struct QueueMeasurement {
    started: Instant,
    active: bool,
}

impl QueueMeasurement {
    fn start() -> Self {
        crate::telemetry::program_queue_started();
        Self {
            started: Instant::now(),
            active: true,
        }
    }

    fn finish(mut self) -> std::time::Duration {
        let duration = self.started.elapsed();
        crate::telemetry::program_queue_finished(duration);
        self.active = false;
        duration
    }
}

impl Drop for QueueMeasurement {
    fn drop(&mut self) {
        if self.active {
            crate::telemetry::program_queue_cancelled();
        }
    }
}

impl Engine {
    pub fn new(store: Arc<dyn TransactionalKv>) -> Self {
        Self::with_runtime(store, Arc::new(SystemRuntime))
    }

    pub fn with_runtime(store: Arc<dyn TransactionalKv>, runtime: Arc<dyn RuntimeEffects>) -> Self {
        Self {
            store,
            read_only: false,
            limits: Limits::default(),
            runtime,
            events: Arc::new(NoopEngineEventHook),
            cache_events_enabled: false,
            observer: None,
            statistics: None,
            planner_mode: PlannerMode::Cost,
            execution_admission: None,
            execution_scheduler: ExecutionScheduler::adaptive(1),
            relation_cache: RelationCache::default(),
            catalog_observers: RwLock::new(Vec::new()),
        }
    }

    pub fn read_only(store: Arc<dyn TransactionalKv>) -> Self {
        let mut engine = Self::new(store);
        engine.read_only = true;
        engine
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn require_write(&self) -> Result<()> {
        if self.read_only {
            return Err(super::Error::message(
                super::ErrorKind::ReadOnly,
                "database is read-only",
            ));
        }
        Ok(())
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
            read_only: false,
            limits,
            runtime,
            events: Arc::new(NoopEngineEventHook),
            cache_events_enabled: false,
            observer: None,
            statistics: None,
            planner_mode: PlannerMode::Cost,
            execution_admission: None,
            execution_scheduler: ExecutionScheduler::adaptive(1),
            relation_cache: RelationCache::default(),
            catalog_observers: RwLock::new(Vec::new()),
        }
    }

    pub fn with_relation_cache_limits(mut self, limits: super::RelationCacheLimits) -> Self {
        self.relation_cache = RelationCache::new(limits);
        if self.cache_events_enabled {
            self.relation_cache.enable_semantic_events();
        }
        self
    }

    #[cfg(test)]
    pub(super) fn relation_cache_stats(&self) -> super::relation_cache::RelationCacheStats {
        self.relation_cache.stats()
    }

    /// Install a semantic event hook before sharing the engine. Production
    /// uses the no-op hook; deterministic tests may suspend at these points.
    pub fn with_event_hook(mut self, events: Arc<dyn EngineEventHook>) -> Self {
        self.relation_cache.enable_semantic_events();
        self.events = events;
        self.cache_events_enabled = true;
        self
    }

    /// Install an execution observer before sharing the engine. The observer
    /// receives one statement observation per executed statement; the no-op
    /// observer disables collection and its fingerprinting cost entirely.
    pub fn with_observer(mut self, observer: Arc<dyn super::observe::ExecutionObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Install the model-snapshot provider consulted when plan views are
    /// collected. Estimates are advisory EXPLAIN output; plans never change.
    pub fn with_statistics_provider(
        mut self,
        statistics: Arc<dyn crate::engine::planner::estimator::StatisticsProvider>,
    ) -> Self {
        self.statistics = Some(statistics);
        self
    }

    pub(crate) fn with_planner_mode(mut self, planner_mode: PlannerMode) -> Self {
        self.planner_mode = planner_mode;
        self
    }

    pub(crate) fn with_execution_capacity(
        mut self,
        program_limit: usize,
        cpu_limit: usize,
    ) -> Self {
        let program_limit = program_limit.max(1);
        self.execution_admission = Some((Arc::new(Semaphore::new(program_limit)), program_limit));
        self.execution_scheduler = ExecutionScheduler::adaptive(cpu_limit);
        self
    }

    async fn admit_execution(&self) -> Option<ExecutionPermit> {
        let (admission, limit) = self.execution_admission.as_ref()?;
        let admission = admission.clone();
        match admission.clone().try_acquire_owned() {
            Ok(permit) => Some(ExecutionPermit::new(permit)),
            Err(TryAcquireError::NoPermits) => {
                let measurement = QueueMeasurement::start();
                let span = tracing::debug_span!(
                    target: "rad::telemetry",
                    "rad.program.queue",
                    otel.kind = "internal",
                    rad.program.admission_limit = *limit,
                    rad.program.queue_duration_ms = tracing::field::Empty,
                    rad.status = tracing::field::Empty,
                );
                let permit = admission
                    .acquire_owned()
                    .instrument(span.clone())
                    .await
                    .expect("execution admission remains open");
                let duration = measurement.finish();
                span.record(
                    "rad.program.queue_duration_ms",
                    duration.as_secs_f64() * 1_000.0,
                );
                span.record("rad.status", "success");
                Some(ExecutionPermit::new(permit))
            }
            Err(TryAcquireError::Closed) => unreachable!("execution admission remains open"),
        }
    }

    pub fn observer(&self) -> Option<&Arc<dyn super::observe::ExecutionObserver>> {
        self.observer.as_ref()
    }

    pub fn statistics(
        &self,
    ) -> Option<&Arc<dyn crate::engine::planner::estimator::StatisticsProvider>> {
        self.statistics.as_ref()
    }

    fn observation(&self) -> super::observe::Observation<'_> {
        super::observe::Observation {
            observer: self.observer.as_ref(),
        }
    }

    pub fn now_unix_micros(&self) -> u64 {
        self.runtime.now().timestamp_micros().max(0) as u64
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

    pub async fn catalog_revision(&self) -> Result<Revision> {
        let transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let result = {
            let mut view = TransactionView(transaction.as_ref());
            catalog::store::current_revision(&mut view).await
        };
        transaction.rollback();
        result.map_err(Into::into)
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
        self.execute_snapshot(query, PlanOptions::default(), false, CacheAccess::Enabled)
            .await
    }

    pub async fn execute_conditional<F>(
        &self,
        query: lir::Query,
        validator_matches: F,
    ) -> Result<ConditionalQueryResult>
    where
        F: FnOnce(&super::QueryValidator) -> bool,
    {
        let transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        // Preparation, validator comparison, and execution use this view. A
        // commit during this operation cannot mix dependency and row states.
        let result = {
            let view = TransactionView(&*transaction);
            self.execute_conditional_on_view(&view, query, validator_matches)
                .await
        };
        transaction.rollback();
        result
    }

    async fn execute_conditional_on_view<F>(
        &self,
        view: &dyn KvView,
        query: lir::Query,
        validator_matches: F,
    ) -> Result<ConditionalQueryResult>
    where
        F: FnOnce(&super::QueryValidator) -> bool,
    {
        let options = PlanOptions {
            mode: self.planner_mode,
            ..PlanOptions::default()
        };
        let validator_started = Instant::now();
        let bind_started = self.runtime.monotonic();
        let prepared = self.prepare_conditional_read(view, &query, options).await?;
        let bind_duration = self.runtime.monotonic().saturating_sub(bind_started);
        self.events
            .reach(EngineEvent::ConditionalQueryPrepared)
            .await;
        let validator = prepared.relation_key.query_validator();
        crate::telemetry::conditional_query_validator_finished(validator_started.elapsed());
        let unchanged = validator_matches(&validator);
        self.events
            .reach(EngineEvent::ConditionalQueryCompared { unchanged })
            .await;
        let statement = &prepared.prepared.statement;
        let plan = statement
            .plan
            .as_ref()
            .expect("a conditional read has a physical plan");
        let stamp = crate::engine::planner::models::DependencyStamp::of(&plan.dependencies);
        if unchanged {
            if let Some(observer) = &self.observer {
                observer.conditional_reuse(super::observe::ConditionalReuseObservation {
                    query: prepared.prepared.fingerprints.as_ref().clone(),
                    stamp,
                });
            }
            return Ok(ConditionalQueryResult::Unchanged { validator });
        }

        // A matching validator returns before this boundary. Scheduler capacity
        // and relation execution work are used only for a changed response.
        let _execution_permit = self.admit_execution().await;
        let execution_grant = self.execution_scheduler.enter();
        self.events
            .reach(EngineEvent::ConditionalQueryExecutionStarted)
            .await;
        let counters = super::observe::KvCounters::new(false);
        let execute_started = self.runtime.monotonic();
        let output = &plan.output;
        let cardinality = plan.cardinality;
        let subrelation_root_key = prepared.relation_key.clone();
        let mut cached = self
            .relation_cache
            .get_or_fill(prepared.relation_key, output, || async {
                let observed = super::observe::ObservedView::new(view, &counters);
                let mut executor = Executor::new(&observed, self.limits);
                executor.set_execution_grant(execution_grant);
                executor.use_subrelation_cache(
                    &self.relation_cache,
                    subrelation_root_key,
                    self.cache_events_enabled.then_some(&self.events),
                );
                let started = Instant::now();
                let frames = executor.run_frames(plan).await?;
                super::frames::validate_frame_cardinality(cardinality, frames.len())?;
                Ok((
                    frames,
                    CachedWork {
                        kv: counters.snapshot(),
                        execution: started.elapsed(),
                    },
                ))
            })
            .await?;
        super::relation_cache::reach_semantic_events(
            self.cache_events_enabled.then_some(&self.events),
            cached.take_events(),
        )
        .await;
        let result = cached.shape(statement.result_cardinality, &statement.result_output)?;
        let execute_duration = self.runtime.monotonic().saturating_sub(execute_started);
        let kv = counters.snapshot();
        if crate::telemetry::enabled() {
            crate::telemetry::statement_finished(
                "query",
                cached.source,
                "success",
                bind_duration,
                execute_duration,
                cached.len() as u64,
                &kv,
            );
        }
        if let Some(observer) = &self.observer {
            observer.statement(super::observe::StatementObservation {
                source: cached.source,
                query: prepared.prepared.fingerprints.as_ref().clone(),
                plan: Some(plan.fingerprint()),
                phase: super::observe::PhaseTimings {
                    bind: bind_duration,
                    execute: execute_duration,
                },
                rows: cached.len() as u64,
                estimate: statement.estimate,
                stamp,
                relations: Vec::new(),
                affected: 0,
                mutated: None,
                kv,
                operators: Vec::new(),
                physical_storage: None,
                join_operators: Vec::new(),
                failure: None,
            });
        }
        Ok(ConditionalQueryResult::Changed { result, validator })
    }

    async fn prepare_conditional_read(
        &self,
        view: &dyn KvView,
        query: &lir::Query,
        options: PlanOptions,
    ) -> Result<PreparedReadResult> {
        let statistics = self
            .statistics
            .as_ref()
            .map(|provider| provider.planning_stats());
        self.relation_cache
            .get_or_prepare_read(view, query, statistics.as_deref(), options, || async {
                let bound = bind::bind(
                    &ViewCatalog {
                        view,
                        relation_cache: Some(&self.relation_cache),
                    },
                    query.clone(),
                )
                .await?;
                let planned = plan_query_with_context(
                    &bound,
                    options,
                    PlanningContext {
                        statistics: statistics.as_deref(),
                    },
                );
                let result_output = bound.root.output().clone();
                let result_cardinality = bound.cardinality;
                Ok(BoundStatement {
                    name: "query".to_owned(),
                    result_output,
                    result_cardinality,
                    bound,
                    plan: Some(planned.plan),
                    estimate: planned.estimate,
                    target: None,
                })
            })
            .await
    }

    /// Conformance oracle: execute the selected physical plan without reusable caches.
    pub async fn execute_uncached(&self, query: lir::Query) -> Result<Datum> {
        self.execute_snapshot(query, PlanOptions::default(), false, CacheAccess::Disabled)
            .await
    }

    /// Conformance oracle: every narrowed access becomes a table scan while
    /// the full residual predicate remains authoritative.
    pub async fn execute_forced(&self, query: lir::Query) -> Result<Datum> {
        self.execute_snapshot(
            query,
            PlanOptions {
                full_scan_only: true,
                ..PlanOptions::default()
            },
            false,
            CacheAccess::Enabled,
        )
        .await
    }

    /// Conformance oracle for correlation: disable distinct-key batching.
    pub async fn execute_nested(&self, query: lir::Query) -> Result<Datum> {
        self.execute_snapshot(query, PlanOptions::default(), true, CacheAccess::Enabled)
            .await
    }

    /// Bind logical LIR and interpret it without a physical plan. This is a
    /// slow semantic oracle for differential and deterministic-scheduler tests.
    pub async fn execute_reference(&self, query: lir::Query) -> Result<Datum> {
        let transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let result = {
            let view = TransactionView(&*transaction);
            match bind::bind(
                &ViewCatalog {
                    view: &view,
                    relation_cache: None,
                },
                query,
            )
            .await
            {
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
        crate::telemetry::relation_cache_lookup("bypass");
        let execution_grant = self.execution_scheduler.enter();
        let view = TransactionView(&*transaction);
        execute_on_view(
            &view,
            query,
            PlanOptions {
                mode: self.planner_mode,
                ..PlanOptions::default()
            },
            false,
            self.limits,
            self.statistics
                .as_ref()
                .map(|provider| provider.planning_stats()),
            execution_grant,
            None,
            None,
        )
        .await
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
        self.execute_program_path(program, options, false, CacheAccess::Enabled)
            .await
    }

    /// Conformance oracle for PIR: execute the program without reusable caches.
    pub async fn execute_program_uncached(
        &self,
        program: Program,
        catalog_policy: CatalogPolicy,
    ) -> Result<ProgramResult> {
        self.execute_program_uncached_with_options(
            program,
            ProgramOptions {
                catalog: catalog_policy,
                ..ProgramOptions::default()
            },
        )
        .await
    }

    /// Conformance oracle for PIR options: execute the program without reusable caches.
    pub async fn execute_program_uncached_with_options(
        &self,
        program: Program,
        options: ProgramOptions,
    ) -> Result<ProgramResult> {
        self.execute_program_path(program, options, false, CacheAccess::Disabled)
            .await
    }

    pub(crate) async fn begin_frontend_transaction(&self) -> Result<Box<dyn Transaction>> {
        self.store
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .map_err(Into::into)
    }

    pub(crate) async fn execute_program_in_transaction(
        &self,
        transaction: &mut dyn Transaction,
        program: &Program,
        catalog_policy: CatalogPolicy,
        relation_cache_eligible: bool,
    ) -> Result<ProgramResult> {
        let effectful = program.statements.iter().any(super::Statement::effectful);
        if effectful {
            self.require_write()?;
        }
        let result_name = super::program::validate(program, catalog_policy)?;
        let _execution_permit = self.admit_execution().await;
        let execution_grant = if effectful {
            super::parallel::ExecutionGrant::serial()
        } else {
            self.execution_scheduler.enter()
        };
        let mut view = TransactionView(&*transaction);
        let statistics = self
            .statistics
            .as_ref()
            .map(|provider| provider.planning_stats());
        super::program::run(
            &mut view,
            program,
            result_name.as_deref(),
            catalog_policy,
            self.limits,
            &self.runtime,
            super::program::RunContext {
                observation: self.observation(),
                relation_cache: relation_cache_eligible.then_some(&self.relation_cache),
                dependency_validation: DependencyValidation::Transaction,
                statistics,
                plan_options: PlanOptions {
                    mode: self.planner_mode,
                    ..PlanOptions::default()
                },
                collect_plan: false,
                execution_grant,
                cache_events: self.cache_events_enabled.then_some(&self.events),
            },
        )
        .await
    }

    pub(crate) async fn commit_frontend_transaction(
        &self,
        transaction: Box<dyn Transaction>,
        catalog_statements: Vec<String>,
    ) -> Result<()> {
        let operation =
            (!catalog_statements.is_empty()).then_some(EngineOperation::CatalogProgram {
                statements: catalog_statements,
            });
        let catalog_changed = operation.is_some();
        self.finish_transaction(transaction, Ok(()), operation)
            .await?;
        if catalog_changed {
            self.notify_catalog_change();
        }
        Ok(())
    }

    /// Execute a PIR program with relational statements interpreted from bound
    /// logical LIR. Transaction, catalog, and mutation orchestration stays the
    /// same so differential tests isolate planner/executor semantics.
    pub async fn execute_program_reference_with_options(
        &self,
        program: Program,
        options: ProgramOptions,
    ) -> Result<ProgramResult> {
        self.execute_program_path(program, options, true, CacheAccess::Disabled)
            .await
    }

    async fn execute_program_path(
        &self,
        program: Program,
        options: ProgramOptions,
        reference: bool,
        cache_access: CacheAccess,
    ) -> Result<ProgramResult> {
        let effectful = program.statements.iter().any(super::Statement::effectful);
        if effectful {
            self.require_write()?;
        }
        let result_name = super::program::validate(&program, options.catalog)?;
        let _execution_permit = self.admit_execution().await;
        let execution_grant = if effectful {
            super::parallel::ExecutionGrant::serial()
        } else {
            self.execution_scheduler.enter()
        };
        let catalog_statements = program
            .statements
            .iter()
            .filter(|statement| !statement.relational())
            .map(|statement| statement.name().to_owned())
            .collect::<Vec<_>>();
        let statistics = self
            .statistics
            .as_ref()
            .map(|provider| provider.planning_stats());
        let plan_options = PlanOptions {
            mode: self.planner_mode,
            ..PlanOptions::default()
        };
        let plans = if effectful || options.dry_run || reference {
            let preflight = self
                .store
                .begin(IsolationLevel::SerializableSnapshot)
                .await?;
            let preflight_result = {
                let mut view = TransactionView(&*preflight);
                match super::program::expect_catalog(&mut view, options.expected_catalog.as_ref())
                    .await
                {
                    Ok(()) => {
                        super::program::preflight(
                            &mut view,
                            &program,
                            options.catalog,
                            options.collect_plan,
                            !reference,
                            false,
                            &self.runtime,
                            statistics.clone(),
                            plan_options,
                        )
                        .await
                    }
                    Err(error) => Err(error),
                }
            };
            preflight.rollback();
            preflight_result?.plans
        } else {
            Vec::new()
        };
        if options.dry_run {
            return Ok(ProgramResult {
                result: Datum::Null,
                statements: Vec::new(),
                plans,
            });
        }

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
                        super::program::RunContext {
                            observation: self.observation(),
                            relation_cache: (!effectful
                                && !options.collect_plan
                                && cache_access == CacheAccess::Enabled)
                                .then_some(&self.relation_cache),
                            dependency_validation: DependencyValidation::Snapshot,
                            statistics: statistics.clone(),
                            plan_options,
                            collect_plan: options.collect_plan,
                            execution_grant,
                            cache_events: self.cache_events_enabled.then_some(&self.events),
                        },
                    )
                    .await
                })
                .map(|mut result| {
                    if reference {
                        result.plans = plans;
                    }
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
                let catalog_changed = catalog_operation.is_some();
                let result = self
                    .finish_transaction(transaction, Ok(result), catalog_operation)
                    .await?;
                if catalog_changed {
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

    pub async fn prepare_program_estimates(
        &self,
        program: &Program,
        statistics: Arc<crate::engine::planner::models::PlannerStats>,
    ) -> Result<Vec<super::PreparedStatementEstimate>> {
        self.prepare_program_estimates_with_mode(program, statistics, self.planner_mode)
            .await
    }

    pub async fn prepare_program_estimates_with_mode(
        &self,
        program: &Program,
        statistics: Arc<crate::engine::planner::models::PlannerStats>,
        planner_mode: PlannerMode,
    ) -> Result<Vec<super::PreparedStatementEstimate>> {
        let transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let result = {
            let mut view = TransactionView(transaction.as_ref());
            super::program::preflight(
                &mut view,
                program,
                CatalogPolicy::RevisionPerStatement,
                false,
                true,
                true,
                &self.runtime,
                Some(statistics),
                PlanOptions {
                    mode: planner_mode,
                    ..PlanOptions::default()
                },
            )
            .await
            .map(|preflight| preflight.estimates)
        };
        transaction.rollback();
        result
    }

    pub async fn create(&self, table: &str, row: Row) -> Result<Row> {
        let mut rows = self.create_many(table, vec![row]).await?;
        Ok(rows.pop().expect("one input produces one row"))
    }

    pub async fn create_many(&self, table: &str, rows: Vec<Row>) -> Result<Vec<Row>> {
        self.require_write()?;
        let transaction = self
            .store
            .begin(IsolationLevel::SerializableSnapshot)
            .await?;
        let result = {
            let mut view = TransactionView(&*transaction);
            create_on_view(&mut view, table, &rows, self.runtime.as_ref()).await
        };
        self.finish_transaction(transaction, result, None).await
    }

    pub async fn create_many_in(
        &self,
        transaction: &mut dyn Transaction,
        table: &str,
        rows: &[Row],
    ) -> Result<Vec<Row>> {
        self.require_write()?;
        let mut view = TransactionView(&*transaction);
        create_on_view(&mut view, table, rows, self.runtime.as_ref()).await
    }

    pub async fn update_many(
        &self,
        table: &str,
        input_type: RowType,
        rows: Vec<Row>,
    ) -> Result<Vec<Row>> {
        self.require_write()?;
        let transaction = self
            .store
            .begin(IsolationLevel::SerializableSnapshot)
            .await?;
        let result = {
            let mut view = TransactionView(&*transaction);
            update_on_view(&mut view, table, &input_type, &rows).await
        };
        self.finish_transaction(transaction, result, None).await
    }

    pub async fn update_many_in(
        &self,
        transaction: &mut dyn Transaction,
        table: &str,
        input_type: &RowType,
        rows: &[Row],
    ) -> Result<Vec<Row>> {
        self.require_write()?;
        let mut view = TransactionView(&*transaction);
        update_on_view(&mut view, table, input_type, rows).await
    }

    pub async fn delete_many(
        &self,
        table: &str,
        input_type: RowType,
        rows: Vec<Row>,
    ) -> Result<Vec<Row>> {
        self.require_write()?;
        let transaction = self
            .store
            .begin(IsolationLevel::SerializableSnapshot)
            .await?;
        let result = {
            let mut view = TransactionView(&*transaction);
            delete_on_view(&mut view, table, &input_type, &rows).await
        };
        self.finish_transaction(transaction, result, None).await
    }

    pub async fn delete_many_in(
        &self,
        transaction: &mut dyn Transaction,
        table: &str,
        input_type: &RowType,
        rows: &[Row],
    ) -> Result<Vec<Row>> {
        self.require_write()?;
        let mut view = TransactionView(&*transaction);
        delete_on_view(&mut view, table, input_type, rows).await
    }

    async fn execute_snapshot(
        &self,
        query: lir::Query,
        options: PlanOptions,
        force_nested: bool,
        cache_access: CacheAccess,
    ) -> Result<Datum> {
        let transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let execution_grant = self.execution_scheduler.enter();
        let relation_cache =
            if cache_access == CacheAccess::Disabled || force_nested || options.full_scan_only {
                crate::telemetry::relation_cache_lookup("bypass");
                None
            } else {
                Some(&self.relation_cache)
            };
        let result = {
            let view = TransactionView(&*transaction);
            execute_on_view(
                &view,
                query,
                PlanOptions {
                    mode: self.planner_mode,
                    ..options
                },
                force_nested,
                self.limits,
                self.statistics
                    .as_ref()
                    .map(|provider| provider.planning_stats()),
                execution_grant,
                relation_cache,
                self.cache_events_enabled.then_some(&self.events),
            )
            .await
        };
        transaction.rollback();
        result
    }

    pub(super) async fn finish_transaction<T>(
        &self,
        transaction: Box<dyn Transaction>,
        result: Result<T>,
        operation: Option<EngineOperation>,
    ) -> Result<T> {
        match result {
            Ok(value) => {
                if let Some(operation) = operation.clone() {
                    self.events
                        .reach(EngineEvent::CommitStarted { operation })
                        .await;
                }
                transaction.commit().await?;
                if let Some(operation) = operation {
                    self.events
                        .reach(EngineEvent::CommitSucceeded { operation })
                        .await;
                }
                Ok(value)
            }
            Err(error) => {
                transaction.rollback();
                Err(error)
            }
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

#[allow(clippy::too_many_arguments)]
async fn execute_on_view(
    view: &dyn KvView,
    query: lir::Query,
    options: PlanOptions,
    force_nested: bool,
    limits: Limits,
    statistics: Option<Arc<crate::engine::planner::models::PlannerStats>>,
    execution_grant: super::parallel::ExecutionGrant,
    relation_cache: Option<&RelationCache>,
    cache_events: Option<&Arc<dyn EngineEventHook>>,
) -> Result<Datum> {
    let bound = bind::bind(
        &ViewCatalog {
            view,
            relation_cache,
        },
        query,
    )
    .await?;
    let planned = plan_query_with_context(
        &bound,
        options,
        PlanningContext {
            statistics: statistics.as_deref(),
        },
    );
    let Some(relation_cache) = relation_cache else {
        let mut executor = Executor::new(view, limits);
        executor.set_execution_grant(execution_grant);
        executor.set_force_nested(force_nested);
        return executor.execute(&planned.plan).await;
    };
    let fingerprints = lir::fingerprint::query(&bound);
    let key = relation_cache
        .key_for_view(
            fingerprints.exact,
            view,
            &planned.plan.dependencies,
            DependencyValidation::Snapshot,
        )
        .await?;
    let output = &planned.plan.output;
    let cardinality = planned.plan.cardinality;
    let subrelation_root_key = key.clone();
    let mut cached = relation_cache
        .get_or_fill(key, output, || async {
            let counters = super::observe::KvCounters::new(false);
            let observed = super::observe::ObservedView::new(view, &counters);
            let mut executor = Executor::new(&observed, limits);
            executor.set_execution_grant(execution_grant);
            executor.use_subrelation_cache(relation_cache, subrelation_root_key, cache_events);
            let started = Instant::now();
            let frames = executor.run_frames(&planned.plan).await?;
            super::frames::validate_frame_cardinality(cardinality, frames.len())?;
            Ok((
                frames,
                CachedWork {
                    kv: counters.snapshot(),
                    execution: started.elapsed(),
                },
            ))
        })
        .await?;
    super::relation_cache::reach_semantic_events(cache_events, cached.take_events()).await;
    cached.shape(cardinality, output)
}

pub(super) struct ViewCatalog<'a> {
    pub(super) view: &'a dyn KvView,
    pub(super) relation_cache: Option<&'a RelationCache>,
}

#[async_trait]
impl bind::Catalog for ViewCatalog<'_> {
    async fn get_table(&self, name: &str) -> catalog::Result<Option<Table>> {
        match self.relation_cache {
            Some(cache) => cache.catalog_table_for_snapshot(self.view, name).await,
            None => catalog::store::get_table(self.view, name).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

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
    use crate::engine::exec::row_store;
    use crate::engine::exec::{ErrorKind, ErrorReason, Statement};
    use crate::engine::kv::Kv;
    use crate::engine::kv::slatedb::Store;
    use crate::engine::lir::{
        self, BinaryOp, Expr, Field, Kind, Literal, RawScalar, Relation, RootCardinality, Type,
        Value,
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

    struct FixedPlannerStats(Arc<crate::engine::planner::models::PlannerStats>);

    impl crate::engine::planner::estimator::StatisticsProvider for FixedPlannerStats {
        fn planning_stats(&self) -> Arc<crate::engine::planner::models::PlannerStats> {
            self.0.clone()
        }
    }

    fn join_cache_table(id: u32, name: &str) -> TableDef {
        TableDef {
            id: SchemaId::new(id).unwrap(),
            name: name.into(),
            columns: vec![
                ColumnDef {
                    id: SchemaId::new(id * 10 + 1).unwrap(),
                    name: "id".into(),
                    scalar_type: ScalarType::Text,
                    nullable: false,
                    format: String::new(),
                    default: None,
                },
                ColumnDef {
                    id: SchemaId::new(id * 10 + 2).unwrap(),
                    name: "customer_id".into(),
                    scalar_type: ScalarType::Text,
                    nullable: true,
                    format: String::new(),
                    default: None,
                },
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        }
    }

    fn complete_join_stats(
        table: &crate::engine::catalog::model::Table,
        rows: u64,
    ) -> crate::engine::planner::models::PlannerStats {
        use crate::engine::planner::models::{
            ColumnSynopsis, PlannerStats, SynopsisCoverage, SynopsisModel,
        };

        let mut statistics = PlannerStats::empty();
        statistics.synopsis_models.insert(
            table.schema_id,
            SynopsisModel {
                table: table.schema_id,
                observed_rows: rows,
                coverage: SynopsisCoverage::Complete,
                sample_size: rows,
                changes_since_collection: 0,
                table_existence_generation: table.existence_generation.get(),
                collected_at_unix_micros: 0,
                catalog_version: 1,
                columns: table
                    .columns
                    .iter()
                    .map(|column| ColumnSynopsis {
                        column: column.schema_id,
                        value_generation: column.value_generation.get(),
                        null_fraction: 0.0,
                        null_count: 0,
                        distinct: rows.max(1),
                        distinct_is_exact: false,
                        average_width: 8,
                        maximum_width: Some(8),
                        minimum: None,
                        maximum: None,
                        most_common_values: Vec::new(),
                        range_distribution: None,
                        degree_sequence: None,
                    })
                    .collect(),
                column_groups: Vec::new(),
                predicate_conditioned_degrees: Vec::new(),
            },
        );
        statistics
    }

    fn customer_orders_query() -> lir::Query {
        let joined = Relation::Join {
            left: Box::new(Relation::Scan {
                table: "customers".into(),
                scope: "customer".into(),
            }),
            right: Box::new(Relation::Scan {
                table: "orders".into(),
                scope: "order".into(),
            }),
            kind: crate::engine::lir::JoinKind::Inner,
            on: Expr::Binary {
                op: BinaryOp::Eq,
                left: Box::new(Expr::Column {
                    scope: "customer".into(),
                    name: "customer_id".into(),
                }),
                right: Box::new(Expr::Column {
                    scope: "order".into(),
                    name: "customer_id".into(),
                }),
            },
        };
        let projected = Relation::Project {
            input: Box::new(joined),
            scope: Some("result".into()),
            spread: Vec::new(),
            fields: vec![
                crate::engine::lir::ProjectField {
                    name: "customer_id".into(),
                    expression: Expr::Column {
                        scope: "customer".into(),
                        name: "id".into(),
                    },
                },
                crate::engine::lir::ProjectField {
                    name: "order_id".into(),
                    expression: Expr::Column {
                        scope: "order".into(),
                        name: "id".into(),
                    },
                },
            ],
        };
        lir::Query {
            root: Relation::Order {
                input: Box::new(projected),
                terms: vec![crate::engine::lir::OrderTerm {
                    expression: Expr::Column {
                        scope: "result".into(),
                        name: "order_id".into(),
                    },
                    descending: false,
                }],
            },
            cardinality: RootCardinality::Many,
            bindings: HashMap::new(),
        }
    }

    fn recursive_edge_query() -> lir::Query {
        let anchor = Relation::Project {
            input: Box::new(Relation::Scan {
                table: "seeds".into(),
                scope: "seed".into(),
            }),
            scope: Some("anchor".into()),
            spread: Vec::new(),
            fields: vec![crate::engine::lir::ProjectField {
                name: "id".into(),
                expression: Expr::Column {
                    scope: "seed".into(),
                    name: "id".into(),
                },
            }],
        };
        let step = Relation::Project {
            input: Box::new(Relation::Join {
                left: Box::new(Relation::RecursiveRef {
                    binding: "reachable".into(),
                    scope: "frontier".into(),
                }),
                right: Box::new(Relation::Scan {
                    table: "edges".into(),
                    scope: "edge".into(),
                }),
                kind: crate::engine::lir::JoinKind::Inner,
                on: Expr::Binary {
                    op: BinaryOp::Eq,
                    left: Box::new(Expr::Column {
                        scope: "frontier".into(),
                        name: "id".into(),
                    }),
                    right: Box::new(Expr::Column {
                        scope: "edge".into(),
                        name: "customer_id".into(),
                    }),
                },
            }),
            scope: Some("step".into()),
            spread: Vec::new(),
            fields: vec![crate::engine::lir::ProjectField {
                name: "id".into(),
                expression: Expr::Column {
                    scope: "edge".into(),
                    name: "id".into(),
                },
            }],
        };
        lir::Query {
            root: Relation::Order {
                input: Box::new(Relation::Ref {
                    binding: "reachable".into(),
                    scope: "result".into(),
                }),
                terms: vec![crate::engine::lir::OrderTerm {
                    expression: Expr::Column {
                        scope: "result".into(),
                        name: "id".into(),
                    },
                    descending: false,
                }],
            },
            cardinality: RootCardinality::Many,
            bindings: HashMap::from([(
                "reachable".into(),
                Relation::Recursive {
                    anchor: Box::new(anchor),
                    step: Box::new(step),
                    accumulation: crate::engine::lir::RecursiveAccumulation::New,
                },
            )]),
        }
    }

    fn customer_order_summary_query() -> lir::Query {
        let joined = Relation::Join {
            left: Box::new(Relation::Scan {
                table: "customers".into(),
                scope: "customer".into(),
            }),
            right: Box::new(Relation::Scan {
                table: "orders".into(),
                scope: "order".into(),
            }),
            kind: crate::engine::lir::JoinKind::Inner,
            on: Expr::Binary {
                op: BinaryOp::Eq,
                left: Box::new(Expr::Column {
                    scope: "customer".into(),
                    name: "id".into(),
                }),
                right: Box::new(Expr::Column {
                    scope: "order".into(),
                    name: "customer_id".into(),
                }),
            },
        };
        let aggregate = Relation::Aggregate {
            input: Box::new(joined),
            scope: Some("summary".into()),
            groups: vec![lir::GroupTerm {
                name: "customer_id".into(),
                expression: Expr::Column {
                    scope: "customer".into(),
                    name: "id".into(),
                },
            }],
            terms: vec![lir::AggregateTerm {
                function: lir::AggregateFunction::Count,
                argument: Some(Expr::Column {
                    scope: "order".into(),
                    name: "id".into(),
                }),
                name: "order_count".into(),
            }],
        };
        lir::Query {
            root: Relation::Order {
                input: Box::new(aggregate),
                terms: vec![lir::OrderTerm {
                    expression: Expr::Column {
                        scope: "summary".into(),
                        name: "customer_id".into(),
                    },
                    descending: false,
                }],
            },
            cardinality: RootCardinality::Many,
            bindings: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn execution_admission_applies_backpressure() {
        let store = Arc::new(Store::memory("execution-admission").await.unwrap());
        let engine = Engine::new(store.clone()).with_execution_capacity(2, 1);

        let first = engine.admit_execution().await.unwrap();
        let second = engine.admit_execution().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), engine.admit_execution())
                .await
                .is_err()
        );

        drop(first);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), engine.admit_execution())
                .await
                .unwrap()
                .is_some()
        );
        drop(second);
        store.close().await.unwrap();
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
    async fn increment_defaults_reserve_batches_and_follow_explicit_floors() {
        let store = Arc::new(Store::memory("exec-increment-batches").await.unwrap());
        catalog::Catalog::new(store.clone())
            .create_table(TableDef {
                id: SchemaId::new(1).unwrap(),
                name: "items".into(),
                columns: vec![
                    ColumnDef {
                        id: SchemaId::new(1).unwrap(),
                        name: "key".into(),
                        scalar_type: ScalarType::Text,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                    ColumnDef {
                        id: SchemaId::new(2).unwrap(),
                        name: "serial".into(),
                        scalar_type: ScalarType::Int64,
                        nullable: false,
                        format: String::new(),
                        default: Some(DefaultValue {
                            function: Some(DefaultFunction::Increment),
                            ..DefaultValue::default()
                        }),
                    },
                    ColumnDef {
                        id: SchemaId::new(3).unwrap(),
                        name: "secondary".into(),
                        scalar_type: ScalarType::Int64,
                        nullable: true,
                        format: String::new(),
                        default: Some(DefaultValue {
                            function: Some(DefaultFunction::Increment),
                            ..DefaultValue::default()
                        }),
                    },
                ],
                primary_key: vec!["key".into()],
                indexes: Vec::new(),
                foreign_keys: Vec::new(),
            })
            .await
            .unwrap();
        let engine = Engine::new(store.clone());

        let created = engine
            .create_many(
                "items",
                vec![
                    Row::from([("key".into(), Value::Text("a".into()))]),
                    Row::from([
                        ("key".into(), Value::Text("b".into())),
                        ("serial".into(), Value::Int64(10)),
                        ("secondary".into(), Value::Null(ScalarType::Int64)),
                    ]),
                    Row::from([
                        ("key".into(), Value::Text("c".into())),
                        ("serial".into(), Value::Int64(-5)),
                        ("secondary".into(), Value::Int64(20)),
                    ]),
                ],
            )
            .await
            .unwrap();
        assert_eq!(created[0]["serial"], Value::Int64(11));
        assert_eq!(created[0]["secondary"], Value::Int64(21));
        assert_eq!(created[1]["serial"], Value::Int64(10));
        assert_eq!(created[1]["secondary"], Value::Null(ScalarType::Int64));
        assert_eq!(created[2]["serial"], Value::Int64(-5));
        assert_eq!(created[2]["secondary"], Value::Int64(20));

        let next = engine
            .create(
                "items",
                Row::from([("key".into(), Value::Text("d".into()))]),
            )
            .await
            .unwrap();
        assert_eq!(next["serial"], Value::Int64(12));
        assert_eq!(next["secondary"], Value::Int64(22));

        let updated = engine
            .update_many(
                "items",
                text_int64_input_type("key", "secondary"),
                vec![Row::from([
                    ("key".into(), Value::Text("d".into())),
                    ("secondary".into(), Value::Int64(100)),
                ])],
            )
            .await
            .unwrap();
        assert_eq!(updated[0]["secondary"], Value::Int64(100));

        let after_update = engine
            .create(
                "items",
                Row::from([("key".into(), Value::Text("e".into()))]),
            )
            .await
            .unwrap();
        assert_eq!(after_update["serial"], Value::Int64(13));
        assert_eq!(after_update["secondary"], Value::Int64(101));
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn increment_allocation_is_transactional_conflicting_and_checked() {
        let store = Arc::new(Store::memory("exec-increment-transactions").await.unwrap());
        catalog::Catalog::new(store.clone())
            .create_table(TableDef {
                id: SchemaId::new(1).unwrap(),
                name: "items".into(),
                columns: vec![
                    ColumnDef {
                        id: SchemaId::new(1).unwrap(),
                        name: "key".into(),
                        scalar_type: ScalarType::Text,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                    ColumnDef {
                        id: SchemaId::new(2).unwrap(),
                        name: "serial".into(),
                        scalar_type: ScalarType::Int64,
                        nullable: false,
                        format: String::new(),
                        default: Some(DefaultValue {
                            function: Some(DefaultFunction::Increment),
                            ..DefaultValue::default()
                        }),
                    },
                ],
                primary_key: vec!["key".into()],
                indexes: Vec::new(),
                foreign_keys: Vec::new(),
            })
            .await
            .unwrap();
        let engine = Engine::new(store.clone());

        let failed = engine.create("items", Row::new()).await.unwrap_err();
        assert_eq!(failed.kind(), ErrorKind::ConstraintViolation);
        let first = engine
            .create(
                "items",
                Row::from([("key".into(), Value::Text("a".into()))]),
            )
            .await
            .unwrap();
        assert_eq!(first["serial"], Value::Int64(1));
        let constraint_failure = engine
            .create(
                "items",
                Row::from([("key".into(), Value::Text("a".into()))]),
            )
            .await
            .unwrap_err();
        assert_eq!(constraint_failure.kind(), ErrorKind::ConstraintViolation);

        let mut rolled_back = store
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        let allocated = engine
            .create_many_in(
                rolled_back.as_mut(),
                "items",
                &[Row::from([("key".into(), Value::Text("b".into()))])],
            )
            .await
            .unwrap();
        assert_eq!(allocated[0]["serial"], Value::Int64(2));
        rolled_back.rollback();
        let reused = engine
            .create(
                "items",
                Row::from([("key".into(), Value::Text("b".into()))]),
            )
            .await
            .unwrap();
        assert_eq!(reused["serial"], Value::Int64(2));

        let mut winner = store
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        let mut loser = store
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        let winner_rows = engine
            .create_many_in(
                winner.as_mut(),
                "items",
                &[Row::from([("key".into(), Value::Text("c".into()))])],
            )
            .await
            .unwrap();
        let loser_rows = engine
            .create_many_in(
                loser.as_mut(),
                "items",
                &[Row::from([("key".into(), Value::Text("d".into()))])],
            )
            .await
            .unwrap();
        assert_eq!(winner_rows[0]["serial"], Value::Int64(3));
        assert_eq!(loser_rows[0]["serial"], Value::Int64(3));
        winner.commit().await.unwrap();
        assert_eq!(
            loser.commit().await.unwrap_err().kind(),
            crate::engine::kv::ErrorKind::Conflict
        );
        let retry = engine
            .create(
                "items",
                Row::from([("key".into(), Value::Text("d".into()))]),
            )
            .await
            .unwrap();
        assert_eq!(retry["serial"], Value::Int64(4));

        engine
            .delete_many(
                "items",
                text_input_type(&["key"]),
                vec![Row::from([("key".into(), Value::Text("d".into()))])],
            )
            .await
            .unwrap();
        let after_delete = engine
            .create(
                "items",
                Row::from([("key".into(), Value::Text("e".into()))]),
            )
            .await
            .unwrap();
        assert_eq!(after_delete["serial"], Value::Int64(5));

        engine
            .create(
                "items",
                Row::from([
                    ("key".into(), Value::Text("max".into())),
                    ("serial".into(), Value::Int64(i64::MAX)),
                ]),
            )
            .await
            .unwrap();
        engine
            .update_many(
                "items",
                text_int64_input_type("key", "serial"),
                vec![Row::from([
                    ("key".into(), Value::Text("max".into())),
                    ("serial".into(), Value::Int64(-1)),
                ])],
            )
            .await
            .unwrap();
        let exhausted = engine
            .create(
                "items",
                Row::from([("key".into(), Value::Text("overflow".into()))]),
            )
            .await
            .unwrap_err();
        assert_eq!(exhausted.reason(), ErrorReason::NumericOverflow);
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn increment_column_lifecycle_preserves_identity_and_cleans_state() {
        let store = Arc::new(Store::memory("exec-increment-lifecycle").await.unwrap());
        let catalog = catalog::Catalog::new(store.clone());
        catalog
            .create_table(TableDef {
                id: SchemaId::new(1).unwrap(),
                name: "items".into(),
                columns: vec![ColumnDef {
                    id: SchemaId::new(1).unwrap(),
                    name: "key".into(),
                    scalar_type: ScalarType::Text,
                    nullable: false,
                    format: String::new(),
                    default: None,
                }],
                primary_key: vec!["key".into()],
                indexes: Vec::new(),
                foreign_keys: Vec::new(),
            })
            .await
            .unwrap();
        let engine = Engine::new(store.clone());
        engine
            .create(
                "items",
                Row::from([("key".into(), Value::Text("old".into()))]),
            )
            .await
            .unwrap();

        let with_increment = catalog
            .create_column(
                "items",
                ColumnDef {
                    id: SchemaId::new(2).unwrap(),
                    name: "serial".into(),
                    scalar_type: ScalarType::Int64,
                    nullable: true,
                    format: String::new(),
                    default: Some(DefaultValue {
                        function: Some(DefaultFunction::Increment),
                        ..DefaultValue::default()
                    }),
                },
            )
            .await
            .unwrap();
        let column_id = with_increment.column("serial").unwrap().schema_id;
        let allocator_key =
            catalog::store::column_increment_key(&with_increment.id, column_id).unwrap();
        let historical = engine.scan_table_rows(&with_increment).await.unwrap();
        assert_eq!(historical[0]["serial"], Value::Null(ScalarType::Int64));
        let generated = engine
            .create(
                "items",
                Row::from([("key".into(), Value::Text("new".into()))]),
            )
            .await
            .unwrap();
        assert_eq!(generated["serial"], Value::Int64(1));

        let renamed = catalog
            .rename_column("items", "serial", "number")
            .await
            .unwrap();
        assert_eq!(renamed.column("number").unwrap().schema_id, column_id);
        let after_rename = engine
            .create(
                "items",
                Row::from([("key".into(), Value::Text("renamed".into()))]),
            )
            .await
            .unwrap();
        assert_eq!(after_rename["number"], Value::Int64(2));
        assert!(
            catalog
                .change_column_insert_default("items", "number", None)
                .await
                .unwrap_err()
                .to_string()
                .contains("immutable")
        );

        catalog.delete_column("items", "number").await.unwrap();
        assert!(Kv::get(&*store, &allocator_key).await.unwrap().is_none());
        let recreated = catalog
            .create_column(
                "items",
                ColumnDef {
                    id: SchemaId::new(3).unwrap(),
                    name: "number".into(),
                    scalar_type: ScalarType::Int64,
                    nullable: true,
                    format: String::new(),
                    default: Some(DefaultValue {
                        function: Some(DefaultFunction::Increment),
                        ..DefaultValue::default()
                    }),
                },
            )
            .await
            .unwrap();
        assert_ne!(recreated.column("number").unwrap().schema_id, column_id);
        let fresh = engine
            .create(
                "items",
                Row::from([("key".into(), Value::Text("fresh".into()))]),
            )
            .await
            .unwrap();
        assert_eq!(fresh["number"], Value::Int64(1));
        let recreated_key = catalog::store::column_increment_key(
            &recreated.id,
            recreated.column("number").unwrap().schema_id,
        )
        .unwrap();
        catalog.delete_table("items").await.unwrap();
        assert!(Kv::get(&*store, &recreated_key).await.unwrap().is_none());
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
            Bytes::from(codec::data_key(&table, &primary_key).unwrap()),
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

    #[tokio::test]
    async fn relation_cache_uses_dependency_local_generations() {
        let store = Arc::new(Store::memory("exec-relation-cache").await.unwrap());
        let catalog = catalog::Catalog::new(store.clone());
        catalog
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
        let engine = Engine::new(store.clone());
        engine
            .create(
                "items",
                Row::from([
                    ("id".into(), Value::Text("one".into())),
                    ("status".into(), Value::Text("open".into())),
                ]),
            )
            .await
            .unwrap();

        let open = projected_column("items", "status", Some(("status", "open")));
        let first = engine.execute(open.clone()).await.unwrap();
        let second = engine.execute(open.clone()).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(engine.relation_cache.stats().misses, 1);
        assert_eq!(engine.relation_cache.stats().hits, 1);
        assert_eq!(engine.relation_cache.stats().catalog_misses, 1);
        assert_eq!(engine.relation_cache.stats().catalog_hits, 1);

        engine
            .execute(projected_column(
                "items",
                "status",
                Some(("status", "closed")),
            ))
            .await
            .unwrap();
        assert_eq!(engine.relation_cache.stats().misses, 2);

        catalog
            .create_table(TableDef {
                id: SchemaId::new(2).unwrap(),
                name: "posts".into(),
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
        engine
            .create(
                "posts",
                Row::from([("id".into(), Value::Text("unrelated".into()))]),
            )
            .await
            .unwrap();
        assert_eq!(engine.execute(open.clone()).await.unwrap(), first);
        assert_eq!(engine.relation_cache.stats().hits, 2);
        assert_eq!(engine.relation_cache.stats().misses, 2);

        engine
            .create(
                "items",
                Row::from([
                    ("id".into(), Value::Text("two".into())),
                    ("status".into(), Value::Text("open".into())),
                ]),
            )
            .await
            .unwrap();
        let after_write = engine.execute(open.clone()).await.unwrap();
        assert!(matches!(after_write, Datum::Array(ref rows) if rows.len() == 2));
        assert_eq!(engine.relation_cache.stats().misses, 3);

        catalog
            .create_column(
                "items",
                ColumnDef {
                    id: SchemaId::new(3).unwrap(),
                    name: "note".into(),
                    scalar_type: ScalarType::Text,
                    nullable: true,
                    format: String::new(),
                    default: None,
                },
            )
            .await
            .unwrap();
        engine.execute(open.clone()).await.unwrap();
        assert_eq!(engine.relation_cache.stats().misses, 4);

        let mut transaction = store.begin(IsolationLevel::Snapshot).await.unwrap();
        engine
            .execute_in(transaction.as_mut(), open.clone())
            .await
            .unwrap();
        engine.execute_in(transaction.as_mut(), open).await.unwrap();
        transaction.rollback();
        assert_eq!(engine.relation_cache.stats().hits, 2);
        assert_eq!(engine.relation_cache.stats().misses, 4);
        store.close().await.unwrap();
    }

    async fn assert_stable_hash_build_reuses_artifact_after_probe_data_changes(store: Arc<Store>) {
        let catalog = catalog::Catalog::new(store.clone());
        let customers = catalog
            .create_table(join_cache_table(10, "customers"))
            .await
            .unwrap();
        catalog
            .create_table(join_cache_table(20, "orders"))
            .await
            .unwrap();
        let statistics = complete_join_stats(&customers, 3);
        let engine = Engine::new(store.clone())
            .with_statistics_provider(Arc::new(FixedPlannerStats(Arc::new(statistics))));
        engine
            .create_many(
                "customers",
                vec![
                    Row::from([
                        ("id".into(), Value::Text("c1".into())),
                        ("customer_id".into(), Value::Text("shared".into())),
                    ]),
                    Row::from([
                        ("id".into(), Value::Text("c2".into())),
                        ("customer_id".into(), Value::Text("shared".into())),
                    ]),
                    Row::from([
                        ("id".into(), Value::Text("c3".into())),
                        ("customer_id".into(), Value::Null(ScalarType::Text)),
                    ]),
                ],
            )
            .await
            .unwrap();
        engine
            .create(
                "orders",
                Row::from([
                    ("id".into(), Value::Text("o1".into())),
                    ("customer_id".into(), Value::Text("shared".into())),
                ]),
            )
            .await
            .unwrap();
        let query = customer_orders_query();

        let first = engine.execute(query.clone()).await.unwrap();
        engine
            .create(
                "orders",
                Row::from([
                    ("id".into(), Value::Text("o2".into())),
                    ("customer_id".into(), Value::Text("shared".into())),
                ]),
            )
            .await
            .unwrap();
        let second = engine.execute(query.clone()).await.unwrap();
        let uncached = engine.execute_uncached(query.clone()).await.unwrap();

        assert!(matches!(first, Datum::Array(ref rows) if rows.len() == 2));
        assert!(matches!(second, Datum::Array(ref rows) if rows.len() == 4));
        assert_eq!(second, uncached);
        let cache = engine.relation_cache_stats();
        assert_eq!(cache.misses, 2);
        assert_eq!(cache.subrelation_misses, 1);
        assert_eq!(cache.subrelation_admissions, 1);
        assert_eq!(cache.subrelation_hits, 1);
        engine
            .create(
                "customers",
                Row::from([
                    ("id".into(), Value::Text("c4".into())),
                    ("customer_id".into(), Value::Text("shared".into())),
                ]),
            )
            .await
            .unwrap();
        let third = engine.execute(query.clone()).await.unwrap();
        assert!(matches!(third, Datum::Array(ref rows) if rows.len() == 6));
        assert_eq!(third, engine.execute_uncached(query).await.unwrap());
        let cache = engine.relation_cache_stats();
        assert_eq!(cache.misses, 3);
        assert_eq!(cache.subrelation_misses, 2);
        assert_eq!(cache.subrelation_admissions, 2);
        assert_eq!(cache.subrelation_hits, 1);
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn stable_hash_build_reuses_artifact_after_probe_data_changes_in_memory() {
        let store = Arc::new(Store::memory("exec-subrelation-cache").await.unwrap());
        assert_stable_hash_build_reuses_artifact_after_probe_data_changes(store).await;
    }

    #[tokio::test]
    async fn stable_hash_build_reuses_artifact_after_probe_data_changes_in_file_storage() {
        let directory = TempDir::new().unwrap();
        let objects: Arc<dyn ObjectStore> = Arc::new(
            LocalFileSystem::new_with_prefix(directory.path()).expect("local object-store root"),
        );
        let store = Arc::new(
            Store::open("exec-subrelation-cache", objects)
                .await
                .unwrap(),
        );
        assert_stable_hash_build_reuses_artifact_after_probe_data_changes(store).await;
    }

    async fn assert_recursive_step_reuses_stable_hash_build(store: Arc<Store>) {
        let catalog = catalog::Catalog::new(store.clone());
        catalog
            .create_table(join_cache_table(10, "seeds"))
            .await
            .unwrap();
        let edges = catalog
            .create_table(join_cache_table(20, "edges"))
            .await
            .unwrap();
        let statistics = complete_join_stats(&edges, 3);
        let engine = Engine::new(store.clone())
            .with_statistics_provider(Arc::new(FixedPlannerStats(Arc::new(statistics))));
        engine
            .create(
                "seeds",
                Row::from([
                    ("id".into(), Value::Text("n0".into())),
                    ("customer_id".into(), Value::Null(ScalarType::Text)),
                ]),
            )
            .await
            .unwrap();
        engine
            .create_many(
                "edges",
                vec![
                    Row::from([
                        ("id".into(), Value::Text("n1".into())),
                        ("customer_id".into(), Value::Text("n0".into())),
                    ]),
                    Row::from([
                        ("id".into(), Value::Text("n2".into())),
                        ("customer_id".into(), Value::Text("n1".into())),
                    ]),
                    Row::from([
                        ("id".into(), Value::Text("n3".into())),
                        ("customer_id".into(), Value::Text("n2".into())),
                    ]),
                ],
            )
            .await
            .unwrap();
        let query = recursive_edge_query();

        let first = engine.execute(query.clone()).await.unwrap();
        assert!(matches!(first, Datum::Array(ref rows) if rows.len() == 4));
        let first_cache = engine.relation_cache_stats();
        assert_eq!(first_cache.subrelation_misses, 1);
        assert_eq!(first_cache.subrelation_admissions, 1);
        assert_eq!(first_cache.subrelation_hits, 3);

        engine
            .create(
                "seeds",
                Row::from([
                    ("id".into(), Value::Text("z0".into())),
                    ("customer_id".into(), Value::Null(ScalarType::Text)),
                ]),
            )
            .await
            .unwrap();
        let second = engine.execute(query.clone()).await.unwrap();
        assert!(matches!(second, Datum::Array(ref rows) if rows.len() == 5));
        assert_eq!(
            second,
            engine.execute_uncached(query.clone()).await.unwrap()
        );
        assert_eq!(
            second,
            engine.execute_reference(query.clone()).await.unwrap()
        );
        let second_cache = engine.relation_cache_stats();
        assert_eq!(second_cache.misses, 2);
        assert_eq!(second_cache.subrelation_misses, 1);
        assert_eq!(second_cache.subrelation_admissions, 1);
        assert!(second_cache.subrelation_hits > first_cache.subrelation_hits);

        engine
            .create(
                "edges",
                Row::from([
                    ("id".into(), Value::Text("n4".into())),
                    ("customer_id".into(), Value::Text("n3".into())),
                ]),
            )
            .await
            .unwrap();
        let third = engine.execute(query.clone()).await.unwrap();
        assert!(matches!(third, Datum::Array(ref rows) if rows.len() == 6));
        assert_eq!(third, engine.execute_uncached(query.clone()).await.unwrap());
        assert_eq!(third, engine.execute_reference(query).await.unwrap());
        let third_cache = engine.relation_cache_stats();
        assert_eq!(third_cache.misses, 3);
        assert_eq!(third_cache.subrelation_misses, 2);
        assert_eq!(third_cache.subrelation_admissions, 2);
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn recursive_step_reuses_stable_hash_build_in_memory() {
        let store = Arc::new(
            Store::memory("exec-recursive-subrelation-cache")
                .await
                .unwrap(),
        );
        assert_recursive_step_reuses_stable_hash_build(store).await;
    }

    #[tokio::test]
    async fn recursive_step_reuses_stable_hash_build_in_file_storage() {
        let directory = TempDir::new().unwrap();
        let objects: Arc<dyn ObjectStore> = Arc::new(
            LocalFileSystem::new_with_prefix(directory.path()).expect("local object-store root"),
        );
        let store = Arc::new(
            Store::open("exec-recursive-subrelation-cache", objects)
                .await
                .unwrap(),
        );
        assert_recursive_step_reuses_stable_hash_build(store).await;
    }

    #[tokio::test]
    async fn grouped_hash_build_reuses_dimension_after_fact_data_changes() {
        let store = Arc::new(Store::memory("exec-grouped-dimension-cache").await.unwrap());
        let catalog = catalog::Catalog::new(store.clone());
        let customers = catalog
            .create_table(join_cache_table(10, "customers"))
            .await
            .unwrap();
        let orders = catalog
            .create_table(join_cache_table(20, "orders"))
            .await
            .unwrap();
        let mut statistics = complete_join_stats(&customers, 2);
        statistics
            .synopsis_models
            .extend(complete_join_stats(&orders, 5_000).synopsis_models);
        let engine = Engine::new(store.clone())
            .with_statistics_provider(Arc::new(FixedPlannerStats(Arc::new(statistics))));
        engine
            .create_many(
                "customers",
                vec![
                    Row::from([
                        ("id".into(), Value::Text("c1".into())),
                        ("customer_id".into(), Value::Text("c1".into())),
                    ]),
                    Row::from([
                        ("id".into(), Value::Text("c2".into())),
                        ("customer_id".into(), Value::Text("c2".into())),
                    ]),
                ],
            )
            .await
            .unwrap();
        engine
            .create(
                "orders",
                Row::from([
                    ("id".into(), Value::Text("o1".into())),
                    ("customer_id".into(), Value::Text("c1".into())),
                ]),
            )
            .await
            .unwrap();
        let query = customer_order_summary_query();

        let first = engine.execute(query.clone()).await.unwrap();
        engine
            .create(
                "orders",
                Row::from([
                    ("id".into(), Value::Text("o2".into())),
                    ("customer_id".into(), Value::Text("c2".into())),
                ]),
            )
            .await
            .unwrap();
        let second = engine.execute(query.clone()).await.unwrap();
        let uncached = engine.execute_uncached(query.clone()).await.unwrap();

        assert!(matches!(first, Datum::Array(ref rows) if rows.len() == 1));
        assert!(matches!(second, Datum::Array(ref rows) if rows.len() == 2));
        assert_eq!(second, uncached);
        let cache = engine.relation_cache_stats();
        assert_eq!(cache.subrelation_misses, 1);
        assert_eq!(cache.subrelation_admissions, 1);
        assert_eq!(cache.subrelation_hits, 1);

        engine
            .create(
                "customers",
                Row::from([
                    ("id".into(), Value::Text("c3".into())),
                    ("customer_id".into(), Value::Text("c3".into())),
                ]),
            )
            .await
            .unwrap();
        assert_eq!(engine.execute(query).await.unwrap(), second);
        let cache = engine.relation_cache_stats();
        assert_eq!(cache.subrelation_misses, 2);
        assert_eq!(cache.subrelation_admissions, 2);
        assert_eq!(cache.subrelation_hits, 1);
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn relation_cache_keeps_pinned_generations_and_conflict_fences() {
        let store = Arc::new(Store::memory("exec-relation-cache-pinned").await.unwrap());
        let catalog = catalog::Catalog::new(store.clone());
        catalog
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
        let engine = Engine::new(store.clone());
        engine
            .create(
                "items",
                Row::from([
                    ("id".into(), Value::Text("one".into())),
                    ("status".into(), Value::Text("open".into())),
                ]),
            )
            .await
            .unwrap();
        let query = projected_column("items", "status", None);
        let program = Program {
            statements: vec![Statement::Query {
                name: "read".into(),
                relation: query.clone(),
            }],
            result: Some("read".into()),
        };
        let mut pinned = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let first = engine
            .execute_program_in_transaction(
                pinned.as_mut(),
                &program,
                CatalogPolicy::Forbidden,
                true,
            )
            .await
            .unwrap();
        let second = engine
            .execute_program_in_transaction(
                pinned.as_mut(),
                &program,
                CatalogPolicy::Forbidden,
                true,
            )
            .await
            .unwrap();
        assert_eq!(first.result, second.result);
        assert!(matches!(first.result, Datum::Array(ref rows) if rows.len() == 1));
        assert_eq!(engine.relation_cache.stats().misses, 1);
        assert_eq!(engine.relation_cache.stats().hits, 1);
        assert_eq!(engine.relation_cache.stats().catalog_hits, 0);
        assert_eq!(engine.relation_cache.stats().catalog_misses, 0);
        assert_eq!(engine.relation_cache.stats().prepared_hits, 0);
        assert_eq!(engine.relation_cache.stats().prepared_misses, 0);

        engine
            .create(
                "items",
                Row::from([
                    ("id".into(), Value::Text("two".into())),
                    ("status".into(), Value::Text("open".into())),
                ]),
            )
            .await
            .unwrap();
        let current = engine.execute(query.clone()).await.unwrap();
        assert!(matches!(current, Datum::Array(ref rows) if rows.len() == 2));
        assert_eq!(engine.relation_cache.stats().misses, 2);
        assert_eq!(engine.relation_cache.stats().catalog_misses, 1);

        let old = engine
            .execute_program_in_transaction(
                pinned.as_mut(),
                &program,
                CatalogPolicy::Forbidden,
                true,
            )
            .await
            .unwrap();
        assert_eq!(old.result, first.result);
        assert_eq!(engine.relation_cache.stats().hits, 2);
        assert_eq!(engine.relation_cache.stats().entries, 2);
        assert_eq!(engine.relation_cache.stats().catalog_misses, 1);
        pinned.rollback();

        let mut conflicting = store
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        engine
            .execute_program_in_transaction(
                conflicting.as_mut(),
                &program,
                CatalogPolicy::Forbidden,
                true,
            )
            .await
            .unwrap();
        assert_eq!(engine.relation_cache.stats().hits, 3);
        engine
            .create(
                "items",
                Row::from([
                    ("id".into(), Value::Text("three".into())),
                    ("status".into(), Value::Text("open".into())),
                ]),
            )
            .await
            .unwrap();
        engine
            .create_many_in(
                conflicting.as_mut(),
                "items",
                &[Row::from([
                    ("id".into(), Value::Text("four".into())),
                    ("status".into(), Value::Text("open".into())),
                ])],
            )
            .await
            .unwrap();
        assert!(conflicting.commit().await.is_err());

        let after_conflict = engine.execute(query).await.unwrap();
        assert!(matches!(after_conflict, Datum::Array(ref rows) if rows.len() == 3));
        assert_eq!(engine.relation_cache.stats().misses, 3);
        assert_eq!(engine.relation_cache.stats().entries, 3);
        store.close().await.unwrap();
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

    fn text_int64_input_type(text: &str, int64: &str) -> RowType {
        RowType {
            fields: vec![
                Field {
                    name: text.into(),
                    slot: crate::engine::lir::SlotId(0),
                    value_type: Type::scalar(Kind::Text, false),
                },
                Field {
                    name: int64.into(),
                    slot: crate::engine::lir::SlotId(1),
                    value_type: Type::scalar(Kind::Int64, false),
                },
            ],
        }
    }

    #[tokio::test]
    async fn row_mutations_advance_data_generation_with_their_commit() {
        let store = Arc::new(Store::memory("exec-data-generation").await.unwrap());
        let table = catalog::Catalog::new(store.clone())
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
        let engine = Engine::new(store.clone());
        let generation = || async {
            catalog::store::read_table_data_generation(&*store, &table.id)
                .await
                .unwrap()
                .stripes()
                .iter()
                .map(|generation| generation.get())
                .sum::<u64>()
        };

        assert_eq!(generation().await, 0);
        engine.create_many("items", Vec::new()).await.unwrap();
        assert_eq!(generation().await, 0);
        engine
            .create(
                "items",
                Row::from([
                    ("id".into(), Value::Text("one".into())),
                    ("value".into(), Value::Text("first".into())),
                ]),
            )
            .await
            .unwrap();
        assert_eq!(generation().await, 1);
        engine
            .update_many(
                "items",
                text_input_type(&["id", "value"]),
                vec![Row::from([
                    ("id".into(), Value::Text("one".into())),
                    ("value".into(), Value::Text("second".into())),
                ])],
            )
            .await
            .unwrap();
        assert_eq!(generation().await, 2);
        engine
            .delete_many(
                "items",
                text_input_type(&["id"]),
                vec![Row::from([("id".into(), Value::Text("one".into()))])],
            )
            .await
            .unwrap();
        assert_eq!(generation().await, 3);

        let error = engine
            .create_many(
                "items",
                vec![
                    Row::from([
                        ("id".into(), Value::Text("dupe".into())),
                        ("value".into(), Value::Text("first".into())),
                    ]),
                    Row::from([
                        ("id".into(), Value::Text("dupe".into())),
                        ("value".into(), Value::Text("second".into())),
                    ]),
                ],
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ConstraintViolation);
        assert_eq!(generation().await, 3);
        assert!(
            row_store::get_columns(
                &*store,
                &table,
                &Row::from([("id".into(), Value::Text("dupe".into()))]),
                &table.columns,
            )
            .await
            .unwrap()
            .is_none()
        );
        store.close().await.unwrap();
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
            let raw = Kv::get(&*store, &codec::data_key(&table, &primary_key).unwrap())
                .await
                .unwrap()
                .unwrap();
            let row = codec::unmarshal_row(&table, &raw).unwrap();
            assert_eq!(row["email"], Value::Text(email.into()));
            let tuple = codec::encode_tuple(&[Value::Text(email.into())]).unwrap();
            assert_eq!(
                Kv::get(
                    &*store,
                    &codec::index_key(&table, &table.indexes[0].id, &tuple, &primary_key).unwrap()
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
            Kv::get(&*store, &codec::data_key(&parents, &key).unwrap())
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
            Kv::get(&*store, &codec::data_key(&parents, &key).unwrap())
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
        let raw = Kv::get(&*store, &codec::data_key(&current, &key).unwrap())
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
            Kv::get(&*store, &codec::data_key(&current, &key).unwrap())
                .await
                .unwrap()
                .is_none()
        );
    }
}
