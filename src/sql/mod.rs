//! PostgreSQL syntax lowering into Rad's transport-neutral PIR and LIR.

use std::collections::HashMap;

use sqlparser::ast::{
    AlterTableOperation, AssignmentTarget, BinaryOperator, ColumnOption, ConflictTarget,
    CreateIndex, CreateTable, DataType, Delete, Distinct as SqlDistinct, DuplicateTreatment,
    Expr as SqlExpr, FromTable, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, Insert,
    JoinConstraint, JoinOperator, LimitClause, ObjectName, OnConflictAction, OnInsert, OrderByExpr,
    OrderByKind, Query as SqlQuery, Select, SelectItem, SelectItemQualifiedWildcardKind, SetExpr,
    SetOperator, SetQuantifier as SqlSetQuantifier, Statement as SqlStatement, TableConstraint,
    TableFactor, TableWithJoins, UnaryOperator, Update, UpdateTableFromKind, Value as SqlValue,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

use crate::engine::catalog::model::{
    ColumnDraft, DefaultFunction, DefaultValue, IndexDef, ScalarType, Table, TableDraft,
};
use crate::engine::exec::{Program, Statement};
use crate::engine::lir::{
    AggregateFunction, AggregateTerm, BinaryOp, BranchArm, Expr, GroupTerm, JoinKind, Kind,
    Literal, OrderTerm, ProjectField, Query, RawScalar, RecursiveAccumulation, Relation,
    RootCardinality, RowsColumn, SetQuantifier, TextComparison, TextMatchPart, UnaryOp,
};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sql: {0}")]
    Parse(#[from] sqlparser::parser::ParserError),
    #[error("sql: {0}")]
    Unsupported(String),
    #[error("sql: {0}")]
    Invalid(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResultColumn {
    pub name: String,
    pub field: String,
    pub scalar_type: ScalarType,
    pub nullable: bool,
    pub format: String,
}

impl ResultColumn {
    fn field(&self) -> &str {
        if self.field.is_empty() {
            &self.name
        } else {
            &self.field
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Parameter {
    pub scalar_type: ScalarType,
    pub value: RawScalar,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandKind {
    Select,
    Insert,
    Update,
    Delete,
    CreateTable,
    CreateIndex,
    AlterTable,
}

impl CommandKind {
    pub fn tag(self) -> &'static str {
        match self {
            Self::Select => "SELECT",
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
            Self::CreateTable => "CREATE TABLE",
            Self::CreateIndex => "CREATE INDEX",
            Self::AlterTable => "ALTER TABLE",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Prepared {
    statement: SqlStatement,
    parameter_types: Vec<ScalarType>,
    result_columns: Vec<ResultColumn>,
    kind: CommandKind,
}

impl Prepared {
    pub fn parameter_types(&self) -> &[ScalarType] {
        &self.parameter_types
    }

    pub fn result_columns(&self) -> &[ResultColumn] {
        &self.result_columns
    }

    pub fn kind(&self) -> CommandKind {
        self.kind
    }

    pub fn sql(&self) -> String {
        self.statement.to_string()
    }
}

#[derive(Debug)]
pub struct Compiled {
    pub program: Option<Program>,
    pub result_columns: Vec<ResultColumn>,
    pub kind: CommandKind,
}

pub fn parse(sql: &str) -> Result<Vec<SqlStatement>> {
    let sql = normalize_postgres_parser_input(sql);
    Ok(Parser::parse_sql(&PostgreSqlDialect {}, &sql)?)
}

fn normalize_postgres_parser_input(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut output = String::with_capacity(sql.len());
    let mut index = 0;
    let mut quoted = false;
    while index < bytes.len() {
        if bytes[index] == b'\'' {
            output.push('\'');
            index += 1;
            if quoted && index < bytes.len() && bytes[index] == b'\'' {
                output.push('\'');
                index += 1;
            } else {
                quoted = !quoted;
            }
            continue;
        }
        if !quoted && bytes[index] == b')' {
            output.push(')');
            index += 1;
            let whitespace = index;
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            if sql[index..]
                .get(..4)
                .is_some_and(|word| word.eq_ignore_ascii_case("sort"))
                && sql
                    .as_bytes()
                    .get(index + 4)
                    .is_none_or(|next| !next.is_ascii_alphanumeric() && *next != b'_')
            {
                output.push_str(&sql[whitespace..index]);
                output.push_str("AS \"sort\"");
                index += 4;
                continue;
            }
            output.push_str(&sql[whitespace..index]);
            continue;
        }
        let character = sql[index..]
            .chars()
            .next()
            .expect("index is within the string");
        output.push(character);
        index += character.len_utf8();
    }
    output
}

pub fn prepare(sql: &str, tables: &[Table], hints: &[Option<ScalarType>]) -> Result<Prepared> {
    let mut statements = parse(sql)?;
    if statements.len() != 1 {
        return Err(Error::Invalid(format!(
            "prepared execution needs exactly one statement, got {}",
            statements.len()
        )));
    }
    let statement = statements.remove(0);
    let draft = compile_statement(&statement, tables, hints, None)?;
    Ok(Prepared {
        statement,
        parameter_types: draft.parameter_types,
        result_columns: draft.result_columns,
        kind: draft.kind,
    })
}

pub fn compile(
    prepared: &Prepared,
    tables: &[Table],
    parameters: &[Parameter],
) -> Result<Compiled> {
    if parameters.len() != prepared.parameter_types.len() {
        return Err(Error::Invalid(format!(
            "expected {} parameters, got {}",
            prepared.parameter_types.len(),
            parameters.len()
        )));
    }
    for (index, (parameter, expected)) in
        parameters.iter().zip(&prepared.parameter_types).enumerate()
    {
        if parameter.scalar_type != *expected {
            return Err(Error::Invalid(format!(
                "parameter ${} is {:?}, expected {:?}",
                index + 1,
                parameter.scalar_type,
                expected
            )));
        }
    }
    let hints = prepared
        .parameter_types
        .iter()
        .copied()
        .map(Some)
        .collect::<Vec<_>>();
    let draft = compile_statement(&prepared.statement, tables, &hints, Some(parameters))?;
    Ok(Compiled {
        program: draft.program,
        result_columns: draft.result_columns,
        kind: draft.kind,
    })
}

pub fn compile_sql(sql: &str, tables: &[Table]) -> Result<Compiled> {
    let prepared = prepare(sql, tables, &[])?;
    compile(&prepared, tables, &[])
}

struct Draft {
    program: Option<Program>,
    parameter_types: Vec<ScalarType>,
    result_columns: Vec<ResultColumn>,
    kind: CommandKind,
}

fn compile_statement(
    statement: &SqlStatement,
    tables: &[Table],
    hints: &[Option<ScalarType>],
    parameters: Option<&[Parameter]>,
) -> Result<Draft> {
    let mut context = Context::new(tables, hints, parameters);
    let (program, result_columns, kind) = match statement {
        SqlStatement::Query(query) => {
            let (relation, columns) = context.select_query(query)?;
            let name = "sql_result".to_owned();
            (
                Some(Program {
                    statements: vec![Statement::Query {
                        name: name.clone(),
                        relation,
                    }],
                    result: Some(name),
                }),
                columns,
                CommandKind::Select,
            )
        }
        SqlStatement::Insert(insert) => {
            let (program, columns) = context.insert(insert)?;
            (Some(program), columns, CommandKind::Insert)
        }
        SqlStatement::Update(update) => {
            let (program, columns) = context.update(update)?;
            (Some(program), columns, CommandKind::Update)
        }
        SqlStatement::Delete(delete) => {
            let (program, columns) = context.delete(delete)?;
            (Some(program), columns, CommandKind::Delete)
        }
        SqlStatement::CreateTable(create) => (
            context.create_table(create)?,
            Vec::new(),
            CommandKind::CreateTable,
        ),
        SqlStatement::CreateIndex(create) => (
            context.create_index(create)?,
            Vec::new(),
            CommandKind::CreateIndex,
        ),
        SqlStatement::AlterTable(alter)
            if alter.operations.iter().all(|operation| {
                matches!(
                    operation,
                    AlterTableOperation::AddConstraint {
                        constraint: TableConstraint::ForeignKey(_),
                        ..
                    } | AlterTableOperation::DropConstraint { .. }
                )
            }) =>
        {
            (None, Vec::new(), CommandKind::AlterTable)
        }
        other => {
            return Err(Error::Unsupported(format!(
                "{} is not supported by the PostgreSQL frontend yet: {other}",
                statement_name(other),
            )));
        }
    };
    Ok(Draft {
        program,
        parameter_types: context.finish_parameter_types()?,
        result_columns,
        kind,
    })
}

fn statement_name(statement: &SqlStatement) -> &'static str {
    match statement {
        SqlStatement::Query(_) => "SELECT",
        SqlStatement::Insert(_) => "INSERT",
        SqlStatement::Update(_) => "UPDATE",
        SqlStatement::Delete(_) => "DELETE",
        SqlStatement::CreateTable(_) => "CREATE TABLE",
        SqlStatement::CreateIndex(_) => "CREATE INDEX",
        SqlStatement::AlterTable(_) => "ALTER TABLE",
        _ => "statement",
    }
}

struct Context<'a> {
    tables: &'a [Table],
    hints: &'a [Option<ScalarType>],
    parameters: Option<&'a [Parameter]>,
    inferred: Vec<Option<ScalarType>>,
    bindings: HashMap<String, Relation>,
    binding_columns: HashMap<String, Vec<ResultColumn>>,
    recursive_binding: Option<String>,
    next_scope: usize,
}

impl<'a> Context<'a> {
    fn new(
        tables: &'a [Table],
        hints: &'a [Option<ScalarType>],
        parameters: Option<&'a [Parameter]>,
    ) -> Self {
        Self {
            tables,
            hints,
            parameters,
            inferred: hints.to_vec(),
            bindings: HashMap::new(),
            binding_columns: HashMap::new(),
            recursive_binding: None,
            next_scope: 0,
        }
    }

    fn scope(&mut self, base: &str) -> String {
        self.next_scope += 1;
        let base = if base.is_empty() { "sql" } else { base };
        format!("{base}_{}", self.next_scope)
    }

    fn finish_parameter_types(self) -> Result<Vec<ScalarType>> {
        self.inferred
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                value.ok_or_else(|| {
                    Error::Invalid(format!("cannot infer the type of parameter ${}", index + 1))
                })
            })
            .collect()
    }

    fn table(&self, name: &ObjectName) -> Result<&'a Table> {
        let name = object_name(name)?;
        self.tables
            .iter()
            .find(|table| table.name == name)
            .ok_or_else(|| Error::Invalid(format!("unknown table {name:?}")))
    }

    fn select_query(&mut self, query: &SqlQuery) -> Result<(Query, Vec<ResultColumn>)> {
        let lowered = self.select_query_in(query, &ExpressionEnv::default())?;
        let columns = lowered.columns.clone();
        Ok((
            Query {
                root: lowered.relation,
                cardinality: if lowered.one {
                    RootCardinality::First
                } else {
                    RootCardinality::Many
                },
                bindings: self.bindings.clone(),
            },
            columns,
        ))
    }

    fn select_query_in(
        &mut self,
        query: &SqlQuery,
        parent: &ExpressionEnv,
    ) -> Result<LoweredQuery> {
        if !query.locks.is_empty() || query.fetch.is_some() {
            return Err(Error::Unsupported(
                "row locks and FETCH are not supported yet".into(),
            ));
        }
        if let Some(with) = &query.with {
            self.register_ctes(with)?;
        }
        match query.body.as_ref() {
            SetExpr::Select(select) => self.select(select, query, parent),
            SetExpr::Query(query) => self.select_query_in(query, parent),
            SetExpr::SetOperation { .. } => self.set_query(query, parent),
            other => Err(Error::Unsupported(format!(
                "query body {other} is not supported yet"
            ))),
        }
    }

    fn register_ctes(&mut self, with: &sqlparser::ast::With) -> Result<()> {
        for cte in &with.cte_tables {
            let name = identifier(&cte.alias.name);
            if self.binding_columns.contains_key(&name) {
                return Err(Error::Invalid(format!("WITH name {name:?} used twice")));
            }
            if with.recursive
                && let SetExpr::SetOperation {
                    left,
                    op: SetOperator::Union,
                    set_quantifier,
                    right,
                } = cte.query.body.as_ref()
                && set_expr_references(right, &name)
            {
                let mut anchor =
                    self.lower_set_expr(left, &ExpressionEnv::default(), &cte.query)?;
                if !cte.alias.columns.is_empty() {
                    anchor = self.rename_output(
                        anchor,
                        &cte.alias
                            .columns
                            .iter()
                            .map(|column| identifier(&column.name))
                            .collect::<Vec<_>>(),
                        &name,
                    )?;
                }
                self.binding_columns
                    .insert(name.clone(), anchor.columns.clone());
                self.recursive_binding = Some(name.clone());
                let step = self.lower_set_expr(right, &ExpressionEnv::default(), &cte.query);
                self.recursive_binding = None;
                let step = step?;
                if anchor.columns.len() != step.columns.len()
                    || anchor
                        .columns
                        .iter()
                        .zip(&step.columns)
                        .any(|(left, right)| left.scalar_type != right.scalar_type)
                {
                    return Err(Error::Invalid(format!(
                        "recursive CTE {name:?} anchor and step have incompatible columns"
                    )));
                }
                let step = self.align_set_branch(
                    step,
                    &anchor.columns,
                    &vec![false; anchor.columns.len()],
                );
                let accumulation = match set_quantifier {
                    SqlSetQuantifier::All => RecursiveAccumulation::All,
                    SqlSetQuantifier::None | SqlSetQuantifier::Distinct => {
                        RecursiveAccumulation::New
                    }
                    other => {
                        return Err(Error::Unsupported(format!(
                            "recursive UNION quantifier {other} is not supported"
                        )));
                    }
                };
                self.bindings.insert(
                    name,
                    Relation::Recursive {
                        anchor: Box::new(anchor.relation),
                        step: Box::new(step.relation),
                        accumulation,
                    },
                );
                continue;
            }
            let mut lowered = self.select_query_in(&cte.query, &ExpressionEnv::default())?;
            if !cte.alias.columns.is_empty() {
                lowered = self.rename_output(
                    lowered,
                    &cte.alias
                        .columns
                        .iter()
                        .map(|column| identifier(&column.name))
                        .collect::<Vec<_>>(),
                    &name,
                )?;
            }
            self.binding_columns
                .insert(name.clone(), lowered.columns.clone());
            self.bindings.insert(name, lowered.relation);
        }
        Ok(())
    }

    fn set_query(&mut self, query: &SqlQuery, parent: &ExpressionEnv) -> Result<LoweredQuery> {
        let mut lowered = self.lower_set_expr(query.body.as_ref(), parent, query)?;
        let mut expressions = ExpressionCompiler::new(self, ExpressionEnv::default())
            .with_output(lowered.scope.clone(), lowered.columns.clone());
        if let Some(order) = &query.order_by {
            let terms = match &order.kind {
                OrderByKind::Expressions(terms) => compile_order_terms(terms, |term| {
                    let expression = order_output_expression(&term.expr, &lowered.columns)?;
                    Ok(expressions.compile(&expression, None)?.expr)
                })?,
                OrderByKind::All(_) => {
                    return Err(Error::Unsupported("ORDER BY ALL is not supported".into()));
                }
            };
            lowered.relation = Relation::Order {
                input: Box::new(lowered.relation),
                terms,
            };
        } else if !lowered.one {
            lowered.relation = order_by_output(lowered.relation, &lowered.columns, &lowered.scope);
        }
        if let Some((offset, limit)) = limit(query.limit_clause.as_ref(), &mut expressions)? {
            lowered.one |= limit.is_some_and(|limit| limit <= 1);
            lowered.relation = Relation::Slice {
                input: Box::new(lowered.relation),
                offset,
                limit,
            };
        }
        Ok(lowered)
    }

    fn lower_set_expr(
        &mut self,
        expression: &SetExpr,
        parent: &ExpressionEnv,
        template: &SqlQuery,
    ) -> Result<LoweredQuery> {
        match expression {
            SetExpr::Select(select) => {
                let mut query = template.clone();
                query.with = None;
                query.order_by = None;
                query.limit_clause = None;
                self.select(select, &query, parent)
            }
            SetExpr::Query(query) => self.select_query_in(query, parent),
            SetExpr::SetOperation {
                left,
                op,
                set_quantifier,
                right,
            } => {
                let mut left = self.lower_set_expr(left, parent, template)?;
                let mut right = self.lower_set_expr(right, parent, template)?;
                if left.columns.len() != right.columns.len() {
                    return Err(Error::Invalid(format!(
                        "set-operation branches return {} and {} columns",
                        left.columns.len(),
                        right.columns.len()
                    )));
                }
                let mut target = left.columns.clone();
                let mut left_casts = vec![false; target.len()];
                let mut right_casts = vec![false; target.len()];
                for index in 0..target.len() {
                    match (
                        left.columns[index].scalar_type,
                        right.columns[index].scalar_type,
                    ) {
                        (left_type, right_type) if left_type == right_type => {}
                        (ScalarType::Int64, ScalarType::Float64) => {
                            target[index].scalar_type = ScalarType::Float64;
                            left_casts[index] = true;
                        }
                        (ScalarType::Float64, ScalarType::Int64) => {
                            target[index].scalar_type = ScalarType::Float64;
                            right_casts[index] = true;
                        }
                        (left_type, right_type) => {
                            return Err(Error::Invalid(format!(
                                "set-operation column {} has types {left_type:?} and {right_type:?}",
                                index + 1
                            )));
                        }
                    }
                    target[index].nullable =
                        left.columns[index].nullable || right.columns[index].nullable;
                }
                left = self.align_set_branch(left, &target, &left_casts);
                right = self.align_set_branch(right, &target, &right_casts);
                let scope = self.scope("set");
                let quantifier = match set_quantifier {
                    SqlSetQuantifier::All => SetQuantifier::All,
                    SqlSetQuantifier::None | SqlSetQuantifier::Distinct => SetQuantifier::Distinct,
                    other => {
                        return Err(Error::Unsupported(format!(
                            "set quantifier {other} is not supported"
                        )));
                    }
                };
                let relation = match op {
                    SetOperator::Union => {
                        let concatenate = Relation::Concatenate {
                            scope: scope.clone(),
                            inputs: vec![left.relation, right.relation],
                        };
                        if quantifier == SetQuantifier::Distinct {
                            Relation::Distinct(Box::new(concatenate))
                        } else {
                            concatenate
                        }
                    }
                    SetOperator::Intersect => Relation::Intersect {
                        scope: scope.clone(),
                        left: Box::new(left.relation),
                        right: Box::new(right.relation),
                        quantifier,
                    },
                    SetOperator::Except => Relation::Except {
                        scope: scope.clone(),
                        left: Box::new(left.relation),
                        right: Box::new(right.relation),
                        quantifier,
                    },
                    SetOperator::Minus => {
                        return Err(Error::Unsupported("MINUS is not PostgreSQL syntax".into()));
                    }
                };
                Ok(LoweredQuery {
                    relation,
                    columns: target,
                    scope,
                    one: false,
                })
            }
            other => Err(Error::Unsupported(format!(
                "set-operation branch {other} is not supported"
            ))),
        }
    }

    fn align_set_branch(
        &mut self,
        branch: LoweredQuery,
        target: &[ResultColumn],
        casts: &[bool],
    ) -> LoweredQuery {
        let scope = self.scope("set_branch");
        let fields = branch
            .columns
            .iter()
            .zip(target)
            .zip(casts)
            .map(|((column, target), cast)| {
                let expression = Expr::Column {
                    scope: branch.scope.clone(),
                    name: column.field().to_owned(),
                };
                ProjectField {
                    name: target.field().to_owned(),
                    expression: if *cast {
                        Expr::Cast {
                            expression: Box::new(expression),
                            to: Kind::Float64,
                        }
                    } else {
                        expression
                    },
                }
            })
            .collect();
        LoweredQuery {
            relation: Relation::Project {
                input: Box::new(branch.relation),
                scope: Some(scope.clone()),
                spread: Vec::new(),
                fields,
            },
            columns: target.to_vec(),
            scope,
            one: branch.one,
        }
    }

    fn rename_output(
        &mut self,
        lowered: LoweredQuery,
        names: &[String],
        base: &str,
    ) -> Result<LoweredQuery> {
        if names.len() != lowered.columns.len() {
            return Err(Error::Invalid(format!(
                "{base} names {} columns for a {}-column query",
                names.len(),
                lowered.columns.len()
            )));
        }
        let scope = self.scope(base);
        let mut columns = lowered.columns.clone();
        let fields = columns
            .iter_mut()
            .zip(names)
            .map(|(column, name)| {
                let field = ProjectField {
                    name: name.clone(),
                    expression: Expr::Column {
                        scope: lowered.scope.clone(),
                        name: column.field().to_owned(),
                    },
                };
                column.name = name.clone();
                column.field = name.clone();
                field
            })
            .collect();
        Ok(LoweredQuery {
            relation: Relation::Project {
                input: Box::new(lowered.relation),
                scope: Some(scope.clone()),
                spread: Vec::new(),
                fields,
            },
            columns,
            scope,
            one: lowered.one,
        })
    }

    fn select(
        &mut self,
        select: &Select,
        query_ast: &SqlQuery,
        parent: &ExpressionEnv,
    ) -> Result<LoweredQuery> {
        let grouped = match &select.group_by {
            sqlparser::ast::GroupByExpr::Expressions(values, modifiers) => {
                !values.is_empty() || !modifiers.is_empty()
            }
            sqlparser::ast::GroupByExpr::All(_) => true,
        };
        let aggregates = select.projection.iter().any(|item| match item {
            SelectItem::UnnamedExpr(expression)
            | SelectItem::ExprWithAlias {
                expr: expression, ..
            } => {
                let mut aggregates = Vec::new();
                collect_aggregate_expressions(expression, &mut aggregates);
                !aggregates.is_empty()
            }
            _ => false,
        });
        if grouped || aggregates || select.having.is_some() {
            return self.aggregate_select(select, query_ast, parent);
        }

        let source = self.select_source(select, parent)?;
        let distinct = select_distinct(select)?;
        let mut one = select.from.is_empty();
        let mut relation = source.relation;
        let output_scope = self.scope("result");
        let mut expressions = ExpressionCompiler::new(self, source.env.clone());
        if let Some(predicate) = &select.selection {
            let predicate = expressions.compile(predicate, Some(ScalarType::Bool))?;
            relation = Relation::Filter {
                input: Box::new(relation),
                predicate: predicate.expr,
            };
        }

        let order_terms = query_ast
            .order_by
            .as_ref()
            .map(|order| match &order.kind {
                OrderByKind::Expressions(terms) => compile_order_terms(terms, |term| {
                    let expression = aggregate_order_expression(&term.expr, &select.projection)?;
                    Ok(expressions.compile(&expression, None)?.expr)
                }),
                OrderByKind::All(_) => {
                    Err(Error::Unsupported("ORDER BY ALL is not supported".into()))
                }
            })
            .transpose()?
            .unwrap_or_else(|| source.default_order.clone());
        let slice = limit(query_ast.limit_clause.as_ref(), &mut expressions)?;
        if let Some((_, limit)) = slice {
            one |= limit.is_some_and(|limit| limit <= 1);
        }
        let (spread, fields, columns) = projection(&select.projection, &mut expressions)?;
        drop(expressions);
        if distinct {
            relation = Relation::Project {
                input: Box::new(relation),
                scope: Some(output_scope.clone()),
                spread,
                fields,
            };
            relation = Relation::Distinct(Box::new(relation));
            let mut output_expressions = ExpressionCompiler::new(self, ExpressionEnv::default())
                .with_output(output_scope.clone(), columns.clone());
            let terms = query_ast
                .order_by
                .as_ref()
                .map(|order| match &order.kind {
                    OrderByKind::Expressions(terms) => compile_order_terms(terms, |term| {
                        let expression =
                            projection_order_expression(&term.expr, &select.projection, &columns);
                        Ok(output_expressions.compile(&expression, None)?.expr)
                    }),
                    OrderByKind::All(_) => {
                        Err(Error::Unsupported("ORDER BY ALL is not supported".into()))
                    }
                })
                .transpose()?
                .unwrap_or_else(|| output_order_terms(&columns, &output_scope));
            relation = Relation::Order {
                input: Box::new(relation),
                terms,
            };
            if let Some((offset, limit)) = slice {
                relation = Relation::Slice {
                    input: Box::new(relation),
                    offset,
                    limit,
                };
            }
        } else if order_terms.is_empty() {
            relation = Relation::Project {
                input: Box::new(relation),
                scope: Some(output_scope.clone()),
                spread,
                fields,
            };
            relation = order_by_output(relation, &columns, &output_scope);
            if let Some((offset, limit)) = slice {
                relation = Relation::Slice {
                    input: Box::new(relation),
                    offset,
                    limit,
                };
            }
        } else {
            relation = Relation::Order {
                input: Box::new(relation),
                terms: order_terms,
            };
            if let Some((offset, limit)) = slice {
                relation = Relation::Slice {
                    input: Box::new(relation),
                    offset,
                    limit,
                };
            }
            relation = Relation::Project {
                input: Box::new(relation),
                scope: Some(output_scope.clone()),
                spread,
                fields,
            };
        }
        Ok(LoweredQuery {
            relation,
            columns,
            scope: output_scope,
            one,
        })
    }

    fn aggregate_select(
        &mut self,
        select: &Select,
        query_ast: &SqlQuery,
        parent: &ExpressionEnv,
    ) -> Result<LoweredQuery> {
        let source = self.select_source(select, parent)?;
        let mut relation = source.relation;
        let aggregate_scope = self.scope("aggregate");
        let output_scope = self.scope("result");
        let mut expressions = ExpressionCompiler::new(self, source.env.clone());
        if let Some(predicate) = &select.selection {
            relation = Relation::Filter {
                input: Box::new(relation),
                predicate: expressions.compile(predicate, Some(ScalarType::Bool))?.expr,
            };
        }
        let group_exprs = match &select.group_by {
            sqlparser::ast::GroupByExpr::Expressions(values, modifiers) if modifiers.is_empty() => {
                values
            }
            sqlparser::ast::GroupByExpr::Expressions(_, _) => {
                return Err(Error::Unsupported(
                    "GROUP BY modifiers are not supported".into(),
                ));
            }
            sqlparser::ast::GroupByExpr::All(_) => {
                return Err(Error::Unsupported("GROUP BY ALL is not supported".into()));
            }
        };
        let group_exprs = group_exprs
            .iter()
            .map(|expression| aggregate_order_expression(expression, &select.projection))
            .collect::<Result<Vec<_>>>()?;
        let mut groups = Vec::with_capacity(group_exprs.len());
        let mut aggregate_columns = Vec::new();
        let mut substitutions = Vec::new();
        for (index, expression) in group_exprs.iter().enumerate() {
            let compiled = expressions.compile(expression, None)?;
            let field = format!("group_{}", index + 1);
            groups.push(GroupTerm {
                name: field.clone(),
                expression: compiled.expr,
            });
            aggregate_columns.push(ResultColumn {
                name: expression_name(expression),
                field,
                scalar_type: compiled.scalar_type,
                nullable: compiled.nullable,
                format: direct_column_format(expression, &source.env),
            });
            substitutions.push((
                expression.clone(),
                aggregate_columns
                    .last()
                    .expect("group column was just added")
                    .clone(),
            ));
        }
        let mut aggregate_expressions = Vec::new();
        for item in &select.projection {
            match item {
                SelectItem::UnnamedExpr(expression)
                | SelectItem::ExprWithAlias {
                    expr: expression, ..
                } => collect_aggregate_expressions(expression, &mut aggregate_expressions),
                _ => {
                    return Err(Error::Unsupported(
                        "wildcard projection is not valid in an aggregate query".into(),
                    ));
                }
            }
        }
        if let Some(having) = &select.having {
            collect_aggregate_expressions(having, &mut aggregate_expressions);
        }
        if let Some(order) = &query_ast.order_by
            && let OrderByKind::Expressions(terms) = &order.kind
        {
            for term in terms {
                collect_aggregate_expressions(&term.expr, &mut aggregate_expressions);
            }
        }
        let mut term_specs = Vec::new();
        for expression in aggregate_expressions {
            let SqlExpr::Function(function_ast) = &expression else {
                unreachable!("aggregate expression collector only returns functions")
            };
            let (function, argument, scalar_type, nullable, distinct) =
                aggregate(function_ast, &mut expressions)?;
            let field = format!("aggregate_{}", term_specs.len() + 1);
            term_specs.push((
                AggregateTerm {
                    function,
                    argument,
                    name: field.clone(),
                },
                distinct,
            ));
            let column = ResultColumn {
                name: expression.to_string(),
                field,
                scalar_type,
                nullable,
                format: String::new(),
            };
            aggregate_columns.push(column.clone());
            substitutions.push((expression, column));
        }
        drop(expressions);
        let mut term_groups = Vec::<(Option<Expr>, Vec<usize>)>::new();
        for (index, (term, distinct)) in term_specs.iter().enumerate() {
            let key = distinct.then(|| {
                term.argument
                    .clone()
                    .expect("DISTINCT aggregate has an argument")
            });
            if let Some((_, indexes)) = term_groups
                .iter_mut()
                .find(|(candidate, _)| *candidate == key)
            {
                indexes.push(index);
            } else {
                term_groups.push((key, vec![index]));
            }
        }
        if term_groups.is_empty() {
            term_groups.push((None, Vec::new()));
        }
        let multiple_branches = term_groups.len() > 1;
        let mut branches = Vec::with_capacity(term_groups.len());
        let mut term_scopes = vec![String::new(); term_specs.len()];
        for (distinct_argument, indexes) in term_groups {
            let (branch_input, replacements) = self.fresh_relation(&relation);
            let branch_scope = if multiple_branches {
                self.scope("aggregate_branch")
            } else {
                aggregate_scope.clone()
            };
            let (input, branch_groups, branch_terms) =
                if let Some(mut distinct_argument) = distinct_argument {
                    remap_expr_scopes(&mut distinct_argument, &replacements);
                    let distinct_scope = self.scope("aggregate_distinct");
                    let mut fields = groups
                        .iter()
                        .map(|group| ProjectField {
                            name: group.name.clone(),
                            expression: {
                                let mut expression = group.expression.clone();
                                remap_expr_scopes(&mut expression, &replacements);
                                expression
                            },
                        })
                        .collect::<Vec<_>>();
                    fields.push(ProjectField {
                        name: "distinct_value".into(),
                        expression: distinct_argument,
                    });
                    let input = Relation::Distinct(Box::new(Relation::Project {
                        input: Box::new(branch_input),
                        scope: Some(distinct_scope.clone()),
                        spread: Vec::new(),
                        fields,
                    }));
                    let branch_groups = groups
                        .iter()
                        .map(|group| GroupTerm {
                            name: group.name.clone(),
                            expression: Expr::Column {
                                scope: distinct_scope.clone(),
                                name: group.name.clone(),
                            },
                        })
                        .collect();
                    let branch_terms = indexes
                        .iter()
                        .map(|index| AggregateTerm {
                            argument: Some(Expr::Column {
                                scope: distinct_scope.clone(),
                                name: "distinct_value".into(),
                            }),
                            ..term_specs[*index].0.clone()
                        })
                        .collect();
                    (input, branch_groups, branch_terms)
                } else {
                    let branch_terms = indexes
                        .iter()
                        .map(|index| {
                            let mut term = term_specs[*index].0.clone();
                            if let Some(argument) = &mut term.argument {
                                remap_expr_scopes(argument, &replacements);
                            }
                            term
                        })
                        .collect();
                    let branch_groups = groups
                        .iter()
                        .cloned()
                        .map(|mut group| {
                            remap_expr_scopes(&mut group.expression, &replacements);
                            group
                        })
                        .collect();
                    (branch_input, branch_groups, branch_terms)
                };
            for index in indexes {
                term_scopes[index] = branch_scope.clone();
            }
            branches.push((
                Relation::Aggregate {
                    input: Box::new(input),
                    scope: Some(branch_scope.clone()),
                    groups: branch_groups,
                    terms: branch_terms,
                },
                branch_scope,
            ));
        }
        let (mut combined, first_scope) = branches.remove(0);
        for (branch, scope) in branches {
            let on = if groups.is_empty() {
                bool_literal(true)
            } else {
                fold_binary(
                    groups
                        .iter()
                        .map(|group| null_safe_equality(&first_scope, &scope, &group.name))
                        .collect(),
                    BinaryOp::And,
                )?
            };
            combined = Relation::Join {
                left: Box::new(combined),
                right: Box::new(branch),
                kind: JoinKind::Inner,
                on,
            };
        }
        relation =
            if multiple_branches {
                let mut fields = groups
                    .iter()
                    .map(|group| ProjectField {
                        name: group.name.clone(),
                        expression: Expr::Column {
                            scope: first_scope.clone(),
                            name: group.name.clone(),
                        },
                    })
                    .collect::<Vec<_>>();
                fields.extend(term_specs.iter().enumerate().map(|(index, (term, _))| {
                    ProjectField {
                        name: term.name.clone(),
                        expression: Expr::Column {
                            scope: term_scopes[index].clone(),
                            name: term.name.clone(),
                        },
                    }
                }));
                Relation::Project {
                    input: Box::new(combined),
                    scope: Some(aggregate_scope.clone()),
                    spread: Vec::new(),
                    fields,
                }
            } else {
                combined
            };
        let mut aggregate_expressions = ExpressionCompiler::new(self, ExpressionEnv::default())
            .with_output(aggregate_scope.clone(), aggregate_columns.clone())
            .with_substitutions(substitutions);
        if let Some(having) = &select.having {
            relation = Relation::Filter {
                input: Box::new(relation),
                predicate: aggregate_expressions
                    .compile(having, Some(ScalarType::Bool))?
                    .expr,
            };
        }
        let mut columns = Vec::with_capacity(select.projection.len());
        let mut output_sources = Vec::with_capacity(select.projection.len());
        let mut output_names = HashMap::<String, usize>::new();
        for item in &select.projection {
            let expression = expression_ast(item);
            let compiled = aggregate_expressions.compile(expression, None)?;
            let name = select_item_name(item);
            let field = unique_name(&name, &mut output_names);
            let format = group_exprs
                .iter()
                .position(|group| group == expression)
                .map(|index| aggregate_columns[index].format.clone())
                .unwrap_or_default();
            columns.push(ResultColumn {
                name,
                field,
                scalar_type: compiled.scalar_type,
                nullable: compiled.nullable,
                format,
            });
            output_sources.push(compiled.expr);
        }
        let distinct = select_distinct(select)?;
        if !distinct {
            let order_terms = query_ast
                .order_by
                .as_ref()
                .map(|order| match &order.kind {
                    OrderByKind::Expressions(terms) => compile_order_terms(terms, |term| {
                        let expression =
                            aggregate_order_expression(&term.expr, &select.projection)?;
                        Ok(aggregate_expressions.compile(&expression, None)?.expr)
                    }),
                    OrderByKind::All(_) => {
                        Err(Error::Unsupported("ORDER BY ALL is not supported".into()))
                    }
                })
                .transpose()?
                .unwrap_or_else(|| {
                    output_sources
                        .iter()
                        .cloned()
                        .map(|expression| OrderTerm {
                            expression,
                            descending: false,
                        })
                        .collect()
                });
            relation = Relation::Order {
                input: Box::new(relation),
                terms: order_terms,
            };
        }
        let mut one = group_exprs.is_empty();
        let slice = limit(query_ast.limit_clause.as_ref(), &mut aggregate_expressions)?;
        if let Some((_, limit)) = slice {
            one |= limit.is_some_and(|limit| limit <= 1);
        }
        if !distinct && let Some((offset, limit)) = slice {
            relation = Relation::Slice {
                input: Box::new(relation),
                offset,
                limit,
            };
        }
        let mut output_fields = Vec::with_capacity(columns.len());
        for (column, expression) in columns.iter().zip(output_sources) {
            output_fields.push(ProjectField {
                name: column.field().to_owned(),
                expression,
            });
        }
        relation = Relation::Project {
            input: Box::new(relation),
            scope: Some(output_scope.clone()),
            spread: Vec::new(),
            fields: output_fields,
        };
        drop(aggregate_expressions);
        if distinct {
            relation = Relation::Distinct(Box::new(relation));
            let mut output_expressions = ExpressionCompiler::new(self, ExpressionEnv::default())
                .with_output(output_scope.clone(), columns.clone());
            let terms = query_ast
                .order_by
                .as_ref()
                .map(|order| match &order.kind {
                    OrderByKind::Expressions(terms) => compile_order_terms(terms, |term| {
                        let expression =
                            projection_order_expression(&term.expr, &select.projection, &columns);
                        Ok(output_expressions.compile(&expression, None)?.expr)
                    }),
                    OrderByKind::All(_) => {
                        Err(Error::Unsupported("ORDER BY ALL is not supported".into()))
                    }
                })
                .transpose()?
                .unwrap_or_else(|| output_order_terms(&columns, &output_scope));
            relation = Relation::Order {
                input: Box::new(relation),
                terms,
            };
            if let Some((offset, limit)) = slice {
                relation = Relation::Slice {
                    input: Box::new(relation),
                    offset,
                    limit,
                };
            }
        }
        Ok(LoweredQuery {
            relation,
            columns,
            scope: output_scope,
            one,
        })
    }

    fn select_source(&mut self, select: &Select, parent: &ExpressionEnv) -> Result<SelectSource> {
        if select.from.is_empty() {
            let scope = self.scope("constant");
            return Ok(SelectSource {
                relation: Relation::Rows {
                    scope: scope.clone(),
                    columns: vec![RowsColumn {
                        name: "_one".into(),
                        kind: Kind::Int64,
                        nullable: false,
                    }],
                    values: vec![vec![RawScalar::Number("1".into())]],
                },
                env: ExpressionEnv::local(
                    vec![ScopeDef {
                        alias: "constant".into(),
                        label: scope.clone(),
                        columns: vec![ResultColumn {
                            name: "_one".into(),
                            field: "_one".into(),
                            scalar_type: ScalarType::Int64,
                            nullable: false,
                            format: String::new(),
                        }],
                        primary_key: Vec::new(),
                    }],
                    parent,
                ),
                default_order: vec![OrderTerm {
                    expression: Expr::Column {
                        scope,
                        name: "_one".into(),
                    },
                    descending: false,
                }],
            });
        }

        self.compile_from_tables(&select.from, parent)
    }

    fn compile_from_tables(
        &mut self,
        tables: &[TableWithJoins],
        parent: &ExpressionEnv,
    ) -> Result<SelectSource> {
        let mut relation = None;
        let mut scopes = Vec::new();
        for from in tables {
            let (mut item, scope) = self.table_factor(&from.relation, parent)?;
            if let Some(left) = relation.take() {
                item = Relation::Join {
                    left: Box::new(left),
                    right: Box::new(item),
                    kind: JoinKind::Inner,
                    on: bool_literal(true),
                };
            }
            scopes.push(scope);
            relation = Some(item);

            for join in &from.joins {
                let (right, mut right_scope) = self.table_factor(&join.relation, parent)?;
                let mut combined = scopes.clone();
                combined.push(right_scope.clone());
                let env = ExpressionEnv::local(combined, parent);
                let (kind, swap, constraint) = match &join.join_operator {
                    JoinOperator::Join(constraint) | JoinOperator::Inner(constraint) => {
                        (JoinKind::Inner, false, constraint)
                    }
                    JoinOperator::Left(constraint) | JoinOperator::LeftOuter(constraint) => {
                        for column in &mut right_scope.columns {
                            column.nullable = true;
                        }
                        (JoinKind::Left, false, constraint)
                    }
                    JoinOperator::Right(constraint) | JoinOperator::RightOuter(constraint) => {
                        for scope in &mut scopes {
                            for column in &mut scope.columns {
                                column.nullable = true;
                            }
                        }
                        (JoinKind::Left, true, constraint)
                    }
                    JoinOperator::CrossJoin(constraint) => (JoinKind::Inner, false, constraint),
                    JoinOperator::FullOuter(_) => {
                        return Err(Error::Unsupported(
                            "FULL OUTER JOIN needs a Rad relational primitive".into(),
                        ));
                    }
                    other => {
                        return Err(Error::Unsupported(format!(
                            "join operator {other:?} is not supported"
                        )));
                    }
                };
                let on = self.join_constraint(constraint, &env, &scopes, &right_scope)?;
                let left = relation.take().expect("FROM relation exists before JOIN");
                relation = Some(if swap {
                    Relation::Join {
                        left: Box::new(right),
                        right: Box::new(left),
                        kind,
                        on,
                    }
                } else {
                    Relation::Join {
                        left: Box::new(left),
                        right: Box::new(right),
                        kind,
                        on,
                    }
                });
                scopes.push(right_scope);
            }
        }
        let default_order = scopes
            .iter()
            .flat_map(|scope| {
                scope.primary_key.iter().map(|name| OrderTerm {
                    expression: Expr::Column {
                        scope: scope.label.clone(),
                        name: name.clone(),
                    },
                    descending: false,
                })
            })
            .collect();
        Ok(SelectSource {
            relation: relation.expect("non-empty FROM produces a relation"),
            env: ExpressionEnv::local(scopes, parent),
            default_order,
        })
    }

    fn table_factor(
        &mut self,
        factor: &TableFactor,
        parent: &ExpressionEnv,
    ) -> Result<(Relation, ScopeDef)> {
        match factor {
            TableFactor::Table {
                name, alias, args, ..
            } => {
                if args.is_some() {
                    return Err(Error::Unsupported(
                        "table functions are not supported yet".into(),
                    ));
                }
                let name = object_name(name)?;
                if let Some(columns) = self.binding_columns.get(&name).cloned() {
                    let alias_name = alias
                        .as_ref()
                        .map(|alias| identifier(&alias.name))
                        .unwrap_or_else(|| name.clone());
                    let label = self.scope(&alias_name);
                    let mut scope = ScopeDef {
                        alias: alias_name,
                        label: label.clone(),
                        columns,
                        primary_key: Vec::new(),
                    };
                    apply_column_aliases(&mut scope, alias.as_ref())?;
                    let relation = if self.recursive_binding.as_deref() == Some(&name) {
                        Relation::RecursiveRef {
                            binding: name,
                            scope: label,
                        }
                    } else {
                        Relation::Ref {
                            binding: name,
                            scope: label,
                        }
                    };
                    return Ok((relation, scope));
                }
                let table = self
                    .tables
                    .iter()
                    .find(|table| table.name == name)
                    .cloned()
                    .ok_or_else(|| Error::Invalid(format!("unknown table {name:?}")))?;
                let alias_name = alias
                    .as_ref()
                    .map(|alias| identifier(&alias.name))
                    .unwrap_or_else(|| table.name.clone());
                let label = self.scope(&alias_name);
                let mut scope = ScopeDef::table(&table, alias_name, label.clone());
                apply_column_aliases(&mut scope, alias.as_ref())?;
                Ok((
                    Relation::Scan {
                        table: table.name.clone(),
                        scope: label,
                    },
                    scope,
                ))
            }
            TableFactor::Derived {
                lateral,
                subquery,
                alias,
                sample,
            } => {
                if sample.is_some() {
                    return Err(Error::Unsupported("TABLESAMPLE is not supported".into()));
                }
                let alias = alias.as_ref().ok_or_else(|| {
                    Error::Invalid("PostgreSQL derived tables need an alias".into())
                })?;
                let empty = ExpressionEnv::default();
                let lowered =
                    self.select_query_in(subquery, if *lateral { parent } else { &empty })?;
                let alias_name = identifier(&alias.name);
                let mut scope = ScopeDef {
                    alias: alias_name,
                    label: lowered.scope,
                    columns: lowered.columns,
                    primary_key: Vec::new(),
                };
                apply_column_aliases(&mut scope, Some(alias))?;
                Ok((lowered.relation, scope))
            }
            other => Err(Error::Unsupported(format!(
                "FROM item {other} is not supported yet"
            ))),
        }
    }

    fn join_constraint(
        &mut self,
        constraint: &JoinConstraint,
        env: &ExpressionEnv,
        left: &[ScopeDef],
        right: &ScopeDef,
    ) -> Result<Expr> {
        match constraint {
            JoinConstraint::On(expression) => ExpressionCompiler::new(self, env.clone())
                .compile(expression, Some(ScalarType::Bool))
                .map(|compiled| compiled.expr),
            JoinConstraint::None => Ok(bool_literal(true)),
            JoinConstraint::Using(names) => {
                let mut predicates = Vec::new();
                for name in names {
                    let name = object_name(name)?;
                    let left_scope = left
                        .iter()
                        .rev()
                        .find(|scope| scope.column(&name).is_some())
                        .ok_or_else(|| {
                            Error::Invalid(format!("unknown JOIN USING column {name:?}"))
                        })?;
                    if right.column(&name).is_none() {
                        return Err(Error::Invalid(format!(
                            "unknown JOIN USING column {name:?}"
                        )));
                    }
                    predicates.push(Expr::Binary {
                        op: BinaryOp::Eq,
                        left: Box::new(Expr::Column {
                            scope: left_scope.label.clone(),
                            name: left_scope
                                .column(&name)
                                .expect("USING column exists")
                                .field()
                                .to_owned(),
                        }),
                        right: Box::new(Expr::Column {
                            scope: right.label.clone(),
                            name: right
                                .column(&name)
                                .expect("USING column exists")
                                .field()
                                .to_owned(),
                        }),
                    });
                }
                fold_binary(predicates, BinaryOp::And)
            }
            JoinConstraint::Natural => {
                let names = right
                    .columns
                    .iter()
                    .filter(|column| {
                        left.iter()
                            .any(|scope| scope.column(&column.name).is_some())
                    })
                    .map(|column| column.name.clone())
                    .collect::<Vec<_>>();
                if names.is_empty() {
                    return Ok(bool_literal(true));
                }
                let mut predicates = Vec::new();
                for name in names {
                    let left_scope = left
                        .iter()
                        .rev()
                        .find(|scope| scope.column(&name).is_some())
                        .expect("natural join name comes from left scope");
                    predicates.push(Expr::Binary {
                        op: BinaryOp::Eq,
                        left: Box::new(Expr::Column {
                            scope: left_scope.label.clone(),
                            name: left_scope
                                .column(&name)
                                .expect("natural column exists")
                                .field()
                                .to_owned(),
                        }),
                        right: Box::new(Expr::Column {
                            scope: right.label.clone(),
                            name: right
                                .column(&name)
                                .expect("natural column exists")
                                .field()
                                .to_owned(),
                        }),
                    });
                }
                fold_binary(predicates, BinaryOp::And)
            }
        }
    }

    fn insert(&mut self, insert: &Insert) -> Result<(Program, Vec<ResultColumn>)> {
        let sqlparser::ast::TableObject::TableName(name) = &insert.table else {
            return Err(Error::Unsupported(
                "INSERT table functions are not supported".into(),
            ));
        };
        let table = self.table(name)?.clone();
        let source = insert
            .source
            .as_ref()
            .ok_or_else(|| Error::Invalid("INSERT needs a VALUES or SELECT source".into()))?;
        let columns = if insert.columns.is_empty() {
            table
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect()
        } else {
            insert
                .columns
                .iter()
                .map(object_name)
                .collect::<Result<Vec<_>>>()?
        };
        for name in &columns {
            if table.column(name).is_none() {
                return Err(Error::Invalid(format!("unknown column {name:?}")));
            }
        }
        let (relation, row_scope) = self.insert_source(&table, &columns, source)?;
        if let Some(on) = &insert.on {
            return self.insert_on_conflict(insert, &table, &columns, relation, row_scope, on);
        }
        let relation = order_by_names(relation, &columns, &row_scope.label);
        self.mutation_program(
            Statement::Create {
                name: "sql_mutation".into(),
                relation: self.query(relation),
                table: table.name.clone(),
            },
            &table,
            insert.returning.as_deref(),
        )
    }

    fn insert_source(
        &mut self,
        table: &Table,
        columns: &[String],
        source: &SqlQuery,
    ) -> Result<(Relation, ScopeDef)> {
        match source.body.as_ref() {
            SetExpr::Values(values) => {
                let scope = self.scope("insert_values");
                let mut rows = Vec::with_capacity(values.rows.len());
                for row in &values.rows {
                    if row.len() != columns.len() {
                        return Err(Error::Invalid(format!(
                            "INSERT row has {} values for {} columns",
                            row.len(),
                            columns.len()
                        )));
                    }
                    let mut output = Vec::with_capacity(row.len());
                    for (expression, name) in row.iter().zip(columns) {
                        let column = table.column(name).expect("insert column exists");
                        output.push(self.raw_value(expression, column.scalar_type)?);
                    }
                    rows.push(output);
                }
                let scope_def = ScopeDef {
                    alias: "excluded".into(),
                    label: scope.clone(),
                    columns: columns
                        .iter()
                        .map(|name| {
                            result_column(table.column(name).expect("insert column exists"))
                        })
                        .collect(),
                    primary_key: columns
                        .iter()
                        .filter(|name| table.primary_key.contains(name))
                        .cloned()
                        .collect(),
                };
                Ok((
                    Relation::Rows {
                        scope,
                        columns: columns
                            .iter()
                            .map(|name| {
                                let column = table.column(name).expect("insert column exists");
                                RowsColumn {
                                    name: name.clone(),
                                    kind: Kind::of(column.scalar_type),
                                    nullable: column.nullable,
                                }
                            })
                            .collect(),
                        values: rows,
                    },
                    scope_def,
                ))
            }
            _ => {
                let lowered = self.select_query_in(source, &ExpressionEnv::default())?;
                if lowered.columns.len() != columns.len() {
                    return Err(Error::Invalid(format!(
                        "INSERT has {} target columns but query returns {}",
                        columns.len(),
                        lowered.columns.len()
                    )));
                }
                let scope = self.scope("insert_select");
                let mut output = Vec::with_capacity(columns.len());
                let mut fields = Vec::with_capacity(columns.len());
                for ((source, name), target) in lowered
                    .columns
                    .iter()
                    .zip(columns)
                    .zip(columns.iter().map(|name| table.column(name).unwrap()))
                {
                    if source.scalar_type != target.scalar_type {
                        return Err(Error::Invalid(format!(
                            "INSERT column {name:?} is {:?}, query returns {:?}",
                            target.scalar_type, source.scalar_type
                        )));
                    }
                    fields.push(ProjectField {
                        name: name.clone(),
                        expression: Expr::Column {
                            scope: lowered.scope.clone(),
                            name: source.field().to_owned(),
                        },
                    });
                    output.push(result_column(target));
                }
                Ok((
                    Relation::Project {
                        input: Box::new(lowered.relation),
                        scope: Some(scope.clone()),
                        spread: Vec::new(),
                        fields,
                    },
                    ScopeDef {
                        alias: "excluded".into(),
                        label: scope,
                        columns: output,
                        primary_key: columns
                            .iter()
                            .filter(|name| table.primary_key.contains(name))
                            .cloned()
                            .collect(),
                    },
                ))
            }
        }
    }

    fn insert_on_conflict(
        &mut self,
        insert: &Insert,
        table: &Table,
        columns: &[String],
        rows: Relation,
        row_scope: ScopeDef,
        on: &OnInsert,
    ) -> Result<(Program, Vec<ResultColumn>)> {
        let OnInsert::OnConflict(conflict) = on else {
            return Err(Error::Unsupported(
                "ON DUPLICATE KEY UPDATE is not PostgreSQL syntax".into(),
            ));
        };
        let targets = conflict_targets(table, conflict.conflict_target.as_ref())?;
        let exists = self.conflict_exists(table, &row_scope, &targets)?;
        let missing = Relation::Filter {
            input: Box::new(rows.clone()),
            predicate: Expr::Unary {
                op: UnaryOp::Not,
                expression: Box::new(exists),
            },
        };
        let missing = order_by_names(missing, columns, &row_scope.label);
        let create = Statement::Create {
            name: "sql_upsert_create".into(),
            relation: self.query(missing),
            table: table.name.clone(),
        };
        if matches!(conflict.action, OnConflictAction::DoNothing) {
            return self.mutation_program(create, table, insert.returning.as_deref());
        }
        let OnConflictAction::DoUpdate(update) = &conflict.action else {
            unreachable!("ON CONFLICT has two actions")
        };
        if targets.len() != 1 {
            return Err(Error::Invalid(
                "ON CONFLICT DO UPDATE needs one conflict target".into(),
            ));
        }
        let target_label = self.scope(&table.name);
        let target_alias = insert
            .table_alias
            .as_ref()
            .map(|alias| identifier(&alias.alias))
            .unwrap_or_else(|| table.name.clone());
        let target_scope = ScopeDef::table(table, target_alias, target_label.clone());
        let on = conflict_join_predicate(&target_scope, &row_scope, &targets[0])?;
        let mut matched = Relation::Join {
            left: Box::new(rows.clone()),
            right: Box::new(Relation::Scan {
                table: table.name.clone(),
                scope: target_label.clone(),
            }),
            kind: JoinKind::Inner,
            on,
        };
        let env = ExpressionEnv::local(
            vec![row_scope.clone(), target_scope.clone()],
            &ExpressionEnv::default(),
        );
        let mut expressions = ExpressionCompiler::new(self, env);
        if let Some(predicate) = &update.selection {
            matched = Relation::Filter {
                input: Box::new(matched),
                predicate: expressions.compile(predicate, Some(ScalarType::Bool))?.expr,
            };
        }
        matched = order_by_names(matched, &table.primary_key, &target_label);
        let mut fields = table
            .primary_key
            .iter()
            .map(|name| ProjectField {
                name: name.clone(),
                expression: Expr::Column {
                    scope: target_label.clone(),
                    name: name.clone(),
                },
            })
            .collect::<Vec<_>>();
        for assignment in &update.assignments {
            let AssignmentTarget::ColumnName(name) = &assignment.target else {
                return Err(Error::Unsupported(
                    "tuple assignments in ON CONFLICT are not supported".into(),
                ));
            };
            let name = object_name(name)?;
            let column = table
                .column(&name)
                .ok_or_else(|| Error::Invalid(format!("unknown column {name:?}")))?;
            if table.primary_key.contains(&name) {
                continue;
            }
            fields.push(ProjectField {
                name,
                expression: expressions
                    .compile(&assignment.value, Some(column.scalar_type))?
                    .expr,
            });
        }
        let update = Statement::Update {
            name: "sql_upsert_update".into(),
            relation: self.query(Relation::Project {
                input: Box::new(matched),
                scope: None,
                spread: Vec::new(),
                fields,
            }),
            table: table.name.clone(),
        };
        let mut statements = vec![update, create];
        let Some(returning) = insert.returning.as_deref() else {
            return Ok((
                Program {
                    statements,
                    result: None,
                },
                Vec::new(),
            ));
        };
        let return_label = self.scope(&table.name);
        let return_scope = ScopeDef::table(table, table.name.clone(), return_label.clone());
        let relation = Relation::Join {
            left: Box::new(rows),
            right: Box::new(Relation::Scan {
                table: table.name.clone(),
                scope: return_label.clone(),
            }),
            kind: JoinKind::Inner,
            on: conflict_join_predicate(&return_scope, &row_scope, &targets[0])?,
        };
        let relation = order_by_names(relation, &table.primary_key, &return_label);
        let env = ExpressionEnv::local(vec![return_scope], &ExpressionEnv::default());
        let mut expressions = ExpressionCompiler::new(self, env);
        let (spread, fields, result_columns) = projection(returning, &mut expressions)?;
        statements.push(Statement::Query {
            name: "sql_result".into(),
            relation: self.query(Relation::Project {
                input: Box::new(relation),
                scope: None,
                spread,
                fields,
            }),
        });
        Ok((
            Program {
                statements,
                result: Some("sql_result".into()),
            },
            result_columns,
        ))
    }

    fn conflict_exists(
        &mut self,
        table: &Table,
        row_scope: &ScopeDef,
        targets: &[Vec<String>],
    ) -> Result<Expr> {
        let mut predicates = Vec::with_capacity(targets.len());
        for target in targets {
            let label = self.scope("conflict");
            let target_scope = ScopeDef::table(table, table.name.clone(), label.clone());
            let predicate = conflict_join_predicate(&target_scope, row_scope, target)?;
            predicates.push(Expr::Exists(Box::new(Relation::Filter {
                input: Box::new(Relation::Scan {
                    table: table.name.clone(),
                    scope: label,
                }),
                predicate,
            })));
        }
        fold_binary(predicates, BinaryOp::Or)
    }

    fn query(&self, root: Relation) -> Query {
        Query {
            root,
            cardinality: RootCardinality::Many,
            bindings: self.bindings.clone(),
        }
    }

    fn fresh_relation(&mut self, relation: &Relation) -> (Relation, HashMap<String, String>) {
        let mut scopes = Vec::new();
        collect_relation_scopes(relation, &mut scopes);
        scopes.sort();
        scopes.dedup();
        let replacements = scopes
            .into_iter()
            .map(|scope| (scope, self.scope("aggregate_input")))
            .collect::<HashMap<_, _>>();
        let mut relation = relation.clone();
        remap_relation_scopes(&mut relation, &replacements);
        (relation, replacements)
    }

    fn update(&mut self, update: &Update) -> Result<(Program, Vec<ResultColumn>)> {
        let TableFactor::Table { name, alias, .. } = &update.table.relation else {
            return Err(Error::Unsupported(
                "derived UPDATE targets are not supported".into(),
            ));
        };
        let table = self.table(name)?.clone();
        let alias_name = alias
            .as_ref()
            .map(|alias| identifier(&alias.name))
            .unwrap_or_else(|| table.name.clone());
        let mut from = vec![update.table.clone()];
        if let Some(UpdateTableFromKind::BeforeSet(items) | UpdateTableFromKind::AfterSet(items)) =
            &update.from
        {
            from.extend(items.clone());
        }
        let source = self.compile_from_tables(&from, &ExpressionEnv::default())?;
        let target_scope = source
            .env
            .local_scopes()
            .iter()
            .find(|scope| scope.alias == alias_name)
            .cloned()
            .ok_or_else(|| Error::Invalid("UPDATE target scope is missing".into()))?;
        let mut relation = source.relation;
        let mut expressions = ExpressionCompiler::new(self, source.env);
        if let Some(predicate) = &update.selection {
            relation = Relation::Filter {
                input: Box::new(relation),
                predicate: expressions.compile(predicate, Some(ScalarType::Bool))?.expr,
            };
        }
        relation = order_by_names(relation, &table.primary_key, &target_scope.label);
        let mut fields = table
            .primary_key
            .iter()
            .map(|name| ProjectField {
                name: name.clone(),
                expression: Expr::Column {
                    scope: target_scope.label.clone(),
                    name: name.clone(),
                },
            })
            .collect::<Vec<_>>();
        for assignment in &update.assignments {
            let AssignmentTarget::ColumnName(name) = &assignment.target else {
                return Err(Error::Unsupported(
                    "tuple assignments are not supported yet".into(),
                ));
            };
            let name = object_name(name)?;
            if table.primary_key.contains(&name) {
                return Err(Error::Invalid("primary keys cannot be updated".into()));
            }
            let column = table
                .column(&name)
                .ok_or_else(|| Error::Invalid(format!("unknown column {name:?}")))?;
            fields.push(ProjectField {
                name,
                expression: expressions
                    .compile(&assignment.value, Some(column.scalar_type))?
                    .expr,
            });
        }
        relation = Relation::Project {
            input: Box::new(relation),
            scope: None,
            spread: Vec::new(),
            fields,
        };
        self.mutation_program(
            Statement::Update {
                name: "sql_mutation".into(),
                relation: self.query(relation),
                table: table.name.clone(),
            },
            &table,
            update.returning.as_deref(),
        )
    }

    fn delete(&mut self, delete: &Delete) -> Result<(Program, Vec<ResultColumn>)> {
        let from = match &delete.from {
            FromTable::WithFromKeyword(from) | FromTable::WithoutKeyword(from) => from,
        };
        if from.len() != 1 {
            return Err(Error::Unsupported(
                "multi-target DELETE is not supported".into(),
            ));
        }
        let TableFactor::Table { name, alias, .. } = &from[0].relation else {
            return Err(Error::Unsupported(
                "derived DELETE targets are not supported".into(),
            ));
        };
        let table = self.table(name)?.clone();
        let alias_name = alias
            .as_ref()
            .map(|alias| identifier(&alias.name))
            .unwrap_or_else(|| table.name.clone());
        let mut sources = from.clone();
        if let Some(using) = &delete.using {
            sources.extend(using.clone());
        }
        let source = self.compile_from_tables(&sources, &ExpressionEnv::default())?;
        let target_scope = source
            .env
            .local_scopes()
            .iter()
            .find(|scope| scope.alias == alias_name)
            .cloned()
            .ok_or_else(|| Error::Invalid("DELETE target scope is missing".into()))?;
        let mut relation = source.relation;
        let mut expressions = ExpressionCompiler::new(self, source.env);
        if let Some(predicate) = &delete.selection {
            relation = Relation::Filter {
                input: Box::new(relation),
                predicate: expressions.compile(predicate, Some(ScalarType::Bool))?.expr,
            };
        }
        relation = order_by_names(relation, &table.primary_key, &target_scope.label);
        relation = Relation::Project {
            input: Box::new(relation),
            scope: None,
            spread: Vec::new(),
            fields: table
                .primary_key
                .iter()
                .map(|name| ProjectField {
                    name: name.clone(),
                    expression: Expr::Column {
                        scope: target_scope.label.clone(),
                        name: name.clone(),
                    },
                })
                .collect(),
        };
        self.mutation_program(
            Statement::Delete {
                name: "sql_mutation".into(),
                relation: self.query(relation),
                table: table.name.clone(),
            },
            &table,
            delete.returning.as_deref(),
        )
    }

    fn mutation_program(
        &mut self,
        mutation: Statement,
        table: &Table,
        returning: Option<&[SelectItem]>,
    ) -> Result<(Program, Vec<ResultColumn>)> {
        let mutation_name = mutation.name().to_owned();
        let Some(returning) = returning else {
            return Ok((
                Program {
                    statements: vec![mutation],
                    result: None,
                },
                Vec::new(),
            ));
        };
        let scope = "returned";
        let relation = Relation::Ref {
            binding: mutation_name,
            scope: scope.into(),
        };
        let relation = order_by_names(relation, &table.primary_key, scope);
        let env = ExpressionEnv::local(
            vec![ScopeDef::table(table, scope.into(), scope.into())],
            &ExpressionEnv::default(),
        );
        let mut expressions = ExpressionCompiler::new(self, env);
        let (spread, fields, columns) = projection(returning, &mut expressions)?;
        let relation = Relation::Project {
            input: Box::new(relation),
            scope: None,
            spread,
            fields,
        };
        Ok((
            Program {
                statements: vec![
                    mutation,
                    Statement::Query {
                        name: "sql_result".into(),
                        relation: query(relation),
                    },
                ],
                result: Some("sql_result".into()),
            },
            columns,
        ))
    }

    fn raw_value(&mut self, expression: &SqlExpr, expected: ScalarType) -> Result<RawScalar> {
        match expression {
            SqlExpr::Value(value) => raw_sql_value(
                &value.value,
                expected,
                &mut self.inferred,
                self.hints,
                self.parameters,
            ),
            SqlExpr::Cast {
                expr, data_type, ..
            } if scalar_type(data_type)?.0 == expected => self.raw_value(expr, expected),
            _ => Err(Error::Unsupported(
                "INSERT VALUES currently accepts literals, parameters, and casts only".into(),
            )),
        }
    }

    fn create_table(&self, create: &CreateTable) -> Result<Option<Program>> {
        let name = object_name(&create.name)?;
        if let Some(existing) = self.tables.iter().find(|table| table.name == name) {
            if create.if_not_exists {
                return Ok(None);
            }
            return Err(Error::Invalid(format!(
                "table {:?} already exists with id {}",
                existing.name, existing.schema_id
            )));
        }
        if create.temporary || create.external || create.query.is_some() {
            return Err(Error::Unsupported(
                "temporary, external, and CREATE TABLE AS tables are not supported".into(),
            ));
        }
        let mut primary_key = Vec::new();
        let mut indexes = Vec::new();
        let mut columns = Vec::with_capacity(create.columns.len());
        for column in &create.columns {
            let column_name = identifier(&column.name);
            let (scalar_type, format) = scalar_type(&column.data_type)?;
            let mut nullable = true;
            let mut default = None;
            for option in &column.options {
                match &option.option {
                    ColumnOption::Null => nullable = true,
                    ColumnOption::NotNull => nullable = false,
                    ColumnOption::PrimaryKey(_) => {
                        nullable = false;
                        primary_key.push(column_name.clone());
                    }
                    ColumnOption::Unique(_) => indexes.push(IndexDef {
                        name: format!("{}_{}_key", name, column_name),
                        columns: vec![column_name.clone()],
                        unique: true,
                    }),
                    ColumnOption::Default(expression) => {
                        default = default_value(expression, scalar_type)?;
                    }
                    ColumnOption::ForeignKey(_) => {}
                    ColumnOption::Check(_) | ColumnOption::Comment(_) => {}
                    other => {
                        return Err(Error::Unsupported(format!(
                            "column option {other} is not supported"
                        )));
                    }
                }
            }
            columns.push(ColumnDraft {
                id: None,
                name: column_name,
                scalar_type,
                nullable,
                format,
                default,
            });
        }
        for constraint in &create.constraints {
            match constraint {
                TableConstraint::PrimaryKey(key) => {
                    primary_key = index_columns(&key.columns)?;
                }
                TableConstraint::Unique(unique) => indexes.push(IndexDef {
                    name: unique
                        .name
                        .as_ref()
                        .or(unique.index_name.as_ref())
                        .map(identifier)
                        .unwrap_or_else(|| {
                            format!(
                                "{}_{}_key",
                                name,
                                index_columns(&unique.columns).unwrap().join("_")
                            )
                        }),
                    columns: index_columns(&unique.columns)?,
                    unique: true,
                }),
                TableConstraint::ForeignKey(_) => {}
                TableConstraint::Check(_) => {}
                other => {
                    return Err(Error::Unsupported(format!(
                        "table constraint {other} is not supported"
                    )));
                }
            }
        }
        if primary_key.is_empty() {
            return Err(Error::Invalid("Rad tables require a primary key".into()));
        }
        Ok(Some(Program {
            statements: vec![Statement::CreateTable {
                name: "sql_create_table".into(),
                table: TableDraft {
                    id: None,
                    name,
                    columns,
                    primary_key,
                    indexes,
                    foreign_keys: Vec::new(),
                },
            }],
            result: None,
        }))
    }

    fn create_index(&self, create: &CreateIndex) -> Result<Option<Program>> {
        let table = self.table(&create.table_name)?;
        let name = create
            .name
            .as_ref()
            .ok_or_else(|| Error::Invalid("CREATE INDEX needs a name".into()))
            .and_then(object_name)?;
        if table.index(&name).is_some() {
            if create.if_not_exists {
                return Ok(None);
            }
            return Err(Error::Invalid(format!("index {name:?} already exists")));
        }
        if create.predicate.is_some() || !create.include.is_empty() {
            return Err(Error::Unsupported(
                "partial and covering indexes are not supported".into(),
            ));
        }
        Ok(Some(Program {
            statements: vec![Statement::CreateIndex {
                name: "sql_create_index".into(),
                table_id: table.schema_id,
                index: IndexDef {
                    name,
                    columns: index_columns(&create.columns)?,
                    unique: create.unique,
                },
            }],
            result: None,
        }))
    }
}

#[derive(Clone)]
struct ScopeDef {
    alias: String,
    label: String,
    columns: Vec<ResultColumn>,
    primary_key: Vec<String>,
}

impl ScopeDef {
    fn table(table: &Table, alias: String, label: String) -> Self {
        Self {
            alias,
            label,
            columns: table.columns.iter().map(result_column).collect(),
            primary_key: table.primary_key.clone(),
        }
    }

    fn column(&self, name: &str) -> Option<&ResultColumn> {
        self.columns.iter().find(|column| column.name == name)
    }
}

fn apply_column_aliases(
    scope: &mut ScopeDef,
    alias: Option<&sqlparser::ast::TableAlias>,
) -> Result<()> {
    let Some(alias) = alias else {
        return Ok(());
    };
    if alias.columns.len() > scope.columns.len() {
        return Err(Error::Invalid(format!(
            "table alias {} names {} columns for a {}-column relation",
            alias.name,
            alias.columns.len(),
            scope.columns.len()
        )));
    }
    for (column, alias) in scope.columns.iter_mut().zip(&alias.columns) {
        column.name = identifier(&alias.name);
    }
    Ok(())
}

#[derive(Clone, Default)]
struct ExpressionEnv {
    levels: Vec<Vec<ScopeDef>>,
}

impl ExpressionEnv {
    fn local(scopes: Vec<ScopeDef>, parent: &Self) -> Self {
        let mut levels = Vec::with_capacity(parent.levels.len() + 1);
        levels.push(scopes);
        levels.extend(parent.levels.clone());
        Self { levels }
    }

    fn local_scopes(&self) -> &[ScopeDef] {
        self.levels.first().map(Vec::as_slice).unwrap_or_default()
    }

    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<(&ScopeDef, &ResultColumn)> {
        for level in &self.levels {
            if let Some(qualifier) = qualifier {
                if let Some(scope) = level.iter().find(|scope| scope.alias == qualifier) {
                    let column = scope.column(name).ok_or_else(|| {
                        Error::Invalid(format!("unknown column {qualifier}.{name}"))
                    })?;
                    return Ok((scope, column));
                }
                continue;
            }
            let mut matches = level
                .iter()
                .filter_map(|scope| scope.column(name).map(|column| (scope, column)));
            let Some(found) = matches.next() else {
                continue;
            };
            if matches.next().is_some() {
                return Err(Error::Invalid(format!(
                    "column reference {name:?} is ambiguous"
                )));
            }
            return Ok(found);
        }
        match qualifier {
            Some(qualifier) => Err(Error::Invalid(format!(
                "unknown table or alias {qualifier:?}"
            ))),
            None => Err(Error::Invalid(format!("unknown column {name:?}"))),
        }
    }
}

struct SelectSource {
    relation: Relation,
    env: ExpressionEnv,
    default_order: Vec<OrderTerm>,
}

struct LoweredQuery {
    relation: Relation,
    columns: Vec<ResultColumn>,
    scope: String,
    one: bool,
}

#[derive(Clone)]
struct CompiledExpr {
    expr: Expr,
    scalar_type: ScalarType,
    nullable: bool,
}

struct ExpressionCompiler<'context, 'catalog> {
    context: &'context mut Context<'catalog>,
    env: ExpressionEnv,
    output: Option<(String, Vec<ResultColumn>)>,
    substitutions: Vec<(SqlExpr, ResultColumn)>,
}

impl<'context, 'catalog> ExpressionCompiler<'context, 'catalog> {
    fn new(context: &'context mut Context<'catalog>, env: ExpressionEnv) -> Self {
        Self {
            context,
            env,
            output: None,
            substitutions: Vec::new(),
        }
    }

    fn with_output(mut self, scope: String, output: Vec<ResultColumn>) -> Self {
        self.output = Some((scope, output));
        self
    }

    fn with_substitutions(mut self, substitutions: Vec<(SqlExpr, ResultColumn)>) -> Self {
        self.substitutions = substitutions;
        self
    }

    fn compile(
        &mut self,
        expression: &SqlExpr,
        expected: Option<ScalarType>,
    ) -> Result<CompiledExpr> {
        if let Some((_, column)) = self
            .substitutions
            .iter()
            .find(|(candidate, _)| candidate == expression)
        {
            let (scope, _) = self
                .output
                .as_ref()
                .ok_or_else(|| Error::Invalid("expression substitution lacks a scope".into()))?;
            return Ok(CompiledExpr {
                expr: Expr::Column {
                    scope: scope.clone(),
                    name: column.field().to_owned(),
                },
                scalar_type: column.scalar_type,
                nullable: column.nullable,
            });
        }
        match expression {
            SqlExpr::Identifier(identifier) => self.column(None, identifier),
            SqlExpr::CompoundIdentifier(parts) if parts.len() == 2 => {
                self.column(Some(&parts[0]), &parts[1])
            }
            SqlExpr::Value(value) => {
                let scalar_type = literal_type(&value.value, expected)?;
                let raw = raw_sql_value(
                    &value.value,
                    scalar_type,
                    &mut self.context.inferred,
                    self.context.hints,
                    self.context.parameters,
                )?;
                Ok(CompiledExpr {
                    nullable: matches!(raw, RawScalar::Null),
                    expr: Expr::Literal(Literal {
                        raw,
                        kind: Some(Kind::of(scalar_type)),
                    }),
                    scalar_type,
                })
            }
            SqlExpr::BinaryOp { left, op, right } => {
                let logical = matches!(op, BinaryOperator::And | BinaryOperator::Or);
                let expected_operand = logical.then_some(ScalarType::Bool);
                let (left, right) = if (is_placeholder(left) || is_string_literal(left))
                    && expected_operand.is_none()
                {
                    let right = self.compile(right, None)?;
                    let left = self.compile(left, Some(right.scalar_type))?;
                    (left, right)
                } else {
                    let left = self.compile(left, expected_operand)?;
                    let right = self.compile(right, Some(left.scalar_type))?;
                    (left, right)
                };
                let (operator, scalar_type) =
                    binary_operator(op, left.scalar_type, right.scalar_type)?;
                Ok(CompiledExpr {
                    expr: Expr::Binary {
                        op: operator,
                        left: Box::new(left.expr),
                        right: Box::new(right.expr),
                    },
                    scalar_type,
                    nullable: left.nullable || right.nullable,
                })
            }
            SqlExpr::UnaryOp { op, expr } => {
                let expected = matches!(op, UnaryOperator::Not | UnaryOperator::BangNot)
                    .then_some(ScalarType::Bool)
                    .or(expected);
                let expression = self.compile(expr, expected)?;
                let op = match op {
                    UnaryOperator::Not | UnaryOperator::BangNot => UnaryOp::Not,
                    UnaryOperator::Minus => UnaryOp::Negate,
                    UnaryOperator::Plus => return Ok(expression),
                    _ => {
                        return Err(Error::Unsupported(format!(
                            "unary operator {op} is not supported"
                        )));
                    }
                };
                Ok(CompiledExpr {
                    scalar_type: expression.scalar_type,
                    nullable: expression.nullable,
                    expr: Expr::Unary {
                        op,
                        expression: Box::new(expression.expr),
                    },
                })
            }
            SqlExpr::IsNull(expr) | SqlExpr::IsNotNull(expr) => {
                let compiled = self.compile(expr, expected)?;
                Ok(CompiledExpr {
                    expr: Expr::Unary {
                        op: if matches!(expression, SqlExpr::IsNull(_)) {
                            UnaryOp::IsNull
                        } else {
                            UnaryOp::IsNotNull
                        },
                        expression: Box::new(compiled.expr),
                    },
                    scalar_type: ScalarType::Bool,
                    nullable: false,
                })
            }
            SqlExpr::IsTrue(expr)
            | SqlExpr::IsNotTrue(expr)
            | SqlExpr::IsFalse(expr)
            | SqlExpr::IsNotFalse(expr) => {
                let value = self.compile(expr, Some(ScalarType::Bool))?;
                let expected_value =
                    matches!(expression, SqlExpr::IsTrue(_) | SqlExpr::IsNotFalse(_));
                let negated = matches!(expression, SqlExpr::IsNotTrue(_) | SqlExpr::IsNotFalse(_));
                let predicate = Expr::Binary {
                    op: BinaryOp::Eq,
                    left: Box::new(value.expr),
                    right: Box::new(bool_literal(expected_value)),
                };
                let is_value = Expr::Branch {
                    arms: vec![BranchArm {
                        when: predicate,
                        then: bool_literal(true),
                    }],
                    otherwise: Box::new(bool_literal(false)),
                };
                Ok(CompiledExpr {
                    expr: if negated {
                        Expr::Unary {
                            op: UnaryOp::Not,
                            expression: Box::new(is_value),
                        }
                    } else {
                        is_value
                    },
                    scalar_type: ScalarType::Bool,
                    nullable: false,
                })
            }
            SqlExpr::Nested(expr) => self.compile(expr, expected),
            SqlExpr::Cast {
                expr, data_type, ..
            } => {
                let (to, _) = scalar_type(data_type)?;
                let expression = self.compile(expr, Some(to))?;
                Ok(CompiledExpr {
                    expr: Expr::Cast {
                        expression: Box::new(expression.expr),
                        to: Kind::of(to),
                    },
                    scalar_type: to,
                    nullable: expression.nullable,
                })
            }
            SqlExpr::InList {
                expr,
                list,
                negated,
            } => {
                let value = self.compile(expr, None)?;
                let mut predicates = Vec::with_capacity(list.len());
                for candidate in list {
                    let candidate = self.compile(candidate, Some(value.scalar_type))?;
                    predicates.push(Expr::Binary {
                        op: BinaryOp::Eq,
                        left: Box::new(value.expr.clone()),
                        right: Box::new(candidate.expr),
                    });
                }
                let mut predicate = fold_binary(predicates, BinaryOp::Or)?;
                if *negated {
                    predicate = Expr::Unary {
                        op: UnaryOp::Not,
                        expression: Box::new(predicate),
                    };
                }
                Ok(CompiledExpr {
                    expr: predicate,
                    scalar_type: ScalarType::Bool,
                    nullable: value.nullable,
                })
            }
            SqlExpr::Between {
                expr,
                negated,
                low,
                high,
            } => {
                let value = self.compile(expr, None)?;
                let low = self.compile(low, Some(value.scalar_type))?;
                let high = self.compile(high, Some(value.scalar_type))?;
                let mut predicate = Expr::Binary {
                    op: BinaryOp::And,
                    left: Box::new(Expr::Binary {
                        op: BinaryOp::Gte,
                        left: Box::new(value.expr.clone()),
                        right: Box::new(low.expr),
                    }),
                    right: Box::new(Expr::Binary {
                        op: BinaryOp::Lte,
                        left: Box::new(value.expr),
                        right: Box::new(high.expr),
                    }),
                };
                if *negated {
                    predicate = Expr::Unary {
                        op: UnaryOp::Not,
                        expression: Box::new(predicate),
                    };
                }
                Ok(CompiledExpr {
                    expr: predicate,
                    scalar_type: ScalarType::Bool,
                    nullable: value.nullable || low.nullable || high.nullable,
                })
            }
            SqlExpr::Like {
                negated,
                any,
                expr,
                pattern,
                escape_char,
            }
            | SqlExpr::ILike {
                negated,
                any,
                expr,
                pattern,
                escape_char,
            } => {
                if *any {
                    return Err(Error::Unsupported("LIKE ANY is not supported".into()));
                }
                self.compile_like(
                    expr,
                    pattern,
                    escape_char.as_ref(),
                    matches!(expression, SqlExpr::ILike { .. }),
                    *negated,
                )
            }
            SqlExpr::Case {
                operand,
                conditions,
                else_result,
                ..
            } => self.compile_case(
                operand.as_deref(),
                conditions,
                else_result.as_deref(),
                expected,
            ),
            SqlExpr::Function(function) => self.compile_scalar_function(function, expected),
            SqlExpr::Exists { subquery, negated } => {
                let lowered = self.context.select_query_in(subquery, &self.env)?;
                let exists = Expr::Exists(Box::new(lowered.relation));
                Ok(CompiledExpr {
                    expr: if *negated {
                        Expr::Unary {
                            op: UnaryOp::Not,
                            expression: Box::new(exists),
                        }
                    } else {
                        exists
                    },
                    scalar_type: ScalarType::Bool,
                    nullable: false,
                })
            }
            SqlExpr::Subquery(subquery) => {
                let lowered = self.context.select_query_in(subquery, &self.env)?;
                if lowered.columns.len() != 1 {
                    return Err(Error::Invalid(format!(
                        "scalar subquery returns {} columns",
                        lowered.columns.len()
                    )));
                }
                let column = lowered.columns[0].clone();
                Ok(CompiledExpr {
                    expr: Expr::Scalar(Box::new(lowered.relation)),
                    scalar_type: column.scalar_type,
                    nullable: true,
                })
            }
            SqlExpr::InSubquery {
                expr,
                subquery,
                negated,
            } => {
                let value = self.compile(expr, None)?;
                let mut lowered = self.context.select_query_in(subquery, &self.env)?;
                if lowered.columns.len() != 1 {
                    return Err(Error::Invalid(format!(
                        "IN subquery returns {} columns",
                        lowered.columns.len()
                    )));
                }
                let column = &lowered.columns[0];
                if column.scalar_type != value.scalar_type {
                    return Err(Error::Invalid(format!(
                        "IN compares {:?} with {:?}",
                        value.scalar_type, column.scalar_type
                    )));
                }
                lowered.relation = Relation::Filter {
                    input: Box::new(lowered.relation),
                    predicate: Expr::Binary {
                        op: BinaryOp::Eq,
                        left: Box::new(value.expr),
                        right: Box::new(Expr::Column {
                            scope: lowered.scope,
                            name: column.field().to_owned(),
                        }),
                    },
                };
                let exists = Expr::Exists(Box::new(lowered.relation));
                Ok(CompiledExpr {
                    expr: if *negated {
                        Expr::Unary {
                            op: UnaryOp::Not,
                            expression: Box::new(exists),
                        }
                    } else {
                        exists
                    },
                    scalar_type: ScalarType::Bool,
                    nullable: false,
                })
            }
            _ => Err(Error::Unsupported(format!(
                "expression {expression} is not supported yet"
            ))),
        }
    }

    fn compile_like(
        &mut self,
        value: &SqlExpr,
        pattern: &SqlExpr,
        escape: Option<&sqlparser::ast::ValueWithSpan>,
        insensitive: bool,
        negated: bool,
    ) -> Result<CompiledExpr> {
        let value = self.compile(value, Some(ScalarType::Text))?;
        let pattern = self.pattern_value(pattern)?;
        let escape = escape
            .map(|value| {
                value
                    .value
                    .clone()
                    .into_string()
                    .ok_or_else(|| Error::Invalid("LIKE ESCAPE must be text".into()))
            })
            .transpose()?
            .unwrap_or_else(|| "\\".into());
        let escape = match escape.chars().collect::<Vec<_>>().as_slice() {
            [] => None,
            [value] => Some(*value),
            _ => {
                return Err(Error::Invalid(
                    "LIKE ESCAPE must be empty or one character".into(),
                ));
            }
        };
        let parts = like_pattern(&pattern, escape)?;
        if insensitive
            && matches!(parts.as_slice(), [TextMatchPart::Literal(value)] if value.is_empty())
        {
            return Err(Error::Unsupported(
                "empty ILIKE patterns need case-folded equality in LIR".into(),
            ));
        }
        let mut expression = if !insensitive
            && parts.len() == 1
            && matches!(parts.first(), Some(TextMatchPart::Literal(_)))
        {
            let TextMatchPart::Literal(pattern) = &parts[0] else {
                unreachable!()
            };
            Expr::Binary {
                op: BinaryOp::Eq,
                left: Box::new(value.expr),
                right: Box::new(Expr::Literal(Literal {
                    raw: RawScalar::Text(pattern.clone()),
                    kind: Some(Kind::Text),
                })),
            }
        } else {
            Expr::TextMatch {
                value: Box::new(value.expr),
                parts,
                comparison: if insensitive {
                    TextComparison::UnicodeSimpleFold
                } else {
                    TextComparison::Exact
                },
            }
        };
        if negated {
            expression = Expr::Unary {
                op: UnaryOp::Not,
                expression: Box::new(expression),
            };
        }
        Ok(CompiledExpr {
            expr: expression,
            scalar_type: ScalarType::Bool,
            nullable: value.nullable,
        })
    }

    fn pattern_value(&mut self, expression: &SqlExpr) -> Result<String> {
        let expression = match expression {
            SqlExpr::Cast { expr, .. } | SqlExpr::Nested(expr) => expr.as_ref(),
            _ => expression,
        };
        let SqlExpr::Value(value) = expression else {
            return Err(Error::Unsupported(
                "row-dependent LIKE patterns need a dynamic matcher in LIR".into(),
            ));
        };
        match raw_sql_value(
            &value.value,
            ScalarType::Text,
            &mut self.context.inferred,
            self.context.hints,
            self.context.parameters,
        )? {
            RawScalar::Text(value) => Ok(value),
            RawScalar::Null if matches!(value.value, SqlValue::Placeholder(_)) => Ok("%".into()),
            RawScalar::Null => Err(Error::Invalid("LIKE pattern must not be NULL".into())),
            _ => Err(Error::Invalid("LIKE pattern must be text".into())),
        }
    }

    fn compile_case(
        &mut self,
        operand: Option<&SqlExpr>,
        conditions: &[sqlparser::ast::CaseWhen],
        else_result: Option<&SqlExpr>,
        expected: Option<ScalarType>,
    ) -> Result<CompiledExpr> {
        if conditions.is_empty() {
            return Err(Error::Invalid("CASE needs at least one WHEN arm".into()));
        }
        let result_type = expected
            .or_else(|| {
                conditions
                    .iter()
                    .find_map(|arm| self.type_hint(&arm.result))
            })
            .or_else(|| else_result.and_then(|expression| self.type_hint(expression)))
            .ok_or_else(|| Error::Invalid("cannot determine CASE result type".into()))?;
        let operand = operand
            .map(|operand| self.compile(operand, None))
            .transpose()?;
        let mut arms = Vec::with_capacity(conditions.len());
        let mut nullable = else_result.is_none();
        for arm in conditions {
            let condition = if let Some(operand) = &operand {
                let candidate = self.compile(&arm.condition, Some(operand.scalar_type))?;
                Expr::Binary {
                    op: BinaryOp::Eq,
                    left: Box::new(operand.expr.clone()),
                    right: Box::new(candidate.expr),
                }
            } else {
                self.compile(&arm.condition, Some(ScalarType::Bool))?.expr
            };
            let result = self.compile(&arm.result, Some(result_type))?;
            nullable |= result.nullable;
            arms.push(BranchArm {
                when: condition,
                then: result.expr,
            });
        }
        let otherwise = if let Some(expression) = else_result {
            let result = self.compile(expression, Some(result_type))?;
            nullable |= result.nullable;
            result.expr
        } else {
            Expr::Literal(Literal {
                raw: RawScalar::Null,
                kind: Some(Kind::of(result_type)),
            })
        };
        Ok(CompiledExpr {
            expr: Expr::Branch {
                arms,
                otherwise: Box::new(otherwise),
            },
            scalar_type: result_type,
            nullable,
        })
    }

    fn compile_scalar_function(
        &mut self,
        function: &sqlparser::ast::Function,
        expected: Option<ScalarType>,
    ) -> Result<CompiledExpr> {
        if function.over.is_some() || function.filter.is_some() {
            return Err(Error::Unsupported(
                "window functions are not supported".into(),
            ));
        }
        let name = object_name(&function.name)?.to_ascii_lowercase();
        let FunctionArguments::List(arguments) = &function.args else {
            return Err(Error::Unsupported(format!("function {name} arguments")));
        };
        let values = arguments
            .args
            .iter()
            .map(|argument| match argument {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expression)) => Ok(expression),
                _ => Err(Error::Unsupported(format!(
                    "function {name} argument {argument}"
                ))),
            })
            .collect::<Result<Vec<_>>>()?;
        match name.as_str() {
            "coalesce" => {
                let result_type = expected
                    .or_else(|| values.iter().find_map(|value| self.type_hint(value)))
                    .ok_or_else(|| {
                        Error::Invalid("cannot determine COALESCE result type".into())
                    })?;
                let mut compiled = values
                    .iter()
                    .map(|value| self.compile(value, Some(result_type)))
                    .collect::<Result<Vec<_>>>()?;
                if compiled.len() == 1 {
                    return Ok(compiled.pop().expect("one COALESCE argument"));
                }
                let otherwise = compiled
                    .pop()
                    .ok_or_else(|| Error::Invalid("COALESCE needs at least one argument".into()))?;
                let mut nullable = otherwise.nullable;
                let arms = compiled
                    .into_iter()
                    .map(|value| {
                        nullable &= value.nullable;
                        BranchArm {
                            when: Expr::Unary {
                                op: UnaryOp::IsNotNull,
                                expression: Box::new(value.expr.clone()),
                            },
                            then: value.expr,
                        }
                    })
                    .collect();
                Ok(CompiledExpr {
                    expr: Expr::Branch {
                        arms,
                        otherwise: Box::new(otherwise.expr),
                    },
                    scalar_type: result_type,
                    nullable,
                })
            }
            "nullif" if values.len() == 2 => {
                let left = self.compile(values[0], expected)?;
                let right = self.compile(values[1], Some(left.scalar_type))?;
                Ok(CompiledExpr {
                    expr: Expr::Branch {
                        arms: vec![BranchArm {
                            when: Expr::Binary {
                                op: BinaryOp::Eq,
                                left: Box::new(left.expr.clone()),
                                right: Box::new(right.expr),
                            },
                            then: Expr::Literal(Literal {
                                raw: RawScalar::Null,
                                kind: Some(Kind::of(left.scalar_type)),
                            }),
                        }],
                        otherwise: Box::new(left.expr),
                    },
                    scalar_type: left.scalar_type,
                    nullable: true,
                })
            }
            _ => Err(Error::Unsupported(format!(
                "scalar function {name} is not supported"
            ))),
        }
    }

    fn type_hint(&self, expression: &SqlExpr) -> Option<ScalarType> {
        if let Some((_, column)) = self
            .substitutions
            .iter()
            .find(|(candidate, _)| candidate == expression)
        {
            return Some(column.scalar_type);
        }
        match expression {
            SqlExpr::Identifier(name) => {
                self.column(None, name).ok().map(|value| value.scalar_type)
            }
            SqlExpr::CompoundIdentifier(parts) if parts.len() == 2 => self
                .column(Some(&parts[0]), &parts[1])
                .ok()
                .map(|value| value.scalar_type),
            SqlExpr::Value(value) => literal_type(&value.value, None).ok(),
            SqlExpr::Cast { data_type, .. } => scalar_type(data_type).ok().map(|value| value.0),
            SqlExpr::Nested(expression) => self.type_hint(expression),
            SqlExpr::BinaryOp { left, op, right } => {
                if matches!(
                    op,
                    BinaryOperator::Eq
                        | BinaryOperator::NotEq
                        | BinaryOperator::Lt
                        | BinaryOperator::LtEq
                        | BinaryOperator::Gt
                        | BinaryOperator::GtEq
                        | BinaryOperator::And
                        | BinaryOperator::Or
                ) {
                    Some(ScalarType::Bool)
                } else {
                    self.type_hint(left).or_else(|| self.type_hint(right))
                }
            }
            SqlExpr::Case {
                conditions,
                else_result,
                ..
            } => conditions
                .iter()
                .find_map(|arm| self.type_hint(&arm.result))
                .or_else(|| {
                    else_result
                        .as_deref()
                        .and_then(|value| self.type_hint(value))
                }),
            _ => None,
        }
    }

    fn column(&self, qualifier: Option<&Ident>, name: &Ident) -> Result<CompiledExpr> {
        let name = identifier(name);
        let qualifier = qualifier.map(identifier);
        if let Some((scope, column)) = self.output.as_ref().and_then(|(scope, columns)| {
            qualifier
                .as_ref()
                .is_none_or(|qualifier| qualifier == scope)
                .then(|| {
                    columns
                        .iter()
                        .find(|column| column.name == name)
                        .map(|column| (scope, column))
                })
                .flatten()
        }) {
            return Ok(CompiledExpr {
                expr: Expr::Column {
                    scope: scope.clone(),
                    name: column.field().to_owned(),
                },
                scalar_type: column.scalar_type,
                nullable: column.nullable,
            });
        }
        let (scope, column) = self.env.lookup(qualifier.as_deref(), &name)?;
        Ok(CompiledExpr {
            expr: Expr::Column {
                scope: scope.label.clone(),
                name: column.field().to_owned(),
            },
            scalar_type: column.scalar_type,
            nullable: column.nullable,
        })
    }
}

fn projection(
    items: &[SelectItem],
    expressions: &mut ExpressionCompiler<'_, '_>,
) -> Result<(Vec<String>, Vec<ProjectField>, Vec<ResultColumn>)> {
    let spread = Vec::new();
    let mut fields = Vec::new();
    let mut columns = Vec::new();
    let mut names = HashMap::<String, usize>::new();
    let mut output_name = |wire: String| {
        let count = names.entry(wire.clone()).or_default();
        *count += 1;
        if *count == 1 {
            wire.clone()
        } else {
            format!("{wire}_{}", *count)
        }
    };
    for item in items {
        match item {
            SelectItem::Wildcard(_) => {
                if expressions.env.local_scopes().is_empty() {
                    return Err(Error::Invalid("SELECT * needs a FROM item".into()));
                }
                for scope in expressions.env.local_scopes() {
                    for column in &scope.columns {
                        let mut column = column.clone();
                        column.field = output_name(column.name.clone());
                        fields.push(ProjectField {
                            name: column.field.clone(),
                            expression: Expr::Column {
                                scope: scope.label.clone(),
                                name: column.field().to_owned(),
                            },
                        });
                        columns.push(column);
                    }
                }
            }
            SelectItem::QualifiedWildcard(SelectItemQualifiedWildcardKind::ObjectName(name), _) => {
                let qualifier = object_name(name)?;
                let scope = expressions
                    .env
                    .local_scopes()
                    .iter()
                    .find(|scope| scope.alias == qualifier)
                    .ok_or_else(|| Error::Invalid(format!("unknown wildcard qualifier {name}")))?;
                for column in &scope.columns {
                    let mut column = column.clone();
                    column.field = output_name(column.name.clone());
                    fields.push(ProjectField {
                        name: column.field.clone(),
                        expression: Expr::Column {
                            scope: scope.label.clone(),
                            name: column.field().to_owned(),
                        },
                    });
                    columns.push(column);
                }
            }
            SelectItem::UnnamedExpr(expression)
            | SelectItem::ExprWithAlias {
                expr: expression, ..
            } => {
                let name = select_item_name(item);
                let field = output_name(name.clone());
                let expression = expressions.compile(expression, None)?;
                fields.push(ProjectField {
                    name: field.clone(),
                    expression: expression.expr,
                });
                columns.push(ResultColumn {
                    name,
                    field,
                    scalar_type: expression.scalar_type,
                    nullable: expression.nullable,
                    format: direct_column_format(expression_ast(item), &expressions.env),
                });
            }
            _ => {
                return Err(Error::Unsupported(format!(
                    "projection item {item} is not supported"
                )));
            }
        }
    }
    Ok((spread, fields, columns))
}

fn expression_ast(item: &SelectItem) -> &SqlExpr {
    match item {
        SelectItem::UnnamedExpr(expression)
        | SelectItem::ExprWithAlias {
            expr: expression, ..
        } => expression,
        _ => unreachable!("only expression projections call expression_ast"),
    }
}

fn direct_column_format(expression: &SqlExpr, env: &ExpressionEnv) -> String {
    let (qualifier, name) = match expression {
        SqlExpr::Identifier(name) => (None, identifier(name)),
        SqlExpr::CompoundIdentifier(parts) if parts.len() == 2 => {
            (Some(identifier(&parts[0])), identifier(&parts[1]))
        }
        _ => return String::new(),
    };
    env.lookup(qualifier.as_deref(), &name)
        .map(|(_, column)| column.format.clone())
        .unwrap_or_default()
}

fn aggregate(
    function: &sqlparser::ast::Function,
    expressions: &mut ExpressionCompiler<'_, '_>,
) -> Result<(AggregateFunction, Option<Expr>, ScalarType, bool, bool)> {
    if function.over.is_some() || function.filter.is_some() {
        return Err(Error::Unsupported(
            "window and filtered aggregates are not supported".into(),
        ));
    }
    let name = object_name(&function.name)?.to_ascii_lowercase();
    let arguments = match &function.args {
        FunctionArguments::List(arguments) => arguments,
        _ => {
            return Err(Error::Unsupported(format!(
                "aggregate {name} has unsupported arguments"
            )));
        }
    };
    if !arguments.clauses.is_empty() {
        return Err(Error::Unsupported(format!(
            "aggregate {name} argument clauses are not supported"
        )));
    }
    let distinct = matches!(
        arguments.duplicate_treatment,
        Some(DuplicateTreatment::Distinct)
    );
    if name == "count"
        && arguments.args.len() == 1
        && matches!(
            arguments.args[0],
            FunctionArg::Unnamed(FunctionArgExpr::Wildcard)
        )
    {
        if distinct {
            return Err(Error::Invalid("COUNT(DISTINCT *) is not valid".into()));
        }
        return Ok((
            AggregateFunction::Count,
            None,
            ScalarType::Int64,
            false,
            false,
        ));
    }
    let [FunctionArg::Unnamed(FunctionArgExpr::Expr(argument))] = arguments.args.as_slice() else {
        return Err(Error::Unsupported(format!(
            "aggregate {name} needs one argument"
        )));
    };
    let argument = expressions.compile(argument, None)?;
    let function = match name.as_str() {
        "count" => AggregateFunction::Count,
        "sum" => AggregateFunction::Sum,
        "avg" => AggregateFunction::Average,
        "min" => AggregateFunction::Min,
        "max" => AggregateFunction::Max,
        _ => {
            return Err(Error::Unsupported(format!(
                "aggregate {name} is not supported"
            )));
        }
    };
    let scalar_type = if function == AggregateFunction::Count {
        ScalarType::Int64
    } else if function == AggregateFunction::Average {
        ScalarType::Float64
    } else {
        argument.scalar_type
    };
    Ok((
        function,
        Some(argument.expr),
        scalar_type,
        function != AggregateFunction::Count,
        distinct,
    ))
}

fn is_aggregate_function(function: &sqlparser::ast::Function) -> bool {
    object_name(&function.name).is_ok_and(|name| {
        matches!(
            name.to_ascii_lowercase().as_str(),
            "count" | "sum" | "avg" | "min" | "max"
        )
    })
}

fn limit(
    clause: Option<&LimitClause>,
    expressions: &mut ExpressionCompiler<'_, '_>,
) -> Result<Option<(usize, Option<usize>)>> {
    let Some(clause) = clause else {
        return Ok(None);
    };
    let LimitClause::LimitOffset {
        limit,
        offset,
        limit_by,
    } = clause
    else {
        return Err(Error::Unsupported(
            "comma LIMIT syntax is not supported".into(),
        ));
    };
    if !limit_by.is_empty() {
        return Err(Error::Unsupported("LIMIT BY is not supported".into()));
    }
    let limit = limit
        .as_ref()
        .map(|value| usize_expression(value, expressions))
        .transpose()?;
    let offset = offset
        .as_ref()
        .map(|value| usize_expression(&value.value, expressions))
        .transpose()?
        .unwrap_or(0);
    Ok(Some((offset, limit)))
}

fn usize_expression(
    expression: &SqlExpr,
    compiler: &mut ExpressionCompiler<'_, '_>,
) -> Result<usize> {
    let expression = compiler.compile(expression, Some(ScalarType::Int64))?;
    let Expr::Literal(Literal {
        raw: RawScalar::Number(value),
        ..
    }) = expression.expr
    else {
        return Err(Error::Invalid(
            "LIMIT/OFFSET must be a bound integer".into(),
        ));
    };
    value.parse::<usize>().map_err(|_| {
        Error::Invalid(format!(
            "LIMIT/OFFSET {value:?} is not a non-negative integer"
        ))
    })
}

fn query(root: Relation) -> Query {
    Query {
        root,
        cardinality: RootCardinality::Many,
        bindings: HashMap::new(),
    }
}

fn order_by_names(relation: Relation, names: &[String], scope: &str) -> Relation {
    Relation::Order {
        input: Box::new(relation),
        terms: names
            .iter()
            .map(|name| OrderTerm {
                expression: Expr::Column {
                    scope: scope.into(),
                    name: name.clone(),
                },
                descending: false,
            })
            .collect(),
    }
}

fn order_by_output(relation: Relation, columns: &[ResultColumn], scope: &str) -> Relation {
    Relation::Order {
        input: Box::new(relation),
        terms: output_order_terms(columns, scope),
    }
}

fn output_order_terms(columns: &[ResultColumn], scope: &str) -> Vec<OrderTerm> {
    columns
        .iter()
        .map(|column| OrderTerm {
            expression: Expr::Column {
                scope: scope.to_owned(),
                name: column.field().to_owned(),
            },
            descending: false,
        })
        .collect()
}

fn compile_order_terms(
    terms: &[OrderByExpr],
    mut compile: impl FnMut(&OrderByExpr) -> Result<Expr>,
) -> Result<Vec<OrderTerm>> {
    let mut output = Vec::with_capacity(terms.len() * 2);
    for term in terms {
        let expression = compile(term)?;
        let descending = term.options.asc == Some(false);
        let nulls_first = term.options.nulls_first.unwrap_or(descending);
        output.push(OrderTerm {
            expression: Expr::Unary {
                op: UnaryOp::IsNull,
                expression: Box::new(expression.clone()),
            },
            descending: nulls_first,
        });
        output.push(OrderTerm {
            expression,
            descending,
        });
    }
    Ok(output)
}

fn bare_identifier(expression: &SqlExpr) -> Option<String> {
    match expression {
        SqlExpr::Identifier(value) => Some(identifier(value)),
        _ => None,
    }
}

fn order_output_expression(expression: &SqlExpr, columns: &[ResultColumn]) -> Result<SqlExpr> {
    if let SqlExpr::Value(value) = expression
        && let SqlValue::Number(value, _) = &value.value
        && let Ok(position) = value.parse::<usize>()
    {
        let column = position
            .checked_sub(1)
            .and_then(|index| columns.get(index))
            .ok_or_else(|| {
                Error::Invalid(format!("ORDER BY position {position} is out of range"))
            })?;
        return Ok(SqlExpr::Identifier(Ident::new(column.name.clone())));
    }
    Ok(expression.clone())
}

fn projection_order_expression(
    expression: &SqlExpr,
    projection: &[SelectItem],
    columns: &[ResultColumn],
) -> SqlExpr {
    if matches!(expression, SqlExpr::Value(value) if matches!(value.value, SqlValue::Number(_, _)))
    {
        return order_output_expression(expression, columns).unwrap_or_else(|_| expression.clone());
    }
    if bare_identifier(expression)
        .is_some_and(|name| columns.iter().any(|column| column.name == name))
    {
        return expression.clone();
    }
    projection
        .iter()
        .zip(columns)
        .find_map(|(item, column)| match item {
            SelectItem::UnnamedExpr(value) | SelectItem::ExprWithAlias { expr: value, .. }
                if value == expression =>
            {
                Some(SqlExpr::Identifier(Ident::new(&column.name)))
            }
            _ => None,
        })
        .unwrap_or_else(|| expression.clone())
}

fn aggregate_order_expression(expression: &SqlExpr, projection: &[SelectItem]) -> Result<SqlExpr> {
    if let SqlExpr::Value(value) = expression
        && let SqlValue::Number(value, _) = &value.value
        && let Ok(position) = value.parse::<usize>()
    {
        return projection
            .get(position.saturating_sub(1))
            .filter(|_| position > 0)
            .map(expression_ast)
            .cloned()
            .ok_or_else(|| {
                Error::Invalid(format!("ORDER BY position {position} is out of range"))
            });
    }
    if let Some(name) = bare_identifier(expression)
        && let Some(projected) = projection.iter().find_map(|item| match item {
            SelectItem::ExprWithAlias { expr, alias } if identifier(alias) == name => {
                Some(expr.clone())
            }
            _ => None,
        })
    {
        return Ok(projected);
    }
    Ok(expression.clone())
}

fn collect_aggregate_expressions(expression: &SqlExpr, output: &mut Vec<SqlExpr>) {
    match expression {
        SqlExpr::Function(function) if is_aggregate_function(function) => {
            if !output.contains(expression) {
                output.push(expression.clone());
            }
        }
        SqlExpr::Function(function) => {
            if let FunctionArguments::List(arguments) = &function.args {
                for argument in &arguments.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(expression)) = argument {
                        collect_aggregate_expressions(expression, output);
                    }
                }
            }
        }
        SqlExpr::BinaryOp { left, right, .. } => {
            collect_aggregate_expressions(left, output);
            collect_aggregate_expressions(right, output);
        }
        SqlExpr::UnaryOp {
            expr: expression, ..
        }
        | SqlExpr::Nested(expression)
        | SqlExpr::IsNull(expression)
        | SqlExpr::IsNotNull(expression)
        | SqlExpr::IsTrue(expression)
        | SqlExpr::IsNotTrue(expression)
        | SqlExpr::IsFalse(expression)
        | SqlExpr::IsNotFalse(expression)
        | SqlExpr::Cast {
            expr: expression, ..
        } => collect_aggregate_expressions(expression, output),
        SqlExpr::InList { expr, list, .. } => {
            collect_aggregate_expressions(expr, output);
            for expression in list {
                collect_aggregate_expressions(expression, output);
            }
        }
        SqlExpr::Between {
            expr, low, high, ..
        } => {
            collect_aggregate_expressions(expr, output);
            collect_aggregate_expressions(low, output);
            collect_aggregate_expressions(high, output);
        }
        SqlExpr::Like { expr, pattern, .. } | SqlExpr::ILike { expr, pattern, .. } => {
            collect_aggregate_expressions(expr, output);
            collect_aggregate_expressions(pattern, output);
        }
        SqlExpr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(operand) = operand {
                collect_aggregate_expressions(operand, output);
            }
            for arm in conditions {
                collect_aggregate_expressions(&arm.condition, output);
                collect_aggregate_expressions(&arm.result, output);
            }
            if let Some(otherwise) = else_result {
                collect_aggregate_expressions(otherwise, output);
            }
        }
        _ => {}
    }
}

fn unique_name(base: &str, names: &mut HashMap<String, usize>) -> String {
    let count = names.entry(base.to_owned()).or_default();
    *count += 1;
    if *count == 1 {
        base.to_owned()
    } else {
        format!("{base}_{}", *count)
    }
}

fn set_expr_references(expression: &SetExpr, name: &str) -> bool {
    match expression {
        SetExpr::Select(select) => select.from.iter().any(|from| {
            table_factor_references(&from.relation, name)
                || from
                    .joins
                    .iter()
                    .any(|join| table_factor_references(&join.relation, name))
        }),
        SetExpr::Query(query) => set_expr_references(query.body.as_ref(), name),
        SetExpr::SetOperation { left, right, .. } => {
            set_expr_references(left, name) || set_expr_references(right, name)
        }
        _ => false,
    }
}

fn table_factor_references(factor: &TableFactor, name: &str) -> bool {
    match factor {
        TableFactor::Table { name: table, .. } => {
            object_name(table).is_ok_and(|table| table == name)
        }
        TableFactor::Derived { subquery, .. } => set_expr_references(subquery.body.as_ref(), name),
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => {
            table_factor_references(&table_with_joins.relation, name)
                || table_with_joins
                    .joins
                    .iter()
                    .any(|join| table_factor_references(&join.relation, name))
        }
        _ => false,
    }
}

fn conflict_targets(table: &Table, target: Option<&ConflictTarget>) -> Result<Vec<Vec<String>>> {
    let targets = match target {
        Some(ConflictTarget::Columns(columns)) => vec![columns.iter().map(identifier).collect()],
        Some(ConflictTarget::OnConstraint(name)) => {
            let name = object_name(name)?;
            if let Some(index) = table.index(&name) {
                vec![
                    table
                        .index_column_names(index)
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                ]
            } else if name == format!("{}_pkey", table.name) {
                vec![table.primary_key.clone()]
            } else {
                return Err(Error::Invalid(format!(
                    "unknown conflict constraint {name:?}"
                )));
            }
        }
        None => {
            let mut targets = vec![table.primary_key.clone()];
            targets.extend(
                table
                    .indexes
                    .iter()
                    .filter(|index| index.unique)
                    .map(|index| {
                        table
                            .index_column_names(index)
                            .into_iter()
                            .map(str::to_owned)
                            .collect()
                    }),
            );
            targets
        }
    };
    for columns in &targets {
        if columns.is_empty() {
            return Err(Error::Invalid("ON CONFLICT target is empty".into()));
        }
        for column in columns {
            if table.column(column).is_none() {
                return Err(Error::Invalid(format!(
                    "unknown conflict column {column:?}"
                )));
            }
        }
    }
    Ok(targets)
}

fn conflict_join_predicate(
    target: &ScopeDef,
    excluded: &ScopeDef,
    columns: &[String],
) -> Result<Expr> {
    let predicates = columns
        .iter()
        .map(|name| {
            let target_column = target.column(name).ok_or_else(|| {
                Error::Invalid(format!("unknown conflict target column {name:?}"))
            })?;
            let excluded_column = excluded.column(name).ok_or_else(|| {
                Error::Invalid(format!("conflict column {name:?} is not inserted"))
            })?;
            Ok(Expr::Binary {
                op: BinaryOp::Eq,
                left: Box::new(Expr::Column {
                    scope: target.label.clone(),
                    name: target_column.field().to_owned(),
                }),
                right: Box::new(Expr::Column {
                    scope: excluded.label.clone(),
                    name: excluded_column.field().to_owned(),
                }),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    fold_binary(predicates, BinaryOp::And)
}

fn select_item_name(item: &SelectItem) -> String {
    match item {
        SelectItem::ExprWithAlias { alias, .. } => identifier(alias),
        SelectItem::UnnamedExpr(expression) => expression_name(expression),
        _ => "column".into(),
    }
}

fn select_distinct(select: &Select) -> Result<bool> {
    match &select.distinct {
        None | Some(SqlDistinct::All) => Ok(false),
        Some(SqlDistinct::Distinct) => Ok(true),
        Some(SqlDistinct::On(_)) => Err(Error::Unsupported(
            "DISTINCT ON needs a per-group first-row relational primitive".into(),
        )),
    }
}

fn expression_name(expression: &SqlExpr) -> String {
    match expression {
        SqlExpr::Identifier(value) => identifier(value),
        SqlExpr::CompoundIdentifier(parts) => parts
            .last()
            .map(identifier)
            .unwrap_or_else(|| "column".into()),
        SqlExpr::Function(function) => {
            object_name(&function.name).unwrap_or_else(|_| "column".into())
        }
        _ => "?column?".into(),
    }
}

fn result_column(column: &crate::engine::catalog::model::Column) -> ResultColumn {
    ResultColumn {
        name: column.name.clone(),
        field: column.name.clone(),
        scalar_type: column.scalar_type,
        nullable: column.nullable,
        format: column.format.clone(),
    }
}

fn binary_operator(
    operator: &BinaryOperator,
    left: ScalarType,
    right: ScalarType,
) -> Result<(BinaryOp, ScalarType)> {
    let operator = match operator {
        BinaryOperator::Eq => BinaryOp::Eq,
        BinaryOperator::NotEq => BinaryOp::Ne,
        BinaryOperator::Lt => BinaryOp::Lt,
        BinaryOperator::LtEq => BinaryOp::Lte,
        BinaryOperator::Gt => BinaryOp::Gt,
        BinaryOperator::GtEq => BinaryOp::Gte,
        BinaryOperator::And => BinaryOp::And,
        BinaryOperator::Or => BinaryOp::Or,
        BinaryOperator::Plus => BinaryOp::Add,
        BinaryOperator::Minus => BinaryOp::Sub,
        BinaryOperator::Multiply => BinaryOp::Mul,
        BinaryOperator::Divide => BinaryOp::Div,
        _ => {
            return Err(Error::Unsupported(format!(
                "binary operator {operator} is not supported"
            )));
        }
    };
    let result = match operator {
        BinaryOp::Eq
        | BinaryOp::Ne
        | BinaryOp::Lt
        | BinaryOp::Lte
        | BinaryOp::Gt
        | BinaryOp::Gte => {
            if left != right {
                return Err(Error::Invalid(format!(
                    "cannot compare {left:?} with {right:?}"
                )));
            }
            ScalarType::Bool
        }
        BinaryOp::And | BinaryOp::Or => {
            if left != ScalarType::Bool || right != ScalarType::Bool {
                return Err(Error::Invalid("AND/OR operands must be boolean".into()));
            }
            ScalarType::Bool
        }
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div => {
            if !matches!(left, ScalarType::Int64 | ScalarType::Float64)
                || !matches!(right, ScalarType::Int64 | ScalarType::Float64)
            {
                return Err(Error::Invalid("arithmetic operands must be numeric".into()));
            }
            if left == ScalarType::Float64 || right == ScalarType::Float64 {
                ScalarType::Float64
            } else {
                ScalarType::Int64
            }
        }
    };
    Ok((operator, result))
}

fn fold_binary(mut expressions: Vec<Expr>, operator: BinaryOp) -> Result<Expr> {
    if expressions.is_empty() {
        return Err(Error::Invalid("expression list must not be empty".into()));
    }
    while expressions.len() > 1 {
        let mut level = Vec::with_capacity(expressions.len().div_ceil(2));
        let mut current = expressions.into_iter();
        while let Some(left) = current.next() {
            level.push(if let Some(right) = current.next() {
                Expr::Binary {
                    op: operator,
                    left: Box::new(left),
                    right: Box::new(right),
                }
            } else {
                left
            });
        }
        expressions = level;
    }
    Ok(expressions.pop().expect("non-empty expression level"))
}

fn collect_relation_scopes(relation: &Relation, scopes: &mut Vec<String>) {
    match relation {
        Relation::Scan { scope, .. }
        | Relation::Rows { scope, .. }
        | Relation::Ref { scope, .. }
        | Relation::RecursiveRef { scope, .. } => scopes.push(scope.clone()),
        Relation::Filter { input, predicate } => {
            collect_relation_scopes(input, scopes);
            collect_expr_relation_scopes(predicate, scopes);
        }
        Relation::Project {
            input,
            scope,
            fields,
            ..
        } => {
            collect_relation_scopes(input, scopes);
            scopes.extend(scope.iter().cloned());
            for field in fields {
                collect_expr_relation_scopes(&field.expression, scopes);
            }
        }
        Relation::Join {
            left, right, on, ..
        } => {
            collect_relation_scopes(left, scopes);
            collect_relation_scopes(right, scopes);
            collect_expr_relation_scopes(on, scopes);
        }
        Relation::Concatenate { scope, inputs } => {
            scopes.push(scope.clone());
            for input in inputs {
                collect_relation_scopes(input, scopes);
            }
        }
        Relation::Intersect {
            scope, left, right, ..
        }
        | Relation::Except {
            scope, left, right, ..
        } => {
            scopes.push(scope.clone());
            collect_relation_scopes(left, scopes);
            collect_relation_scopes(right, scopes);
        }
        Relation::Aggregate {
            input,
            scope,
            groups,
            terms,
        } => {
            collect_relation_scopes(input, scopes);
            scopes.extend(scope.iter().cloned());
            for group in groups {
                collect_expr_relation_scopes(&group.expression, scopes);
            }
            for term in terms {
                if let Some(argument) = &term.argument {
                    collect_expr_relation_scopes(argument, scopes);
                }
            }
        }
        Relation::Order { input, terms } => {
            collect_relation_scopes(input, scopes);
            for term in terms {
                collect_expr_relation_scopes(&term.expression, scopes);
            }
        }
        Relation::Slice { input, .. } | Relation::Distinct(input) => {
            collect_relation_scopes(input, scopes);
        }
        Relation::Recursive { anchor, step, .. } => {
            collect_relation_scopes(anchor, scopes);
            collect_relation_scopes(step, scopes);
        }
    }
}

fn collect_expr_relation_scopes(expression: &Expr, scopes: &mut Vec<String>) {
    match expression {
        Expr::Unary { expression, .. } | Expr::Cast { expression, .. } => {
            collect_expr_relation_scopes(expression, scopes);
        }
        Expr::Binary { left, right, .. } => {
            collect_expr_relation_scopes(left, scopes);
            collect_expr_relation_scopes(right, scopes);
        }
        Expr::Branch { arms, otherwise } => {
            for arm in arms {
                collect_expr_relation_scopes(&arm.when, scopes);
                collect_expr_relation_scopes(&arm.then, scopes);
            }
            collect_expr_relation_scopes(otherwise, scopes);
        }
        Expr::TextMatch { value, .. } => collect_expr_relation_scopes(value, scopes),
        Expr::Exists(relation)
        | Expr::First(relation)
        | Expr::Scalar(relation)
        | Expr::Array(relation) => collect_relation_scopes(relation, scopes),
        Expr::Literal(_) | Expr::Column { .. } => {}
    }
}

fn remap_relation_scopes(relation: &mut Relation, replacements: &HashMap<String, String>) {
    match relation {
        Relation::Scan { scope, .. }
        | Relation::Rows { scope, .. }
        | Relation::Ref { scope, .. }
        | Relation::RecursiveRef { scope, .. } => remap_scope(scope, replacements),
        Relation::Filter { input, predicate } => {
            remap_relation_scopes(input, replacements);
            remap_expr_scopes(predicate, replacements);
        }
        Relation::Project {
            input,
            scope,
            spread,
            fields,
        } => {
            remap_relation_scopes(input, replacements);
            if let Some(scope) = scope {
                remap_scope(scope, replacements);
            }
            for scope in spread {
                remap_scope(scope, replacements);
            }
            for field in fields {
                remap_expr_scopes(&mut field.expression, replacements);
            }
        }
        Relation::Join {
            left, right, on, ..
        } => {
            remap_relation_scopes(left, replacements);
            remap_relation_scopes(right, replacements);
            remap_expr_scopes(on, replacements);
        }
        Relation::Concatenate { scope, inputs } => {
            remap_scope(scope, replacements);
            for input in inputs {
                remap_relation_scopes(input, replacements);
            }
        }
        Relation::Intersect {
            scope, left, right, ..
        }
        | Relation::Except {
            scope, left, right, ..
        } => {
            remap_scope(scope, replacements);
            remap_relation_scopes(left, replacements);
            remap_relation_scopes(right, replacements);
        }
        Relation::Aggregate {
            input,
            scope,
            groups,
            terms,
        } => {
            remap_relation_scopes(input, replacements);
            if let Some(scope) = scope {
                remap_scope(scope, replacements);
            }
            for group in groups {
                remap_expr_scopes(&mut group.expression, replacements);
            }
            for term in terms {
                if let Some(argument) = &mut term.argument {
                    remap_expr_scopes(argument, replacements);
                }
            }
        }
        Relation::Order { input, terms } => {
            remap_relation_scopes(input, replacements);
            for term in terms {
                remap_expr_scopes(&mut term.expression, replacements);
            }
        }
        Relation::Slice { input, .. } | Relation::Distinct(input) => {
            remap_relation_scopes(input, replacements);
        }
        Relation::Recursive { anchor, step, .. } => {
            remap_relation_scopes(anchor, replacements);
            remap_relation_scopes(step, replacements);
        }
    }
}

fn remap_expr_scopes(expression: &mut Expr, replacements: &HashMap<String, String>) {
    match expression {
        Expr::Column { scope, .. } => remap_scope(scope, replacements),
        Expr::Unary { expression, .. } | Expr::Cast { expression, .. } => {
            remap_expr_scopes(expression, replacements);
        }
        Expr::Binary { left, right, .. } => {
            remap_expr_scopes(left, replacements);
            remap_expr_scopes(right, replacements);
        }
        Expr::Branch { arms, otherwise } => {
            for arm in arms {
                remap_expr_scopes(&mut arm.when, replacements);
                remap_expr_scopes(&mut arm.then, replacements);
            }
            remap_expr_scopes(otherwise, replacements);
        }
        Expr::TextMatch { value, .. } => remap_expr_scopes(value, replacements),
        Expr::Exists(relation)
        | Expr::First(relation)
        | Expr::Scalar(relation)
        | Expr::Array(relation) => remap_relation_scopes(relation, replacements),
        Expr::Literal(_) => {}
    }
}

fn remap_scope(scope: &mut String, replacements: &HashMap<String, String>) {
    if let Some(replacement) = replacements.get(scope) {
        *scope = replacement.clone();
    }
}

fn null_safe_equality(left_scope: &str, right_scope: &str, name: &str) -> Expr {
    let left = Expr::Column {
        scope: left_scope.to_owned(),
        name: name.to_owned(),
    };
    let right = Expr::Column {
        scope: right_scope.to_owned(),
        name: name.to_owned(),
    };
    Expr::Binary {
        op: BinaryOp::Or,
        left: Box::new(Expr::Binary {
            op: BinaryOp::Eq,
            left: Box::new(left.clone()),
            right: Box::new(right.clone()),
        }),
        right: Box::new(Expr::Binary {
            op: BinaryOp::And,
            left: Box::new(Expr::Unary {
                op: UnaryOp::IsNull,
                expression: Box::new(left),
            }),
            right: Box::new(Expr::Unary {
                op: UnaryOp::IsNull,
                expression: Box::new(right),
            }),
        }),
    }
}

fn bool_literal(value: bool) -> Expr {
    Expr::Literal(Literal {
        raw: RawScalar::Bool(value),
        kind: Some(Kind::Bool),
    })
}

fn like_pattern(pattern: &str, escape: Option<char>) -> Result<Vec<TextMatchPart>> {
    let mut parts = Vec::new();
    let mut literal = String::new();
    let mut characters = pattern.chars();
    while let Some(character) = characters.next() {
        if escape == Some(character) {
            let escaped = characters.next().ok_or_else(|| {
                Error::Invalid("LIKE pattern ends with its escape character".into())
            })?;
            literal.push(escaped);
            continue;
        }
        match character {
            '%' => {
                if !literal.is_empty() {
                    parts.push(TextMatchPart::Literal(std::mem::take(&mut literal)));
                }
                if !matches!(parts.last(), Some(TextMatchPart::AnyMany)) {
                    parts.push(TextMatchPart::AnyMany);
                }
            }
            '_' => {
                return Err(Error::Unsupported(
                    "LIKE '_' needs a one-rune wildcard in LIR".into(),
                ));
            }
            character => literal.push(character),
        }
    }
    if !literal.is_empty() {
        parts.push(TextMatchPart::Literal(literal));
    }
    if parts.is_empty() {
        parts.push(TextMatchPart::Literal(String::new()));
    }
    Ok(parts)
}

fn literal_type(value: &SqlValue, expected: Option<ScalarType>) -> Result<ScalarType> {
    match value {
        SqlValue::Number(value, _) => Ok(expected.unwrap_or_else(|| {
            if value.contains(['.', 'e', 'E']) {
                ScalarType::Float64
            } else {
                ScalarType::Int64
            }
        })),
        SqlValue::Boolean(_) => Ok(expected.unwrap_or(ScalarType::Bool)),
        SqlValue::Null | SqlValue::Placeholder(_) => {
            expected.ok_or_else(|| Error::Invalid(format!("cannot infer the type of {value}")))
        }
        value if value.clone().into_string().is_some() => Ok(expected.unwrap_or(ScalarType::Text)),
        _ => Err(Error::Unsupported(format!(
            "literal {value} is not supported"
        ))),
    }
}

fn raw_sql_value(
    value: &SqlValue,
    expected: ScalarType,
    inferred: &mut Vec<Option<ScalarType>>,
    hints: &[Option<ScalarType>],
    parameters: Option<&[Parameter]>,
) -> Result<RawScalar> {
    match value {
        SqlValue::Placeholder(name) => {
            let index = placeholder_index(name)?;
            if inferred.len() <= index {
                inferred.resize(index + 1, None);
            }
            let hinted = hints.get(index).copied().flatten();
            if let Some(hinted) = hinted
                && hinted != expected
            {
                return Err(Error::Invalid(format!(
                    "parameter ${} is hinted as {hinted:?}, expected {expected:?}",
                    index + 1
                )));
            }
            if let Some(previous) = inferred[index]
                && previous != expected
            {
                return Err(Error::Invalid(format!(
                    "parameter ${} is used as both {previous:?} and {expected:?}",
                    index + 1
                )));
            }
            inferred[index] = Some(expected);
            Ok(parameters
                .and_then(|parameters| parameters.get(index))
                .map(|parameter| parameter.value.clone())
                .unwrap_or(RawScalar::Null))
        }
        SqlValue::Number(value, _) => Ok(RawScalar::Number(value.clone())),
        SqlValue::Boolean(value) => Ok(RawScalar::Bool(*value)),
        SqlValue::Null => Ok(RawScalar::Null),
        value => {
            let text = value
                .clone()
                .into_string()
                .ok_or_else(|| Error::Unsupported(format!("literal {value} is not supported")))?;
            match expected {
                ScalarType::Text => Ok(RawScalar::Text(text)),
                ScalarType::Int64 => text
                    .parse::<i64>()
                    .map(|_| RawScalar::Number(text.clone()))
                    .map_err(|_| Error::Invalid(format!("invalid int64 literal {text:?}"))),
                ScalarType::Float64 => text
                    .parse::<f64>()
                    .ok()
                    .filter(|value| value.is_finite())
                    .map(|_| RawScalar::Number(text.clone()))
                    .ok_or_else(|| Error::Invalid(format!("invalid float64 literal {text:?}"))),
                ScalarType::Bool => match text.to_ascii_lowercase().as_str() {
                    "true" | "t" | "1" => Ok(RawScalar::Bool(true)),
                    "false" | "f" | "0" => Ok(RawScalar::Bool(false)),
                    _ => Err(Error::Invalid(format!("invalid boolean literal {text:?}"))),
                },
            }
        }
    }
}

fn placeholder_index(value: &str) -> Result<usize> {
    value
        .strip_prefix('$')
        .and_then(|value| value.parse::<usize>().ok())
        .and_then(|value| value.checked_sub(1))
        .ok_or_else(|| Error::Invalid(format!("invalid PostgreSQL parameter {value:?}")))
}

fn is_placeholder(expression: &SqlExpr) -> bool {
    matches!(expression, SqlExpr::Value(value) if matches!(value.value, SqlValue::Placeholder(_)))
}

fn is_string_literal(expression: &SqlExpr) -> bool {
    matches!(expression, SqlExpr::Value(value) if value.value.clone().into_string().is_some())
}

fn scalar_type(data_type: &DataType) -> Result<(ScalarType, String)> {
    let value = data_type.to_string().to_ascii_lowercase();
    if value.starts_with("timestamp") && value.contains("with time zone") {
        return Ok((ScalarType::Int64, "timestamptz".into()));
    }
    let base = value.split(['(', '[', ' ']).next().unwrap_or(&value);
    let result = match base {
        "text" => (ScalarType::Text, "text"),
        "varchar" | "character" | "char" | "name" => (ScalarType::Text, ""),
        "uuid" => (ScalarType::Text, "uuid"),
        "json" => (ScalarType::Text, "json"),
        "jsonb" => (ScalarType::Text, "jsonb"),
        "bytea" => (ScalarType::Text, "bytea"),
        "smallint" | "integer" | "int" | "int2" | "int4" | "int8" | "bigint" => {
            (ScalarType::Int64, "")
        }
        "timestamp" => (ScalarType::Int64, "timestamp"),
        "timestamptz" => (ScalarType::Int64, "timestamptz"),
        "real" | "float" | "float4" | "float8" | "double" | "numeric" | "decimal" => {
            (ScalarType::Float64, "")
        }
        "boolean" | "bool" => (ScalarType::Bool, ""),
        _ => {
            return Err(Error::Unsupported(format!(
                "PostgreSQL type {data_type} is not supported"
            )));
        }
    };
    Ok((result.0, result.1.to_owned()))
}

fn default_value(expression: &SqlExpr, scalar_type: ScalarType) -> Result<Option<DefaultValue>> {
    let expression = match expression {
        SqlExpr::Cast { expr, .. } | SqlExpr::Nested(expr) => expr.as_ref(),
        _ => expression,
    };
    if let SqlExpr::Function(function) = expression {
        let name = object_name(&function.name)?.to_ascii_lowercase();
        if matches!(name.as_str(), "now" | "current_timestamp") {
            return Ok(Some(DefaultValue {
                function: Some(DefaultFunction::NowMs),
                ..DefaultValue::default()
            }));
        }
        if matches!(name.as_str(), "gen_random_uuid" | "uuid_generate_v4") {
            return Ok(Some(DefaultValue {
                function: Some(DefaultFunction::Uuid),
                ..DefaultValue::default()
            }));
        }
    }
    if expression
        .to_string()
        .eq_ignore_ascii_case("current_timestamp")
    {
        return Ok(Some(DefaultValue {
            function: Some(DefaultFunction::NowMs),
            ..DefaultValue::default()
        }));
    }
    if let SqlExpr::UnaryOp {
        op: UnaryOperator::Minus,
        expr,
    } = expression
        && let SqlExpr::Value(value) = expr.as_ref()
        && let SqlValue::Number(value, _) = &value.value
    {
        return default_value(
            &SqlExpr::Value(sqlparser::ast::ValueWithSpan::from(SqlValue::Number(
                format!("-{value}"),
                false,
            ))),
            scalar_type,
        );
    }
    let SqlExpr::Value(value) = expression else {
        return Err(Error::Unsupported(format!(
            "default {expression} is not supported"
        )));
    };
    let mut default = DefaultValue::default();
    match (
        scalar_type,
        raw_sql_value(&value.value, scalar_type, &mut Vec::new(), &[], None)?,
    ) {
        (ScalarType::Text, RawScalar::Text(value)) => default.text = value,
        (ScalarType::Int64, RawScalar::Number(value)) => {
            default.int64 = value
                .parse()
                .map_err(|_| Error::Invalid(format!("invalid integer default {value:?}")))?;
        }
        (ScalarType::Int64, RawScalar::Text(value)) => {
            default.int64 = value
                .parse()
                .map_err(|_| Error::Invalid(format!("invalid integer default {value:?}")))?;
        }
        (ScalarType::Float64, RawScalar::Number(value)) => {
            default.float64 = value
                .parse()
                .map_err(|_| Error::Invalid(format!("invalid float default {value:?}")))?;
        }
        (ScalarType::Float64, RawScalar::Text(value)) => {
            default.float64 = value
                .parse()
                .map_err(|_| Error::Invalid(format!("invalid float default {value:?}")))?;
        }
        (ScalarType::Bool, RawScalar::Bool(value)) => default.bool_value = value,
        (ScalarType::Bool, RawScalar::Text(value)) => {
            default.bool_value = match value.to_ascii_lowercase().as_str() {
                "true" | "t" | "1" => true,
                "false" | "f" | "0" => false,
                _ => {
                    return Err(Error::Invalid(format!("invalid boolean default {value:?}")));
                }
            };
        }
        (_, RawScalar::Null) => return Ok(None),
        _ => {
            return Err(Error::Invalid(format!(
                "default {expression} has the wrong type"
            )));
        }
    }
    Ok(Some(default))
}

fn index_columns(columns: &[sqlparser::ast::IndexColumn]) -> Result<Vec<String>> {
    columns
        .iter()
        .map(|column| match &column.column.expr {
            SqlExpr::Identifier(value) => Ok(identifier(value)),
            expression => Err(Error::Unsupported(format!(
                "index expression {expression} is not supported"
            ))),
        })
        .collect()
}

fn object_name(name: &ObjectName) -> Result<String> {
    let parts = &name.0;
    let last = parts
        .last()
        .ok_or_else(|| Error::Invalid("empty object name".into()))?;
    if parts.len() > 2 {
        return Err(Error::Unsupported(format!(
            "catalog-qualified name {name} is not supported"
        )));
    }
    if parts.len() == 2 {
        let schema = parts[0].as_ident().map(identifier).unwrap_or_default();
        if schema != "public" {
            return Err(Error::Unsupported(format!(
                "schema {schema:?} is not supported"
            )));
        }
    }
    last.as_ident()
        .map(identifier)
        .ok_or_else(|| Error::Unsupported(format!("object name {name} is not an identifier")))
}

fn identifier(identifier: &Ident) -> String {
    if identifier.quote_style.is_some() {
        identifier.value.clone()
    } else {
        identifier.value.to_ascii_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::catalog::identity::{
        ColumnId, DefinitionGeneration, ExistenceGeneration, SchemaId, TableId, ValueGeneration,
        WriteProtocolGeneration,
    };
    use crate::engine::catalog::model::{Column, Index};

    use super::*;

    fn users() -> Table {
        Table {
            id: TableId::new("users"),
            schema_id: SchemaId::new(1).unwrap(),
            name: "users".into(),
            definition_generation: DefinitionGeneration::default(),
            existence_generation: ExistenceGeneration::default(),
            write_protocol_generation: WriteProtocolGeneration::default(),
            columns: vec![
                Column {
                    id: ColumnId::new("users-id"),
                    schema_id: SchemaId::new(1).unwrap(),
                    name: "id".into(),
                    value_generation: ValueGeneration::default(),
                    scalar_type: ScalarType::Int64,
                    nullable: false,
                    format: String::new(),
                    insert_default: None,
                    missing_value: None,
                },
                Column {
                    id: ColumnId::new("users-name"),
                    schema_id: SchemaId::new(2).unwrap(),
                    name: "name".into(),
                    value_generation: ValueGeneration::default(),
                    scalar_type: ScalarType::Text,
                    nullable: false,
                    format: String::new(),
                    insert_default: None,
                    missing_value: None,
                },
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::<Index>::new(),
            foreign_keys: Vec::new(),
            constraints: Vec::new(),
        }
    }

    #[test]
    fn select_lowers_to_ordered_lir() {
        let compiled = compile_sql(
            "SELECT id, name FROM users WHERE id = 7 ORDER BY name DESC LIMIT 2",
            &[users()],
        )
        .unwrap();
        assert_eq!(compiled.kind, CommandKind::Select);
        assert_eq!(compiled.result_columns.len(), 2);
        let program = compiled.program.unwrap();
        let Statement::Query { relation, .. } = &program.statements[0] else {
            panic!("expected query statement")
        };
        assert!(matches!(relation.root, Relation::Project { .. }));
    }

    #[test]
    fn insert_parameter_types_come_from_target_columns() {
        let prepared = prepare(
            "INSERT INTO users (id, name) VALUES ($1, $2) RETURNING id, name",
            &[users()],
            &[],
        )
        .unwrap();
        assert_eq!(
            prepared.parameter_types(),
            &[ScalarType::Int64, ScalarType::Text]
        );
        assert_eq!(prepared.result_columns().len(), 2);
    }

    #[test]
    fn postgres_string_literals_coerce_to_the_column_type() {
        compile_sql("SELECT id FROM users WHERE id = '7'", &[users()]).unwrap();
    }

    #[test]
    fn accepts_ent_aggregate_alias_named_sort() {
        let statements = parse("SELECT min(name) sort FROM users").unwrap();
        assert_eq!(statements.len(), 1);
    }

    #[test]
    fn distinct_preserves_explicit_output_order() {
        let compiled = compile_sql(
            "SELECT DISTINCT name AS label, id AS position FROM users ORDER BY position NULLS FIRST, label DESC NULLS LAST",
            &[users()],
        )
        .unwrap();
        let program = compiled.program.unwrap();
        let Statement::Query { relation, .. } = &program.statements[0] else {
            panic!("expected query statement")
        };
        let Relation::Order { input, terms } = &relation.root else {
            panic!("expected distinct output to be ordered")
        };
        assert!(matches!(input.as_ref(), Relation::Distinct(_)));
        assert_eq!(terms.len(), 4);
        assert!(matches!(
            &terms[1].expression,
            Expr::Column { name, .. } if name == "position"
        ));
        assert!(terms[0].descending);
        assert!(matches!(
            &terms[3].expression,
            Expr::Column { name, .. } if name == "label"
        ));
        assert!(!terms[2].descending);
        assert!(terms[3].descending);
    }

    #[test]
    fn large_in_lists_lower_to_balanced_predicates() {
        let values = (0..4096)
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let compiled = compile_sql(
            &format!("SELECT id FROM users WHERE id IN ({values})"),
            &[users()],
        )
        .unwrap();
        let program = compiled.program.unwrap();
        let Statement::Query { relation, .. } = &program.statements[0] else {
            panic!("expected query statement")
        };
        let Relation::Project { input, .. } = &relation.root else {
            panic!("expected projected query")
        };
        let Relation::Order { input, .. } = input.as_ref() else {
            panic!("expected ordered query")
        };
        let Relation::Filter { predicate, .. } = input.as_ref() else {
            panic!("expected filtered query")
        };
        let mut pending = vec![(predicate, 1usize)];
        let mut depth = 0;
        while let Some((expression, current)) = pending.pop() {
            depth = depth.max(current);
            if let Expr::Binary { left, right, .. } = expression {
                pending.push((left, current + 1));
                pending.push((right, current + 1));
            }
        }
        assert!(depth <= 14, "balanced IN predicate depth was {depth}");
    }

    #[test]
    fn create_table_preserves_postgres_wire_type_metadata() {
        let compiled = compile_sql(
            "CREATE TABLE events (id uuid NOT NULL PRIMARY KEY, body jsonb NOT NULL, occurred_at timestamp with time zone NOT NULL)",
            &[],
        )
        .unwrap();
        let program = compiled.program.unwrap();
        let Statement::CreateTable { table, .. } = &program.statements[0] else {
            panic!("expected create table statement")
        };
        assert_eq!(table.columns[0].scalar_type, ScalarType::Text);
        assert_eq!(table.columns[0].format, "uuid");
        assert_eq!(table.columns[1].format, "jsonb");
        assert_eq!(table.columns[2].scalar_type, ScalarType::Int64);
        assert_eq!(table.columns[2].format, "timestamptz");
    }

    #[test]
    fn lowers_relational_sql_surface() {
        let table = users();
        for sql in [
            "SELECT u.id, CASE WHEN u.name ILIKE 'a%' THEN u.name ELSE 'other' END AS label FROM users u LEFT JOIN users p ON p.id = u.id WHERE EXISTS (SELECT 1 FROM users x WHERE x.id = u.id)",
            "WITH named(id, name) AS (SELECT id, name FROM users) SELECT id FROM named",
            "SELECT id FROM users UNION ALL SELECT id FROM users",
            "SELECT id FROM users INTERSECT SELECT id FROM users",
            "SELECT id FROM users EXCEPT SELECT id FROM users",
            "SELECT name, id FROM users ORDER BY 2 DESC NULLS LAST",
            "SELECT ALL id FROM users",
            "WITH RECURSIVE nums(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM nums WHERE n < 3) SELECT n FROM nums",
            "SELECT id, COUNT(*) + 1 AS total FROM users GROUP BY id HAVING COUNT(*) > 0 ORDER BY total DESC",
            "SELECT id AS key, COUNT(*) FROM users GROUP BY 1 ORDER BY key",
            "SELECT COALESCE(MAX(name), 'none') AS latest FROM users",
            "SELECT id, COUNT(*), COUNT(DISTINCT name), MAX(name) FROM users GROUP BY id",
        ] {
            compile_sql(sql, std::slice::from_ref(&table))
                .unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
    }

    #[test]
    fn lowers_mutation_relation_sources_and_conflicts() {
        let table = users();
        for sql in [
            "INSERT INTO users (id, name) SELECT id + 10, name FROM users",
            "INSERT INTO users (id, name) VALUES (1, 'a') ON CONFLICT (id) DO NOTHING RETURNING id",
            "INSERT INTO users (id, name) VALUES (1, 'a') ON CONFLICT (id) DO UPDATE SET name = excluded.name RETURNING id, name",
            "UPDATE users AS u SET name = source.name FROM users AS source WHERE u.id = source.id RETURNING id",
            "DELETE FROM users AS u USING users AS source WHERE u.id = source.id RETURNING id",
        ] {
            compile_sql(sql, std::slice::from_ref(&table))
                .unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
    }
}
