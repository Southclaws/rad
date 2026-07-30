//! Deliberately naive interpreter for bound logical LIR.
//!
//! This is the semantic oracle for the physical planner and executor. It does
//! not consume a physical plan, choose indexes, batch crossings, or reuse any
//! production relational operator. Scalar evaluation and physical row decoding
//! are shared; both have focused tests of their own.

use std::cmp::Ordering;
use std::collections::HashMap;

use async_recursion::async_recursion;
use num_bigint::BigInt;

use crate::engine::kv::KvView;
use crate::engine::lir::bound::{self, Expr, Relation, RelationNode};
use crate::engine::lir::eval::{Env, evaluate, evaluate_datum, evaluate_predicate};
use crate::engine::lir::{
    AggregateFunction, Datum, JoinKind, Kind, RecursiveAccumulation, RowType, SetQuantifier,
    SlotId, TriBool, Type, Value,
};

use super::{Error, ErrorKind, Limits, Result, row_store, shape_frames};

pub struct ReferenceExecutor<'a> {
    view: &'a dyn KvView,
    limits: Limits,
    next_slot: SlotId,
    bindings: HashMap<String, Vec<Env>>,
    frontier: HashMap<String, Vec<Env>>,
}

impl<'a> ReferenceExecutor<'a> {
    pub fn new(view: &'a dyn KvView, limits: Limits) -> Self {
        Self {
            view,
            limits,
            next_slot: SlotId(0),
            bindings: HashMap::new(),
            frontier: HashMap::new(),
        }
    }

    pub fn seed_bindings(&mut self, bindings: HashMap<String, Vec<Env>>) {
        self.bindings = bindings;
    }

    pub async fn execute(&mut self, query: &bound::Query) -> Result<Datum> {
        let frames = self.run_frames(query).await?;
        shape_frames(query.cardinality, query.root.output(), &frames)
    }

    pub async fn run_frames(&mut self, query: &bound::Query) -> Result<Vec<Env>> {
        self.next_slot = query.next_slot;
        for binding in &query.bindings {
            let frames = if binding.recursive {
                self.fixpoint(binding).await?
            } else {
                self.relation(&binding.root, &Env::new()).await?
            };
            self.bindings.insert(binding.name.clone(), frames);
        }
        self.relation(&query.root, &Env::new()).await
    }

    #[async_recursion]
    async fn relation(&mut self, relation: &Relation, outer: &Env) -> Result<Vec<Env>> {
        match &relation.node {
            RelationNode::Scan { table, .. } => {
                let rows = row_store::scan_table_columns(self.view, table, &table.columns).await?;
                Ok(rows
                    .into_iter()
                    .map(|row| {
                        let mut frame = outer.clone();
                        for field in &relation.output().fields {
                            if let Some(value) = row.get(&field.name) {
                                frame.set_scalar(field.slot, value.clone());
                            }
                        }
                        frame
                    })
                    .collect())
            }
            RelationNode::Rows { values, .. } => Ok(values
                .iter()
                .map(|values| {
                    let mut frame = outer.clone();
                    for (field, value) in relation.output().fields.iter().zip(values) {
                        frame.set_scalar(field.slot, value.clone());
                    }
                    frame
                })
                .collect()),
            RelationNode::Filter { input, predicate } => {
                let mut output = Vec::new();
                for frame in self.relation(input, outer).await? {
                    if self.predicate(predicate, &frame).await? == TriBool::True {
                        output.push(frame);
                    }
                }
                Ok(output)
            }
            RelationNode::Project { input, fields, .. } => {
                let mut output = Vec::new();
                for frame in self.relation(input, outer).await? {
                    let mut projected = outer.clone();
                    for field in fields {
                        projected.insert(field.slot, self.datum(&field.expression, &frame).await?);
                    }
                    output.push(projected);
                }
                Ok(output)
            }
            RelationNode::Join {
                left,
                right,
                kind,
                on,
            } => {
                let left_rows = self.relation(left, outer).await?;
                let right_rows = self.relation(right, outer).await?;
                let mut output = Vec::new();
                for left_row in left_rows {
                    let mut matched = false;
                    for right_row in &right_rows {
                        let merged = merge(&left_row, right_row);
                        if self.predicate(on, &merged).await? == TriBool::True {
                            matched = true;
                            output.push(merged);
                        }
                    }
                    if !matched && *kind == JoinKind::Left {
                        let mut padded = left_row;
                        for field in &right.output().fields {
                            padded.insert(field.slot, Datum::Null);
                        }
                        output.push(padded);
                    }
                }
                Ok(output)
            }
            RelationNode::Concatenate { inputs, .. } => {
                let mut output = Vec::new();
                for input in inputs {
                    for row in self.relation(input, outer).await? {
                        output.push(remap(relation.output(), input.output(), &row, outer));
                    }
                }
                Ok(output)
            }
            RelationNode::Intersect {
                left,
                right,
                quantifier,
                ..
            } => {
                self.set_operation(relation, left, right, *quantifier, false, outer)
                    .await
            }
            RelationNode::Except {
                left,
                right,
                quantifier,
                ..
            } => {
                self.set_operation(relation, left, right, *quantifier, true, outer)
                    .await
            }
            RelationNode::Aggregate {
                input,
                groups,
                terms,
            } => self.aggregate(input, groups, terms, outer).await,
            RelationNode::Order { input, terms } => {
                let rows = self.relation(input, outer).await?;
                let mut decorated = Vec::with_capacity(rows.len());
                for row in rows {
                    let mut keys = Vec::with_capacity(terms.len());
                    for term in terms {
                        keys.push(self.scalar(&term.expression, &row).await?);
                    }
                    decorated.push((row, keys));
                }
                let error = std::cell::RefCell::new(None);
                decorated.sort_by(|(_, left), (_, right)| {
                    for ((left, right), term) in left.iter().zip(right).zip(terms) {
                        let mut ordering = match left.compare(right) {
                            Ok(ordering) => ordering,
                            Err(cause) => {
                                *error.borrow_mut() = Some(cause.to_string());
                                Ordering::Equal
                            }
                        };
                        if term.descending {
                            ordering = ordering.reverse();
                        }
                        if ordering != Ordering::Equal {
                            return ordering;
                        }
                    }
                    Ordering::Equal
                });
                if let Some(cause) = error.into_inner() {
                    return Err(internal(format!("reference: {cause}")));
                }
                Ok(decorated.into_iter().map(|(row, _)| row).collect())
            }
            RelationNode::Slice {
                input,
                offset,
                limit,
            } => Ok(self
                .relation(input, outer)
                .await?
                .into_iter()
                .skip(*offset)
                .take(limit.unwrap_or(usize::MAX))
                .collect()),
            RelationNode::Ref {
                binding, canonical, ..
            } => {
                let rows = self.bindings.get(binding).ok_or_else(|| {
                    internal(format!("reference: binding {binding:?} was not committed"))
                })?;
                Ok(rows
                    .iter()
                    .map(|row| remap_canonical(relation.output(), canonical, row, outer))
                    .collect())
            }
            RelationNode::RecursiveRef {
                binding, canonical, ..
            } => {
                let rows = self.frontier.get(binding).ok_or_else(|| {
                    internal(format!(
                        "reference: frontier for {binding:?} is unavailable"
                    ))
                })?;
                Ok(rows
                    .iter()
                    .map(|row| remap_canonical(relation.output(), canonical, row, outer))
                    .collect())
            }
            RelationNode::Distinct(input) => {
                let mut seen = Vec::<OracleRow>::new();
                let mut output = Vec::new();
                for row in self.relation(input, outer).await? {
                    let identity = OracleRow::from_env(relation.output(), &row);
                    if !seen.contains(&identity) {
                        seen.push(identity);
                        output.push(row);
                    }
                }
                Ok(output)
            }
        }
    }

    async fn set_operation(
        &mut self,
        output: &Relation,
        left: &Relation,
        right: &Relation,
        quantifier: SetQuantifier,
        subtract: bool,
        outer: &Env,
    ) -> Result<Vec<Env>> {
        let left_rows = self.relation(left, outer).await?;
        let right_rows = self.relation(right, outer).await?;
        let mut right_counts = Vec::<(OracleRow, usize)>::new();
        for row in right_rows {
            increment(&mut right_counts, OracleRow::from_env(right.output(), &row));
        }
        let mut emitted = Vec::<OracleRow>::new();
        let mut rows = Vec::new();
        for row in left_rows {
            let identity = OracleRow::from_env(left.output(), &row);
            let count = count_mut(&mut right_counts, &identity);
            let keep = if quantifier == SetQuantifier::Distinct {
                let first = !emitted.contains(&identity);
                // Distinct set operations never consume right-side counts, so
                // presence alone proves membership here.
                let exists = count.is_some();
                first && subtract != exists
            } else if let Some(count) = count.filter(|count| **count > 0) {
                *count -= 1;
                !subtract
            } else {
                subtract
            };
            if keep {
                emitted.push(identity);
                rows.push(remap(output.output(), left.output(), &row, outer));
            }
        }
        Ok(rows)
    }

    async fn aggregate(
        &mut self,
        input: &Relation,
        groups: &[bound::BoundGroupTerm],
        terms: &[bound::BoundAggregateTerm],
        outer: &Env,
    ) -> Result<Vec<Env>> {
        struct Group {
            key: OracleRow,
            values: Vec<Value>,
            accumulators: Vec<Accumulator>,
        }

        let mut folded = Vec::<Group>::new();
        for row in self.relation(input, outer).await? {
            let mut values = Vec::with_capacity(groups.len());
            for group in groups {
                values.push(self.scalar(&group.expression, &row).await?);
            }
            let key = OracleRow::from_values(&values);
            let position = folded.iter().position(|group| group.key == key);
            let index = position.unwrap_or_else(|| {
                folded.push(Group {
                    key,
                    values,
                    accumulators: (0..terms.len()).map(|_| Accumulator::default()).collect(),
                });
                folded.len() - 1
            });
            for (term, accumulator) in terms.iter().zip(&mut folded[index].accumulators) {
                if let Some(argument) = &term.argument {
                    let value = self.scalar(argument, &row).await?;
                    if !value.is_null() {
                        accumulator.accumulate(term.function, value)?;
                    }
                } else {
                    accumulator.count += 1;
                }
            }
        }
        if groups.is_empty() && folded.is_empty() {
            folded.push(Group {
                key: OracleRow(Vec::new()),
                values: Vec::new(),
                accumulators: (0..terms.len()).map(|_| Accumulator::default()).collect(),
            });
        }
        folded
            .into_iter()
            .map(|group| {
                let mut row = outer.clone();
                for (term, value) in groups.iter().zip(group.values) {
                    row.set_scalar(term.slot, value);
                }
                for (term, accumulator) in terms.iter().zip(group.accumulators) {
                    row.set_scalar(term.slot, accumulator.finish(term)?);
                }
                Ok(row)
            })
            .collect()
    }

    async fn fixpoint(&mut self, binding: &bound::Binding) -> Result<Vec<Env>> {
        let step = binding
            .step
            .as_ref()
            .ok_or_else(|| internal("reference: recursive binding has no step"))?;
        let anchor_projection = projection(&binding.output, binding.root.output())?;
        let step_projection = projection(&binding.output, step.output())?;
        let mut seen = Vec::<OracleRow>::new();
        let mut result = Vec::new();
        let mut frontier = Vec::new();
        for row in self.relation(&binding.root, &Env::new()).await? {
            let row = project(&anchor_projection, &row)?;
            let identity = OracleRow::from_env(&binding.output, &row);
            if binding.accumulation != Some(RecursiveAccumulation::New) || !seen.contains(&identity)
            {
                seen.push(identity);
                result.push(row.clone());
                frontier.push(row);
            }
        }
        let mut iteration = 0;
        while !frontier.is_empty() {
            if iteration >= self.limits.max_iterations {
                return Err(Error::message(
                    ErrorKind::RecursionLimit,
                    format!(
                        "reference: recursive binding {:?} exceeded {} iterations",
                        binding.name, self.limits.max_iterations
                    ),
                ));
            }
            iteration += 1;
            self.frontier.insert(binding.name.clone(), frontier);
            let produced = self.relation(step, &Env::new()).await?;
            let mut next = Vec::new();
            for row in produced {
                let row = project(&step_projection, &row)?;
                let identity = OracleRow::from_env(&binding.output, &row);
                if binding.accumulation != Some(RecursiveAccumulation::New)
                    || !seen.contains(&identity)
                {
                    seen.push(identity);
                    result.push(row.clone());
                    next.push(row);
                }
            }
            if result.len() > self.limits.max_rows {
                return Err(Error::message(
                    ErrorKind::RecursionLimit,
                    format!(
                        "reference: recursive binding {:?} exceeded {} rows",
                        binding.name, self.limits.max_rows
                    ),
                ));
            }
            frontier = next;
        }
        self.frontier.remove(&binding.name);
        Ok(result)
    }

    async fn predicate(&mut self, expression: &Expr, environment: &Env) -> Result<TriBool> {
        let mut environment = environment.clone();
        let expression = self.resolve_crossings(expression, &mut environment).await?;
        Ok(evaluate_predicate(&expression, &environment)?)
    }

    async fn scalar(&mut self, expression: &Expr, environment: &Env) -> Result<Value> {
        let mut environment = environment.clone();
        let expression = self.resolve_crossings(expression, &mut environment).await?;
        Ok(evaluate(&expression, &environment)?)
    }

    async fn datum(&mut self, expression: &Expr, environment: &Env) -> Result<Datum> {
        let mut environment = environment.clone();
        let expression = self.resolve_crossings(expression, &mut environment).await?;
        Ok(evaluate_datum(&expression, &environment)?)
    }

    #[async_recursion]
    async fn resolve_crossings(
        &mut self,
        expression: &Expr,
        environment: &mut Env,
    ) -> Result<Expr> {
        let (relation, value_type, datum) = match expression {
            Expr::Exists(relation) => (
                relation.as_ref(),
                Type::scalar(Kind::Bool, false),
                Datum::scalar(Value::Bool(
                    !self.relation(relation, environment).await?.is_empty(),
                )),
            ),
            Expr::First {
                relation,
                value_type,
            } => {
                let rows = self.relation(relation, environment).await?;
                (
                    relation.as_ref(),
                    value_type.clone(),
                    shape_frames(
                        crate::engine::lir::RootCardinality::First,
                        relation.output(),
                        &rows,
                    )?,
                )
            }
            Expr::Scalar {
                relation,
                value_type,
            } => {
                let rows = self.relation(relation, environment).await?;
                (
                    relation.as_ref(),
                    value_type.clone(),
                    shape_frames(
                        crate::engine::lir::RootCardinality::Scalar,
                        relation.output(),
                        &rows,
                    )?,
                )
            }
            Expr::Array {
                relation,
                value_type,
            } => {
                let rows = self.relation(relation, environment).await?;
                (
                    relation.as_ref(),
                    value_type.clone(),
                    shape_frames(
                        crate::engine::lir::RootCardinality::Many,
                        relation.output(),
                        &rows,
                    )?,
                )
            }
            Expr::Unary {
                op,
                expression,
                value_type,
            } => {
                return Ok(Expr::Unary {
                    op: *op,
                    expression: Box::new(self.resolve_crossings(expression, environment).await?),
                    value_type: value_type.clone(),
                });
            }
            Expr::Binary {
                op,
                left,
                right,
                value_type,
            } => {
                return Ok(Expr::Binary {
                    op: *op,
                    left: Box::new(self.resolve_crossings(left, environment).await?),
                    right: Box::new(self.resolve_crossings(right, environment).await?),
                    value_type: value_type.clone(),
                });
            }
            Expr::Cast {
                expression,
                to,
                value_type,
            } => {
                return Ok(Expr::Cast {
                    expression: Box::new(self.resolve_crossings(expression, environment).await?),
                    to: *to,
                    value_type: value_type.clone(),
                });
            }
            Expr::Branch {
                arms,
                otherwise,
                value_type,
            } => {
                let mut resolved = Vec::with_capacity(arms.len());
                for arm in arms {
                    resolved.push(bound::BranchArm {
                        when: self.resolve_crossings(&arm.when, environment).await?,
                        then: self.resolve_crossings(&arm.then, environment).await?,
                    });
                }
                return Ok(Expr::Branch {
                    arms: resolved,
                    otherwise: Box::new(self.resolve_crossings(otherwise, environment).await?),
                    value_type: value_type.clone(),
                });
            }
            Expr::TextMatch {
                value,
                pattern,
                value_type,
            } => {
                return Ok(Expr::TextMatch {
                    value: Box::new(self.resolve_crossings(value, environment).await?),
                    pattern: pattern.clone(),
                    value_type: value_type.clone(),
                });
            }
            Expr::Literal(_) | Expr::SlotRef { .. } => return Ok(expression.clone()),
        };
        let slot = self.next_slot;
        self.next_slot.0 += 1;
        environment.insert(slot, datum);
        Ok(Expr::SlotRef {
            slot,
            name: format!(
                "reference crossing over {} fields",
                relation.output().fields.len()
            ),
            value_type,
        })
    }
}

fn merge(left: &Env, right: &Env) -> Env {
    let mut output = left.clone();
    output.extend_from(right);
    output
}

fn remap(output: &RowType, source: &RowType, row: &Env, outer: &Env) -> Env {
    let mut mapped = outer.clone();
    for (output, source) in output.fields.iter().zip(&source.fields) {
        if let Some(datum) = row.get(source.slot) {
            mapped.insert(output.slot, datum.clone());
        }
    }
    mapped
}

fn remap_canonical(output: &RowType, canonical: &[SlotId], row: &Env, outer: &Env) -> Env {
    let mut mapped = outer.clone();
    for (field, canonical) in output.fields.iter().zip(canonical) {
        if let Some(datum) = row.get(*canonical) {
            mapped.insert(field.slot, datum.clone());
        }
    }
    mapped
}

fn projection(canonical: &RowType, source: &RowType) -> Result<Vec<(SlotId, SlotId)>> {
    canonical
        .fields
        .iter()
        .map(|field| {
            source
                .fields
                .iter()
                .find(|source| source.name == field.name)
                .map(|source| (source.slot, field.slot))
                .ok_or_else(|| {
                    internal(format!(
                        "reference: recursive output missing field {:?}",
                        field.name
                    ))
                })
        })
        .collect()
}

fn project(projection: &[(SlotId, SlotId)], row: &Env) -> Result<Env> {
    let mut output = Env::new();
    for (source, target) in projection {
        let datum = row.get(*source).cloned().ok_or_else(|| {
            internal(format!(
                "reference: recursive row missing source slot {}",
                source.0
            ))
        })?;
        output.insert(*target, datum);
    }
    Ok(output)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OracleRow(Vec<OracleCell>);

impl OracleRow {
    fn from_env(output: &RowType, environment: &Env) -> Self {
        Self(
            output
                .fields
                .iter()
                .map(|field| OracleCell::from_datum(environment.get(field.slot)))
                .collect(),
        )
    }

    fn from_values(values: &[Value]) -> Self {
        Self(values.iter().map(OracleCell::from_value).collect())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum OracleCell {
    Null,
    Text(String),
    Int64(i64),
    Float64(u64),
    Bool(bool),
    Object(Vec<(String, OracleCell)>),
    Array(Vec<OracleCell>),
}

impl OracleCell {
    fn from_datum(datum: Option<&Datum>) -> Self {
        match datum {
            None | Some(Datum::Null) => Self::Null,
            Some(Datum::Scalar(value)) => Self::from_value(value),
            Some(Datum::Object(fields)) => Self::Object(
                fields
                    .iter()
                    .map(|field| (field.name.clone(), Self::from_datum(Some(&field.datum))))
                    .collect(),
            ),
            Some(Datum::Array(elements)) => Self::Array(
                elements
                    .iter()
                    .map(|element| Self::from_datum(Some(element)))
                    .collect(),
            ),
        }
    }

    fn from_value(value: &Value) -> Self {
        match value {
            Value::Null(_) => Self::Null,
            Value::Text(value) => Self::Text(value.clone()),
            Value::Int64(value) => Self::Int64(*value),
            Value::Float64(value) => Self::Float64(if value.is_nan() {
                f64::NAN.to_bits()
            } else if *value == 0.0 {
                0.0_f64.to_bits()
            } else {
                value.to_bits()
            }),
            Value::Bool(value) => Self::Bool(*value),
        }
    }
}

fn increment(counts: &mut Vec<(OracleRow, usize)>, row: OracleRow) {
    if let Some((_, count)) = counts.iter_mut().find(|(candidate, _)| *candidate == row) {
        *count += 1;
    } else {
        counts.push((row, 1));
    }
}

fn count_mut<'a>(counts: &'a mut [(OracleRow, usize)], row: &OracleRow) -> Option<&'a mut usize> {
    counts
        .iter_mut()
        .find(|(candidate, _)| candidate == row)
        .map(|(_, count)| count)
}

#[derive(Default)]
struct Accumulator {
    count: i64,
    values: i64,
    integer_sum: BigInt,
    float_sum: f64,
    minimum: Option<Value>,
    maximum: Option<Value>,
}

impl Accumulator {
    fn accumulate(&mut self, function: AggregateFunction, value: Value) -> Result<()> {
        self.count += 1;
        match function {
            AggregateFunction::Count => {}
            AggregateFunction::Sum | AggregateFunction::Average => {
                self.values += 1;
                match value {
                    Value::Int64(value) => {
                        self.integer_sum += value;
                        self.float_sum += value as f64;
                    }
                    Value::Float64(value) => self.float_sum += value,
                    _ => return Err(internal("reference: non-numeric aggregate argument")),
                }
            }
            AggregateFunction::Min | AggregateFunction::Max => {
                self.values += 1;
                if self
                    .minimum
                    .as_ref()
                    .is_none_or(|minimum| value.compare(minimum).is_ok_and(|order| order.is_lt()))
                {
                    self.minimum = Some(value.clone());
                }
                if self
                    .maximum
                    .as_ref()
                    .is_none_or(|maximum| value.compare(maximum).is_ok_and(|order| order.is_gt()))
                {
                    self.maximum = Some(value);
                }
            }
        }
        Ok(())
    }

    fn finish(self, term: &bound::BoundAggregateTerm) -> Result<Value> {
        let null = || {
            Value::Null(
                term.value_type
                    .kind
                    .catalog_type()
                    .expect("bound aggregate output is scalar"),
            )
        };
        Ok(match term.function {
            AggregateFunction::Count => Value::Int64(self.count),
            AggregateFunction::Sum if self.values == 0 => null(),
            AggregateFunction::Sum if term.value_type.kind == Kind::Int64 => {
                Value::Int64(i64::try_from(self.integer_sum).map_err(|_| {
                    Error::with_reason(
                        ErrorKind::Runtime,
                        super::ErrorReason::NumericOverflow,
                        "reference: integer overflow in sum",
                    )
                })?)
            }
            AggregateFunction::Sum => Value::Float64(self.float_sum),
            AggregateFunction::Average if self.values == 0 => null(),
            AggregateFunction::Average => Value::Float64(self.float_sum / self.values as f64),
            AggregateFunction::Min => self.minimum.unwrap_or_else(null),
            AggregateFunction::Max => self.maximum.unwrap_or_else(null),
        })
    }
}

fn internal(message: impl Into<String>) -> Error {
    Error::message(ErrorKind::Internal, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::kv::slatedb::Store;
    use crate::engine::lir::{BinaryOp, Field, ObjectField};

    fn aggregate_term(function: AggregateFunction, kind: Kind) -> bound::BoundAggregateTerm {
        bound::BoundAggregateTerm {
            function,
            argument: None,
            name: format!("{function:?}"),
            slot: SlotId(0),
            value_type: Type::scalar(kind, true),
        }
    }

    #[test]
    fn reference_mutation_smoke_accumulators_discriminate_every_aggregate_arm() {
        let mut count = Accumulator::default();
        count
            .accumulate(AggregateFunction::Count, Value::Int64(9))
            .unwrap();
        assert_eq!(
            count
                .finish(&aggregate_term(AggregateFunction::Count, Kind::Int64))
                .unwrap(),
            Value::Int64(1)
        );

        let mut integer = Accumulator::default();
        for value in [2, 5, -1] {
            integer
                .accumulate(AggregateFunction::Sum, Value::Int64(value))
                .unwrap();
        }
        assert_eq!(
            integer
                .finish(&aggregate_term(AggregateFunction::Sum, Kind::Int64))
                .unwrap(),
            Value::Int64(6)
        );

        let mut float = Accumulator::default();
        for value in [1.5, 2.25, -0.25] {
            float
                .accumulate(AggregateFunction::Average, Value::Float64(value))
                .unwrap();
        }
        assert_eq!(
            float
                .finish(&aggregate_term(AggregateFunction::Average, Kind::Float64))
                .unwrap(),
            Value::Float64(3.5 / 3.0)
        );

        let mut float_sum = Accumulator::default();
        for value in [1.5, 2.25, -0.25] {
            float_sum
                .accumulate(AggregateFunction::Sum, Value::Float64(value))
                .unwrap();
        }
        assert_eq!(
            float_sum
                .finish(&aggregate_term(AggregateFunction::Sum, Kind::Float64))
                .unwrap(),
            Value::Float64(3.5)
        );

        let mut extrema = Accumulator::default();
        for value in [4, -2, 7] {
            extrema
                .accumulate(AggregateFunction::Min, Value::Int64(value))
                .unwrap();
        }
        assert_eq!(
            extrema
                .finish(&aggregate_term(AggregateFunction::Min, Kind::Int64))
                .unwrap(),
            Value::Int64(-2)
        );

        let mut maximum = Accumulator::default();
        for value in [4, -2, 7] {
            maximum
                .accumulate(AggregateFunction::Max, Value::Int64(value))
                .unwrap();
        }
        assert_eq!(
            maximum
                .finish(&aggregate_term(AggregateFunction::Max, Kind::Int64))
                .unwrap(),
            Value::Int64(7)
        );

        assert!(matches!(
            Accumulator::default()
                .finish(&aggregate_term(AggregateFunction::Sum, Kind::Float64))
                .unwrap(),
            Value::Null(_)
        ));
        assert!(matches!(
            Accumulator::default()
                .finish(&aggregate_term(AggregateFunction::Average, Kind::Float64))
                .unwrap(),
            Value::Null(_)
        ));
    }

    #[tokio::test]
    async fn reference_mutation_smoke_crossings_allocate_fresh_slots() {
        let field = Field {
            name: "n".into(),
            slot: SlotId(0),
            value_type: Type::scalar(Kind::Int64, false),
        };
        let empty = Relation::rows("empty", vec![field.clone()], Vec::new());
        let one = Relation::rows("one", vec![field], vec![vec![Value::Int64(1)]]);
        let expression = Expr::binary(
            BinaryOp::Ne,
            Expr::Exists(Box::new(empty)),
            Expr::Exists(Box::new(one)),
        );
        let store = Store::memory("reference-crossing-slot-allocation")
            .await
            .unwrap();
        let mut executor = ReferenceExecutor::new(&store, Limits::default());
        executor.next_slot = SlotId(10);
        let mut environment = Env::new();
        environment.set_scalar(SlotId(9), Value::Bool(false));

        let resolved = executor
            .resolve_crossings(&expression, &mut environment)
            .await
            .unwrap();
        let Expr::Binary { left, right, .. } = resolved else {
            panic!("crossing comparison must remain binary")
        };
        assert!(matches!(
            *left,
            Expr::SlotRef {
                slot: SlotId(10),
                ..
            }
        ));
        assert!(matches!(
            *right,
            Expr::SlotRef {
                slot: SlotId(11),
                ..
            }
        ));
        assert_eq!(
            environment.get(SlotId(10)),
            Some(&Datum::scalar(Value::Bool(false)))
        );
        assert_eq!(
            environment.get(SlotId(11)),
            Some(&Datum::scalar(Value::Bool(true)))
        );
        assert_eq!(
            environment.get(SlotId(9)),
            Some(&Datum::scalar(Value::Bool(false)))
        );
        assert_eq!(executor.next_slot, SlotId(12));
    }

    #[test]
    fn reference_mutation_smoke_independent_row_identity_normalizes_floats_at_every_depth() {
        assert_eq!(
            OracleCell::from_datum(Some(&Datum::Scalar(Value::Float64(0.0)))),
            OracleCell::from_datum(Some(&Datum::Scalar(Value::Float64(-0.0))))
        );

        let positive = Datum::Array(vec![Datum::Object(vec![ObjectField {
            name: "value".into(),
            datum: Datum::Scalar(Value::Float64(0.0)),
        }])]);
        let negative = Datum::Array(vec![Datum::Object(vec![ObjectField {
            name: "value".into(),
            datum: Datum::Scalar(Value::Float64(-0.0)),
        }])]);
        assert_eq!(
            OracleCell::from_datum(Some(&positive)),
            OracleCell::from_datum(Some(&negative))
        );
    }
}
