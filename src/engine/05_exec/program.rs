//! Ordered, atomic PIR program execution.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::Instrument as _;

use crate::engine::catalog;
use crate::engine::catalog::Mutation as CatalogMutation;
use crate::engine::catalog::identity::{SchemaId, TransitionId};
use crate::engine::catalog::model::{
    ColumnDraft, ColumnReplacementDef, ConstraintDef, DefaultFunction, DefaultValue, IndexDef,
    Revision, ScalarType, SchemaTransition, TableDraft, TransitionControl,
};
use crate::engine::kv::KvView;
use crate::engine::lir::eval::Env;
use crate::engine::lir::{Datum, Row, RowType};
use crate::engine::planner::bind::{
    BoundStatement, Mutation, MutationKind, ProgramBinder, ProgramStatement,
};
use crate::engine::planner::explain::PlanView;
use crate::runtime::RuntimeEffects;

use super::{Error, ErrorKind, Executor, Limits, ReferenceExecutor, Result, row_store, write};

#[derive(Clone, Copy)]
pub(super) enum ExecutionPath {
    Production,
    Reference,
}

type SubrelationCacheExecution<'a> = (
    &'a super::relation_cache::RelationCache,
    super::relation_cache::RelationCacheKey,
    Option<&'a Arc<dyn super::EngineEventHook>>,
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogPolicy {
    Forbidden,
    RevisionPerStatement,
    RevisionPerProgram,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CatalogExpectation {
    pub version: catalog::identity::CatalogVersion,
    pub hash: String,
}

impl From<&Revision> for CatalogExpectation {
    fn from(revision: &Revision) -> Self {
        Self {
            version: revision.version,
            hash: revision.hash.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProgramOptions {
    pub catalog: CatalogPolicy,
    pub expected_catalog: Option<CatalogExpectation>,
    pub dry_run: bool,
    pub collect_plan: bool,
}

impl Default for ProgramOptions {
    fn default() -> Self {
        Self {
            catalog: CatalogPolicy::Forbidden,
            expected_catalog: None,
            dry_run: false,
            collect_plan: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Program {
    pub statements: Vec<Statement>,
    pub result: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DefaultSpec {
    Generator(DefaultFunction),
    Text(String),
    Number(String),
    Bool(bool),
}

impl DefaultSpec {
    pub(crate) fn from_catalog(
        value: &DefaultValue,
        scalar_type: ScalarType,
        format: &str,
    ) -> Self {
        if let Some(function) = value.function {
            return Self::Generator(function);
        }
        match scalar_type {
            ScalarType::Text => Self::Text(value.text.clone()),
            ScalarType::Int64 => Self::Number(value.int64.to_string()),
            ScalarType::Float64 => Self::Number(value.float64.to_string()),
            ScalarType::Bool => Self::Bool(value.bool_value),
            ScalarType::Bytes => {
                Self::Text(crate::identifiers::Format::recognize(format).map_or_else(
                    || crate::identifiers::encode_base64(&value.bytes),
                    |format| {
                        crate::identifiers::render(format, &value.bytes)
                            .expect("catalog identifier default was validated")
                    },
                ))
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Statement {
    Query {
        name: String,
        relation: crate::engine::lir::Query,
    },
    Create {
        name: String,
        relation: crate::engine::lir::Query,
        table: String,
    },
    Update {
        name: String,
        relation: crate::engine::lir::Query,
        table: String,
    },
    Delete {
        name: String,
        relation: crate::engine::lir::Query,
        table: String,
    },
    CreateTable {
        name: String,
        table: TableDraft,
    },
    RenameTable {
        name: String,
        table_id: SchemaId,
        to: String,
    },
    DeleteTable {
        name: String,
        table_id: SchemaId,
    },
    CreateColumn {
        name: String,
        table_id: SchemaId,
        column: ColumnDraft,
    },
    RenameColumn {
        name: String,
        table_id: SchemaId,
        column_id: SchemaId,
        to: String,
    },
    ChangeColumnDefault {
        name: String,
        table_id: SchemaId,
        column_id: SchemaId,
        default: Option<DefaultSpec>,
    },
    DeleteColumn {
        name: String,
        table_id: SchemaId,
        column_id: SchemaId,
    },
    CreateIndex {
        name: String,
        table_id: SchemaId,
        index: IndexDef,
    },
    DeleteIndex {
        name: String,
        table_id: SchemaId,
        index: String,
    },
    StartIndexBuild {
        name: String,
        table_id: SchemaId,
        index: IndexDef,
        prerequisites: Vec<TransitionId>,
        after: Vec<String>,
    },
    StartColumnReplacement {
        name: String,
        table_id: SchemaId,
        column_id: SchemaId,
        replacement: ColumnReplacementDef,
        after: Vec<String>,
    },
    StartConstraintValidation {
        name: String,
        table_id: SchemaId,
        constraint: ConstraintDef,
        after: Vec<String>,
    },
}

impl Statement {
    pub fn name(&self) -> &str {
        match self {
            Self::Query { name, .. }
            | Self::Create { name, .. }
            | Self::Update { name, .. }
            | Self::Delete { name, .. }
            | Self::CreateTable { name, .. }
            | Self::RenameTable { name, .. }
            | Self::DeleteTable { name, .. }
            | Self::CreateColumn { name, .. }
            | Self::RenameColumn { name, .. }
            | Self::ChangeColumnDefault { name, .. }
            | Self::DeleteColumn { name, .. }
            | Self::CreateIndex { name, .. }
            | Self::DeleteIndex { name, .. }
            | Self::StartIndexBuild { name, .. }
            | Self::StartColumnReplacement { name, .. }
            | Self::StartConstraintValidation { name, .. } => name,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Query { .. } => "query",
            Self::Create { .. } => "create",
            Self::Update { .. } => "update",
            Self::Delete { .. } => "delete",
            Self::CreateTable { .. } => "create_table",
            Self::RenameTable { .. } => "rename_table",
            Self::DeleteTable { .. } => "delete_table",
            Self::CreateColumn { .. } => "create_column",
            Self::RenameColumn { .. } => "rename_column",
            Self::ChangeColumnDefault { .. } => "change_column_default",
            Self::DeleteColumn { .. } => "delete_column",
            Self::CreateIndex { .. } => "create_index",
            Self::DeleteIndex { .. } => "delete_index",
            Self::StartIndexBuild { .. } => "start_index_build",
            Self::StartColumnReplacement { .. } => "start_column_replacement",
            Self::StartConstraintValidation { .. } => "start_constraint_validation",
        }
    }

    pub fn relational(&self) -> bool {
        matches!(
            self,
            Self::Query { .. } | Self::Create { .. } | Self::Update { .. } | Self::Delete { .. }
        )
    }

    pub fn effectful(&self) -> bool {
        !matches!(self, Self::Query { .. })
    }

    fn binder_statement(&self) -> Option<ProgramStatement> {
        match self {
            Self::Query { name, relation } => Some(ProgramStatement {
                name: name.clone(),
                relation: relation.clone(),
                mutation: None,
            }),
            Self::Create {
                name,
                relation,
                table,
            } => Some(ProgramStatement {
                name: name.clone(),
                relation: relation.clone(),
                mutation: Some(Mutation {
                    kind: MutationKind::Create,
                    table: table.clone(),
                }),
            }),
            Self::Update {
                name,
                relation,
                table,
            } => Some(ProgramStatement {
                name: name.clone(),
                relation: relation.clone(),
                mutation: Some(Mutation {
                    kind: MutationKind::Update,
                    table: table.clone(),
                }),
            }),
            Self::Delete {
                name,
                relation,
                table,
            } => Some(ProgramStatement {
                name: name.clone(),
                relation: relation.clone(),
                mutation: Some(Mutation {
                    kind: MutationKind::Delete,
                    table: table.clone(),
                }),
            }),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StatementResult {
    pub name: String,
    pub affected: usize,
    pub control: Option<TransitionControl>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProgramResult {
    pub result: Datum,
    pub statements: Vec<StatementResult>,
    pub plans: Vec<StatementPlan>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatementPlan {
    pub name: String,
    pub plan: PlanView,
    pub measurement: Option<StatementPlanMeasurement>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatementPlanMeasurement {
    pub planning_micros: u64,
    pub execution_micros: u64,
    pub result_rows: u64,
    pub logical_kv: super::observe::KvWork,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logical_scans: Option<super::observe::KvScanTrace>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operator_trace: Option<super::observe::OperatorTrace>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub physical_storage: Option<crate::engine::kv::telemetry::PhysicalRequestTrace>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub join_operators: Vec<super::observe::JoinOperatorMeasurement>,
}

#[derive(Clone, Debug)]
pub struct PreparedStatementEstimate {
    pub name: String,
    pub query: crate::engine::lir::bound::Query,
    pub plan: crate::engine::planner::physical::Plan,
    pub active: crate::engine::planner::estimator::Estimate,
}

pub(super) struct PreflightResult {
    pub plans: Vec<StatementPlan>,
    pub estimates: Vec<PreparedStatementEstimate>,
}

pub(super) fn validate(program: &Program, policy: CatalogPolicy) -> Result<Option<String>> {
    if program.statements.is_empty() {
        return Err(input("exec: a program needs at least one statement"));
    }
    let mut names = HashSet::with_capacity(program.statements.len());
    for statement in &program.statements {
        if statement.name().is_empty() {
            return Err(input("exec: statement name must not be empty"));
        }
        if !names.insert(statement.name()) {
            return Err(input(format!(
                "exec: duplicate statement name {:?}",
                statement.name()
            )));
        }
        if !statement.relational() && policy == CatalogPolicy::Forbidden {
            return Err(input(format!(
                "exec: catalog statement {:?} is forbidden by this entrypoint",
                statement.name()
            )));
        }
    }
    if let Some(result) = &program.result {
        let statement = program
            .statements
            .iter()
            .find(|statement| statement.name() == result)
            .ok_or_else(|| input(format!("exec: result names unknown statement {result:?}")))?;
        if !statement.relational() {
            return Err(input(format!(
                "exec: result names catalog statement {result:?}"
            )));
        }
        return Ok(Some(result.clone()));
    }
    let relational = program
        .statements
        .iter()
        .filter(|statement| statement.relational())
        .collect::<Vec<_>>();
    match relational.as_slice() {
        [] => Ok(None),
        [statement] if program.statements.len() == 1 => Ok(Some(statement.name().to_owned())),
        _ => Err(input(format!(
            "exec: a program with {} statements must name its result",
            program.statements.len()
        ))),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn preflight(
    view: &mut dyn KvView,
    program: &Program,
    policy: CatalogPolicy,
    collect_plan: bool,
    physical: bool,
    collect_estimates: bool,
    runtime: &Arc<dyn RuntimeEffects>,
    statistics: Option<Arc<crate::engine::planner::models::PlannerStats>>,
    plan_options: crate::engine::planner::PlanOptions,
) -> Result<PreflightResult> {
    let names = relational_names(program);
    let mut binder = ProgramBinder::new(names)?;
    binder.set_statistics(statistics.clone());
    binder.set_plan_options(plan_options);
    let mut transitions = HashMap::new();
    let mut catalog_changed = false;
    let mut schema_changed = false;
    let mut plans = Vec::new();
    let mut estimates = Vec::new();
    for statement in &program.statements {
        if let Some(binding) = statement.binder_statement() {
            let catalog = super::engine::ViewCatalog {
                view: &*view,
                relation_cache: None,
            };
            let bound = if physical || collect_plan {
                binder.bind(&catalog, binding).await?
            } else {
                binder.bind_reference(&catalog, binding).await?
            };
            if collect_plan {
                let plan = bound.plan.as_ref().expect("plan requested");
                let mut plan_view = PlanView::with_mode(plan, plan_options.mode);
                if let Some(stats) = &statistics {
                    plan_view.annotate_estimates(stats, bound.estimate, &bound.bound, plan);
                }
                plans.push(StatementPlan {
                    name: bound.name.clone(),
                    plan: plan_view,
                    measurement: None,
                });
            }
            if collect_estimates
                && let (Some(plan), Some(active)) = (bound.plan.clone(), bound.estimate)
            {
                estimates.push(PreparedStatementEstimate {
                    name: bound.name.clone(),
                    query: bound.bound.clone(),
                    plan,
                    active,
                });
            }
        } else {
            let mut mutation = CatalogMutation::with_runtime(view, runtime.clone());
            let transition = apply_catalog(&mut mutation, statement, &transitions, false).await?;
            catalog_changed |= mutation.catalog_changed();
            schema_changed |= mutation.schema_changed();
            if policy == CatalogPolicy::RevisionPerStatement {
                mutation.finish().await?;
            }
            if let Some(transition) = transition {
                transitions.insert(statement.name().to_owned(), transition.id);
            }
        }
    }
    if policy == CatalogPolicy::RevisionPerProgram {
        if catalog_changed {
            catalog::store::bump_catalog_generation(view).await?;
        }
        if schema_changed {
            catalog::store::bump_revision(view, runtime.now().into()).await?;
        }
    }
    Ok(PreflightResult { plans, estimates })
}

pub(super) struct RunContext<'a> {
    pub observation: super::observe::Observation<'a>,
    pub relation_cache: Option<&'a super::relation_cache::RelationCache>,
    pub dependency_validation: super::relation_cache::DependencyValidation,
    pub statistics: Option<Arc<crate::engine::planner::models::PlannerStats>>,
    pub plan_options: crate::engine::planner::PlanOptions,
    pub collect_plan: bool,
    pub execution_grant: super::parallel::ExecutionGrant,
    pub cache_events: Option<&'a Arc<dyn super::EngineEventHook>>,
}

pub(super) async fn run(
    view: &mut dyn KvView,
    program: &Program,
    result_name: Option<&str>,
    policy: CatalogPolicy,
    limits: Limits,
    runtime: &Arc<dyn RuntimeEffects>,
    context: RunContext<'_>,
) -> Result<ProgramResult> {
    Box::pin(run_with_path(
        view,
        program,
        result_name,
        policy,
        limits,
        ExecutionPath::Production,
        runtime,
        context.observation,
        context.statistics,
        context.plan_options,
        context.collect_plan,
        context.execution_grant,
        context.relation_cache,
        context.dependency_validation,
        context.cache_events,
    ))
    .await
}

pub(super) async fn run_reference(
    view: &mut dyn KvView,
    program: &Program,
    result_name: Option<&str>,
    policy: CatalogPolicy,
    limits: Limits,
    runtime: &Arc<dyn RuntimeEffects>,
) -> Result<ProgramResult> {
    Box::pin(run_with_path(
        view,
        program,
        result_name,
        policy,
        limits,
        ExecutionPath::Reference,
        runtime,
        super::observe::Observation::default(),
        None,
        crate::engine::planner::PlanOptions::default(),
        false,
        super::parallel::ExecutionGrant::serial(),
        None,
        super::relation_cache::DependencyValidation::Transaction,
        None,
    ))
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_with_path(
    view: &mut dyn KvView,
    program: &Program,
    result_name: Option<&str>,
    policy: CatalogPolicy,
    limits: Limits,
    path: ExecutionPath,
    runtime: &Arc<dyn RuntimeEffects>,
    observation: super::observe::Observation<'_>,
    statistics: Option<Arc<crate::engine::planner::models::PlannerStats>>,
    plan_options: crate::engine::planner::PlanOptions,
    collect_plan: bool,
    execution_grant: super::parallel::ExecutionGrant,
    relation_cache: Option<&super::relation_cache::RelationCache>,
    dependency_validation: super::relation_cache::DependencyValidation,
    cache_events: Option<&Arc<dyn super::EngineEventHook>>,
) -> Result<ProgramResult> {
    let mut binder = ProgramBinder::new(relational_names(program))?;
    let mut bindings = HashMap::<String, Vec<Env>>::new();
    let mut transitions = HashMap::new();
    let mut summaries = Vec::with_capacity(program.statements.len());
    let mut result = Datum::Null;
    let mut plans = Vec::new();

    Box::pin(run_statements(
        view,
        program,
        result_name,
        policy,
        limits,
        path,
        &mut binder,
        &mut bindings,
        &mut transitions,
        &mut summaries,
        &mut result,
        runtime,
        observation,
        statistics,
        plan_options,
        collect_plan,
        &execution_grant,
        relation_cache,
        dependency_validation,
        cache_events,
        &mut plans,
    ))
    .await?;
    Ok(ProgramResult {
        result,
        statements: summaries,
        plans,
    })
}

pub(super) async fn expect_catalog(
    view: &mut dyn KvView,
    expected: Option<&CatalogExpectation>,
) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let actual = catalog::store::current_revision(view).await?;
    if actual.version != expected.version || actual.hash != expected.hash {
        return Err(Error::message(
            ErrorKind::Conflict,
            format!(
                "exec: catalog changed: expected version {} hash {}, got version {} hash {}",
                expected.version, expected.hash, actual.version, actual.hash
            ),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_statements(
    view: &mut dyn KvView,
    program: &Program,
    result_name: Option<&str>,
    policy: CatalogPolicy,
    limits: Limits,
    path: ExecutionPath,
    binder: &mut ProgramBinder,
    bindings: &mut HashMap<String, Vec<Env>>,
    transitions: &mut HashMap<String, TransitionId>,
    summaries: &mut Vec<StatementResult>,
    result: &mut Datum,
    runtime: &Arc<dyn RuntimeEffects>,
    observation: super::observe::Observation<'_>,
    statistics: Option<Arc<crate::engine::planner::models::PlannerStats>>,
    plan_options: crate::engine::planner::PlanOptions,
    collect_plan: bool,
    execution_grant: &super::parallel::ExecutionGrant,
    relation_cache: Option<&super::relation_cache::RelationCache>,
    dependency_validation: super::relation_cache::DependencyValidation,
    cache_events: Option<&Arc<dyn super::EngineEventHook>>,
    plans: &mut Vec<StatementPlan>,
) -> Result<()> {
    let mut catalog_changed = false;
    let mut schema_changed = false;
    let tracing_statements = matches!(path, ExecutionPath::Production)
        && tracing::event_enabled!(target: "rad::program", tracing::Level::DEBUG);
    let tracing_spans = matches!(path, ExecutionPath::Production)
        && tracing::enabled!(target: "rad::telemetry", tracing::Level::INFO);
    let tracing_operator_events = matches!(path, ExecutionPath::Production)
        && tracing::event_enabled!(target: "rad::telemetry", tracing::Level::DEBUG);
    let tracing_operator_spans = matches!(path, ExecutionPath::Production)
        && tracing::enabled!(target: "rad::telemetry", tracing::Level::DEBUG);
    let observing = matches!(path, ExecutionPath::Production)
        && (observation.enabled() || tracing_statements || tracing_spans);
    let measuring = matches!(path, ExecutionPath::Production)
        && (observing || collect_plan || crate::telemetry::enabled());
    let measure_operators = matches!(path, ExecutionPath::Production)
        && (collect_plan || tracing_operator_events || tracing_operator_spans);
    binder.set_statistics(statistics.clone());
    binder.set_plan_options(plan_options);
    for (statement_index, statement) in program.statements.iter().enumerate() {
        let statement_span = tracing::info_span!(
            target: "rad::telemetry",
            "rad.statement.execute",
            otel.kind = "internal",
            rad.statement.kind = statement.kind(),
            rad.statement.source = tracing::field::Empty,
            rad.statement.fingerprint = tracing::field::Empty,
            rad.statement.plan_fingerprint = tracing::field::Empty,
            rad.statement.result_rows = tracing::field::Empty,
            rad.statement.affected_rows = tracing::field::Empty,
            rad.statement.bind_duration_us = tracing::field::Empty,
            rad.statement.execution_duration_us = tracing::field::Empty,
            rad.kv.gets = tracing::field::Empty,
            rad.kv.puts = tracing::field::Empty,
            rad.kv.deletes = tracing::field::Empty,
            rad.kv.scans = tracing::field::Empty,
            rad.kv.bytes_read = tracing::field::Empty,
            rad.kv.bytes_written = tracing::field::Empty,
            rad.status = tracing::field::Empty,
            error.type = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
        );
        if statement.relational() {
            let catalog = super::engine::ViewCatalog {
                view: &*view,
                relation_cache: relation_cache.filter(|_| {
                    dependency_validation == super::relation_cache::DependencyValidation::Snapshot
                }),
            };
            let bind_started = measuring.then(|| runtime.monotonic());
            let cache = relation_cache.filter(|_| {
                program.statements.len() == 1 && matches!(statement, Statement::Query { .. })
            });
            if relation_cache.is_some() && cache.is_none() {
                crate::telemetry::relation_cache_lookup("bypass");
            }
            let prepared_cache = cache.filter(|_| {
                // A prepared-read hit does not advance ProgramBinder state. This is
                // safe only when the program contains one query statement because
                // no subsequent statement can depend on that binder state.
                dependency_validation == super::relation_cache::DependencyValidation::Snapshot
                    && matches!(path, ExecutionPath::Production)
            });
            let prepared = if let (
                Some(prepared_cache),
                Statement::Query {
                    relation: query, ..
                },
            ) = (prepared_cache, statement)
            {
                let bind_span = statement_span.clone();
                prepared_cache
                    .get_or_prepare_read(&*view, query, statistics.as_deref(), plan_options, || {
                        let binding = statement
                            .binder_statement()
                            .expect("a relational statement has binder input");
                        async {
                            binder
                                .bind(&catalog, binding)
                                .instrument(bind_span)
                                .await
                                .map_err(Into::into)
                        }
                    })
                    .await
                    .map(|result| {
                        (
                            result.prepared.statement.clone(),
                            Some(result.prepared.fingerprints.clone()),
                            Some(result.relation_key),
                        )
                    })
            } else {
                let binding = statement
                    .binder_statement()
                    .expect("a relational statement has binder input");
                match path {
                    ExecutionPath::Production => {
                        binder
                            .bind(&catalog, binding)
                            .instrument(statement_span.clone())
                            .await
                    }
                    ExecutionPath::Reference => {
                        binder
                            .bind_reference(&catalog, binding)
                            .instrument(statement_span.clone())
                            .await
                    }
                }
                .map_err(Into::into)
                .map(|bound| (Arc::new(bound), None, None))
            };
            let (bound, prepared_fingerprints, prepared_relation_key) = match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    statement_span.record("rad.status", "error");
                    statement_span.record("error.type", error.kind().as_str());
                    statement_span.record("otel.status_code", "ERROR");
                    return Err(error);
                }
            };
            let bind = bind_started.map(|started| runtime.monotonic().saturating_sub(started));
            let fingerprints = prepared_fingerprints.or_else(|| {
                (observing || cache.is_some())
                    .then(|| Arc::new(crate::engine::lir::fingerprint::query(&bound.bound)))
            });
            let cache_key = if let Some(cache) = cache {
                Some((
                    cache,
                    match prepared_relation_key {
                        Some(key) => key,
                        None => {
                            let plan = bound.plan.as_ref().expect("production plan is present");
                            cache
                                .key_for_view(
                                    fingerprints
                                        .as_ref()
                                        .expect("cache fingerprint is present")
                                        .exact,
                                    &*view,
                                    &plan.dependencies,
                                    dependency_validation,
                                )
                                .await?
                        }
                    },
                ))
            } else {
                None
            };
            let execute_started = measuring.then(|| runtime.monotonic());
            let counters = (measuring || cache_key.is_some())
                .then(|| super::observe::KvCounters::new(collect_plan));
            let subrelation_cache = cache_key
                .as_ref()
                .map(|(cache, key)| (*cache, key.clone(), cache_events));
            let mut binding_rows = Vec::new();
            let mut measured = Vec::new();
            let mut join_operators = Vec::new();
            let mut operators = Vec::new();
            let relational = async {
                if let Some(counters) = &counters {
                    let mut observed = super::observe::ObservedView::new(&*view, counters);
                    run_relational(
                        &mut observed,
                        Some(counters),
                        statement,
                        &bound,
                        bindings,
                        limits,
                        path,
                        runtime.as_ref(),
                        measuring,
                        &mut binding_rows,
                        &mut measured,
                        &mut join_operators,
                        measure_operators,
                        &mut operators,
                        execution_grant,
                        subrelation_cache.clone(),
                    )
                    .await
                } else {
                    run_relational(
                        view,
                        None,
                        statement,
                        &bound,
                        bindings,
                        limits,
                        path,
                        runtime.as_ref(),
                        measuring,
                        &mut binding_rows,
                        &mut measured,
                        &mut join_operators,
                        measure_operators,
                        &mut operators,
                        execution_grant,
                        subrelation_cache.clone(),
                    )
                    .await
                }
            };
            let execution = async {
                if let Some((cache, key)) = cache_key {
                    let mut cached = cache
                        .get_or_fill(key, &bound.result_output, || async {
                            let started = runtime.monotonic();
                            let frames = relational.await?;
                            super::frames::validate_frame_cardinality(
                                bound.result_cardinality,
                                frames.len(),
                            )?;
                            Ok((
                                frames,
                                super::relation_cache::CachedWork {
                                    kv: counters
                                        .as_ref()
                                        .expect("cache counters are present")
                                        .snapshot(),
                                    execution: runtime.monotonic().saturating_sub(started),
                                },
                            ))
                        })
                        .await?;
                    super::relation_cache::reach_semantic_events(
                        cache_events,
                        cached.take_events(),
                    )
                    .await;
                    Ok(cached)
                } else {
                    relational
                        .await
                        .map(super::relation_cache::RelationCacheResult::executed)
                }
            };
            let execution = execution.instrument(statement_span.clone());
            let (rows, physical_storage) = if measure_operators {
                let (rows, trace) = Box::pin(crate::engine::kv::telemetry::observe_request(
                    "slatedb", execution,
                ))
                .await;
                (rows, Some(trace))
            } else {
                (execution.await, None)
            };
            let rows = match rows {
                Ok(rows) => rows,
                Err(error) => {
                    statement_span.record("rad.status", "error");
                    statement_span.record("error.type", error.kind().as_str());
                    statement_span.record("otel.status_code", "ERROR");
                    return Err(error);
                }
            };
            let source = rows.source;
            let affected = rows.len();
            let execute = execute_started
                .map(|started| runtime.monotonic().saturating_sub(started))
                .unwrap_or_default();
            let kv = counters
                .as_ref()
                .map(super::observe::KvCounters::snapshot)
                .unwrap_or_default();
            let logical_scans = counters
                .as_ref()
                .and_then(super::observe::KvCounters::scan_trace);
            let dropped_operators = operators
                .len()
                .saturating_sub(super::observe::MAX_OPERATOR_MEASUREMENTS)
                as u64;
            operators.truncate(super::observe::MAX_OPERATOR_MEASUREMENTS);
            let execution_micros = execute.as_micros().min(u128::from(u64::MAX)) as u64;
            let attributed_micros = operators
                .iter()
                .filter(|measurement| measurement.parent_operator_id.is_none())
                .map(|measurement| measurement.inclusive_micros)
                .fold(0u64, u64::saturating_add);
            let operator_trace = measure_operators.then(|| super::observe::OperatorTrace {
                format: super::observe::OPERATOR_TRACE_FORMAT,
                operators: operators.clone(),
                dropped: dropped_operators,
                unattributed_micros: execution_micros.saturating_sub(attributed_micros),
            });
            if crate::telemetry::enabled() {
                crate::telemetry::statement_finished(
                    statement.kind(),
                    source,
                    "success",
                    bind.unwrap_or_default(),
                    execute,
                    rows.len() as u64,
                    &kv,
                );
                for operator in &operators {
                    crate::telemetry::operator_finished(operator);
                }
            }
            if observing {
                let fingerprints = fingerprints.expect("observation fingerprint is present");
                let stamp = bound.plan.as_ref().map_or_else(
                    crate::engine::planner::models::DependencyStamp::default,
                    |plan| crate::engine::planner::models::DependencyStamp::of(&plan.dependencies),
                );
                let estimate = bound.estimate;
                let relations = measured
                    .iter()
                    .map(|(family, rows)| super::observe::RelationObservation {
                        family: *family,
                        rows: *rows,
                        estimate: statistics.as_ref().map(|stats| {
                            crate::engine::planner::estimator::Estimator::new(stats)
                                .relation(family, None, stamp)
                        }),
                    })
                    .collect();
                let plan_fingerprint = bound
                    .plan
                    .as_ref()
                    .map(crate::engine::planner::physical::Plan::fingerprint);
                statement_span.record("rad.statement.fingerprint", fingerprints.family.to_string());
                if let Some(plan_fingerprint) = plan_fingerprint {
                    statement_span.record(
                        "rad.statement.plan_fingerprint",
                        plan_fingerprint.to_string(),
                    );
                }
                if tracing_statements {
                    let request = crate::logging::request_context();
                    let (trace_id, span_id) = crate::telemetry::span_ids(&statement_span);
                    let plan = bound
                        .plan
                        .as_ref()
                        .map(|plan| format!("{plan:?}"))
                        .unwrap_or_default();
                    tracing::debug!(
                        target: "rad::program",
                        event = "program.statement",
                        component = "executor",
                        transport = request.transport,
                        request_id = request.request_id,
                        trace_id,
                        span_id,
                        transaction_id = request.transaction_id,
                        client_ip = request.client_ip,
                        statement = statement.name(),
                        statement_source = source.as_str(),
                        statement_fingerprint = %fingerprints.exact,
                        statement_family_fingerprint = %fingerprints.family,
                        relation_fingerprint = %fingerprints.root.family,
                        plan_fingerprint = plan_fingerprint.map(|value| value.to_string()).unwrap_or_default(),
                        plan,
                        bind_duration_us = bind.unwrap_or_default().as_micros() as u64,
                        execution_duration_us = execute.as_micros() as u64,
                        result_rows = rows.len() as u64,
                        affected_rows = if statement.effectful() {
                            affected as u64
                        } else {
                            0
                        },
                        kv_gets = kv.gets,
                        kv_puts = kv.puts,
                        kv_deletes = kv.deletes,
                        kv_scans = kv.scans,
                        kv_iterated = kv.iterated,
                        kv_bytes_read = kv.bytes_read,
                        kv_bytes_written = kv.bytes_written,
                        message = "program statement executed"
                    );
                }
                if let Some(observer) = observation.observer {
                    observer.statement(super::observe::StatementObservation {
                        source,
                        query: fingerprints.as_ref().clone(),
                        estimate,
                        stamp,
                        relations,
                        plan: plan_fingerprint,
                        phase: super::observe::PhaseTimings {
                            bind: bind.unwrap_or_default(),
                            execute,
                        },
                        rows: rows.len() as u64,
                        affected: affected as u64,
                        mutated: bound.target.as_ref().map(|table| table.schema_id),
                        kv,
                        operators: operators.clone(),
                        physical_storage: physical_storage.clone(),
                        join_operators: join_operators.clone(),
                        failure: None,
                    });
                }
            }
            statement_span.record("rad.statement.source", source.as_str());
            statement_span.record("rad.statement.result_rows", rows.len() as u64);
            statement_span.record(
                "rad.statement.affected_rows",
                if statement.effectful() {
                    affected as u64
                } else {
                    0
                },
            );
            statement_span.record(
                "rad.statement.bind_duration_us",
                bind.unwrap_or_default().as_micros() as u64,
            );
            statement_span.record(
                "rad.statement.execution_duration_us",
                execute.as_micros() as u64,
            );
            statement_span.record("rad.kv.gets", kv.gets);
            statement_span.record("rad.kv.puts", kv.puts);
            statement_span.record("rad.kv.deletes", kv.deletes);
            statement_span.record("rad.kv.scans", kv.scans);
            statement_span.record("rad.kv.bytes_read", kv.bytes_read);
            statement_span.record("rad.kv.bytes_written", kv.bytes_written);
            statement_span.record("rad.status", "success");
            if tracing_operator_events {
                for measurement in &operators {
                    tracing::debug!(
                        target: "rad::telemetry",
                        parent: &statement_span,
                        event = "operator.completed",
                        rad.operator.id = measurement.operator_id,
                        rad.operator.parent_id = measurement.parent_operator_id,
                        rad.operator.name = measurement.operator,
                        rad.operator.relation_fingerprint = measurement
                            .relation_fingerprint
                            .map(|fingerprint| fingerprint.to_string()),
                        rad.operator.open_duration_us = measurement.open_micros,
                        rad.operator.inclusive_duration_us = measurement.inclusive_micros,
                        rad.operator.exclusive_duration_us = measurement.exclusive_micros,
                        rad.operator.calls = measurement.calls,
                        rad.operator.input_rows = measurement.input_rows,
                        rad.operator.output_rows = measurement.output_rows,
                        rad.operator.input_complete = measurement.input_complete,
                        rad.operator.complete = measurement.complete,
                    );
                }
                if let Some(storage) = &physical_storage {
                    for cache in &storage.caches {
                        tracing::debug!(
                            target: "rad::telemetry",
                            parent: &statement_span,
                            event = "storage.cache",
                            rad.storage.backend = storage.backend,
                            rad.storage.scope = storage.scope,
                            rad.storage.coverage = storage.coverage,
                            rad.storage.cache.tier = cache.tier.as_str(),
                            rad.storage.cache.entry_kind = cache.entry_kind,
                            rad.storage.cache.accesses = cache.accesses,
                            rad.storage.cache.hits = cache.hits,
                            rad.storage.cache.misses = cache.misses,
                            rad.storage.cache.errors = cache.errors,
                        );
                    }
                    for request in &storage.requests {
                        tracing::debug!(
                            target: "rad::telemetry",
                            parent: &statement_span,
                            event = "storage.request",
                            rad.storage.backend = storage.backend,
                            rad.storage.scope = storage.scope,
                            rad.storage.coverage = storage.coverage,
                            rad.storage.service_tier = request.service_tier.as_str(),
                            rad.storage.request.class = request.class.as_str(),
                            rad.storage.request.requests = request.requests,
                            rad.storage.request.completed = request.completed,
                            rad.storage.request.errors = request.errors,
                            rad.storage.request.bytes = request.bytes,
                            rad.storage.request.duration_us = request.duration_micros,
                        );
                    }
                }
            }
            if collect_plan {
                let plan = bound.plan.as_ref().expect("execution plan requested");
                let mut plan_view = PlanView::with_mode(plan, plan_options.mode);
                if let Some(statistics) = &statistics {
                    plan_view.annotate_estimates(statistics, bound.estimate, &bound.bound, plan);
                }
                plans.push(StatementPlan {
                    name: bound.name.clone(),
                    plan: plan_view,
                    measurement: Some(StatementPlanMeasurement {
                        planning_micros: bind
                            .unwrap_or_default()
                            .as_micros()
                            .min(u128::from(u64::MAX))
                            as u64,
                        execution_micros,
                        result_rows: rows.len() as u64,
                        logical_kv: kv,
                        logical_scans,
                        operator_trace,
                        physical_storage,
                        join_operators,
                    }),
                });
            }
            if result_name == Some(statement.name()) {
                *result = rows.shape(bound.result_cardinality, &bound.result_output)?;
            }
            // A binding serves only a later statement. Restoring cached rows
            // after the final statement adds a full result copy that no
            // consumer can read.
            if statement_index + 1 < program.statements.len() {
                bindings.insert(
                    statement.name().to_owned(),
                    rows.into_frames(&bound.result_output),
                );
            }
            summaries.push(StatementResult {
                name: statement.name().to_owned(),
                affected,
                control: None,
            });
            continue;
        }

        let mut mutation = CatalogMutation::with_runtime(view, runtime.clone());
        let transition = match apply_catalog(&mut mutation, statement, transitions, true).await {
            Ok(transition) => transition,
            Err(error) => {
                statement_span.record("rad.status", "error");
                statement_span.record("error.type", error.kind().as_str());
                statement_span.record("otel.status_code", "ERROR");
                return Err(error);
            }
        };
        statement_span.record("rad.status", "success");
        catalog_changed |= mutation.catalog_changed();
        schema_changed |= mutation.schema_changed();
        if policy == CatalogPolicy::RevisionPerStatement {
            mutation.finish().await?;
        }
        if let Some(transition) = &transition {
            transitions.insert(statement.name().to_owned(), transition.id.clone());
        }
        summaries.push(StatementResult {
            name: statement.name().to_owned(),
            affected: 1,
            control: transition.map(|transition| transition.control()),
        });
    }
    if policy == CatalogPolicy::RevisionPerProgram {
        if catalog_changed {
            catalog::store::bump_catalog_generation(view).await?;
        }
        if schema_changed {
            catalog::store::bump_revision(view, runtime.now().into()).await?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_relational(
    view: &mut dyn KvView,
    kv_counters: Option<&super::observe::KvCounters>,
    statement: &Statement,
    bound: &BoundStatement,
    bindings: &HashMap<String, Vec<Env>>,
    limits: Limits,
    path: ExecutionPath,
    runtime: &dyn RuntimeEffects,
    measure_relations: bool,
    binding_rows: &mut Vec<(String, u64)>,
    measured: &mut Vec<(crate::engine::lir::fingerprint::Fingerprint, u64)>,
    join_operators: &mut Vec<super::observe::JoinOperatorMeasurement>,
    measure_operators: bool,
    operators: &mut Vec<super::observe::OperatorMeasurement>,
    execution_grant: &super::parallel::ExecutionGrant,
    subrelation_cache: Option<SubrelationCacheExecution<'_>>,
) -> Result<Vec<Env>> {
    let input = match path {
        ExecutionPath::Production => {
            let plan = bound
                .plan
                .as_ref()
                .expect("production program binding has a physical plan");
            let mut executor = Executor::new(&*view, limits);
            executor.set_execution_grant(execution_grant.clone());
            if let Some(kv_counters) = kv_counters {
                executor.observe_kv_work(kv_counters);
            }
            if let Some((cache, root_key, events)) = subrelation_cache {
                executor.use_subrelation_cache(cache, root_key, events);
            }
            if measure_relations {
                executor.enable_measurements();
            }
            if measure_operators {
                executor.enable_operator_measurements();
            }
            executor.seed_bindings(bindings.clone());
            let frames = executor.run_frames(plan).await?;
            for binding in &plan.bindings {
                if let Some(rows) = executor.binding_cardinality(&binding.name) {
                    binding_rows.push((binding.name.clone(), rows));
                }
            }
            measured.extend_from_slice(executor.measured());
            join_operators.extend_from_slice(executor.join_measurements());
            operators.extend(executor.operator_measurements());
            frames
        }
        ExecutionPath::Reference => {
            let mut executor = ReferenceExecutor::new(&*view, limits);
            executor.seed_bindings(bindings.clone());
            executor.run_frames(&bound.bound).await?
        }
    };
    if matches!(statement, Statement::Query { .. }) {
        return Ok(input);
    }
    let input_output = bound.bound.root.output();
    let rows = frames_to_rows(input_output, &input)?;
    let table = bound.target.as_ref().ok_or_else(|| {
        Error::message(
            ErrorKind::Internal,
            "exec: mutation statement has no target schema",
        )
    })?;
    let rows = match statement {
        Statement::Create { .. } => super::mutate::create(view, table, &rows, runtime).await?,
        Statement::Update { .. } => super::mutate::update(view, table, input_output, &rows).await?,
        Statement::Delete { .. } => super::mutate::delete(view, table, input_output, &rows).await?,
        _ => unreachable!("relational mutation kind checked above"),
    };
    rows_to_frames(&bound.result_output, &rows)
}

fn frames_to_rows(output: &RowType, frames: &[Env]) -> Result<Vec<Row>> {
    frames
        .iter()
        .map(|frame| {
            output
                .fields
                .iter()
                .map(|field| {
                    frame
                        .scalar_at(field.slot, &field.name, &field.value_type)
                        .map(|value| (field.name.clone(), value))
                        .map_err(Into::into)
                })
                .collect()
        })
        .collect()
}

fn rows_to_frames(output: &RowType, rows: &[Row]) -> Result<Vec<Env>> {
    rows.iter()
        .map(|row| {
            let mut frame = Env::new();
            for field in &output.fields {
                let value = row.get(&field.name).ok_or_else(|| {
                    Error::message(
                        ErrorKind::Internal,
                        format!("exec: mutation result lacks column {:?}", field.name),
                    )
                })?;
                frame.set_scalar(field.slot, value.clone());
            }
            Ok(frame)
        })
        .collect()
}

async fn apply_catalog(
    mutation: &mut CatalogMutation<'_>,
    statement: &Statement,
    transitions: &HashMap<String, TransitionId>,
    backfill: bool,
) -> Result<Option<SchemaTransition>> {
    let transition = match statement {
        Statement::CreateTable { table, .. } => {
            mutation.create_table(table.clone()).await?;
            None
        }
        Statement::RenameTable { table_id, to, .. } => {
            mutation.rename_table_by_schema_id(*table_id, to).await?;
            None
        }
        Statement::DeleteTable { table_id, .. } => {
            mutation.delete_table_by_schema_id(*table_id).await?;
            None
        }
        Statement::CreateColumn {
            table_id, column, ..
        } => {
            mutation
                .create_column_by_schema_id(*table_id, column.clone())
                .await?;
            None
        }
        Statement::RenameColumn {
            table_id,
            column_id,
            to,
            ..
        } => {
            mutation
                .rename_column_by_schema_id(*table_id, *column_id, to)
                .await?;
            None
        }
        Statement::ChangeColumnDefault {
            table_id,
            column_id,
            default,
            ..
        } => {
            let (_, column) = mutation.column_by_schema_id(*table_id, *column_id).await?;
            let default = default
                .clone()
                .map(|default| resolve_default(default, column.scalar_type, &column.format))
                .transpose()?;
            mutation
                .change_column_insert_default_by_schema_id(*table_id, *column_id, default)
                .await?;
            None
        }
        Statement::DeleteColumn {
            table_id,
            column_id,
            ..
        } => {
            mutation
                .delete_column_by_schema_id(*table_id, *column_id)
                .await?;
            None
        }
        Statement::CreateIndex {
            table_id, index, ..
        } => {
            let table = mutation.table_by_schema_id(*table_id).await?;
            let created = mutation.create_index(&table.name, index.clone()).await?;
            if backfill {
                backfill_index(mutation.view(), &table, &created).await?;
            }
            None
        }
        Statement::DeleteIndex {
            table_id, index, ..
        } => {
            mutation.delete_index_by_schema_id(*table_id, index).await?;
            None
        }
        Statement::StartIndexBuild {
            table_id,
            index,
            prerequisites,
            after,
            ..
        } => {
            let prerequisites = resolve_after(prerequisites.clone(), after, transitions)?;
            Some(
                mutation
                    .start_index_build_with_prerequisites(*table_id, index.clone(), prerequisites)
                    .await?,
            )
        }
        Statement::StartColumnReplacement {
            table_id,
            column_id,
            replacement,
            after,
            ..
        } => {
            let mut replacement = replacement.clone();
            replacement.prerequisites =
                resolve_after(replacement.prerequisites, after, transitions)?;
            Some(
                mutation
                    .start_column_replacement(*table_id, *column_id, replacement)
                    .await?,
            )
        }
        Statement::StartConstraintValidation {
            table_id,
            constraint,
            after,
            ..
        } => {
            let mut constraint = constraint.clone();
            constraint.prerequisites = resolve_after(constraint.prerequisites, after, transitions)?;
            Some(
                mutation
                    .start_constraint_validation(*table_id, constraint)
                    .await?,
            )
        }
        _ => {
            return Err(Error::message(
                ErrorKind::Internal,
                "exec: relational statement reached catalog executor",
            ));
        }
    };
    Ok(transition)
}

pub(crate) fn resolve_default(
    spec: DefaultSpec,
    scalar_type: ScalarType,
    format: &str,
) -> Result<DefaultValue> {
    let mismatch = || {
        input(format!(
            "catalog: literal default is not compatible with {scalar_type:?}"
        ))
    };
    Ok(match (spec, scalar_type) {
        (
            DefaultSpec::Generator(function @ (DefaultFunction::UuidV4 | DefaultFunction::UuidV7)),
            ScalarType::Bytes,
        ) if format == "uuid" => DefaultValue {
            function: Some(function),
            ..DefaultValue::default()
        },
        (DefaultSpec::Generator(DefaultFunction::Ulid), ScalarType::Bytes) if format == "ulid" => {
            DefaultValue {
                function: Some(DefaultFunction::Ulid),
                ..DefaultValue::default()
            }
        }
        (DefaultSpec::Generator(DefaultFunction::Xid), ScalarType::Bytes) if format == "xid" => {
            DefaultValue {
                function: Some(DefaultFunction::Xid),
                ..DefaultValue::default()
            }
        }
        (DefaultSpec::Generator(DefaultFunction::NowMs), ScalarType::Int64) => DefaultValue {
            function: Some(DefaultFunction::NowMs),
            ..DefaultValue::default()
        },
        (DefaultSpec::Generator(DefaultFunction::Increment), ScalarType::Int64) => DefaultValue {
            function: Some(DefaultFunction::Increment),
            ..DefaultValue::default()
        },
        (DefaultSpec::Text(text), ScalarType::Text) => DefaultValue {
            text,
            ..DefaultValue::default()
        },
        (DefaultSpec::Bool(bool_value), ScalarType::Bool) => DefaultValue {
            bool_value,
            ..DefaultValue::default()
        },
        (DefaultSpec::Text(text), ScalarType::Bytes) => DefaultValue {
            bytes: if let Some(format) = crate::identifiers::Format::recognize(format) {
                crate::identifiers::parse(format, &text)
                    .map_err(|error| input(format!("catalog: {error}")))?
            } else {
                crate::identifiers::decode_base64(&text)
                    .map_err(|error| input(format!("catalog: invalid bytes default: {error}")))?
            },
            ..DefaultValue::default()
        },
        (DefaultSpec::Number(number), ScalarType::Int64) => DefaultValue {
            int64: number
                .parse()
                .map_err(|_| input(format!("catalog: {number:?} is not an int64 default")))?,
            ..DefaultValue::default()
        },
        (DefaultSpec::Number(number), ScalarType::Float64) => {
            let float64 = number
                .parse::<f64>()
                .map_err(|_| input(format!("catalog: {number:?} is not a float64 default")))?;
            if !float64.is_finite() {
                return Err(input("catalog: float64 default must be finite"));
            }
            DefaultValue {
                float64,
                ..DefaultValue::default()
            }
        }
        _ => return Err(mismatch()),
    })
}

fn resolve_after(
    mut prerequisites: Vec<TransitionId>,
    after: &[String],
    transitions: &HashMap<String, TransitionId>,
) -> Result<Vec<TransitionId>> {
    let mut names = HashSet::new();
    for name in after {
        if !names.insert(name) {
            return Err(input(format!("exec: duplicate after reference {name:?}")));
        }
        prerequisites.push(transitions.get(name).cloned().ok_or_else(|| {
            input(format!(
                "exec: prerequisite statement {name:?} is not an earlier transition start"
            ))
        })?);
    }
    prerequisites.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    prerequisites.dedup();
    Ok(prerequisites)
}

async fn backfill_index(
    view: &dyn KvView,
    table: &catalog::model::Table,
    index: &catalog::model::Index,
) -> Result<()> {
    let rows = row_store::scan_table_columns(view, table, &table.columns).await?;
    for row in rows {
        write::backfill_index_entry(view, table, index, &row).await?;
    }
    Ok(())
}

fn relational_names(program: &Program) -> Vec<String> {
    program
        .statements
        .iter()
        .filter(|statement| statement.relational())
        .map(|statement| statement.name().to_owned())
        .collect()
}

fn input(message: impl Into<String>) -> Error {
    Error::message(ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::engine::catalog::identity::SchemaId;
    use crate::engine::catalog::model::{
        ColumnConversion, ColumnDraft, ColumnReplacementDef, ConstraintDef, ConstraintKind,
        IndexDef, ScalarType, TableDraft, TransitionKind, TransitionState,
    };
    use crate::engine::kv::TransactionalKv;
    use crate::engine::kv::fault::{
        FaultAction, FaultController, FaultRule, FaultingKv, Operation, TracePhase,
    };
    use crate::engine::kv::slatedb::Store;
    use crate::engine::lir::{Kind, RawScalar, Relation, RootCardinality, RowsColumn, Value};

    use super::*;
    use crate::engine::exec::Engine;

    fn tasks_table() -> TableDraft {
        TableDraft {
            id: Some(SchemaId::new(1).unwrap()),
            name: "tasks".into(),
            columns: vec![
                ColumnDraft {
                    id: Some(SchemaId::new(1).unwrap()),
                    name: "id".into(),
                    scalar_type: ScalarType::Text,
                    nullable: false,
                    format: String::new(),
                    default: None,
                },
                ColumnDraft {
                    id: Some(SchemaId::new(2).unwrap()),
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
        }
    }

    fn rows(values: &[(&str, &str)]) -> crate::engine::lir::Query {
        crate::engine::lir::Query {
            root: Relation::Rows {
                scope: "input".into(),
                columns: vec![
                    RowsColumn {
                        name: "id".into(),
                        kind: Kind::Text,
                        nullable: false,
                    },
                    RowsColumn {
                        name: "status".into(),
                        kind: Kind::Text,
                        nullable: false,
                    },
                ],
                values: values
                    .iter()
                    .map(|(id, status)| {
                        vec![
                            RawScalar::Text((*id).into()),
                            RawScalar::Text((*status).into()),
                        ]
                    })
                    .collect(),
            },
            cardinality: RootCardinality::Many,
            bindings: HashMap::new(),
        }
    }

    fn result_ref(binding: &str) -> crate::engine::lir::Query {
        crate::engine::lir::Query {
            root: Relation::Ref {
                binding: binding.into(),
                scope: "result".into(),
            },
            cardinality: RootCardinality::ExactlyOne,
            bindings: HashMap::new(),
        }
    }

    fn scan(table: &str) -> crate::engine::lir::Query {
        crate::engine::lir::Query {
            root: Relation::Scan {
                table: table.into(),
                scope: "scan".into(),
            },
            cardinality: RootCardinality::ExactlyOne,
            bindings: HashMap::new(),
        }
    }

    fn scan_program(table: &str) -> Program {
        Program {
            statements: vec![Statement::Query {
                name: "read".into(),
                relation: crate::engine::lir::Query {
                    root: Relation::Order {
                        input: Box::new(Relation::Scan {
                            table: table.into(),
                            scope: "scan".into(),
                        }),
                        terms: vec![crate::engine::lir::OrderTerm {
                            expression: crate::engine::lir::Expr::Column {
                                scope: "scan".into(),
                                name: "id".into(),
                            },
                            descending: false,
                        }],
                    },
                    cardinality: RootCardinality::Many,
                    bindings: HashMap::new(),
                },
            }],
            result: Some("read".into()),
        }
    }

    async fn setup(name: &str) -> (Arc<Store>, Engine, catalog::Catalog) {
        let store = Arc::new(Store::memory(name).await.unwrap());
        let engine = Engine::new(store.clone());
        let catalog = catalog::Catalog::new(store.clone());
        (store, engine, catalog)
    }

    #[tokio::test]
    async fn mutation_result_is_bound_once_for_later_statements() {
        let (_store, engine, catalog) = setup("pir-result-binding").await;
        catalog.create_table(tasks_table()).await.unwrap();

        let result = engine
            .execute_program(
                Program {
                    statements: vec![
                        Statement::Create {
                            name: "created".into(),
                            relation: rows(&[("a", "new")]),
                            table: "tasks".into(),
                        },
                        Statement::Query {
                            name: "read".into(),
                            relation: result_ref("created"),
                        },
                    ],
                    result: Some("read".into()),
                },
                CatalogPolicy::Forbidden,
            )
            .await
            .unwrap();

        assert_eq!(result.statements[0].affected, 1);
        assert_eq!(result.statements[1].affected, 1);
        assert_eq!(
            result.result,
            Datum::Object(vec![
                crate::engine::lir::ObjectField {
                    name: "id".into(),
                    datum: Datum::Scalar(Value::Text("a".into()))
                },
                crate::engine::lir::ObjectField {
                    name: "status".into(),
                    datum: Datum::Scalar(Value::Text("new".into()))
                },
            ])
        );
    }

    #[derive(Default)]
    struct RecordingObserver {
        observations: std::sync::Mutex<Vec<super::super::observe::StatementObservation>>,
    }

    impl super::super::observe::ExecutionObserver for RecordingObserver {
        fn statement(&self, observation: super::super::observe::StatementObservation) {
            self.observations.lock().unwrap().push(observation);
        }
    }

    #[tokio::test]
    async fn repeated_single_query_programs_use_the_relation_cache() {
        let (store, _engine, catalog) = setup("pir-relation-cache").await;
        catalog.create_table(tasks_table()).await.unwrap();
        let observer = Arc::new(RecordingObserver::default());
        let engine = Engine::new(store).with_observer(observer.clone());
        engine
            .execute_program(
                Program {
                    statements: vec![Statement::Create {
                        name: "seed".into(),
                        relation: rows(&[("a", "open")]),
                        table: "tasks".into(),
                    }],
                    result: None,
                },
                CatalogPolicy::Forbidden,
            )
            .await
            .unwrap();
        observer.observations.lock().unwrap().clear();
        let read = Program {
            statements: vec![Statement::Query {
                name: "read".into(),
                relation: crate::engine::lir::Query {
                    root: Relation::Order {
                        input: Box::new(Relation::Scan {
                            table: "tasks".into(),
                            scope: "task".into(),
                        }),
                        terms: vec![crate::engine::lir::OrderTerm {
                            expression: crate::engine::lir::Expr::Column {
                                scope: "task".into(),
                                name: "id".into(),
                            },
                            descending: false,
                        }],
                    },
                    cardinality: RootCardinality::Many,
                    bindings: HashMap::new(),
                },
            }],
            result: None,
        };

        let first = engine
            .execute_program(read.clone(), CatalogPolicy::Forbidden)
            .await
            .unwrap();
        let second = engine
            .execute_program(read, CatalogPolicy::Forbidden)
            .await
            .unwrap();
        assert_eq!(first.result, second.result);
        let observations = observer.observations.lock().unwrap();
        assert_eq!(observations.len(), 2);
        assert_eq!(
            observations[0].source,
            super::super::observe::StatementSource::Executed
        );
        assert_eq!(
            observations[1].source,
            super::super::observe::StatementSource::RelationCache
        );
        assert_eq!(observations[1].kv, super::super::observe::KvWork::default());
        assert!(observations[1].operators.is_empty());
        let cache = engine.relation_cache_stats();
        assert_eq!(cache.prepared_misses, 1);
        assert_eq!(cache.prepared_hits, 1);
        assert_eq!(cache.prepared_admissions, 1);
        assert_eq!(cache.prepared_entries, 1);
    }

    #[tokio::test]
    async fn prepared_read_preserves_the_current_statement_name() {
        let (store, _engine, catalog) = setup("pir-prepared-read-name").await;
        catalog.create_table(tasks_table()).await.unwrap();
        let engine = Engine::new(store);
        let named_read = |name: &str| {
            let mut program = scan_program("tasks");
            let Statement::Query {
                name: statement_name,
                ..
            } = &mut program.statements[0]
            else {
                unreachable!("scan program contains one query")
            };
            *statement_name = name.to_owned();
            program.result = Some(name.to_owned());
            program
        };
        let first = engine
            .execute_program(named_read("first"), CatalogPolicy::Forbidden)
            .await
            .unwrap();
        let second = engine
            .execute_program(named_read("second"), CatalogPolicy::Forbidden)
            .await
            .unwrap();

        assert_eq!(first.statements[0].name, "first");
        assert_eq!(second.statements[0].name, "second");
        assert_eq!(engine.relation_cache_stats().prepared_hits, 1);
    }

    #[tokio::test]
    async fn prepared_read_reuses_a_plan_after_a_data_change() {
        let (store, _engine, catalog) = setup("pir-prepared-read-data").await;
        catalog.create_table(tasks_table()).await.unwrap();
        let engine = Engine::new(store);
        engine
            .create(
                "tasks",
                crate::engine::lir::Row::from([
                    ("id".into(), Value::Text("a".into())),
                    ("status".into(), Value::Text("open".into())),
                ]),
            )
            .await
            .unwrap();
        let read = scan_program("tasks");

        let first = engine
            .execute_program(read.clone(), CatalogPolicy::Forbidden)
            .await
            .unwrap();
        engine
            .create(
                "tasks",
                crate::engine::lir::Row::from([
                    ("id".into(), Value::Text("b".into())),
                    ("status".into(), Value::Text("done".into())),
                ]),
            )
            .await
            .unwrap();
        let second = engine
            .execute_program(read, CatalogPolicy::Forbidden)
            .await
            .unwrap();

        assert!(matches!(first.result, Datum::Array(ref rows) if rows.len() == 1));
        assert!(matches!(second.result, Datum::Array(ref rows) if rows.len() == 2));
        let cache = engine.relation_cache_stats();
        assert_eq!(cache.prepared_misses, 1);
        assert_eq!(cache.prepared_hits, 1);
        assert_eq!(cache.prepared_entries, 1);
        assert_eq!(cache.misses, 2);
    }

    #[tokio::test]
    async fn prepared_read_rebinds_after_a_relevant_catalog_change() {
        let (store, _engine, catalog) = setup("pir-prepared-read-catalog").await;
        catalog.create_table(tasks_table()).await.unwrap();
        let engine = Engine::new(store);
        let read = scan_program("tasks");

        engine
            .execute_program(read.clone(), CatalogPolicy::Forbidden)
            .await
            .unwrap();
        catalog
            .create_column(
                "tasks",
                crate::engine::catalog::model::ColumnDef {
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
        let result = engine
            .execute_program(read, CatalogPolicy::Forbidden)
            .await
            .unwrap();

        assert!(matches!(result.result, Datum::Array(ref rows) if rows.is_empty()));
        let cache = engine.relation_cache_stats();
        assert_eq!(cache.prepared_misses, 2);
        assert_eq!(cache.prepared_hits, 0);
        assert_eq!(cache.prepared_admissions, 2);
        assert_eq!(cache.prepared_superseded, 1);
        assert_eq!(cache.prepared_entries, 2);
    }

    #[tokio::test]
    async fn production_statements_emit_observations_with_fingerprints() {
        let (store, _engine, catalog) = setup("pir-observe").await;
        catalog.create_table(tasks_table()).await.unwrap();
        let observer = Arc::new(RecordingObserver::default());
        let engine = Engine::new(store).with_observer(observer.clone());

        let program = |id: &str| Program {
            statements: vec![
                Statement::Create {
                    name: "created".into(),
                    relation: rows(&[(id, "new")]),
                    table: "tasks".into(),
                },
                Statement::Query {
                    name: "read".into(),
                    relation: result_ref("created"),
                },
            ],
            result: Some("read".into()),
        };
        engine
            .execute_program(program("a"), CatalogPolicy::Forbidden)
            .await
            .unwrap();

        let first: Vec<_> = observer.observations.lock().unwrap().drain(..).collect();
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].rows, 1);
        assert_eq!(first[0].affected, 1);
        assert!(first[0].plan.is_some());
        assert!(first[1].plan.is_some());
        assert!(
            first[0].kv.puts >= 1,
            "create must charge KV writes: {:?}",
            first[0].kv
        );
        assert_ne!(first[0].query.exact, first[1].query.exact);
        let created_tables: Vec<u32> = first[0].query.tables.iter().map(|id| id.get()).collect();
        assert_eq!(created_tables, Vec::<u32>::new());

        engine
            .execute_program(program("b"), CatalogPolicy::Forbidden)
            .await
            .unwrap();
        let second: Vec<_> = observer.observations.lock().unwrap().drain(..).collect();
        assert_eq!(second.len(), 2);
        assert_ne!(first[0].query.exact, second[0].query.exact);
        assert_eq!(first[0].query.family, second[0].query.family);
        assert_eq!(first[1].query.exact, second[1].query.exact);
        assert_eq!(first[1].plan, second[1].plan);
    }

    struct FixedStats(Arc<crate::engine::planner::models::PlannerStats>);

    impl crate::engine::planner::estimator::StatisticsProvider for FixedStats {
        fn planning_stats(&self) -> Arc<crate::engine::planner::models::PlannerStats> {
            self.0.clone()
        }
    }

    struct SwappingStats(std::sync::RwLock<Arc<crate::engine::planner::models::PlannerStats>>);

    impl crate::engine::planner::estimator::StatisticsProvider for SwappingStats {
        fn planning_stats(&self) -> Arc<crate::engine::planner::models::PlannerStats> {
            self.0.read().expect("statistics read lock").clone()
        }
    }

    #[tokio::test]
    async fn prepared_read_refreshes_when_statistics_identity_changes() {
        let (store, _engine, catalog) = setup("pir-prepared-read-statistics").await;
        catalog.create_table(tasks_table()).await.unwrap();
        let mut first = crate::engine::planner::models::PlannerStats::empty();
        first.snapshot_identity = "first".into();
        let provider = Arc::new(SwappingStats(std::sync::RwLock::new(Arc::new(first))));
        let engine = Engine::new(store).with_statistics_provider(provider.clone());
        let read = scan_program("tasks");

        engine
            .execute_program(read.clone(), CatalogPolicy::Forbidden)
            .await
            .unwrap();
        engine
            .execute_program(read.clone(), CatalogPolicy::Forbidden)
            .await
            .unwrap();
        let mut second = crate::engine::planner::models::PlannerStats::empty();
        second.snapshot_identity = "second".into();
        *provider.0.write().expect("statistics write lock") = Arc::new(second);
        engine
            .execute_program(read, CatalogPolicy::Forbidden)
            .await
            .unwrap();

        let cache = engine.relation_cache_stats();
        assert_eq!(cache.prepared_misses, 2);
        assert_eq!(cache.prepared_hits, 1);
        assert_eq!(cache.prepared_admissions, 2);
        assert_eq!(cache.prepared_entries, 2);
    }

    #[tokio::test]
    async fn collected_plans_carry_estimates_when_models_exist() {
        use crate::engine::planner::models::{PlannerStats, SynopsisCoverage, SynopsisModel};

        let (store, _engine, catalog) = setup("pir-plan-estimates").await;
        let catalog_table = catalog.create_table(tasks_table()).await.unwrap();

        let table = SchemaId::new(1).unwrap();
        let mut stats = PlannerStats::empty();
        stats.synopsis_models.insert(
            table,
            SynopsisModel {
                table,
                observed_rows: 42,
                coverage: SynopsisCoverage::Complete,
                sample_size: 42,
                changes_since_collection: 0,
                table_existence_generation: catalog_table.existence_generation.get(),
                collected_at_unix_micros: 0,
                catalog_version: 1,
                columns: Vec::new(),
                column_groups: Vec::new(),
                predicate_conditioned_degrees: Vec::new(),
            },
        );
        let engine =
            Engine::new(store).with_statistics_provider(Arc::new(FixedStats(Arc::new(stats))));

        let result = engine
            .execute_program_with_options(
                Program {
                    statements: vec![
                        Statement::Create {
                            name: "created".into(),
                            relation: rows(&[("a", "new")]),
                            table: "tasks".into(),
                        },
                        Statement::Query {
                            name: "read".into(),
                            relation: scan("tasks"),
                        },
                    ],
                    result: Some("read".into()),
                },
                ProgramOptions {
                    collect_plan: true,
                    ..ProgramOptions::default()
                },
            )
            .await
            .unwrap();

        let read_plan = &result.plans[1].plan;
        assert!(result.plans.iter().all(|plan| plan.measurement.is_some()));
        let read_scans = result.plans[1]
            .measurement
            .as_ref()
            .and_then(|measurement| measurement.logical_scans.as_ref())
            .expect("executed scan trace");
        let read_measurement = result.plans[1].measurement.as_ref().unwrap();
        let operator_trace = read_measurement
            .operator_trace
            .as_ref()
            .expect("executed operator trace");
        assert_eq!(operator_trace.format, "rad-operator-trace-v1");
        assert_eq!(operator_trace.dropped, 0);
        assert!(
            operator_trace
                .operators
                .iter()
                .any(|operator| operator.operator == "TableScan")
        );
        let physical_storage = read_measurement
            .physical_storage
            .as_ref()
            .expect("executed physical storage trace");
        assert_eq!(physical_storage.format, "rad-physical-storage-trace-v1");
        assert_eq!(physical_storage.backend, "slatedb");
        assert_eq!(physical_storage.coverage, "cache_and_backing_reads");
        assert_eq!(read_scans.format, "rad-logical-scan-trace-v1");
        assert_eq!(read_scans.dropped, 0);
        assert!(read_scans.scans.iter().any(|scan| {
            scan.purpose == crate::engine::kv::ScanPurpose::AccessPath
                && scan.snapshot_position.is_some()
                && scan.iterated == 1
                && scan.complete
        }));
        let created_plan = serde_json::to_value(&result.plans[0].plan).unwrap();
        assert!(
            created_plan["estimates"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| {
                    entry["target"] == "root"
                        && entry["source"] == "structural"
                        && entry["cardinality"] == 1
                        && entry["interval"]["kind"] == "exact"
                })
        );
        let json = serde_json::to_value(read_plan).unwrap();
        assert_eq!(json["statisticsPublishedAtMicros"], 0);
        let estimates = json["estimates"].as_array().unwrap();
        assert_eq!(
            estimates
                .iter()
                .find(|entry| entry["target"] == "root")
                .unwrap()["relation"],
            json["root"]["relation"]
        );
        assert!(estimates.iter().any(|entry| {
            entry["target"] == "root" && entry["source"] == "synopsis" && entry["cardinality"] == 42
        }));
        assert!(estimates.iter().any(|entry| {
            entry["target"] == "table:1"
                && entry["interval"]["kind"] == "exact"
                && entry["sampleSize"] == 42
        }));
    }

    struct CountingStats {
        calls: std::sync::atomic::AtomicUsize,
        statistics: Arc<crate::engine::planner::models::PlannerStats>,
    }

    impl crate::engine::planner::estimator::StatisticsProvider for CountingStats {
        fn planning_stats(&self) -> Arc<crate::engine::planner::models::PlannerStats> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.statistics.clone()
        }
    }

    #[tokio::test]
    async fn one_statistics_snapshot_covers_preflight_and_execution() {
        let (store, _engine, catalog) = setup("pir-pinned-statistics").await;
        catalog.create_table(tasks_table()).await.unwrap();
        let provider = Arc::new(CountingStats {
            calls: std::sync::atomic::AtomicUsize::new(0),
            statistics: Arc::new(crate::engine::planner::models::PlannerStats::empty()),
        });
        let engine = Engine::new(store).with_statistics_provider(provider.clone());
        let mut relation = rows(&[("a", "new")]);
        relation.cardinality = RootCardinality::ExactlyOne;
        engine
            .execute_program_with_options(
                Program {
                    statements: vec![Statement::Query {
                        name: "read".into(),
                        relation,
                    }],
                    result: Some("read".into()),
                },
                ProgramOptions {
                    collect_plan: true,
                    ..ProgramOptions::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn read_only_program_uses_one_storage_snapshot() {
        let store = Arc::new(Store::memory("pir-one-read-snapshot").await.unwrap());
        let controller = FaultController::default();
        let engine = Engine::new(Arc::new(FaultingKv::new(store, controller.clone())));
        let mut relation = rows(&[("a", "new")]);
        relation.cardinality = RootCardinality::ExactlyOne;

        engine
            .execute_program_with_options(
                Program {
                    statements: vec![Statement::Query {
                        name: "read".into(),
                        relation,
                    }],
                    result: Some("read".into()),
                },
                ProgramOptions {
                    collect_plan: true,
                    ..ProgramOptions::default()
                },
            )
            .await
            .unwrap();

        let begins = controller
            .trace()
            .into_iter()
            .filter(|event| {
                event.operation == Operation::Begin && event.phase == TracePhase::Started
            })
            .count();
        assert_eq!(begins, 1);
    }

    #[tokio::test]
    async fn observations_pair_the_estimate_with_the_actual_row_count() {
        use crate::engine::planner::models::{
            PlannerStats, SynopsisCoverage, SynopsisModel, q_error_x100,
        };

        let (store, _engine, catalog) = setup("pir-estimate-actual").await;
        let catalog_table = catalog.create_table(tasks_table()).await.unwrap();

        let table = SchemaId::new(1).unwrap();
        let mut stats = PlannerStats::empty();
        stats.synopsis_models.insert(
            table,
            SynopsisModel {
                table,
                observed_rows: 900,
                coverage: SynopsisCoverage::Complete,
                sample_size: 900,
                changes_since_collection: 0,
                table_existence_generation: catalog_table.existence_generation.get(),
                collected_at_unix_micros: 0,
                catalog_version: 1,
                columns: Vec::new(),
                column_groups: Vec::new(),
                predicate_conditioned_degrees: Vec::new(),
            },
        );
        let observer = Arc::new(RecordingObserver::default());
        let engine = Engine::new(store)
            .with_observer(observer.clone())
            .with_statistics_provider(Arc::new(FixedStats(Arc::new(stats))));

        engine
            .execute_program(
                Program {
                    statements: vec![
                        Statement::Create {
                            name: "created".into(),
                            relation: rows(&[("a", "new")]),
                            table: "tasks".into(),
                        },
                        Statement::Query {
                            name: "read".into(),
                            relation: scan("tasks"),
                        },
                    ],
                    result: Some("read".into()),
                },
                CatalogPolicy::Forbidden,
            )
            .await
            .unwrap();

        let observations: Vec<_> = observer.observations.lock().unwrap().drain(..).collect();
        let read = observations
            .iter()
            .find(|observation| observation.plan.is_some() && observation.mutated.is_none())
            .expect("the read statement was observed");
        assert_eq!(read.rows, 1);
        let estimate = read.estimate.expect("a synopsis was installed");
        assert_eq!(estimate.cardinality, 900);
        assert_eq!(
            estimate.source,
            crate::engine::planner::estimator::EstimateSource::Synopsis
        );
        assert_eq!(q_error_x100(estimate.cardinality, read.rows), 90_000);
    }

    #[tokio::test]
    async fn a_relation_measured_as_a_binding_is_recognised_elsewhere() {
        let (store, _engine, catalog) = setup("pir-relation-attribution").await;
        catalog.create_table(tasks_table()).await.unwrap();
        let observer = Arc::new(RecordingObserver::default());
        let engine = Engine::new(store).with_observer(observer.clone());

        engine
            .execute_program(
                Program {
                    statements: vec![Statement::Create {
                        name: "seed".into(),
                        relation: rows(&[("a", "open"), ("b", "open"), ("c", "done")]),
                        table: "tasks".into(),
                    }],
                    result: None,
                },
                CatalogPolicy::Forbidden,
            )
            .await
            .unwrap();
        observer.observations.lock().unwrap().clear();

        // One statement defines the scan as a named binding; a second
        // reaches the same relation inline through its root.
        let mut bindings = HashMap::new();
        bindings.insert(
            "everything".into(),
            Relation::Scan {
                table: "tasks".into(),
                scope: "s".into(),
            },
        );
        // Two references make the binding worth materializing; a single
        // reference is replayed and therefore has no count to report.
        let via_binding = crate::engine::lir::Query {
            root: Relation::Order {
                input: Box::new(Relation::Concatenate {
                    scope: "both".into(),
                    inputs: vec![
                        Relation::Ref {
                            binding: "everything".into(),
                            scope: "first".into(),
                        },
                        Relation::Ref {
                            binding: "everything".into(),
                            scope: "second".into(),
                        },
                    ],
                }),
                terms: vec![crate::engine::lir::OrderTerm {
                    expression: crate::engine::lir::Expr::Column {
                        scope: "both".into(),
                        name: "id".into(),
                    },
                    descending: false,
                }],
            },
            cardinality: RootCardinality::Many,
            bindings,
        };
        engine
            .execute_program(
                Program {
                    statements: vec![Statement::Query {
                        name: "read".into(),
                        relation: via_binding,
                    }],
                    result: Some("read".into()),
                },
                CatalogPolicy::Forbidden,
            )
            .await
            .unwrap();

        let observations: Vec<_> = observer.observations.lock().unwrap().drain(..).collect();
        let statement = observations
            .iter()
            .find(|observation| !observation.relations.is_empty())
            .expect("attributed nodes were measured");
        // The scan of the seeded table is one of the attributed nodes and
        // produced every seeded row.
        let measured = statement
            .relations
            .iter()
            .find(|relation| relation.rows == 3)
            .expect("the scan produced every seeded row");

        // The same relation, fingerprinted from a query that never used a
        // binding, carries the identity the measurement was filed under.
        let inline = crate::engine::lir::Query {
            root: Relation::Order {
                input: Box::new(Relation::Scan {
                    table: "tasks".into(),
                    scope: "elsewhere".into(),
                }),
                terms: vec![crate::engine::lir::OrderTerm {
                    expression: crate::engine::lir::Expr::Column {
                        scope: "elsewhere".into(),
                        name: "id".into(),
                    },
                    descending: false,
                }],
            },
            cardinality: RootCardinality::Many,
            bindings: HashMap::new(),
        };
        engine
            .execute_program(
                Program {
                    statements: vec![Statement::Query {
                        name: "read".into(),
                        relation: inline,
                    }],
                    result: Some("read".into()),
                },
                CatalogPolicy::Forbidden,
            )
            .await
            .unwrap();
        let later: Vec<_> = observer.observations.lock().unwrap().drain(..).collect();
        let inline_subtrees = &later[0].query.subtrees;
        assert!(
            inline_subtrees
                .iter()
                .any(|digests| digests.family == measured.family),
            "the scan measured inside the binding must be the same family inline"
        );
    }

    #[tokio::test]
    async fn nested_relations_are_measured_and_truncated_ones_are_withheld() {
        let (store, _engine, catalog) = setup("pir-nested-attribution").await;
        catalog.create_table(tasks_table()).await.unwrap();
        let observer = Arc::new(RecordingObserver::default());
        let engine = Engine::new(store).with_observer(observer.clone());

        engine
            .execute_program(
                Program {
                    statements: vec![Statement::Create {
                        name: "seed".into(),
                        relation: rows(&[("a", "open"), ("b", "open"), ("c", "done")]),
                        table: "tasks".into(),
                    }],
                    result: None,
                },
                CatalogPolicy::Forbidden,
            )
            .await
            .unwrap();

        let ordered_scan = || Relation::Order {
            input: Box::new(Relation::Scan {
                table: "tasks".into(),
                scope: "t".into(),
            }),
            terms: vec![crate::engine::lir::OrderTerm {
                expression: crate::engine::lir::Expr::Column {
                    scope: "t".into(),
                    name: "id".into(),
                },
                descending: true,
            }],
        };
        let run = |root: Relation| {
            let engine = &engine;
            let observer = observer.clone();
            async move {
                observer.observations.lock().unwrap().clear();
                engine
                    .execute_program_with_options(
                        Program {
                            statements: vec![Statement::Query {
                                name: "read".into(),
                                relation: crate::engine::lir::Query {
                                    root,
                                    cardinality: RootCardinality::Many,
                                    bindings: HashMap::new(),
                                },
                            }],
                            result: Some("read".into()),
                        },
                        ProgramOptions {
                            collect_plan: true,
                            ..ProgramOptions::default()
                        },
                    )
                    .await
                    .unwrap();
                observer
                    .observations
                    .lock()
                    .unwrap()
                    .drain(..)
                    .next()
                    .unwrap()
            }
        };

        // Nothing downstream cuts the stream short, so every attributed
        // relation in the chain reports what it produced.
        let whole = run(ordered_scan()).await;
        assert!(
            whole.relations.len() >= 2,
            "stacked relations each get their own measurement: {:?}",
            whole.relations
        );
        assert!(
            whole.relations.iter().all(|relation| relation.rows == 3),
            "every relation in the chain saw all three rows: {:?}",
            whole.relations
        );
        for relation in &whole.relations {
            assert!(
                whole
                    .query
                    .subtrees
                    .iter()
                    .any(|digests| digests.family == relation.family),
                "a measured family must be a subtree of the statement"
            );
        }

        // A limit abandons the operators beneath it part-way, so their row
        // counts are not their cardinality and are withheld entirely.
        let limited = run(Relation::Slice {
            input: Box::new(ordered_scan()),
            offset: 0,
            limit: Some(2),
        })
        .await;
        // The sort must drain its input before it can emit, so the scan
        // beneath it is exhausted and trustworthy; the sort itself is
        // abandoned once the slice has two rows, so it reports nothing.
        let mut rows: Vec<u64> = limited
            .relations
            .iter()
            .map(|relation| relation.rows)
            .collect();
        rows.sort_unstable();
        assert_eq!(
            rows,
            vec![2, 3],
            "the slice and the exhausted scan report; the abandoned sort does not: {:?}",
            limited.relations
        );
        let operator = |name| {
            limited
                .operators
                .iter()
                .find(|measurement| measurement.operator == name)
                .unwrap_or_else(|| panic!("missing {name} measurement"))
        };
        let scan = operator("TableScan");
        assert_eq!(scan.output_rows, 3);
        assert!(scan.complete);
        let sort = operator("Sort");
        assert_eq!(sort.input_rows, 3);
        assert_eq!(sort.output_rows, 2);
        assert!(sort.input_complete);
        assert!(!sort.complete);
        let slice = operator("Slice");
        assert_eq!(slice.input_rows, 2);
        assert_eq!(slice.output_rows, 2);
        assert!(slice.complete);
        assert!(
            limited.operators.iter().all(|measurement| {
                measurement.exclusive_micros <= measurement.inclusive_micros
            })
        );
    }

    #[tokio::test]
    async fn observation_never_changes_results() {
        let program = || Program {
            statements: vec![
                Statement::Create {
                    name: "created".into(),
                    relation: rows(&[("a", "new")]),
                    table: "tasks".into(),
                },
                Statement::Query {
                    name: "read".into(),
                    relation: scan("tasks"),
                },
            ],
            result: Some("read".into()),
        };

        let (_store, engine, catalog) = setup("pir-observe-off").await;
        catalog.create_table(tasks_table()).await.unwrap();
        let unobserved = engine
            .execute_program(program(), CatalogPolicy::Forbidden)
            .await;

        let (store, _engine, catalog) = setup("pir-observe-on").await;
        catalog.create_table(tasks_table()).await.unwrap();
        let engine = Engine::new(store).with_observer(Arc::new(RecordingObserver::default()));
        let observed = engine
            .execute_program(program(), CatalogPolicy::Forbidden)
            .await;

        match (unobserved, observed) {
            (Ok(unobserved), Ok(observed)) => {
                assert_eq!(unobserved.result, observed.result);
                assert_eq!(unobserved.statements, observed.statements);
            }
            (Err(unobserved), Err(observed)) => {
                assert_eq!(format!("{unobserved:?}"), format!("{observed:?}"));
            }
            (unobserved, observed) => {
                panic!("outcomes diverged: {unobserved:?} vs {observed:?}")
            }
        }
    }

    #[tokio::test]
    async fn reference_program_executes_writes_and_statement_bindings() {
        let (_store, engine, catalog) = setup("pir-reference-program").await;
        catalog.create_table(tasks_table()).await.unwrap();
        let result = engine
            .execute_program_reference_with_options(
                Program {
                    statements: vec![
                        Statement::Create {
                            name: "created".into(),
                            relation: rows(&[("a", "new")]),
                            table: "tasks".into(),
                        },
                        Statement::Query {
                            name: "read".into(),
                            relation: result_ref("created"),
                        },
                    ],
                    result: Some("read".into()),
                },
                ProgramOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.statements[0].affected, 1);
        assert!(matches!(result.result, Datum::Object(_)));
        assert_eq!(
            engine.execute_reference(scan("tasks")).await.unwrap(),
            result.result
        );
    }

    #[tokio::test]
    async fn dry_run_returns_plans_without_committing_catalog_or_data() {
        let (_store, engine, catalog) = setup("pir-dry-run").await;
        let result = engine
            .execute_program_with_options(
                Program {
                    statements: vec![
                        Statement::CreateTable {
                            name: "table".into(),
                            table: tasks_table(),
                        },
                        Statement::Create {
                            name: "created".into(),
                            relation: rows(&[("a", "new")]),
                            table: "tasks".into(),
                        },
                    ],
                    result: Some("created".into()),
                },
                ProgramOptions {
                    catalog: CatalogPolicy::RevisionPerProgram,
                    dry_run: true,
                    collect_plan: true,
                    ..ProgramOptions::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(result.result, Datum::Null);
        assert!(result.statements.is_empty());
        assert_eq!(result.plans.len(), 1);
        assert_eq!(result.plans[0].name, "created");
        assert!(result.plans[0].measurement.is_none());
        assert!(catalog.get_table("tasks").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn expected_catalog_fences_preflight_and_execution() {
        let (_store, engine, catalog) = setup("pir-catalog-expectation").await;
        let stale = CatalogExpectation::from(&catalog.revision().await.unwrap());
        catalog.create_table(tasks_table()).await.unwrap();
        let current = CatalogExpectation::from(&catalog.revision().await.unwrap());
        let mut relation = rows(&[("a", "new")]);
        relation.cardinality = RootCardinality::ExactlyOne;
        let program = Program {
            statements: vec![Statement::Query {
                name: "rows".into(),
                relation,
            }],
            result: None,
        };
        let error = engine
            .execute_program_with_options(
                program.clone(),
                ProgramOptions {
                    expected_catalog: Some(stale),
                    ..ProgramOptions::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Conflict);

        let result = engine
            .execute_program_with_options(
                program,
                ProgramOptions {
                    expected_catalog: Some(current),
                    ..ProgramOptions::default()
                },
            )
            .await
            .unwrap();
        assert!(matches!(result.result, Datum::Object(_)));
    }

    #[tokio::test]
    async fn later_statement_failure_rolls_back_earlier_writes() {
        let (_store, engine, catalog) = setup("pir-rollback").await;
        catalog.create_table(tasks_table()).await.unwrap();

        let error = engine
            .execute_program(
                Program {
                    statements: vec![
                        Statement::Create {
                            name: "created".into(),
                            relation: rows(&[("a", "new")]),
                            table: "tasks".into(),
                        },
                        Statement::Update {
                            name: "missing".into(),
                            relation: rows(&[("b", "done")]),
                            table: "tasks".into(),
                        },
                    ],
                    result: Some("missing".into()),
                },
                CatalogPolicy::Forbidden,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MutationNotFound);

        let table = catalog.get_table("tasks").await.unwrap().unwrap();
        let transaction = _store
            .begin(crate::engine::kv::IsolationLevel::Snapshot)
            .await
            .unwrap();
        let view = crate::engine::kv::TransactionView(&*transaction);
        assert!(
            row_store::scan_table_columns(&view, &table, &table.columns)
                .await
                .unwrap()
                .is_empty()
        );
        transaction.rollback();
    }

    #[tokio::test]
    async fn catalog_change_is_visible_to_later_binding_and_write() {
        let (_store, engine, catalog) = setup("pir-catalog-visibility").await;
        catalog.create_table(tasks_table()).await.unwrap();

        let result = engine
            .execute_program(
                Program {
                    statements: vec![
                        Statement::RenameTable {
                            name: "rename".into(),
                            table_id: SchemaId::new(1).unwrap(),
                            to: "work".into(),
                        },
                        Statement::Create {
                            name: "created".into(),
                            relation: rows(&[("a", "new")]),
                            table: "work".into(),
                        },
                        Statement::Query {
                            name: "read".into(),
                            relation: result_ref("created"),
                        },
                    ],
                    result: Some("read".into()),
                },
                CatalogPolicy::RevisionPerProgram,
            )
            .await
            .unwrap();

        assert!(catalog.get_table("tasks").await.unwrap().is_none());
        assert!(catalog.get_table("work").await.unwrap().is_some());
        assert!(matches!(result.result, Datum::Object(_)));
    }

    #[tokio::test]
    async fn unique_index_backfill_rejects_duplicates_and_rolls_back_definition() {
        let (_store, engine, catalog) = setup("pir-index-backfill").await;
        catalog.create_table(tasks_table()).await.unwrap();
        engine
            .create_many(
                "tasks",
                vec![
                    Row::from([
                        ("id".into(), Value::Text("a".into())),
                        ("status".into(), Value::Text("same".into())),
                    ]),
                    Row::from([
                        ("id".into(), Value::Text("b".into())),
                        ("status".into(), Value::Text("same".into())),
                    ]),
                ],
            )
            .await
            .unwrap();

        let error = engine
            .execute_program(
                Program {
                    statements: vec![Statement::CreateIndex {
                        name: "index".into(),
                        table_id: SchemaId::new(1).unwrap(),
                        index: IndexDef {
                            name: "by_status".into(),
                            columns: vec!["status".into()],
                            unique: true,
                        },
                    }],
                    result: None,
                },
                CatalogPolicy::RevisionPerProgram,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ConstraintViolation);
        assert!(
            catalog
                .get_table("tasks")
                .await
                .unwrap()
                .unwrap()
                .index("by_status")
                .is_none()
        );
    }

    #[tokio::test]
    async fn pir_starts_typed_transition_controls_and_resolves_earlier_dependencies() {
        let (_store, engine, catalog) = setup("pir-transition-controls").await;
        let mut table = tasks_table();
        table.columns[1].nullable = true;
        catalog.create_table(table).await.unwrap();
        let before = catalog.revision().await.unwrap();
        let result = engine
            .execute_program_with_options(
                Program {
                    statements: vec![
                        Statement::StartColumnReplacement {
                            name: "replace".into(),
                            table_id: SchemaId::new(1).unwrap(),
                            column_id: SchemaId::new(2).unwrap(),
                            replacement: ColumnReplacementDef {
                                scalar_type: ScalarType::Int64,
                                nullable: true,
                                format: String::new(),
                                default: None,
                                conversion: ColumnConversion::StrictBuiltin,
                                prerequisites: Vec::new(),
                            },
                            after: Vec::new(),
                        },
                        Statement::StartIndexBuild {
                            name: "build".into(),
                            table_id: SchemaId::new(1).unwrap(),
                            index: IndexDef {
                                name: "tasks_status_idx".into(),
                                columns: vec!["status".into()],
                                unique: false,
                            },
                            prerequisites: Vec::new(),
                            after: vec!["replace".into()],
                        },
                        Statement::StartConstraintValidation {
                            name: "validate".into(),
                            table_id: SchemaId::new(1).unwrap(),
                            constraint: ConstraintDef {
                                name: "tasks_status_required".into(),
                                kind: ConstraintKind::NotNull,
                                column_id: SchemaId::new(2).unwrap(),
                                prerequisites: Vec::new(),
                            },
                            after: vec!["replace".into()],
                        },
                    ],
                    result: None,
                },
                ProgramOptions {
                    catalog: CatalogPolicy::RevisionPerProgram,
                    ..ProgramOptions::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(result.statements.len(), 3);
        for statement in &result.statements {
            assert_eq!(statement.affected, 1);
            assert_eq!(statement.control.as_ref().unwrap().kind, "transition");
        }
        let replacement = result.statements[0].control.as_ref().unwrap();
        assert_eq!(
            replacement.transition_kind,
            TransitionKind::ColumnReplacement
        );
        assert_eq!(replacement.state, TransitionState::Building);
        for control in result.statements[1..]
            .iter()
            .map(|statement| statement.control.as_ref().unwrap())
        {
            assert_eq!(control.state, TransitionState::Waiting);
            assert_eq!(
                control.prerequisites,
                vec![replacement.transition_id.clone()]
            );
        }
        assert_eq!(
            result.statements[1]
                .control
                .as_ref()
                .unwrap()
                .transition_kind,
            TransitionKind::IndexBuild
        );
        assert_eq!(
            result.statements[2]
                .control
                .as_ref()
                .unwrap()
                .transition_kind,
            TransitionKind::ConstraintValidation
        );
        assert_eq!(catalog.revision().await.unwrap().version, before.version);
    }

    #[tokio::test]
    async fn invalid_pir_transition_dependencies_are_rejected_atomically() {
        for (name, after) in [
            ("forward", vec!["later".to_owned()]),
            ("self", vec!["replace".to_owned()]),
            (
                "duplicate",
                vec!["replace".to_owned(), "replace".to_owned()],
            ),
        ] {
            let (_store, engine, catalog) = setup(&format!("pir-invalid-after-{name}")).await;
            catalog.create_table(tasks_table()).await.unwrap();
            let mut statements = vec![Statement::StartColumnReplacement {
                name: "replace".into(),
                table_id: SchemaId::new(1).unwrap(),
                column_id: SchemaId::new(2).unwrap(),
                replacement: ColumnReplacementDef {
                    scalar_type: ScalarType::Int64,
                    nullable: false,
                    format: String::new(),
                    default: None,
                    conversion: ColumnConversion::StrictBuiltin,
                    prerequisites: Vec::new(),
                },
                after: if name == "self" {
                    after.clone()
                } else {
                    Vec::new()
                },
            }];
            if name != "self" {
                statements.push(Statement::StartIndexBuild {
                    name: if name == "forward" { "build" } else { "later" }.into(),
                    table_id: SchemaId::new(1).unwrap(),
                    index: IndexDef {
                        name: "tasks_status_idx".into(),
                        columns: vec!["status".into()],
                        unique: false,
                    },
                    prerequisites: Vec::new(),
                    after,
                });
                if name == "forward" {
                    statements.push(Statement::StartIndexBuild {
                        name: "later".into(),
                        table_id: SchemaId::new(1).unwrap(),
                        index: IndexDef {
                            name: "tasks_status_later_idx".into(),
                            columns: vec!["status".into()],
                            unique: false,
                        },
                        prerequisites: Vec::new(),
                        after: Vec::new(),
                    });
                }
            }
            engine
                .execute_program_with_options(
                    Program {
                        statements,
                        result: None,
                    },
                    ProgramOptions {
                        catalog: CatalogPolicy::RevisionPerProgram,
                        ..ProgramOptions::default()
                    },
                )
                .await
                .expect_err("invalid completion dependency must fail preflight");
            assert!(
                catalog.list_transitions().await.unwrap().is_empty(),
                "case {name} persisted a partial transition graph"
            );
        }
    }

    #[tokio::test]
    async fn read_only_pir_uses_a_rollback_snapshot_and_never_attempts_commit() {
        let store = Arc::new(Store::memory("pir-read-only-no-commit").await.unwrap());
        catalog::Catalog::new(store.clone())
            .create_table(tasks_table())
            .await
            .unwrap();
        let controller = FaultController::new(vec![FaultRule {
            operation: Operation::Commit,
            occurrence: 1,
            action: FaultAction::ErrorBefore(crate::engine::kv::ErrorKind::Unavailable),
        }]);
        let faulting = Arc::new(FaultingKv::new(store, controller.clone()));
        let engine = Engine::new(faulting);
        let mut read_relation = rows(&[("a", "visible")]);
        read_relation.cardinality = RootCardinality::ExactlyOne;

        let read = engine
            .execute_program(
                Program {
                    statements: vec![Statement::Query {
                        name: "read".into(),
                        relation: read_relation,
                    }],
                    result: None,
                },
                CatalogPolicy::Forbidden,
            )
            .await
            .unwrap();
        assert_eq!(read.statements[0].affected, 1);
        assert!(
            controller
                .trace()
                .iter()
                .all(|event| event.operation != Operation::Commit),
            "a read-only program attempted a physical commit"
        );

        let error = engine
            .execute_program(
                Program {
                    statements: vec![Statement::Create {
                        name: "write".into(),
                        relation: rows(&[("a", "new")]),
                        table: "tasks".into(),
                    }],
                    result: None,
                },
                CatalogPolicy::Forbidden,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Storage);
        assert!(
            controller
                .trace()
                .iter()
                .any(|event| event.operation == Operation::Commit),
            "the fault rule was not exercised by the effectful control case"
        );
    }

    #[tokio::test]
    async fn mutation_shape_errors_are_rejected_during_program_preflight() {
        let (_store, engine, catalog) = setup("pir-mutation-preflight").await;
        catalog.create_table(tasks_table()).await.unwrap();
        let relation = crate::engine::lir::Query {
            root: Relation::Rows {
                scope: "input".into(),
                columns: vec![RowsColumn {
                    name: "id".into(),
                    kind: Kind::Text,
                    nullable: false,
                }],
                values: vec![vec![RawScalar::Text("a".into())]],
            },
            cardinality: RootCardinality::Many,
            bindings: HashMap::new(),
        };
        let error = engine
            .execute_program(
                Program {
                    statements: vec![Statement::Create {
                        name: "invalid".into(),
                        relation,
                        table: "tasks".into(),
                    }],
                    result: None,
                },
                CatalogPolicy::Forbidden,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn multi_statement_program_requires_an_explicit_result() {
        let program = Program {
            statements: vec![
                Statement::Query {
                    name: "a".into(),
                    relation: result_ref("x"),
                },
                Statement::Query {
                    name: "b".into(),
                    relation: result_ref("x"),
                },
            ],
            result: None,
        };
        assert_eq!(
            validate(&program, CatalogPolicy::Forbidden)
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn program_envelope_rejects_empty_duplicate_and_invalid_result_selection() {
        let query = |name: &str| Statement::Query {
            name: name.into(),
            relation: result_ref("x"),
        };
        let catalog = |name: &str| Statement::RenameTable {
            name: name.into(),
            table_id: SchemaId::new(1).unwrap(),
            to: "renamed".into(),
        };

        for program in [
            Program {
                statements: Vec::new(),
                result: None,
            },
            Program {
                statements: vec![query("duplicate"), query("duplicate")],
                result: Some("duplicate".into()),
            },
            Program {
                statements: vec![query("known")],
                result: Some("unknown".into()),
            },
            Program {
                statements: vec![catalog("catalog")],
                result: Some("catalog".into()),
            },
            Program {
                statements: vec![catalog("catalog"), query("rows")],
                result: None,
            },
        ] {
            assert_eq!(
                validate(&program, CatalogPolicy::RevisionPerProgram)
                    .unwrap_err()
                    .kind(),
                ErrorKind::InvalidInput
            );
        }

        assert_eq!(
            validate(
                &Program {
                    statements: vec![query("rows")],
                    result: None,
                },
                CatalogPolicy::RevisionPerProgram,
            )
            .unwrap(),
            Some("rows".into())
        );
        assert_eq!(
            validate(
                &Program {
                    statements: vec![catalog("first"), catalog("second")],
                    result: None,
                },
                CatalogPolicy::RevisionPerProgram,
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn unresolved_wire_defaults_populate_exactly_the_typed_catalog_field() {
        let cases = [
            (
                DefaultSpec::Generator(DefaultFunction::UuidV4),
                ScalarType::Bytes,
                "uuid",
                DefaultValue {
                    function: Some(DefaultFunction::UuidV4),
                    ..DefaultValue::default()
                },
            ),
            (
                DefaultSpec::Generator(DefaultFunction::NowMs),
                ScalarType::Int64,
                "",
                DefaultValue {
                    function: Some(DefaultFunction::NowMs),
                    ..DefaultValue::default()
                },
            ),
            (
                DefaultSpec::Generator(DefaultFunction::Increment),
                ScalarType::Int64,
                "",
                DefaultValue {
                    function: Some(DefaultFunction::Increment),
                    ..DefaultValue::default()
                },
            ),
            (
                DefaultSpec::Text("rad".into()),
                ScalarType::Text,
                "",
                DefaultValue {
                    text: "rad".into(),
                    ..DefaultValue::default()
                },
            ),
            (
                DefaultSpec::Number("-17".into()),
                ScalarType::Int64,
                "",
                DefaultValue {
                    int64: -17,
                    ..DefaultValue::default()
                },
            ),
            (
                DefaultSpec::Number("1.25".into()),
                ScalarType::Float64,
                "",
                DefaultValue {
                    float64: 1.25,
                    ..DefaultValue::default()
                },
            ),
            (
                DefaultSpec::Bool(true),
                ScalarType::Bool,
                "",
                DefaultValue {
                    bool_value: true,
                    ..DefaultValue::default()
                },
            ),
        ];
        for (spec, scalar_type, format, expected) in cases {
            assert_eq!(
                DefaultSpec::from_catalog(&expected, scalar_type, format),
                spec
            );
            assert_eq!(
                resolve_default(spec, scalar_type, format).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn unresolved_wire_defaults_reject_mismatched_and_invalid_values() {
        for (spec, scalar_type, format) in [
            (DefaultSpec::Text("1".into()), ScalarType::Int64, ""),
            (DefaultSpec::Bool(true), ScalarType::Text, ""),
            (DefaultSpec::Number("1".into()), ScalarType::Bool, ""),
            (
                DefaultSpec::Generator(DefaultFunction::UuidV4),
                ScalarType::Int64,
                "uuid",
            ),
            (
                DefaultSpec::Generator(DefaultFunction::NowMs),
                ScalarType::Text,
                "",
            ),
            (
                DefaultSpec::Generator(DefaultFunction::Increment),
                ScalarType::Text,
                "",
            ),
            (
                DefaultSpec::Number("not-a-number".into()),
                ScalarType::Int64,
                "",
            ),
            (DefaultSpec::Number("NaN".into()), ScalarType::Float64, ""),
            (DefaultSpec::Number("inf".into()), ScalarType::Float64, ""),
        ] {
            assert!(resolve_default(spec, scalar_type, format).is_err());
        }
    }
}
