use std::collections::{HashMap, HashSet};

use crate::engine::lir::bound::{self, RelationNode};
use crate::engine::lir::{self, Cardinality, Kind, SlotId, Type};

use super::expression::{coerce_literal, contains_crossing};
use super::{Binder, duplicate_column, invalid};
use crate::engine::planner::analysis::{conjuncts, underlying_scan, visit_scan_filters};
use crate::engine::planner::{Reason, Result};

impl Binder<'_> {
    #[async_recursion::async_recursion]
    pub(super) async fn bind_relation(
        &mut self,
        relation: lir::Relation,
    ) -> Result<bound::Relation> {
        match relation {
            lir::Relation::Scan { table, scope } => self.bind_scan(table, scope).await,
            lir::Relation::Rows {
                scope,
                columns,
                values,
            } => self.bind_rows(scope, columns, values),
            lir::Relation::Ref { binding, scope } => self.bind_reference(binding, scope),
            lir::Relation::RecursiveRef { binding, scope } => {
                self.bind_recursive_reference(binding, scope)
            }
            lir::Relation::Recursive { .. } => Err(invalid(
                Reason::Invalid,
                "planner: a recursive relation is only valid as a binding body, not an ordinary node",
            )),
            lir::Relation::Distinct(input) => {
                Ok(bound::Relation::distinct(self.bind_relation(*input).await?))
            }
            lir::Relation::Filter { input, predicate } => {
                let input = self.bind_relation(*input).await?;
                let predicate = self.bind_expression(predicate).await?;
                if predicate.value_type().kind != Kind::Bool {
                    return Err(invalid(
                        Reason::TypeMismatch,
                        format!(
                            "planner: filter predicate must be boolean, got {}",
                            predicate.value_type()
                        ),
                    ));
                }
                let mut filter = bound::Relation::filter(input, predicate);
                Self::refine_unique(&mut filter);
                Ok(filter)
            }
            lir::Relation::Project {
                input,
                scope,
                spread,
                fields,
            } => self.bind_project(*input, scope, spread, fields).await,
            lir::Relation::Join {
                left,
                right,
                kind,
                on,
            } => self.bind_join(*left, *right, kind, on).await,
            lir::Relation::Concatenate { scope, inputs } => {
                self.bind_concatenate(scope, inputs).await
            }
            lir::Relation::Intersect {
                scope,
                left,
                right,
                quantifier,
            } => self.bind_intersect(scope, *left, *right, quantifier).await,
            lir::Relation::Except {
                scope,
                left,
                right,
                quantifier,
            } => self.bind_except(scope, *left, *right, quantifier).await,
            lir::Relation::Aggregate {
                input,
                scope,
                groups,
                terms,
            } => self.bind_aggregate(*input, scope, groups, terms).await,
            lir::Relation::Order { input, terms } => {
                let input = self.bind_relation(*input).await?;
                if terms.is_empty() {
                    return Err(invalid(
                        Reason::Invalid,
                        "planner: order needs at least one term",
                    ));
                }
                let mut bound_terms = Vec::with_capacity(terms.len() + 2);
                for term in terms {
                    let expression = self.bind_expression(term.expression).await?;
                    if !expression.value_type().kind.is_scalar() {
                        return Err(invalid(
                            Reason::TypeMismatch,
                            format!(
                                "planner: cannot order by a {} value",
                                expression.value_type().kind
                            ),
                        ));
                    }
                    bound_terms.push(bound::BoundOrderTerm {
                        expression,
                        descending: term.descending,
                    });
                }
                append_tie_breaker(&input, &mut bound_terms);
                Ok(bound::Relation::order(input, bound_terms))
            }
            lir::Relation::Slice {
                input,
                offset,
                limit,
            } => {
                let input = self.bind_relation(*input).await?;
                if offset > 0 && !input.is_ordered() {
                    return Err(invalid(
                        Reason::NondeterministicOrder,
                        "planner: slice offset over an unordered relation would make membership depend on the access path — add an order",
                    ));
                }
                Ok(bound::Relation::slice(input, offset, limit))
            }
        }
    }

    async fn bind_scan(&mut self, table: String, scope: String) -> Result<bound::Relation> {
        if scope.is_empty() {
            return Err(invalid(
                Reason::Invalid,
                format!("planner: scan of {table:?} needs a scope label"),
            ));
        }
        if self.labels.contains(&scope) {
            return Err(invalid(
                Reason::DuplicateScope,
                format!("planner: duplicate scope {scope:?}"),
            ));
        }
        let Some(table_definition) = self.catalog.get_table(&table).await? else {
            return Err(invalid(
                Reason::UnknownTable,
                format!("planner: unknown table {table:?}"),
            ));
        };
        let slots = self.fresh_slots(table_definition.columns.len());
        let scan = bound::Relation::scan(table_definition, scope.clone(), slots);
        self.expose_scope(scope, scan.clone())?;
        Ok(scan)
    }

    fn bind_rows(
        &mut self,
        scope: String,
        columns: Vec<lir::RowsColumn>,
        values: Vec<Vec<lir::RawScalar>>,
    ) -> Result<bound::Relation> {
        if scope.is_empty() {
            return Err(invalid(
                Reason::Invalid,
                "planner: rows needs a scope label",
            ));
        }
        if self.labels.contains(&scope) {
            return Err(invalid(
                Reason::DuplicateScope,
                format!("planner: duplicate scope {scope:?}"),
            ));
        }
        if columns.is_empty() {
            return Err(invalid(
                Reason::Invalid,
                format!("planner: rows ({scope}) needs at least one column"),
            ));
        }
        let mut names = HashSet::new();
        for column in &columns {
            if column.name.is_empty() || !names.insert(column.name.clone()) {
                return Err(invalid(
                    Reason::ProjectionCollision,
                    format!("planner: rows ({scope}) has an empty or duplicate column name"),
                ));
            }
            if !column.kind.is_scalar() {
                return Err(invalid(
                    Reason::TypeMismatch,
                    format!(
                        "planner: rows ({scope}) column {:?} has unsupported type {}",
                        column.name, column.kind
                    ),
                ));
            }
        }
        let mut bound_values = Vec::with_capacity(values.len());
        for (row_index, row) in values.into_iter().enumerate() {
            if row.len() != columns.len() {
                return Err(invalid(
                    Reason::Invalid,
                    format!(
                        "planner: rows ({scope}) row {row_index} has {} values, want {}",
                        row.len(),
                        columns.len()
                    ),
                ));
            }
            let mut cells = Vec::with_capacity(row.len());
            for (column_index, raw) in row.into_iter().enumerate() {
                let column = &columns[column_index];
                let expression = coerce_literal(raw, &Type::scalar(column.kind, false))?;
                let bound::Expr::Literal(value) = expression else {
                    unreachable!("literal coercion produces a literal")
                };
                if value.is_null() && !column.nullable {
                    return Err(invalid(
                        Reason::TypeMismatch,
                        format!(
                            "planner: rows ({scope}) row {row_index} column {:?} is not nullable",
                            column.name
                        ),
                    ));
                }
                cells.push(value);
            }
            bound_values.push(cells);
        }
        let slots = self.fresh_slots(columns.len());
        let fields = columns
            .into_iter()
            .zip(slots)
            .map(|(column, slot)| lir::Field {
                name: column.name,
                slot,
                value_type: Type::scalar(column.kind, column.nullable),
            })
            .collect();
        let rows = bound::Relation::rows(scope.clone(), fields, bound_values);
        self.expose_scope(scope, rows.clone())?;
        Ok(rows)
    }

    fn bind_reference(&mut self, binding: String, scope: String) -> Result<bound::Relation> {
        if scope.is_empty() {
            return Err(invalid(
                Reason::Invalid,
                format!("planner: ref of binding {binding:?} needs a scope label"),
            ));
        }
        if self.labels.contains(&scope) {
            return Err(invalid(
                Reason::DuplicateScope,
                format!("planner: duplicate scope {scope:?}"),
            ));
        }
        let Some(binding_value) = self.bindings.get(&binding).cloned() else {
            return Err(invalid(
                Reason::UnknownBinding,
                format!("planner: unknown binding {binding:?}"),
            ));
        };
        self.used.insert(binding.clone());
        let output = if binding_value.recursive {
            binding_value.output
        } else {
            binding_value.root.output().clone()
        };
        let (fields, canonical) = self.fresh_occurrence(&output);
        let reference = bound::Relation::reference(binding, scope.clone(), fields, canonical);
        self.expose_scope(scope, reference.clone())?;
        Ok(reference)
    }

    fn bind_recursive_reference(
        &mut self,
        binding: String,
        scope: String,
    ) -> Result<bound::Relation> {
        if self.recursing.as_deref() != Some(&binding) {
            return Err(invalid(
                Reason::UnknownBinding,
                format!(
                    "planner: recursive_ref to {binding:?} is legal only inside that binding's step"
                ),
            ));
        }
        let Some(binding_value) = self.bindings.get(&binding).cloned() else {
            return Err(invalid(
                Reason::UnknownBinding,
                format!("planner: unknown recursive binding {binding:?}"),
            ));
        };
        let (fields, canonical) = self.fresh_occurrence(&binding_value.output);
        let reference =
            bound::Relation::recursive_reference(binding, scope.clone(), fields, canonical);
        self.expose_scope(scope, reference.clone())?;
        Ok(reference)
    }

    async fn bind_project(
        &mut self,
        input: lir::Relation,
        scope: Option<String>,
        spread: Vec<String>,
        fields: Vec<lir::ProjectField>,
    ) -> Result<bound::Relation> {
        let mark = self.scopes.len();
        let input = self.bind_relation(input).await?;
        let mut names = HashSet::new();
        let mut output = Vec::new();
        for label in spread {
            let Some(entry) = self.find_scope(&label, mark) else {
                return Err(invalid(
                    Reason::UnknownScope,
                    format!(
                        "planner: spread scope {label:?} is not produced beneath the projection"
                    ),
                ));
            };
            let spread_fields = entry.relation.output().fields.clone();
            for field in spread_fields {
                add_project_field(
                    &mut names,
                    &mut output,
                    field.name.clone(),
                    field.slot,
                    bound::Expr::slot(
                        field.slot,
                        format!("{label}.{}", field.name),
                        field.value_type,
                    ),
                )?;
            }
        }
        for field in fields {
            let expression = self.bind_expression(field.expression).await?;
            let slot = self.slot_for(&expression);
            add_project_field(&mut names, &mut output, field.name, slot, expression)?;
        }
        if output.is_empty() {
            return Err(invalid(
                Reason::Invalid,
                "planner: projection has no fields",
            ));
        }
        self.scopes.truncate(mark);
        let label = scope.unwrap_or_default();
        let project = bound::Relation::project(input, label.clone(), output);
        if !label.is_empty() {
            self.expose_scope(label, project.clone())?;
        }
        Ok(project)
    }

    async fn bind_join(
        &mut self,
        left: lir::Relation,
        right: lir::Relation,
        kind: lir::JoinKind,
        on: lir::Expr,
    ) -> Result<bound::Relation> {
        let left = self.bind_relation(left).await?;
        let right = self.bind_relation(right).await?;
        for slot in right.free_slots().slots() {
            if left.produced().contains(slot) {
                return Err(invalid(
                    Reason::DependentJoin,
                    format!(
                        "planner: join right side references {} from the left side; join inputs are independent",
                        slot_description(&left, slot).unwrap_or_else(|| "a column".into())
                    ),
                ));
            }
        }
        let on = self.bind_expression(on).await?;
        if on.value_type().kind != Kind::Bool {
            return Err(invalid(
                Reason::TypeMismatch,
                format!(
                    "planner: join condition must be boolean, got {}",
                    on.value_type()
                ),
            ));
        }
        if contains_crossing(&on) {
            return Err(invalid(
                Reason::Invalid,
                "planner: a join condition cannot contain a sub-relation crossing — filter above the join instead",
            ));
        }
        Ok(bound::Relation::join(left, right, kind, on))
    }

    async fn bind_aggregate(
        &mut self,
        input: lir::Relation,
        scope: Option<String>,
        groups: Vec<lir::GroupTerm>,
        terms: Vec<lir::AggregateTerm>,
    ) -> Result<bound::Relation> {
        let mark = self.scopes.len();
        let input = self.bind_relation(input).await?;
        let mut names = HashSet::new();
        let mut bound_groups = Vec::with_capacity(groups.len());
        for group in groups {
            let name = if group.name.is_empty() {
                match &group.expression {
                    lir::Expr::Column { name, .. } => name.clone(),
                    _ => {
                        return Err(invalid(
                            Reason::ProjectionCollision,
                            "planner: group expression needs an output name",
                        ));
                    }
                }
            } else {
                group.name
            };
            let expression = self.bind_expression(group.expression).await?;
            if !expression.value_type().kind.is_scalar() {
                return Err(invalid(
                    Reason::TypeMismatch,
                    "planner: group expressions must be scalar",
                ));
            }
            if !names.insert(name.clone()) {
                return Err(invalid(
                    Reason::ProjectionCollision,
                    "planner: aggregate output names must be unique",
                ));
            }
            let slot = self.slot_for(&expression);
            bound_groups.push(bound::BoundGroupTerm {
                name,
                slot,
                expression,
            });
        }
        let mut bound_terms = Vec::with_capacity(terms.len());
        for term in terms {
            if term.name.is_empty() || !names.insert(term.name.clone()) {
                return Err(invalid(
                    Reason::ProjectionCollision,
                    "planner: aggregate output names must be non-empty and unique",
                ));
            }
            let argument = match term.argument {
                Some(argument) => Some(self.bind_expression(argument).await?),
                None => None,
            };
            validate_aggregate(term.function, argument.as_ref())?;
            let value_type = bound::aggregate_term_type(term.function, argument.as_ref());
            let slot = self.next_slot;
            self.next_slot.0 += 1;
            bound_terms.push(bound::BoundAggregateTerm {
                function: term.function,
                argument,
                name: term.name,
                slot,
                value_type,
            });
        }
        if bound_groups.is_empty() && bound_terms.is_empty() {
            return Err(invalid(
                Reason::Invalid,
                "planner: aggregate needs at least one group or term",
            ));
        }
        self.scopes.truncate(mark);
        let aggregate = bound::Relation::aggregate(input, bound_groups, bound_terms);
        let label = scope.unwrap_or_default();
        if !label.is_empty() {
            self.expose_scope(label, aggregate.clone())?;
        }
        Ok(aggregate)
    }

    async fn bind_concatenate(
        &mut self,
        scope: String,
        inputs: Vec<lir::Relation>,
    ) -> Result<bound::Relation> {
        if inputs.len() < 2 {
            return Err(invalid(
                Reason::Invalid,
                format!(
                    "planner: concatenate needs at least two inputs, got {}",
                    inputs.len()
                ),
            ));
        }
        let mark = self.scopes.len();
        let inputs = self.bind_set_inputs("concatenate", &scope, inputs).await?;
        compatible_set_inputs("concatenate", &inputs)?;
        let first = inputs[0].output().fields.clone();
        let slots = self.fresh_slots(first.len());
        let fields = first
            .into_iter()
            .enumerate()
            .map(|(index, field)| lir::Field {
                name: field.name,
                slot: slots[index],
                value_type: Type::scalar(
                    field.value_type.kind,
                    inputs
                        .iter()
                        .any(|input| input.output().fields[index].value_type.nullable),
                ),
            })
            .collect();
        self.scopes.truncate(mark);
        let relation = bound::Relation::concatenate(inputs, scope.clone(), fields);
        self.expose_scope(scope, relation.clone())?;
        Ok(relation)
    }

    async fn bind_intersect(
        &mut self,
        scope: String,
        left: lir::Relation,
        right: lir::Relation,
        quantifier: lir::SetQuantifier,
    ) -> Result<bound::Relation> {
        let (inputs, fields) = self
            .bind_binary_set_operation("intersect", &scope, left, right)
            .await?;
        let relation = bound::Relation::intersect(
            inputs[0].clone(),
            inputs[1].clone(),
            quantifier,
            scope.clone(),
            fields,
        );
        self.expose_scope(scope, relation.clone())?;
        Ok(relation)
    }

    async fn bind_except(
        &mut self,
        scope: String,
        left: lir::Relation,
        right: lir::Relation,
        quantifier: lir::SetQuantifier,
    ) -> Result<bound::Relation> {
        let (inputs, fields) = self
            .bind_binary_set_operation("except", &scope, left, right)
            .await?;
        let relation = bound::Relation::except(
            inputs[0].clone(),
            inputs[1].clone(),
            quantifier,
            scope.clone(),
            fields,
        );
        self.expose_scope(scope, relation.clone())?;
        Ok(relation)
    }

    async fn bind_binary_set_operation(
        &mut self,
        operation: &str,
        scope: &str,
        left: lir::Relation,
        right: lir::Relation,
    ) -> Result<(Vec<bound::Relation>, Vec<lir::Field>)> {
        let mark = self.scopes.len();
        let inputs = self
            .bind_set_inputs(operation, scope, vec![left, right])
            .await?;
        compatible_set_inputs(operation, &inputs)?;
        let source = inputs[0].output().fields.clone();
        let slots = self.fresh_slots(source.len());
        let fields = source
            .into_iter()
            .zip(slots)
            .map(|(field, slot)| lir::Field { slot, ..field })
            .collect();
        self.scopes.truncate(mark);
        Ok((inputs, fields))
    }

    async fn bind_set_inputs(
        &mut self,
        operation: &str,
        scope: &str,
        inputs: Vec<lir::Relation>,
    ) -> Result<Vec<bound::Relation>> {
        if scope.is_empty() {
            return Err(invalid(
                Reason::Invalid,
                format!("planner: {operation} needs a scope label"),
            ));
        }
        let mut bound_inputs = Vec::with_capacity(inputs.len());
        let mut earlier = bound::SlotSet::default();
        for (index, input) in inputs.into_iter().enumerate() {
            let input = self.bind_relation(input).await?;
            if input
                .free_slots()
                .slots()
                .iter()
                .any(|slot| earlier.contains(*slot))
            {
                return Err(invalid(
                    Reason::DependentJoin,
                    format!(
                        "planner: {operation} input {} references another input; inputs are independent relations",
                        index + 1
                    ),
                ));
            }
            earlier = earlier.union(input.produced());
            bound_inputs.push(input);
        }
        Ok(bound_inputs)
    }

    fn refine_unique(relation: &mut bound::Relation) {
        let RelationNode::Filter { .. } = &relation.node else {
            return;
        };
        let Some(scan) = underlying_scan(relation) else {
            return;
        };
        let slots: HashMap<_, _> = scan
            .output()
            .fields
            .iter()
            .map(|field| (field.slot, field.name.clone()))
            .collect();
        let mut pinned = HashSet::new();
        let _ = visit_scan_filters(relation, &mut |predicate| {
            for conjunct in conjuncts(predicate) {
                let bound::Expr::Binary {
                    op: lir::BinaryOp::Eq,
                    left,
                    right,
                    ..
                } = conjunct
                else {
                    continue;
                };
                for (candidate, other) in [(&**left, &**right), (&**right, &**left)] {
                    if let bound::Expr::SlotRef { slot, .. } = candidate
                        && let Some(column) = slots.get(slot)
                        && other
                            .free_slots()
                            .slots()
                            .iter()
                            .all(|slot| !scan.produced().contains(*slot))
                    {
                        pinned.insert(column.clone());
                    }
                }
            }
        });
        let covers = |columns: &[String]| {
            !columns.is_empty() && columns.iter().all(|column| pinned.contains(column))
        };
        let table = scan.scan_table();
        let unique = covers(&table.primary_key)
            || table
                .indexes
                .iter()
                .any(|index| index.unique && covers(&index.columns));
        if unique {
            relation.refine_cardinality(Cardinality { min: 0, max: 1 });
        }
    }
}

fn add_project_field(
    names: &mut HashSet<String>,
    fields: &mut Vec<bound::ProjectField>,
    name: String,
    slot: SlotId,
    expression: bound::Expr,
) -> Result<()> {
    if name.is_empty() || !names.insert(name.clone()) {
        return Err(invalid(
            Reason::ProjectionCollision,
            format!("planner: duplicate or empty projection field {name:?}"),
        ));
    }
    fields.push(bound::ProjectField {
        name,
        slot,
        expression,
    });
    Ok(())
}

fn validate_aggregate(
    function: lir::AggregateFunction,
    argument: Option<&bound::Expr>,
) -> Result<()> {
    match function {
        lir::AggregateFunction::Count => {
            if argument.is_some_and(|argument| !argument.value_type().kind.is_scalar()) {
                return Err(invalid(
                    Reason::TypeMismatch,
                    "planner: count needs a scalar argument",
                ));
            }
        }
        lir::AggregateFunction::Sum | lir::AggregateFunction::Average => {
            if !argument.is_some_and(|argument| argument.value_type().kind.is_numeric()) {
                return Err(invalid(
                    Reason::TypeMismatch,
                    "planner: sum/avg needs a numeric argument",
                ));
            }
        }
        lir::AggregateFunction::Min | lir::AggregateFunction::Max => {
            if !argument.is_some_and(|argument| argument.value_type().kind.is_scalar()) {
                return Err(invalid(
                    Reason::TypeMismatch,
                    "planner: min/max needs a scalar argument",
                ));
            }
        }
    }
    Ok(())
}

fn compatible_set_inputs(operation: &str, inputs: &[bound::Relation]) -> Result<()> {
    let first = &inputs[0].output().fields;
    if let Some(duplicate) = duplicate_column(inputs[0].output()) {
        return Err(invalid(
            Reason::ProjectionCollision,
            format!("planner: {operation} output has duplicate column {duplicate:?}"),
        ));
    }
    for (input_index, input) in inputs.iter().enumerate().skip(1) {
        if input.output().fields.len() != first.len() {
            return Err(invalid(
                Reason::TypeMismatch,
                format!(
                    "planner: {operation} input 1 has {} columns but input {} has {}",
                    first.len(),
                    input_index + 1,
                    input.output().fields.len()
                ),
            ));
        }
    }
    for (field_index, field) in first.iter().enumerate() {
        if !field.value_type.kind.is_scalar() {
            return Err(invalid(
                Reason::TypeMismatch,
                format!("planner: {operation} combines scalar columns"),
            ));
        }
        for input in &inputs[1..] {
            let other = &input.output().fields[field_index];
            if other.name != field.name || other.value_type.kind != field.value_type.kind {
                return Err(invalid(
                    Reason::TypeMismatch,
                    format!(
                        "planner: {operation} column {} must have the same name and kind in every input",
                        field_index + 1
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn unique_key_fields(relation: &bound::Relation) -> Option<Vec<lir::Field>> {
    match &relation.node {
        RelationNode::Scan { table, .. } => {
            let mut key = Vec::with_capacity(table.primary_key.len());
            for column in &table.primary_key {
                key.push(relation.output().lookup(column)?.clone());
            }
            Some(key)
        }
        RelationNode::Filter { input, .. }
        | RelationNode::Order { input, .. }
        | RelationNode::Slice { input, .. } => unique_key_fields(input),
        RelationNode::Distinct(_) => Some(relation.output().fields.clone()),
        RelationNode::Intersect { quantifier, .. } | RelationNode::Except { quantifier, .. }
            if *quantifier == lir::SetQuantifier::Distinct =>
        {
            Some(relation.output().fields.clone())
        }
        RelationNode::Project { input, .. } => {
            let key = unique_key_fields(input)?;
            let output_slots = relation.output().slots();
            key.iter()
                .all(|field| output_slots.contains(&field.slot))
                .then_some(key)
        }
        RelationNode::Aggregate { groups, .. } if !groups.is_empty() => groups
            .iter()
            .map(|group| relation.output().lookup(&group.name).cloned())
            .collect(),
        _ => None,
    }
}

fn append_tie_breaker(relation: &bound::Relation, terms: &mut Vec<bound::BoundOrderTerm>) {
    let Some(key) = unique_key_fields(relation) else {
        return;
    };
    let referenced: HashSet<_> = terms
        .iter()
        .filter_map(|term| match &term.expression {
            bound::Expr::SlotRef { slot, .. } => Some(*slot),
            _ => None,
        })
        .collect();
    for field in key {
        if !referenced.contains(&field.slot) {
            terms.push(bound::BoundOrderTerm {
                expression: bound::Expr::slot(field.slot, field.name, field.value_type),
                descending: false,
            });
        }
    }
}

fn slot_description(relation: &bound::Relation, slot: SlotId) -> Option<String> {
    if let RelationNode::Scan { scope, .. } = &relation.node
        && let Some(field) = relation
            .output()
            .fields
            .iter()
            .find(|field| field.slot == slot)
    {
        return Some(format!("column {:?} of scope {scope:?}", field.name));
    }
    for input in relation.inputs() {
        if let Some(description) = slot_description(input, slot) {
            return Some(description);
        }
    }
    relation
        .output()
        .fields
        .iter()
        .find(|field| field.slot == slot)
        .map(|field| format!("column {:?}", field.name))
}
