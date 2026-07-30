//! Incremental binding for PIR's ordered, transaction-scoped statements.

use std::collections::{HashMap, HashSet};

use crate::engine::catalog::model::Table;
use crate::engine::lir::bound;
use crate::engine::lir::{self, RootCardinality, SlotId};

use super::{Binder, Catalog};
use crate::engine::planner::physical::Plan;
use crate::engine::planner::{PlanOptions, Reason, Result, plan_query};

#[derive(Clone, Debug, PartialEq)]
pub struct ProgramStatement {
    pub name: String,
    pub relation: lir::Query,
    pub mutation: Option<Mutation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mutation {
    pub kind: MutationKind,
    pub table: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationKind {
    Create,
    Update,
    Delete,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BoundStatement {
    pub name: String,
    /// The logical, slot-bound query is retained for the independent
    /// reference interpreter. Production execution uses `plan` exclusively.
    pub bound: bound::Query,
    /// Absent on the independent reference path, which deliberately stops
    /// after logical binding.
    pub plan: Option<Plan>,
    pub result_output: lir::RowType,
    pub result_cardinality: RootCardinality,
    pub target: Option<Table>,
}

/// Stateful because catalog statements between calls can change what the next
/// relational statement sees through the transaction-backed catalog reader.
pub struct ProgramBinder {
    program: HashMap<String, bound::Binding>,
    reserved: HashSet<String>,
    names: Vec<String>,
    next: usize,
    next_slot: SlotId,
}

impl ProgramBinder {
    /// Reserve the complete namespace up front, so a local binding can never
    /// resolve differently merely because a later statement has not run yet.
    pub fn new(names: Vec<String>) -> Result<Self> {
        let mut reserved = HashSet::with_capacity(names.len());
        for name in &names {
            if name.is_empty() {
                return Err(super::invalid(
                    Reason::Invalid,
                    "planner: statement name must not be empty",
                ));
            }
            if !reserved.insert(name.clone()) {
                return Err(super::invalid(
                    Reason::BindingCollision,
                    format!("planner: duplicate statement name {name:?}"),
                ));
            }
        }
        Ok(Self {
            program: HashMap::new(),
            reserved,
            names,
            next: 0,
            next_slot: SlotId(0),
        })
    }

    /// Bind and plan the next relational statement, then publish its result as
    /// a binding visible to subsequent statements.
    pub async fn bind(
        &mut self,
        catalog: &dyn Catalog,
        statement: ProgramStatement,
    ) -> Result<BoundStatement> {
        self.bind_inner(catalog, statement, true).await
    }

    pub async fn bind_reference(
        &mut self,
        catalog: &dyn Catalog,
        statement: ProgramStatement,
    ) -> Result<BoundStatement> {
        self.bind_inner(catalog, statement, false).await
    }

    async fn bind_inner(
        &mut self,
        catalog: &dyn Catalog,
        statement: ProgramStatement,
        physical: bool,
    ) -> Result<BoundStatement> {
        let Some(expected) = self.names.get(self.next) else {
            return Err(super::invalid(
                Reason::Invalid,
                format!("planner: unexpected statement {:?}", statement.name),
            ));
        };
        if &statement.name != expected {
            return Err(super::invalid(
                Reason::Invalid,
                format!(
                    "planner: expected statement {expected:?}, got {:?}",
                    statement.name
                ),
            ));
        }

        let mut binder = Binder::new(catalog);
        binder.next_slot = self.next_slot;
        binder.reserved = self.reserved.clone();
        let mutation = statement.mutation.is_some();
        let bound = if mutation {
            binder
                .bind_bag(statement.relation, Some(&self.program))
                .await
        } else {
            binder
                .bind_query(statement.relation, Some(&self.program))
                .await
        }
        .map_err(|error| error.context(format!("planner: statement {:?}", statement.name)))?;

        let mut plan = physical.then(|| plan_query(&bound, PlanOptions::default()));
        let (result_root, result_output, result_cardinality, target) = if let Some(mutation) =
            statement.mutation
        {
            let table_name = mutation.table;
            let (root, table) =
                Self::table_schema(&mut binder, &table_name)
                    .await
                    .map_err(|error| {
                        error.context(format!("planner: statement {:?}", statement.name))
                    })?;
            validate_mutation_input(mutation.kind, &bound.root.output().clone(), &table).map_err(
                |error| error.context(format!("planner: statement {:?}", statement.name)),
            )?;
            if let Some(plan) = &mut plan {
                plan.dependencies.add_table_write(&table);
            }
            (
                root.clone(),
                root.output().clone(),
                RootCardinality::Many,
                Some(table),
            )
        } else {
            (
                bound.root.clone(),
                bound.root.output().clone(),
                bound.cardinality,
                None,
            )
        };
        self.next_slot = binder.next_slot;

        self.program.insert(
            statement.name.clone(),
            bound::Binding {
                name: statement.name.clone(),
                root: result_root.clone(),
                output: result_root.output().clone(),
                plan_sensitive: false,
                recursive: false,
                step: None,
                accumulation: None,
            },
        );
        self.next += 1;
        Ok(BoundStatement {
            name: statement.name,
            bound,
            plan,
            result_output,
            result_cardinality,
            target,
        })
    }

    async fn table_schema(binder: &mut Binder<'_>, name: &str) -> Result<(bound::Relation, Table)> {
        if name.is_empty() {
            return Err(super::invalid(
                Reason::Invalid,
                "planner: mutation statement needs a target table",
            ));
        }
        let Some(table) = binder.catalog.get_table(name).await? else {
            return Err(super::invalid(
                Reason::UnknownTable,
                format!("planner: unknown table {name:?}"),
            ));
        };
        let slots = binder.fresh_slots(table.columns.len());
        Ok((bound::Relation::scan(table.clone(), "", slots), table))
    }
}

fn validate_mutation_input(kind: MutationKind, input: &lir::RowType, table: &Table) -> Result<()> {
    let mut names = HashSet::new();
    for field in &input.fields {
        if !names.insert(field.name.as_str()) {
            return Err(super::invalid(
                Reason::ProjectionCollision,
                format!("planner: mutation input duplicates column {:?}", field.name),
            ));
        }
        let column = table.column(&field.name).ok_or_else(|| {
            super::invalid(
                Reason::UnknownColumn,
                format!(
                    "planner: mutation of {:?} references unknown column {:?}",
                    table.name, field.name
                ),
            )
        })?;
        if field.value_type.kind.catalog_type() != Some(column.scalar_type) {
            return Err(super::invalid(
                Reason::TypeMismatch,
                format!(
                    "planner: mutation column {:?} has type {}, want {:?}",
                    field.name, field.value_type.kind, column.scalar_type
                ),
            ));
        }
        if field.value_type.nullable && !column.nullable && kind != MutationKind::Delete {
            return Err(super::invalid(
                Reason::TypeMismatch,
                format!(
                    "planner: nullable input is not assignable to non-null column {:?}",
                    field.name
                ),
            ));
        }
    }

    match kind {
        MutationKind::Create => {
            for column in &table.columns {
                if !names.contains(column.name.as_str())
                    && !column.nullable
                    && column.insert_default.is_none()
                {
                    return Err(super::invalid(
                        Reason::TypeMismatch,
                        format!(
                            "planner: create of {:?} omits required column {:?}",
                            table.name, column.name
                        ),
                    ));
                }
            }
        }
        MutationKind::Update => {
            for primary_key in &table.primary_key {
                if !names.contains(primary_key.as_str()) {
                    return Err(super::invalid(
                        Reason::TypeMismatch,
                        format!(
                            "planner: update of {:?} must include primary-key column {primary_key:?}",
                            table.name
                        ),
                    ));
                }
            }
            if input
                .fields
                .iter()
                .all(|field| table.primary_key.contains(&field.name))
            {
                return Err(super::invalid(
                    Reason::TypeMismatch,
                    format!("planner: update of {:?} assigns no columns", table.name),
                ));
            }
        }
        MutationKind::Delete => {
            if input.fields.len() != table.primary_key.len()
                || input
                    .fields
                    .iter()
                    .any(|field| !table.primary_key.contains(&field.name))
            {
                return Err(super::invalid(
                    Reason::TypeMismatch,
                    format!(
                        "planner: delete of {:?} must contain exactly its primary key",
                        table.name
                    ),
                ));
            }
        }
    }
    Ok(())
}
