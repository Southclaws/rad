//! Unbound LIR to trusted, slot-addressed LIR binding.

mod expression;
mod program;
mod recursive;
mod relation;

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;

use crate::engine::catalog;
use crate::engine::catalog::model::Table;
use crate::engine::lir::bound;
use crate::engine::lir::{self, SlotId};

use super::{Error, Reason, Result};

pub use program::{BoundStatement, Mutation, MutationKind, ProgramBinder, ProgramStatement};

#[async_trait]
pub trait Catalog: Send + Sync {
    async fn get_table(&self, name: &str) -> catalog::Result<Option<Table>>;
}

#[async_trait]
impl Catalog for catalog::Catalog {
    async fn get_table(&self, name: &str) -> catalog::Result<Option<Table>> {
        self.get_table(name).await
    }
}

pub async fn bind(catalog: &dyn Catalog, query: lir::Query) -> Result<bound::Query> {
    Binder::new(catalog).bind_query(query, None).await
}

#[derive(Clone)]
struct Scope {
    label: String,
    relation: bound::Relation,
}

struct Binder<'a> {
    catalog: &'a dyn Catalog,
    next_slot: SlotId,
    scopes: Vec<Scope>,
    labels: HashSet<String>,
    bindings: HashMap<String, bound::Binding>,
    used: HashSet<String>,
    reserved: HashSet<String>,
    recursing: Option<String>,
}

impl<'a> Binder<'a> {
    fn new(catalog: &'a dyn Catalog) -> Self {
        Self {
            catalog,
            next_slot: SlotId(0),
            scopes: Vec::new(),
            labels: HashSet::new(),
            bindings: HashMap::new(),
            used: HashSet::new(),
            reserved: HashSet::new(),
            recursing: None,
        }
    }

    async fn bind_query(
        &mut self,
        query: lir::Query,
        program: Option<&HashMap<String, bound::Binding>>,
    ) -> Result<bound::Query> {
        let cardinality = query.cardinality;
        let (root, bindings) = self.bind_body(query, program).await?;
        if cardinality != lir::RootCardinality::Scalar {
            require_unique_output(&root, "the query root")?;
        }
        if cardinality == lir::RootCardinality::Scalar && root.output().fields.len() != 1 {
            return Err(invalid(
                Reason::ScalarArity,
                format!(
                    "planner: a scalar query needs a single-column root, got {} columns",
                    root.output().fields.len()
                ),
            ));
        }
        if cardinality == lir::RootCardinality::Scalar && !root.cardinality().at_most_one() {
            return Err(invalid(
                Reason::NondeterministicOrder,
                "planner: root scalar asserts at most one row, but the relation may produce more — aggregate it, slice it, or pin a unique key",
            ));
        }
        if cardinality == lir::RootCardinality::Many && !root.is_ordered() {
            return Err(invalid(
                Reason::NondeterministicOrder,
                "planner: root cardinality \"many\" needs an explicit order — observable collections must not depend on the access path",
            ));
        }
        if cardinality == lir::RootCardinality::First
            && !root.cardinality().at_most_one()
            && !root.is_ordered()
        {
            return Err(invalid(
                Reason::NondeterministicOrder,
                "planner: root cardinality \"first\" over an unordered multi-row relation would make results depend on the access path — add an order or make the relation at-most-one",
            ));
        }
        Ok(bound::Query {
            root,
            cardinality,
            bindings,
            next_slot: self.next_slot,
        })
    }

    async fn bind_bag(
        &mut self,
        query: lir::Query,
        program: Option<&HashMap<String, bound::Binding>>,
    ) -> Result<bound::Query> {
        let (root, bindings) = self.bind_body(query, program).await?;
        Ok(bound::Query {
            root,
            cardinality: lir::RootCardinality::Many,
            bindings,
            next_slot: self.next_slot,
        })
    }

    async fn bind_body(
        &mut self,
        query: lir::Query,
        program: Option<&HashMap<String, bound::Binding>>,
    ) -> Result<(bound::Relation, Vec<bound::Binding>)> {
        self.labels.clear();
        self.scopes.clear();
        self.used.clear();
        self.bindings = program.cloned().unwrap_or_default();

        let order = binding_order(&query.bindings)?;
        let mut bindings = Vec::with_capacity(order.len());
        for name in &order {
            if self.reserved.contains(name) {
                return Err(invalid(
                    Reason::BindingCollision,
                    format!("planner: binding {name:?} shadows a statement name"),
                ));
            }
            let relation = query.bindings[name].clone();
            if let lir::Relation::Recursive {
                anchor,
                step,
                accumulation,
            } = relation
            {
                let binding = self
                    .bind_recursive_binding(name, *anchor, *step, accumulation)
                    .await
                    .map_err(|error| error.context(format!("planner: binding {name:?}")))?;
                bindings.push(binding);
                continue;
            }
            let body = self
                .bind_relation(relation)
                .await
                .map_err(|error| error.context(format!("planner: binding {name:?}")))?;
            self.scopes.clear();
            if let Some(duplicate) = duplicate_column(body.output()) {
                return Err(invalid(
                    Reason::BindingCollision,
                    format!(
                        "planner: binding {name:?} output has duplicate column {duplicate:?} — project the body to a unique set of columns"
                    ),
                ));
            }
            let binding = bound::Binding {
                name: name.clone(),
                root: body.clone(),
                output: body.output().clone(),
                plan_sensitive: lir::inspect::plan_sensitive(&body),
                recursive: false,
                step: None,
                accumulation: None,
            };
            self.bindings.insert(name.clone(), binding.clone());
            bindings.push(binding);
        }

        let root = self.bind_relation(query.root).await?;
        for name in order {
            if !self.used.contains(&name) {
                return Err(invalid(
                    Reason::UnknownBinding,
                    format!("planner: binding {name:?} is never referenced"),
                ));
            }
        }
        Ok((root, bindings))
    }

    fn expose_scope(&mut self, label: String, relation: bound::Relation) -> Result<()> {
        if !self.labels.insert(label.clone()) {
            return Err(invalid(
                Reason::DuplicateScope,
                format!("planner: duplicate scope {label:?}"),
            ));
        }
        self.scopes.push(Scope { label, relation });
        Ok(())
    }

    fn find_scope(&self, label: &str, from: usize) -> Option<&Scope> {
        self.scopes[from..]
            .iter()
            .rev()
            .find(|entry| entry.label == label)
    }

    fn fresh_slots(&mut self, count: usize) -> Vec<SlotId> {
        (0..count)
            .map(|_| {
                let slot = self.next_slot;
                self.next_slot.0 += 1;
                slot
            })
            .collect()
    }

    fn fresh_occurrence(&mut self, output: &lir::RowType) -> (Vec<lir::Field>, Vec<SlotId>) {
        let mut canonical = Vec::with_capacity(output.fields.len());
        let fields = output
            .fields
            .iter()
            .map(|field| {
                canonical.push(field.slot);
                let slot = self.next_slot;
                self.next_slot.0 += 1;
                lir::Field {
                    name: field.name.clone(),
                    slot,
                    value_type: field.value_type.clone(),
                }
            })
            .collect();
        (fields, canonical)
    }

    fn slot_for(&mut self, expression: &bound::Expr) -> SlotId {
        if let bound::Expr::SlotRef { slot, .. } = expression {
            *slot
        } else {
            let slot = self.next_slot;
            self.next_slot.0 += 1;
            slot
        }
    }
}

fn invalid(reason: Reason, message: impl Into<String>) -> Error {
    Error::invalid(reason, message)
}

fn duplicate_column(output: &lir::RowType) -> Option<&str> {
    let mut seen = HashSet::new();
    output
        .fields
        .iter()
        .map(|field| field.name.as_str())
        .find(|name| !seen.insert(*name))
}

fn require_unique_output(relation: &bound::Relation, description: &str) -> Result<()> {
    if let Some(duplicate) = duplicate_column(relation.output()) {
        return Err(invalid(
            Reason::ProjectionCollision,
            format!(
                "planner: {description} has duplicate column {duplicate:?} — project it to a unique set of columns"
            ),
        ));
    }
    Ok(())
}

fn binding_order(bindings: &HashMap<String, lir::Relation>) -> Result<Vec<String>> {
    let mut names: Vec<_> = bindings.keys().cloned().collect();
    names.sort();
    if names.iter().any(String::is_empty) {
        return Err(invalid(
            Reason::Invalid,
            "planner: binding names must not be empty",
        ));
    }
    fn visit(
        name: &str,
        bindings: &HashMap<String, lir::Relation>,
        states: &mut HashMap<String, u8>,
        order: &mut Vec<String>,
    ) -> Result<()> {
        if !bindings.contains_key(name) {
            return Ok(());
        }
        match states.get(name).copied() {
            Some(1) => {
                return Err(invalid(
                    Reason::BindingCycle,
                    format!("planner: binding {name:?} is part of a binding cycle"),
                ));
            }
            Some(2) => return Ok(()),
            _ => {}
        }
        states.insert(name.into(), 1);
        let mut dependencies = Vec::new();
        collect_binding_dependencies(&bindings[name], &mut dependencies);
        for dependency in dependencies {
            visit(&dependency, bindings, states, order)?;
        }
        states.insert(name.into(), 2);
        order.push(name.into());
        Ok(())
    }
    let mut states = HashMap::new();
    let mut order = Vec::with_capacity(names.len());
    for name in names {
        visit(&name, bindings, &mut states, &mut order)?;
    }
    Ok(order)
}

fn collect_binding_dependencies(relation: &lir::Relation, output: &mut Vec<String>) {
    if let lir::Relation::Ref { binding, .. } = relation {
        output.push(binding.clone());
    }
    walk_unbound_relation_children(relation, &mut |relation| {
        collect_binding_dependencies(relation, output)
    });
}

fn walk_unbound_relation_children(
    relation: &lir::Relation,
    visitor: &mut impl FnMut(&lir::Relation),
) {
    match relation {
        lir::Relation::Scan { .. }
        | lir::Relation::Rows { .. }
        | lir::Relation::Ref { .. }
        | lir::Relation::RecursiveRef { .. } => {}
        lir::Relation::Filter { input, predicate } => {
            visitor(input);
            walk_unbound_expression_relations(predicate, visitor);
        }
        lir::Relation::Project { input, fields, .. } => {
            visitor(input);
            for field in fields {
                walk_unbound_expression_relations(&field.expression, visitor);
            }
        }
        lir::Relation::Join {
            left, right, on, ..
        } => {
            visitor(left);
            visitor(right);
            walk_unbound_expression_relations(on, visitor);
        }
        lir::Relation::Concatenate { inputs, .. } => inputs.iter().for_each(&mut *visitor),
        lir::Relation::Intersect { left, right, .. }
        | lir::Relation::Except { left, right, .. } => {
            visitor(left);
            visitor(right);
        }
        lir::Relation::Aggregate {
            input,
            groups,
            terms,
            ..
        } => {
            visitor(input);
            for group in groups {
                walk_unbound_expression_relations(&group.expression, visitor);
            }
            for term in terms {
                if let Some(argument) = &term.argument {
                    walk_unbound_expression_relations(argument, visitor);
                }
            }
        }
        lir::Relation::Order { input, terms } => {
            visitor(input);
            for term in terms {
                walk_unbound_expression_relations(&term.expression, visitor);
            }
        }
        lir::Relation::Slice { input, .. } | lir::Relation::Distinct(input) => visitor(input),
        lir::Relation::Recursive { anchor, step, .. } => {
            visitor(anchor);
            visitor(step);
        }
    }
}

fn walk_unbound_expression_relations(
    expression: &lir::Expr,
    visitor: &mut impl FnMut(&lir::Relation),
) {
    match expression {
        lir::Expr::Literal(_) | lir::Expr::Column { .. } => {}
        lir::Expr::Unary { expression, .. }
        | lir::Expr::Cast { expression, .. }
        | lir::Expr::TextMatch {
            value: expression, ..
        } => walk_unbound_expression_relations(expression, visitor),
        lir::Expr::Binary { left, right, .. } => {
            walk_unbound_expression_relations(left, visitor);
            walk_unbound_expression_relations(right, visitor);
        }
        lir::Expr::Branch {
            arms, otherwise, ..
        } => {
            for arm in arms {
                walk_unbound_expression_relations(&arm.when, visitor);
                walk_unbound_expression_relations(&arm.then, visitor);
            }
            walk_unbound_expression_relations(otherwise, visitor);
        }
        lir::Expr::Exists(relation)
        | lir::Expr::First(relation)
        | lir::Expr::Scalar(relation)
        | lir::Expr::Array(relation) => visitor(relation),
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::catalog::identity::{
        DefinitionGeneration, ExistenceGeneration, SchemaId, ValueGeneration,
        WriteProtocolGeneration,
    };
    use crate::engine::catalog::model::{Column, ScalarType};
    use crate::engine::lir::bound::RelationNode;
    use crate::engine::lir::{Kind, RawScalar};

    use super::*;

    struct FixtureCatalog(Table);

    #[async_trait]
    impl Catalog for FixtureCatalog {
        async fn get_table(&self, name: &str) -> catalog::Result<Option<Table>> {
            Ok((name == self.0.name).then(|| self.0.clone()))
        }
    }

    fn table() -> Table {
        Table {
            id: "tasks-table".into(),
            schema_id: SchemaId::new(1).unwrap(),
            name: "tasks".into(),
            definition_generation: DefinitionGeneration::ZERO,
            existence_generation: ExistenceGeneration::ZERO,
            write_protocol_generation: WriteProtocolGeneration::ZERO,
            columns: [
                ("id", ScalarType::Text),
                ("status", ScalarType::Text),
                ("priority", ScalarType::Int64),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, (name, scalar_type))| Column {
                id: format!("column-{name}").into(),
                schema_id: SchemaId::new(index as u32 + 1).unwrap(),
                name: name.into(),
                value_generation: ValueGeneration::ZERO,
                scalar_type,
                nullable: false,
                format: String::new(),
                insert_default: None,
                missing_value: None,
            })
            .collect(),
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            constraints: Vec::new(),
        }
    }

    fn scan() -> lir::Relation {
        lir::Relation::Scan {
            table: "tasks".into(),
            scope: "t".into(),
        }
    }

    fn column(name: &str) -> lir::Expr {
        lir::Expr::Column {
            scope: "t".into(),
            name: name.into(),
        }
    }

    fn query(root: lir::Relation, cardinality: lir::RootCardinality) -> lir::Query {
        lir::Query {
            root,
            cardinality,
            bindings: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn binding_allocates_dense_slots_and_completes_observable_ordering() {
        let catalog = FixtureCatalog(table());
        let query = query(
            lir::Relation::Order {
                input: Box::new(scan()),
                terms: vec![lir::OrderTerm {
                    expression: column("status"),
                    descending: false,
                }],
            },
            lir::RootCardinality::Many,
        );
        let bound = bind(&catalog, query).await.unwrap();
        assert_eq!(bound.next_slot, SlotId(3));
        assert_eq!(
            bound.root.output().slots(),
            [SlotId(0), SlotId(1), SlotId(2)]
        );
        let RelationNode::Order { terms, .. } = &bound.root.node else {
            panic!("expected order")
        };
        assert_eq!(terms.len(), 2, "primary key must complete the tie-break");
        assert!(matches!(
            &terms[1].expression,
            bound::Expr::SlotRef {
                slot: SlotId(0),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn raw_numbers_are_typed_by_the_catalog_column() {
        let catalog = FixtureCatalog(table());
        let predicate = lir::Expr::Binary {
            op: lir::BinaryOp::Eq,
            left: Box::new(column("priority")),
            right: Box::new(lir::Expr::Literal(lir::Literal {
                raw: RawScalar::Number("9007199254740993".into()),
                kind: None,
            })),
        };
        let root = lir::Relation::Order {
            input: Box::new(lir::Relation::Filter {
                input: Box::new(scan()),
                predicate,
            }),
            terms: vec![lir::OrderTerm {
                expression: column("id"),
                descending: false,
            }],
        };
        let bound = bind(&catalog, query(root, lir::RootCardinality::Many))
            .await
            .unwrap();
        let RelationNode::Order { input, .. } = &bound.root.node else {
            panic!("expected order")
        };
        let RelationNode::Filter { predicate, .. } = &input.node else {
            panic!("expected filter")
        };
        assert!(matches!(
            predicate,
            bound::Expr::Binary { right, .. }
                if matches!(&**right, bound::Expr::Literal(lir::Value::Int64(9007199254740993)))
        ));
    }

    #[tokio::test]
    async fn bare_column_group_inherits_its_column_name() {
        let catalog = FixtureCatalog(table());
        let aggregate = lir::Relation::Aggregate {
            input: Box::new(scan()),
            scope: Some("grouped".into()),
            groups: vec![lir::GroupTerm {
                name: String::new(),
                expression: column("status"),
            }],
            terms: Vec::new(),
        };
        let root = lir::Relation::Order {
            input: Box::new(aggregate),
            terms: vec![lir::OrderTerm {
                expression: lir::Expr::Column {
                    scope: "grouped".into(),
                    name: "status".into(),
                },
                descending: false,
            }],
        };
        let bound = bind(&catalog, query(root, lir::RootCardinality::Many))
            .await
            .unwrap();
        let RelationNode::Order { input, .. } = &bound.root.node else {
            panic!("expected order")
        };
        assert_eq!(input.output().fields[0].name, "status");
        assert_eq!(input.output().fields[0].value_type.kind, Kind::Text);
    }

    #[tokio::test]
    async fn recursive_nullability_reaches_a_fixpoint() {
        let anchor = lir::Relation::Rows {
            scope: "anchor".into(),
            columns: vec![
                lir::RowsColumn {
                    name: "a".into(),
                    kind: Kind::Text,
                    nullable: false,
                },
                lir::RowsColumn {
                    name: "b".into(),
                    kind: Kind::Text,
                    nullable: false,
                },
            ],
            values: vec![vec![
                RawScalar::Text("root".into()),
                RawScalar::Text("root".into()),
            ]],
        };
        let step = lir::Relation::Project {
            input: Box::new(lir::Relation::RecursiveRef {
                binding: "chain".into(),
                scope: "parent".into(),
            }),
            scope: Some("step".into()),
            spread: Vec::new(),
            fields: vec![
                lir::ProjectField {
                    name: "a".into(),
                    expression: lir::Expr::Literal(lir::Literal {
                        raw: RawScalar::Null,
                        kind: Some(Kind::Text),
                    }),
                },
                lir::ProjectField {
                    name: "b".into(),
                    expression: lir::Expr::Column {
                        scope: "parent".into(),
                        name: "a".into(),
                    },
                },
            ],
        };
        let query = lir::Query {
            root: lir::Relation::Slice {
                input: Box::new(lir::Relation::Ref {
                    binding: "chain".into(),
                    scope: "result".into(),
                }),
                offset: 0,
                limit: Some(1),
            },
            cardinality: lir::RootCardinality::First,
            bindings: HashMap::from([(
                "chain".into(),
                lir::Relation::Recursive {
                    anchor: Box::new(anchor),
                    step: Box::new(step),
                    accumulation: lir::RecursiveAccumulation::New,
                },
            )]),
        };

        let bound = bind(&FixtureCatalog(table()), query).await.unwrap();
        assert_eq!(bound.bindings.len(), 1);
        for name in ["a", "b"] {
            assert!(
                bound.bindings[0]
                    .output
                    .lookup(name)
                    .unwrap()
                    .value_type
                    .nullable,
                "{name} must widen through repeated recursive binding"
            );
        }
    }

    #[tokio::test]
    async fn semantic_validation_matrix_preserves_every_rejection_class() {
        fn text(value: &str) -> lir::Expr {
            lir::Expr::Literal(lir::Literal {
                raw: RawScalar::Text(value.into()),
                kind: None,
            })
        }
        fn number(value: &str) -> lir::Expr {
            lir::Expr::Literal(lir::Literal {
                raw: RawScalar::Number(value.into()),
                kind: None,
            })
        }
        fn boolean(value: bool) -> lir::Expr {
            lir::Expr::Literal(lir::Literal {
                raw: RawScalar::Bool(value),
                kind: None,
            })
        }
        fn scoped(scope: &str, name: &str) -> lir::Expr {
            lir::Expr::Column {
                scope: scope.into(),
                name: name.into(),
            }
        }
        fn binary(op: lir::BinaryOp, left: lir::Expr, right: lir::Expr) -> lir::Expr {
            lir::Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            }
        }
        fn filtered(predicate: lir::Expr) -> lir::Relation {
            lir::Relation::Filter {
                input: Box::new(scan()),
                predicate,
            }
        }

        let cases = vec![
            (
                "unknown table",
                lir::Relation::Scan {
                    table: "ghosts".into(),
                    scope: "g".into(),
                },
                Reason::UnknownTable,
            ),
            (
                "scan without scope",
                lir::Relation::Scan {
                    table: "tasks".into(),
                    scope: String::new(),
                },
                Reason::Invalid,
            ),
            (
                "duplicate join scope",
                lir::Relation::Join {
                    left: Box::new(scan()),
                    right: Box::new(scan()),
                    kind: lir::JoinKind::Inner,
                    on: boolean(true),
                },
                Reason::DuplicateScope,
            ),
            (
                "unknown column",
                filtered(binary(lir::BinaryOp::Eq, scoped("t", "ghost"), text("x"))),
                Reason::UnknownColumn,
            ),
            (
                "unqualified column",
                filtered(binary(lir::BinaryOp::Eq, scoped("", "status"), text("x"))),
                Reason::UnknownScope,
            ),
            (
                "non-boolean filter",
                filtered(column("status")),
                Reason::TypeMismatch,
            ),
            (
                "non-boolean conjunction",
                filtered(binary(lir::BinaryOp::And, column("status"), boolean(true))),
                Reason::TypeMismatch,
            ),
            (
                "non-boolean negation",
                filtered(lir::Expr::Unary {
                    op: lir::UnaryOp::Not,
                    expression: Box::new(column("status")),
                }),
                Reason::TypeMismatch,
            ),
            (
                "non-numeric arithmetic",
                filtered(binary(
                    lir::BinaryOp::Eq,
                    binary(lir::BinaryOp::Add, column("status"), text("x")),
                    text("y"),
                )),
                Reason::TypeMismatch,
            ),
            (
                "mixed comparison kinds",
                filtered(binary(
                    lir::BinaryOp::Eq,
                    column("priority"),
                    column("status"),
                )),
                Reason::TypeMismatch,
            ),
            (
                "invalid cast",
                filtered(binary(
                    lir::BinaryOp::Eq,
                    lir::Expr::Cast {
                        expression: Box::new(column("status")),
                        to: Kind::Bool,
                    },
                    boolean(true),
                )),
                Reason::TypeMismatch,
            ),
            (
                "sum over text",
                lir::Relation::Aggregate {
                    input: Box::new(scan()),
                    scope: None,
                    groups: Vec::new(),
                    terms: vec![lir::AggregateTerm {
                        function: lir::AggregateFunction::Sum,
                        argument: Some(column("status")),
                        name: "sum".into(),
                    }],
                },
                Reason::TypeMismatch,
            ),
            (
                "aggregate term without name",
                lir::Relation::Aggregate {
                    input: Box::new(scan()),
                    scope: None,
                    groups: Vec::new(),
                    terms: vec![lir::AggregateTerm {
                        function: lir::AggregateFunction::Count,
                        argument: None,
                        name: String::new(),
                    }],
                },
                Reason::ProjectionCollision,
            ),
            (
                "duplicate aggregate output",
                lir::Relation::Aggregate {
                    input: Box::new(scan()),
                    scope: None,
                    groups: Vec::new(),
                    terms: vec![
                        lir::AggregateTerm {
                            function: lir::AggregateFunction::Count,
                            argument: None,
                            name: "n".into(),
                        },
                        lir::AggregateTerm {
                            function: lir::AggregateFunction::Count,
                            argument: None,
                            name: "n".into(),
                        },
                    ],
                },
                Reason::ProjectionCollision,
            ),
            (
                "empty aggregate",
                lir::Relation::Aggregate {
                    input: Box::new(scan()),
                    scope: None,
                    groups: Vec::new(),
                    terms: Vec::new(),
                },
                Reason::Invalid,
            ),
            (
                "computed group without name",
                lir::Relation::Aggregate {
                    input: Box::new(scan()),
                    scope: None,
                    groups: vec![lir::GroupTerm {
                        name: String::new(),
                        expression: binary(lir::BinaryOp::Add, column("priority"), number("1")),
                    }],
                    terms: Vec::new(),
                },
                Reason::ProjectionCollision,
            ),
            (
                "empty projection",
                lir::Relation::Project {
                    input: Box::new(scan()),
                    scope: None,
                    spread: Vec::new(),
                    fields: Vec::new(),
                },
                Reason::Invalid,
            ),
            (
                "duplicate projection output",
                lir::Relation::Project {
                    input: Box::new(scan()),
                    scope: None,
                    spread: Vec::new(),
                    fields: vec![
                        lir::ProjectField {
                            name: "x".into(),
                            expression: column("id"),
                        },
                        lir::ProjectField {
                            name: "x".into(),
                            expression: column("status"),
                        },
                    ],
                },
                Reason::ProjectionCollision,
            ),
            (
                "spread collision",
                lir::Relation::Project {
                    input: Box::new(scan()),
                    scope: None,
                    spread: vec!["t".into()],
                    fields: vec![lir::ProjectField {
                        name: "status".into(),
                        expression: column("id"),
                    }],
                },
                Reason::ProjectionCollision,
            ),
            (
                "unknown spread scope",
                lir::Relation::Project {
                    input: Box::new(scan()),
                    scope: None,
                    spread: vec!["ghost".into()],
                    fields: vec![lir::ProjectField {
                        name: "id".into(),
                        expression: column("id"),
                    }],
                },
                Reason::UnknownScope,
            ),
            (
                "order without terms",
                lir::Relation::Order {
                    input: Box::new(scan()),
                    terms: Vec::new(),
                },
                Reason::Invalid,
            ),
            (
                "order by array",
                lir::Relation::Order {
                    input: Box::new(scan()),
                    terms: vec![lir::OrderTerm {
                        expression: lir::Expr::Array(Box::new(lir::Relation::Order {
                            input: Box::new(lir::Relation::Rows {
                                scope: "inner".into(),
                                columns: vec![lir::RowsColumn {
                                    name: "value".into(),
                                    kind: Kind::Text,
                                    nullable: false,
                                }],
                                values: vec![vec![RawScalar::Text("x".into())]],
                            }),
                            terms: vec![lir::OrderTerm {
                                expression: scoped("inner", "value"),
                                descending: false,
                            }],
                        })),
                        descending: false,
                    }],
                },
                Reason::TypeMismatch,
            ),
            (
                "spread across crossing boundary",
                lir::Relation::Project {
                    input: Box::new(scan()),
                    scope: Some("outer".into()),
                    spread: Vec::new(),
                    fields: vec![lir::ProjectField {
                        name: "nested".into(),
                        expression: lir::Expr::Array(Box::new(lir::Relation::Project {
                            input: Box::new(lir::Relation::Scan {
                                table: "tasks".into(),
                                scope: "u".into(),
                            }),
                            scope: Some("inner".into()),
                            spread: vec!["t".into()],
                            fields: vec![lir::ProjectField {
                                name: "id".into(),
                                expression: scoped("u", "id"),
                            }],
                        })),
                    }],
                },
                Reason::UnknownScope,
            ),
            (
                "dependent join",
                lir::Relation::Join {
                    left: Box::new(scan()),
                    right: Box::new(lir::Relation::Filter {
                        input: Box::new(lir::Relation::Scan {
                            table: "tasks".into(),
                            scope: "u".into(),
                        }),
                        predicate: binary(lir::BinaryOp::Eq, scoped("u", "id"), scoped("t", "id")),
                    }),
                    kind: lir::JoinKind::Inner,
                    on: boolean(true),
                },
                Reason::DependentJoin,
            ),
        ];

        let catalog = FixtureCatalog(table());
        for (name, root, expected) in cases {
            let error = bind(&catalog, query(root, lir::RootCardinality::ExactlyOne))
                .await
                .unwrap_err();
            assert_eq!(error.reason(), expected, "case {name}: {error}");
        }
    }

    #[tokio::test]
    async fn unordered_observable_collection_is_rejected_by_reason() {
        let catalog = FixtureCatalog(table());
        let error = bind(&catalog, query(scan(), lir::RootCardinality::Many))
            .await
            .unwrap_err();
        assert_eq!(error.reason(), Reason::NondeterministicOrder);
    }
}
