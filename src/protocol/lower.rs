//! Mechanical lowering from Schemancer wire types into engine IR.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::engine::exec::ErrorReason;
use crate::engine::lir;
use crate::protocol::generated::lir as wire;
use crate::protocol::generated::pir as pir_wire;

#[derive(Debug, thiserror::Error)]
#[error("protocol: {message}")]
pub struct LowerError {
    reason: ErrorReason,
    message: String,
}

impl LowerError {
    fn new(message: impl Into<String>) -> Self {
        Self::with_reason(ErrorReason::SchemaViolation, message)
    }

    fn with_reason(reason: ErrorReason, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }

    pub fn reason(&self) -> ErrorReason {
        self.reason
    }
}

pub type LowerResult<T> = std::result::Result<T, LowerError>;

pub fn lower_pir(program: pir_wire::Program) -> LowerResult<crate::engine::exec::Program> {
    Ok(crate::engine::exec::Program {
        result: pir_optional(program.result),
        statements: program
            .statements
            .into_iter()
            .map(lower_statement)
            .collect::<LowerResult<_>>()?,
    })
}

fn lower_statement(statement: pir_wire::Statement) -> LowerResult<crate::engine::exec::Statement> {
    use crate::engine::exec::Statement;
    Ok(match statement {
        pir_wire::Statement::QueryStatement(statement) => Statement::Query {
            name: statement.name,
            relation: lower_relation(statement.relation)?,
        },
        pir_wire::Statement::CreateStatement(statement) => Statement::Create {
            name: statement.name,
            relation: lower_relation(statement.relation)?,
            table: statement.table,
        },
        pir_wire::Statement::UpdateStatement(statement) => Statement::Update {
            name: statement.name,
            relation: lower_relation(statement.relation)?,
            table: statement.table,
        },
        pir_wire::Statement::DeleteStatement(statement) => Statement::Delete {
            name: statement.name,
            relation: lower_relation(statement.relation)?,
            table: statement.table,
        },
        pir_wire::Statement::CreateTableStatement(statement) => Statement::CreateTable {
            name: statement.name,
            table: lower_table(statement.table)?,
        },
        pir_wire::Statement::RenameTableStatement(statement) => Statement::RenameTable {
            name: statement.name,
            table_id: schema_id(statement.table_id)?,
            to: statement.to,
        },
        pir_wire::Statement::DeleteTableStatement(statement) => Statement::DeleteTable {
            name: statement.name,
            table_id: schema_id(statement.table_id)?,
        },
        pir_wire::Statement::CreateColumnStatement(statement) => Statement::CreateColumn {
            name: statement.name,
            table_id: schema_id(statement.table_id)?,
            column: lower_column(statement.column)?,
        },
        pir_wire::Statement::RenameColumnStatement(statement) => Statement::RenameColumn {
            name: statement.name,
            table_id: schema_id(statement.table_id)?,
            column_id: schema_id(statement.column_id)?,
            to: statement.to,
        },
        pir_wire::Statement::ChangeColumnDefaultStatement(statement) => {
            Statement::ChangeColumnDefault {
                name: statement.name,
                table_id: schema_id(statement.table_id)?,
                column_id: schema_id(statement.column_id)?,
                default: pir_optional(statement.default)
                    .map(lower_default_spec)
                    .transpose()?,
            }
        }
        pir_wire::Statement::DeleteColumnStatement(statement) => Statement::DeleteColumn {
            name: statement.name,
            table_id: schema_id(statement.table_id)?,
            column_id: schema_id(statement.column_id)?,
        },
        pir_wire::Statement::CreateIndexStatement(statement) => Statement::CreateIndex {
            name: statement.name,
            table_id: schema_id(statement.table_id)?,
            index: lower_index(statement.index),
        },
        pir_wire::Statement::DeleteIndexStatement(statement) => Statement::DeleteIndex {
            name: statement.name,
            table_id: schema_id(statement.table_id)?,
            index: statement.index,
        },
        pir_wire::Statement::StartIndexBuildStatement(statement) => Statement::StartIndexBuild {
            name: statement.name,
            table_id: schema_id(statement.table_id)?,
            index: lower_index(statement.index),
            prerequisites: transitions(pir_optional(statement.prerequisites).unwrap_or_default()),
            after: pir_optional(statement.after).unwrap_or_default(),
        },
        pir_wire::Statement::StartColumnReplacementStatement(statement) => {
            Statement::StartColumnReplacement {
                name: statement.name,
                table_id: schema_id(statement.table_id)?,
                column_id: schema_id(statement.column_id)?,
                replacement: lower_replacement(statement.replacement)?,
                after: pir_optional(statement.after).unwrap_or_default(),
            }
        }
        pir_wire::Statement::StartConstraintValidationStatement(statement) => {
            Statement::StartConstraintValidation {
                name: statement.name,
                table_id: schema_id(statement.table_id)?,
                constraint: lower_constraint(statement.constraint)?,
                after: pir_optional(statement.after).unwrap_or_default(),
            }
        }
    })
}

fn lower_relation(raw: crate::protocol::RawJson) -> LowerResult<lir::Query> {
    let query = serde_json::from_str::<wire::Query>(raw.as_str())
        .map_err(|error| LowerError::new(format!("invalid LIR relation: {error}")))?;
    lower_lir(query)
}

fn lower_table(
    table: pir_wire::TableDefinition,
) -> LowerResult<crate::engine::catalog::model::TableDraft> {
    Ok(crate::engine::catalog::model::TableDraft {
        id: pir_optional(table.id).map(schema_id).transpose()?,
        name: table.name,
        columns: table
            .columns
            .into_iter()
            .map(lower_column)
            .collect::<LowerResult<_>>()?,
        primary_key: table.primary_key,
        indexes: pir_optional(table.indexes)
            .unwrap_or_default()
            .into_iter()
            .map(lower_index)
            .collect(),
        foreign_keys: pir_optional(table.foreign_keys)
            .unwrap_or_default()
            .into_iter()
            .map(|foreign_key| crate::engine::catalog::model::ForeignKeyDef {
                name: foreign_key.name,
                columns: foreign_key.columns,
                ref_table: foreign_key.ref_table,
                ref_columns: foreign_key.ref_columns,
            })
            .collect(),
    })
}

fn lower_column(
    column: pir_wire::ColumnDefinition,
) -> LowerResult<crate::engine::catalog::model::ColumnDraft> {
    let scalar_type = pir_scalar_type(column.r#type);
    Ok(crate::engine::catalog::model::ColumnDraft {
        id: pir_optional(column.id).map(schema_id).transpose()?,
        name: column.name,
        scalar_type,
        nullable: pir_optional(column.nullable).unwrap_or(false),
        format: pir_optional(column.format).unwrap_or_default(),
        default: pir_optional(column.default)
            .map(|default| lower_default(default, scalar_type))
            .transpose()?,
    })
}

fn lower_index(index: pir_wire::IndexDefinition) -> crate::engine::catalog::model::IndexDef {
    crate::engine::catalog::model::IndexDef {
        name: index.name,
        columns: index.columns,
        unique: pir_optional(index.unique).unwrap_or(false),
    }
}

fn lower_replacement(
    replacement: pir_wire::ColumnReplacementDefinition,
) -> LowerResult<crate::engine::catalog::model::ColumnReplacementDef> {
    let scalar_type = pir_scalar_type(replacement.r#type);
    Ok(crate::engine::catalog::model::ColumnReplacementDef {
        scalar_type,
        nullable: replacement.nullable,
        format: pir_optional(replacement.format).unwrap_or_default(),
        default: pir_optional(replacement.default)
            .map(|default| lower_default(default, scalar_type))
            .transpose()?,
        conversion: crate::engine::catalog::model::ColumnConversion::StrictBuiltin,
        prerequisites: transitions(pir_optional(replacement.prerequisites).unwrap_or_default()),
    })
}

fn lower_constraint(
    constraint: pir_wire::ConstraintValidationDefinition,
) -> LowerResult<crate::engine::catalog::model::ConstraintDef> {
    if constraint.kind != "not_null" {
        return Err(LowerError::new(format!(
            "unsupported constraint kind {:?}",
            constraint.kind
        )));
    }
    Ok(crate::engine::catalog::model::ConstraintDef {
        name: constraint.name,
        kind: crate::engine::catalog::model::ConstraintKind::NotNull,
        column_id: schema_id(constraint.column_id)?,
        prerequisites: transitions(pir_optional(constraint.prerequisites).unwrap_or_default()),
    })
}

fn lower_default(
    default: pir_wire::ColumnDefault,
    scalar_type: crate::engine::catalog::model::ScalarType,
) -> LowerResult<crate::engine::catalog::model::DefaultValue> {
    crate::engine::exec::resolve_default(lower_default_spec(default)?, scalar_type)
        .map_err(|error| LowerError::new(error.to_string()))
}

fn lower_default_spec(
    default: pir_wire::ColumnDefault,
) -> LowerResult<crate::engine::exec::DefaultSpec> {
    use crate::engine::catalog::model::DefaultFunction;
    use crate::engine::exec::DefaultSpec;
    Ok(match default {
        pir_wire::ColumnDefault::GeneratorDefault(default) => {
            DefaultSpec::Generator(match default.func {
                pir_wire::GeneratorDefaultFunc::UUID => DefaultFunction::Uuid,
                pir_wire::GeneratorDefaultFunc::NowMs => DefaultFunction::NowMs,
            })
        }
        pir_wire::ColumnDefault::LiteralDefault(default) => {
            let value = serde_json::from_str::<serde_json::Value>(default.value.as_str())
                .map_err(|error| LowerError::new(format!("invalid literal default: {error}")))?;
            match value {
                serde_json::Value::String(text) => DefaultSpec::Text(text),
                serde_json::Value::Bool(value) => DefaultSpec::Bool(value),
                serde_json::Value::Number(value) => DefaultSpec::Number(value.to_string()),
                _ => return Err(LowerError::new("literal default must be a non-null scalar")),
            }
        }
    })
}

fn schema_id(value: i64) -> LowerResult<crate::engine::catalog::identity::SchemaId> {
    let value =
        u32::try_from(value).map_err(|_| LowerError::new(format!("invalid schema ID {value}")))?;
    crate::engine::catalog::identity::SchemaId::new(value)
        .map_err(|error| LowerError::new(error.to_string()))
}

fn transitions(values: Vec<String>) -> Vec<crate::engine::catalog::identity::TransitionId> {
    values.into_iter().map(Into::into).collect()
}

fn pir_scalar_type(value: pir_wire::ColumnType) -> crate::engine::catalog::model::ScalarType {
    match value {
        pir_wire::ColumnType::Text => crate::engine::catalog::model::ScalarType::Text,
        pir_wire::ColumnType::Int64 => crate::engine::catalog::model::ScalarType::Int64,
        pir_wire::ColumnType::Float64 => crate::engine::catalog::model::ScalarType::Float64,
        pir_wire::ColumnType::Bool => crate::engine::catalog::model::ScalarType::Bool,
    }
}

fn pir_optional<T>(value: pir_wire::OptionalField<T>) -> Option<T> {
    match value {
        pir_wire::OptionalField::Missing => None,
        pir_wire::OptionalField::Present(value) => Some(value),
    }
}

pub fn lower_lir(query: wire::Query) -> LowerResult<lir::Query> {
    let wire::Query {
        bindings,
        nodes,
        root,
    } = query;
    let mut graph = Graph {
        nodes,
        building: HashSet::new(),
        reached: HashSet::new(),
    };
    let root_relation = graph.relation(&root.node)?;
    let mut lowered_bindings = HashMap::new();
    for (name, binding) in optional(bindings).unwrap_or_default() {
        let relation = match binding {
            wire::Binding::DerivedBinding(binding) => graph.relation(&binding.node)?,
            wire::Binding::RecursiveBinding(binding) => lir::Relation::Recursive {
                anchor: Box::new(graph.relation(&binding.anchor)?),
                step: Box::new(graph.relation(&binding.step)?),
                accumulation: match binding.accumulation {
                    wire::RecursiveBindingAccumulation::All => lir::RecursiveAccumulation::All,
                    wire::RecursiveBindingAccumulation::New => lir::RecursiveAccumulation::New,
                },
            },
        };
        lowered_bindings.insert(name, relation);
    }
    if graph.reached.len() != graph.nodes.len() {
        let mut orphaned = graph
            .nodes
            .keys()
            .filter(|name| !graph.reached.contains(*name))
            .cloned()
            .collect::<Vec<_>>();
        orphaned.sort();
        return Err(LowerError::with_reason(
            ErrorReason::UnreachableNode,
            format!("unreachable node definitions: {orphaned:?}"),
        ));
    }
    Ok(lir::Query {
        root: root_relation,
        cardinality: root_cardinality(root.cardinality),
        bindings: lowered_bindings,
    })
}

struct Graph {
    nodes: BTreeMap<String, wire::Node>,
    building: HashSet<String>,
    reached: HashSet<String>,
}

impl Graph {
    fn relation(&mut self, name: &str) -> LowerResult<lir::Relation> {
        if name.is_empty() {
            return Err(LowerError::new("missing node reference"));
        }
        if self.building.contains(name) {
            return Err(LowerError::with_reason(
                ErrorReason::NodeCycle,
                format!("node {name:?} is part of a cycle"),
            ));
        }
        if !self.reached.insert(name.to_owned()) {
            return Err(LowerError::with_reason(
                ErrorReason::SharedNode,
                format!(
                    "node {name:?} has more than one consumer and would create a duplicate scope"
                ),
            ));
        }
        let node = self.nodes.get(name).cloned().ok_or_else(|| {
            LowerError::with_reason(ErrorReason::UnknownNode, format!("unknown node {name:?}"))
        })?;
        self.building.insert(name.to_owned());
        let result = self.lower_node(node);
        self.building.remove(name);
        result
    }

    fn lower_node(&mut self, node: wire::Node) -> LowerResult<lir::Relation> {
        Ok(match node {
            wire::Node::ScanNode(node) => lir::Relation::Scan {
                table: node.table,
                scope: node.scope,
            },
            wire::Node::RowsNode(node) => {
                let columns = node
                    .columns
                    .into_iter()
                    .map(|column| lir::RowsColumn {
                        name: column.name,
                        kind: scalar_kind(column.r#type),
                        nullable: optional(column.nullable).unwrap_or(false),
                    })
                    .collect::<Vec<_>>();
                let values = node
                    .rows
                    .into_iter()
                    .enumerate()
                    .map(|(row_index, row)| {
                        if row.len() != columns.len() {
                            return Err(LowerError::new(format!(
                                "row {row_index} has {} cells, want {}",
                                row.len(),
                                columns.len()
                            )));
                        }
                        row.into_iter()
                            .zip(&columns)
                            .map(|(cell, column)| lower_cell(cell, column.kind))
                            .collect::<LowerResult<Vec<_>>>()
                    })
                    .collect::<LowerResult<Vec<_>>>()?;
                lir::Relation::Rows {
                    scope: node.scope,
                    columns,
                    values,
                }
            }
            wire::Node::FilterNode(node) => lir::Relation::Filter {
                input: Box::new(self.relation(&node.input)?),
                predicate: self.expression(node.predicate)?,
            },
            wire::Node::ProjectNode(node) => lir::Relation::Project {
                input: Box::new(self.relation(&node.input)?),
                scope: optional(node.scope),
                spread: optional(node.spread).unwrap_or_default(),
                fields: optional(node.fields)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|field| {
                        Ok(lir::ProjectField {
                            name: field.r#as,
                            expression: self.expression(field.expr)?,
                        })
                    })
                    .collect::<LowerResult<Vec<_>>>()?,
            },
            wire::Node::JoinNode(node) => lir::Relation::Join {
                left: Box::new(self.relation(&node.left)?),
                right: Box::new(self.relation(&node.right)?),
                kind: match node.join {
                    wire::JoinNodeJoin::Inner => lir::JoinKind::Inner,
                    wire::JoinNodeJoin::Left => lir::JoinKind::Left,
                },
                on: self.expression(node.on)?,
            },
            wire::Node::ConcatenateNode(node) => lir::Relation::Concatenate {
                scope: node.scope,
                inputs: node
                    .inputs
                    .into_iter()
                    .map(|name| self.relation(&name))
                    .collect::<LowerResult<_>>()?,
            },
            wire::Node::IntersectNode(node) => lir::Relation::Intersect {
                scope: node.scope,
                left: Box::new(self.relation(&node.left)?),
                right: Box::new(self.relation(&node.right)?),
                quantifier: match node.quantifier {
                    wire::IntersectNodeQuantifier::All => lir::SetQuantifier::All,
                    wire::IntersectNodeQuantifier::Distinct => lir::SetQuantifier::Distinct,
                },
            },
            wire::Node::ExceptNode(node) => lir::Relation::Except {
                scope: node.scope,
                left: Box::new(self.relation(&node.left)?),
                right: Box::new(self.relation(&node.right)?),
                quantifier: match node.quantifier {
                    wire::ExceptNodeQuantifier::All => lir::SetQuantifier::All,
                    wire::ExceptNodeQuantifier::Distinct => lir::SetQuantifier::Distinct,
                },
            },
            wire::Node::AggregateNode(node) => lir::Relation::Aggregate {
                input: Box::new(self.relation(&node.input)?),
                scope: optional(node.scope),
                groups: optional(node.groups)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|term| {
                        Ok(lir::GroupTerm {
                            name: optional(term.r#as).unwrap_or_default(),
                            expression: self.expression(term.expr)?,
                        })
                    })
                    .collect::<LowerResult<_>>()?,
                terms: optional(node.aggs)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|term| {
                        Ok(lir::AggregateTerm {
                            function: match term.r#fn {
                                wire::AggTermFn::Count => lir::AggregateFunction::Count,
                                wire::AggTermFn::Sum => lir::AggregateFunction::Sum,
                                wire::AggTermFn::Avg => lir::AggregateFunction::Average,
                                wire::AggTermFn::Min => lir::AggregateFunction::Min,
                                wire::AggTermFn::Max => lir::AggregateFunction::Max,
                            },
                            argument: optional(term.arg)
                                .map(|argument| self.expression(argument))
                                .transpose()?,
                            name: term.r#as,
                        })
                    })
                    .collect::<LowerResult<_>>()?,
            },
            wire::Node::OrderNode(node) => lir::Relation::Order {
                input: Box::new(self.relation(&node.input)?),
                terms: node
                    .terms
                    .into_iter()
                    .map(|term| {
                        Ok(lir::OrderTerm {
                            expression: self.expression(term.expr)?,
                            descending: optional(term.desc).unwrap_or(false),
                        })
                    })
                    .collect::<LowerResult<_>>()?,
            },
            wire::Node::SliceNode(node) => lir::Relation::Slice {
                input: Box::new(self.relation(&node.input)?),
                offset: nonnegative(optional(node.offset).unwrap_or(0), "slice offset")?,
                limit: optional(node.limit)
                    .map(|value| nonnegative(value, "slice limit"))
                    .transpose()?,
            },
            wire::Node::RefNode(node) => lir::Relation::Ref {
                binding: node.binding,
                scope: node.scope,
            },
            wire::Node::RecursiveRefNode(node) => lir::Relation::RecursiveRef {
                binding: node.binding,
                scope: node.scope,
            },
            wire::Node::DistinctNode(node) => {
                lir::Relation::Distinct(Box::new(self.relation(&node.input)?))
            }
        })
    }

    fn expression(&mut self, expression: wire::Expr) -> LowerResult<lir::Expr> {
        Ok(match expression {
            wire::Expr::LiteralExpr(expression) => lower_literal(expression.value),
            wire::Expr::ColumnExpr(expression) => lir::Expr::Column {
                scope: expression.scope,
                name: expression.column,
            },
            wire::Expr::UnaryExpr(expression) => lir::Expr::Unary {
                op: match expression.op {
                    wire::UnaryExprOp::Not => lir::UnaryOp::Not,
                    wire::UnaryExprOp::Negate => lir::UnaryOp::Negate,
                    wire::UnaryExprOp::IsNull => lir::UnaryOp::IsNull,
                    wire::UnaryExprOp::IsNotNull => lir::UnaryOp::IsNotNull,
                },
                expression: Box::new(self.expression(expression.expr)?),
            },
            wire::Expr::BinaryExpr(expression) => lir::Expr::Binary {
                op: binary_op(expression.op),
                left: Box::new(self.expression(expression.left)?),
                right: Box::new(self.expression(expression.right)?),
            },
            wire::Expr::CastExpr(expression) => lir::Expr::Cast {
                expression: Box::new(self.expression(expression.expr)?),
                to: scalar_kind(expression.to),
            },
            wire::Expr::BranchExpr(expression) => lir::Expr::Branch {
                arms: expression
                    .branches
                    .into_iter()
                    .map(|arm| {
                        Ok(lir::BranchArm {
                            when: self.expression(arm.when)?,
                            then: self.expression(arm.then)?,
                        })
                    })
                    .collect::<LowerResult<_>>()?,
                otherwise: Box::new(self.expression(expression.r#else)?),
            },
            wire::Expr::TextMatchExpr(expression) => lir::Expr::TextMatch {
                value: Box::new(self.expression(expression.value)?),
                parts: expression
                    .parts
                    .into_iter()
                    .map(|part| match part {
                        wire::TextMatchExprPart::LiteralTextMatchPart(part) => {
                            lir::TextMatchPart::Literal(part.value)
                        }
                        wire::TextMatchExprPart::AnyManyTextMatchPart(_) => {
                            lir::TextMatchPart::AnyMany
                        }
                    })
                    .collect(),
                comparison: match optional(expression.comparison)
                    .unwrap_or(wire::TextComparison::Exact)
                {
                    wire::TextComparison::Exact => lir::TextComparison::Exact,
                    wire::TextComparison::UnicodeSimpleFold => {
                        lir::TextComparison::UnicodeSimpleFold
                    }
                },
            },
            wire::Expr::CrossingExprExists(expression) => {
                lir::Expr::Exists(Box::new(self.relation(&expression.node)?))
            }
            wire::Expr::CrossingExprFirst(expression) => {
                lir::Expr::First(Box::new(self.relation(&expression.node)?))
            }
            wire::Expr::CrossingExprScalar(expression) => {
                lir::Expr::Scalar(Box::new(self.relation(&expression.node)?))
            }
            wire::Expr::CrossingExprArray(expression) => {
                lir::Expr::Array(Box::new(self.relation(&expression.node)?))
            }
        })
    }
}

fn lower_literal(value: wire::Value) -> lir::Expr {
    let (raw, kind) = match value {
        wire::Value::TextValue(value) => (
            optional(value.value)
                .map(lir::RawScalar::Text)
                .unwrap_or(lir::RawScalar::Null),
            lir::Kind::Text,
        ),
        wire::Value::Int64Value(value) => (
            optional(value.value)
                .map(lir::RawScalar::Number)
                .unwrap_or(lir::RawScalar::Null),
            lir::Kind::Int64,
        ),
        wire::Value::Float64Value(value) => (
            optional(value.value)
                .map(lir::RawScalar::Number)
                .unwrap_or(lir::RawScalar::Null),
            lir::Kind::Float64,
        ),
        wire::Value::BoolValue(value) => {
            let raw = optional(value.value)
                .map(lir::RawScalar::Bool)
                .unwrap_or(lir::RawScalar::Null);
            (raw, lir::Kind::Bool)
        }
    };
    lir::Expr::Literal(lir::Literal {
        raw,
        kind: Some(kind),
    })
}

fn lower_cell(cell: wire::Cell, kind: lir::Kind) -> LowerResult<lir::RawScalar> {
    let Some(value) = cell.0 else {
        return Ok(lir::RawScalar::Null);
    };
    Ok(match kind {
        lir::Kind::Text => lir::RawScalar::Text(value),
        lir::Kind::Int64 | lir::Kind::Float64 => lir::RawScalar::Number(value),
        lir::Kind::Bool => lir::RawScalar::Bool(parse_bool(&value)?),
        _ => return Err(LowerError::new("rows column has a non-scalar type")),
    })
}

fn parse_bool(value: &str) -> LowerResult<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(LowerError::new(format!("invalid bool payload {value:?}"))),
    }
}

fn optional<T>(value: wire::OptionalField<T>) -> Option<T> {
    match value {
        wire::OptionalField::Missing => None,
        wire::OptionalField::Present(value) => Some(value),
    }
}

fn nonnegative(value: i64, description: &str) -> LowerResult<usize> {
    usize::try_from(value)
        .map_err(|_| LowerError::new(format!("{description} must be non-negative")))
}

fn scalar_kind(value: wire::ScalarType) -> lir::Kind {
    match value {
        wire::ScalarType::Text => lir::Kind::Text,
        wire::ScalarType::Int64 => lir::Kind::Int64,
        wire::ScalarType::Float64 => lir::Kind::Float64,
        wire::ScalarType::Bool => lir::Kind::Bool,
    }
}

fn root_cardinality(value: wire::RootCardinality) -> lir::RootCardinality {
    match value {
        wire::RootCardinality::Many => lir::RootCardinality::Many,
        wire::RootCardinality::First => lir::RootCardinality::First,
        wire::RootCardinality::ExactlyOne => lir::RootCardinality::ExactlyOne,
        wire::RootCardinality::Scalar => lir::RootCardinality::Scalar,
    }
}

fn binary_op(value: wire::BinaryExprOp) -> lir::BinaryOp {
    match value {
        wire::BinaryExprOp::Eq => lir::BinaryOp::Eq,
        wire::BinaryExprOp::Ne => lir::BinaryOp::Ne,
        wire::BinaryExprOp::Lt => lir::BinaryOp::Lt,
        wire::BinaryExprOp::Lte => lir::BinaryOp::Lte,
        wire::BinaryExprOp::Gt => lir::BinaryOp::Gt,
        wire::BinaryExprOp::Gte => lir::BinaryOp::Gte,
        wire::BinaryExprOp::And => lir::BinaryOp::And,
        wire::BinaryExprOp::Or => lir::BinaryOp::Or,
        wire::BinaryExprOp::Add => lir::BinaryOp::Add,
        wire::BinaryExprOp::Sub => lir::BinaryOp::Sub,
        wire::BinaryExprOp::Mul => lir::BinaryOp::Mul,
        wire::BinaryExprOp::Div => lir::BinaryOp::Div,
    }
}
