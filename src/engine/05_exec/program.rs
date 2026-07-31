//! Ordered, atomic PIR program execution.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

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

use super::{
    Error, ErrorKind, Executor, Limits, ReferenceExecutor, Result, row_store, shape_frames, write,
};

#[derive(Clone, Copy)]
pub(super) enum ExecutionPath {
    Production,
    Reference,
}

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
    pub(crate) fn from_catalog(value: &DefaultValue, scalar_type: ScalarType) -> Self {
        if let Some(function) = value.function {
            return Self::Generator(function);
        }
        match scalar_type {
            ScalarType::Text => Self::Text(value.text.clone()),
            ScalarType::Int64 => Self::Number(value.int64.to_string()),
            ScalarType::Float64 => Self::Number(value.float64.to_string()),
            ScalarType::Bool => Self::Bool(value.bool_value),
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

#[derive(Clone, Debug, PartialEq)]
pub struct StatementPlan {
    pub name: String,
    pub plan: PlanView,
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

pub(super) async fn preflight(
    view: &mut dyn KvView,
    program: &Program,
    policy: CatalogPolicy,
    collect_plan: bool,
    physical: bool,
    runtime: &Arc<dyn RuntimeEffects>,
) -> Result<Vec<StatementPlan>> {
    let names = relational_names(program);
    let mut binder = ProgramBinder::new(names)?;
    let mut transitions = HashMap::new();
    let mut catalog_changed = false;
    let mut schema_changed = false;
    let mut plans = Vec::new();
    for statement in &program.statements {
        if let Some(binding) = statement.binder_statement() {
            let catalog = super::engine::ViewCatalog { view: &*view };
            let bound = if physical || collect_plan {
                binder.bind(&catalog, binding).await?
            } else {
                binder.bind_reference(&catalog, binding).await?
            };
            if collect_plan {
                plans.push(StatementPlan {
                    name: bound.name.clone(),
                    plan: PlanView::new(bound.plan.as_ref().expect("plan requested")),
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
    Ok(plans)
}

pub(super) async fn run(
    view: &mut dyn KvView,
    program: &Program,
    result_name: Option<&str>,
    policy: CatalogPolicy,
    limits: Limits,
    runtime: &Arc<dyn RuntimeEffects>,
) -> Result<ProgramResult> {
    run_with_path(
        view,
        program,
        result_name,
        policy,
        limits,
        ExecutionPath::Production,
        runtime,
    )
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
    run_with_path(
        view,
        program,
        result_name,
        policy,
        limits,
        ExecutionPath::Reference,
        runtime,
    )
    .await
}

async fn run_with_path(
    view: &mut dyn KvView,
    program: &Program,
    result_name: Option<&str>,
    policy: CatalogPolicy,
    limits: Limits,
    path: ExecutionPath,
    runtime: &Arc<dyn RuntimeEffects>,
) -> Result<ProgramResult> {
    let mut binder = ProgramBinder::new(relational_names(program))?;
    let mut bindings = HashMap::<String, Vec<Env>>::new();
    let mut transitions = HashMap::new();
    let mut summaries = Vec::with_capacity(program.statements.len());
    let mut result = Datum::Null;

    run_statements(
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
    )
    .await?;
    Ok(ProgramResult {
        result,
        statements: summaries,
        plans: Vec::new(),
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
) -> Result<()> {
    let mut catalog_changed = false;
    let mut schema_changed = false;
    for statement in &program.statements {
        if let Some(binding) = statement.binder_statement() {
            let catalog = super::engine::ViewCatalog { view: &*view };
            let bound = match path {
                ExecutionPath::Production => binder.bind(&catalog, binding).await?,
                ExecutionPath::Reference => binder.bind_reference(&catalog, binding).await?,
            };
            let frames = run_relational(
                view,
                statement,
                &bound,
                bindings,
                limits,
                path,
                runtime.as_ref(),
            )
            .await?;
            let affected = frames.len();
            if result_name == Some(statement.name()) {
                *result = shape_frames(bound.result_cardinality, &bound.result_output, &frames)?;
            }
            bindings.insert(statement.name().to_owned(), frames);
            summaries.push(StatementResult {
                name: statement.name().to_owned(),
                affected,
                control: None,
            });
            continue;
        }

        let mut mutation = CatalogMutation::with_runtime(view, runtime.clone());
        let transition = apply_catalog(&mut mutation, statement, transitions, true).await?;
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

async fn run_relational(
    view: &mut dyn KvView,
    statement: &Statement,
    bound: &BoundStatement,
    bindings: &HashMap<String, Vec<Env>>,
    limits: Limits,
    path: ExecutionPath,
    runtime: &dyn RuntimeEffects,
) -> Result<Vec<Env>> {
    let input = match path {
        ExecutionPath::Production => {
            let plan = bound
                .plan
                .as_ref()
                .expect("production program binding has a physical plan");
            let mut executor = Executor::new(&*view, limits);
            executor.seed_bindings(bindings.clone());
            executor.run_frames(plan).await?
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
                .map(|default| resolve_default(default, column.scalar_type))
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

pub(crate) fn resolve_default(spec: DefaultSpec, scalar_type: ScalarType) -> Result<DefaultValue> {
    let mismatch = || {
        input(format!(
            "catalog: literal default is not compatible with {scalar_type:?}"
        ))
    };
    Ok(match (spec, scalar_type) {
        (DefaultSpec::Generator(DefaultFunction::Uuid), ScalarType::Text) => DefaultValue {
            function: Some(DefaultFunction::Uuid),
            ..DefaultValue::default()
        },
        (DefaultSpec::Generator(DefaultFunction::NowMs), ScalarType::Int64) => DefaultValue {
            function: Some(DefaultFunction::NowMs),
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
        FaultAction, FaultController, FaultRule, FaultingKv, Operation,
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
                DefaultSpec::Generator(DefaultFunction::Uuid),
                ScalarType::Text,
                DefaultValue {
                    function: Some(DefaultFunction::Uuid),
                    ..DefaultValue::default()
                },
            ),
            (
                DefaultSpec::Generator(DefaultFunction::NowMs),
                ScalarType::Int64,
                DefaultValue {
                    function: Some(DefaultFunction::NowMs),
                    ..DefaultValue::default()
                },
            ),
            (
                DefaultSpec::Text("rad".into()),
                ScalarType::Text,
                DefaultValue {
                    text: "rad".into(),
                    ..DefaultValue::default()
                },
            ),
            (
                DefaultSpec::Number("-17".into()),
                ScalarType::Int64,
                DefaultValue {
                    int64: -17,
                    ..DefaultValue::default()
                },
            ),
            (
                DefaultSpec::Number("1.25".into()),
                ScalarType::Float64,
                DefaultValue {
                    float64: 1.25,
                    ..DefaultValue::default()
                },
            ),
            (
                DefaultSpec::Bool(true),
                ScalarType::Bool,
                DefaultValue {
                    bool_value: true,
                    ..DefaultValue::default()
                },
            ),
        ];
        for (spec, scalar_type, expected) in cases {
            assert_eq!(DefaultSpec::from_catalog(&expected, scalar_type), spec);
            assert_eq!(resolve_default(spec, scalar_type).unwrap(), expected);
        }
    }

    #[test]
    fn unresolved_wire_defaults_reject_mismatched_and_invalid_values() {
        for (spec, scalar_type) in [
            (DefaultSpec::Text("1".into()), ScalarType::Int64),
            (DefaultSpec::Bool(true), ScalarType::Text),
            (DefaultSpec::Number("1".into()), ScalarType::Bool),
            (
                DefaultSpec::Generator(DefaultFunction::Uuid),
                ScalarType::Int64,
            ),
            (
                DefaultSpec::Generator(DefaultFunction::NowMs),
                ScalarType::Text,
            ),
            (
                DefaultSpec::Number("not-a-number".into()),
                ScalarType::Int64,
            ),
            (DefaultSpec::Number("NaN".into()), ScalarType::Float64),
            (DefaultSpec::Number("inf".into()), ScalarType::Float64),
        ] {
            assert!(resolve_default(spec, scalar_type).is_err());
        }
    }
}
