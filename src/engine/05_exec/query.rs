//! Production physical-query executor.
//!
//! Streaming nodes run through the pull pipeline. Blocking and stateful nodes
//! retain a semantics-first materialised implementation while their pull forms
//! are introduced incrementally. The independent reference interpreter lives
//! outside this executor.

use std::cmp::Ordering;
use std::collections::HashMap;

use async_recursion::async_recursion;
use num_bigint::BigInt;

use crate::engine::catalog::store::admit_catalog_dependencies;
use crate::engine::kv::KvView;
use crate::engine::lir::bound::{self, RelationNode};
use crate::engine::lir::eval::{
    CanonicalRowKey, CanonicalRowSet, Env, canonical_row_key, evaluate, evaluate_datum,
    evaluate_predicate,
};
use crate::engine::lir::{
    AggregateFunction, Datum, Kind, ObjectField, RecursiveAccumulation, RootCardinality, RowType,
    SetQuantifier, SlotId, TriBool, Value,
};
use crate::engine::planner::analysis::{ConstValue, CorrelationKind};
use crate::engine::planner::physical::{
    AttachSpec, BindingPlan, BindingPlanKind, BindingStrategy, CrossingKind, Node, Plan,
};

use super::codec;
use super::row_store;
use super::{Error, ErrorKind, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub max_iterations: usize,
    pub max_rows: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_iterations: 10_000,
            max_rows: 1_000_000,
        }
    }
}

pub struct Executor<'a> {
    view: &'a dyn KvView,
    limits: Limits,
    force_nested: bool,
    bindings: HashMap<String, Vec<Env>>,
    frontier: HashMap<String, Vec<Env>>,
    plans: HashMap<String, BindingPlan>,
}

struct SetOperation<'a> {
    left: &'a Node,
    right: &'a Node,
    quantifier: SetQuantifier,
    subtract: bool,
    left_output: &'a RowType,
    right_output: &'a RowType,
    output: &'a RowType,
}

impl<'a> Executor<'a> {
    pub fn new(view: &'a dyn KvView, limits: Limits) -> Self {
        Self {
            view,
            limits,
            force_nested: false,
            bindings: HashMap::new(),
            frontier: HashMap::new(),
            plans: HashMap::new(),
        }
    }

    /// Test seam proving key-correlated batching and nested evaluation agree.
    pub fn set_force_nested(&mut self, force_nested: bool) {
        self.force_nested = force_nested;
    }

    /// Program execution uses this to expose earlier statement results.
    pub fn seed_bindings(&mut self, bindings: HashMap<String, Vec<Env>>) {
        self.bindings = bindings;
    }

    pub async fn run_frames(&mut self, plan: &Plan) -> Result<Vec<Env>> {
        admit_catalog_dependencies(self.view, &plan.dependencies).await?;
        self.commit_bindings(&plan.bindings).await?;
        self.execute_node(&plan.root, &Env::new()).await
    }

    pub async fn execute(&mut self, plan: &Plan) -> Result<Datum> {
        let frames = self.run_frames(plan).await?;
        shape_frames(plan.cardinality, &plan.output, &frames)
    }

    async fn commit_bindings(&mut self, bindings: &[BindingPlan]) -> Result<()> {
        self.plans.extend(
            bindings
                .iter()
                .cloned()
                .map(|binding| (binding.name.clone(), binding)),
        );
        for binding in bindings {
            match &binding.kind {
                BindingPlanKind::Derived {
                    plan,
                    strategy: BindingStrategy::Materialize,
                } => {
                    let frames = self.execute_node(plan, &Env::new()).await?;
                    self.bindings.insert(binding.name.clone(), frames);
                }
                BindingPlanKind::Derived {
                    strategy: BindingStrategy::Replay,
                    ..
                } => {}
                BindingPlanKind::Recursive {
                    anchor,
                    step,
                    step_output,
                    accumulation,
                } => {
                    let frames = self
                        .commit_recursive(binding, anchor, step, step_output, *accumulation)
                        .await?;
                    self.bindings.insert(binding.name.clone(), frames);
                }
            }
        }
        Ok(())
    }

    #[async_recursion]
    async fn execute_node(&mut self, node: &Node, outer: &Env) -> Result<Vec<Env>> {
        if super::pipeline::supports(node) {
            return super::pipeline::execute(self.view, node, outer).await;
        }
        match node {
            Node::PrimaryKeyGet {
                scan,
                key,
                decode_columns,
                ..
            } => {
                let table = scan_table(scan);
                let mut values = crate::engine::lir::Row::new();
                for (column, constant) in table.primary_key.iter().zip(key) {
                    let value = resolve_constant(constant, outer)?;
                    if value.is_null() {
                        return Ok(Vec::new());
                    }
                    values.insert(column.clone(), value);
                }
                Ok(
                    row_store::get_columns(self.view, table, &values, decode_columns)
                        .await?
                        .map(|row| vec![row_to_frame(scan, &row, outer)])
                        .unwrap_or_default(),
                )
            }
            Node::TableScan {
                scan,
                decode_columns,
                ..
            } => Ok(
                row_store::scan_table_columns(self.view, scan_table(scan), decode_columns)
                    .await?
                    .iter()
                    .map(|row| row_to_frame(scan, row, outer))
                    .collect(),
            ),
            Node::IndexRangeScan {
                scan,
                index,
                equality_prefix,
                range,
                decode_columns,
                ..
            } => {
                let equality_prefix = equality_prefix
                    .iter()
                    .map(|constant| resolve_constant(constant, outer))
                    .collect::<Result<Vec<_>>>()?;
                if equality_prefix.iter().any(Value::is_null) {
                    return Ok(Vec::new());
                }
                let range = range.as_ref().map(|range| row_store::Range {
                    lower: range
                        .lower
                        .as_ref()
                        .map(|bound| (&bound.value, bound.inclusive)),
                    upper: range
                        .upper
                        .as_ref()
                        .map(|bound| (&bound.value, bound.inclusive)),
                });
                Ok(row_store::scan_index_range_columns(
                    self.view,
                    scan_table(scan),
                    index,
                    &equality_prefix,
                    range,
                    decode_columns,
                )
                .await?
                .iter()
                .map(|row| row_to_frame(scan, row, outer))
                .collect())
            }
            Node::Rows(relation) => {
                let RelationNode::Rows { values, .. } = &relation.node else {
                    unreachable!()
                };
                Ok(values
                    .iter()
                    .map(|values| {
                        let mut frame = new_frame(outer);
                        for (field, value) in relation.output().fields.iter().zip(values) {
                            frame.set_scalar(field.slot, value.clone());
                        }
                        frame
                    })
                    .collect())
            }
            Node::Filter { input, predicate } => Ok(self
                .execute_node(input, outer)
                .await?
                .into_iter()
                .map(|frame| {
                    let keep = evaluate_predicate(predicate, &frame)? == TriBool::True;
                    Ok((frame, keep))
                })
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .filter_map(|(frame, keep)| keep.then_some(frame))
                .collect()),
            Node::Attach {
                input,
                specifications,
            } => {
                let mut frames = self.execute_node(input, outer).await?;
                for specification in specifications {
                    self.attach(specification, &mut frames, outer).await?;
                }
                Ok(frames)
            }
            Node::Project { input, fields } => self
                .execute_node(input, outer)
                .await?
                .into_iter()
                .map(|input| {
                    let mut output = new_frame(outer);
                    for field in fields {
                        output.insert(field.slot, evaluate_datum(&field.expression, &input)?);
                    }
                    Ok(output)
                })
                .collect(),
            Node::Sort { input, terms } => {
                let frames = self.execute_node(input, outer).await?;
                let keys = frames
                    .iter()
                    .map(|frame| {
                        terms
                            .iter()
                            .map(|term| evaluate(&term.expression, frame).map_err(Into::into))
                            .collect::<Result<Vec<_>>>()
                    })
                    .collect::<Result<Vec<_>>>()?;
                let mut positions = (0..frames.len()).collect::<Vec<_>>();
                let comparison_error = std::cell::RefCell::new(None);
                positions.sort_by(|left, right| {
                    for (index, term) in terms.iter().enumerate() {
                        let comparison = match keys[*left][index].compare(&keys[*right][index]) {
                            Ok(comparison) => comparison,
                            Err(error) => {
                                *comparison_error.borrow_mut() = Some(error.to_string());
                                Ordering::Equal
                            }
                        };
                        let comparison = if term.descending {
                            comparison.reverse()
                        } else {
                            comparison
                        };
                        if comparison != Ordering::Equal {
                            return comparison;
                        }
                    }
                    Ordering::Equal
                });
                if let Some(error) = comparison_error.into_inner() {
                    return Err(Error::message(
                        ErrorKind::Internal,
                        format!("exec: {error}"),
                    ));
                }
                Ok(positions
                    .into_iter()
                    .map(|position| frames[position].clone())
                    .collect())
            }
            Node::Slice {
                input,
                offset,
                limit,
            } => Ok(self
                .execute_node(input, outer)
                .await?
                .into_iter()
                .skip(*offset)
                .take(limit.unwrap_or(usize::MAX))
                .collect()),
            Node::NestedLoopJoin {
                left,
                right,
                kind,
                on,
                right_output,
            } => {
                let left = self.execute_node(left, outer).await?;
                let right = self.execute_node(right, outer).await?;
                let mut output = Vec::new();
                for left in left {
                    let mut matched = false;
                    for right in &right {
                        let merged = merge_frames(&left, right);
                        if evaluate_predicate(on, &merged)? == TriBool::True {
                            matched = true;
                            output.push(merged);
                        }
                    }
                    if *kind == crate::engine::lir::JoinKind::Left && !matched {
                        let mut padded = left;
                        for field in &right_output.fields {
                            padded.insert(field.slot, Datum::Null);
                        }
                        output.push(padded);
                    }
                }
                Ok(output)
            }
            Node::Concatenate {
                inputs,
                input_outputs,
                output,
            } => {
                let mut frames = Vec::new();
                for (input, input_output) in inputs.iter().zip(input_outputs) {
                    frames.extend(
                        self.execute_node(input, outer)
                            .await?
                            .iter()
                            .map(|frame| remap_positional(output, input_output, frame, outer)),
                    );
                }
                Ok(frames)
            }
            Node::Intersect {
                left,
                right,
                quantifier,
                left_output,
                right_output,
                output,
            } => {
                self.execute_set_operation(
                    SetOperation {
                        left,
                        right,
                        quantifier: *quantifier,
                        subtract: false,
                        left_output,
                        right_output,
                        output,
                    },
                    outer,
                )
                .await
            }
            Node::Except {
                left,
                right,
                quantifier,
                left_output,
                right_output,
                output,
            } => {
                self.execute_set_operation(
                    SetOperation {
                        left,
                        right,
                        quantifier: *quantifier,
                        subtract: true,
                        left_output,
                        right_output,
                        output,
                    },
                    outer,
                )
                .await
            }
            Node::Distinct { input, output } => {
                let mut seen = CanonicalRowSet::new(output.fields.clone());
                Ok(self
                    .execute_node(input, outer)
                    .await?
                    .into_iter()
                    .filter(|frame| seen.insert(frame))
                    .collect())
            }
            Node::Aggregate {
                input,
                groups,
                terms,
            } => self.execute_aggregate(input, groups, terms, outer).await,
            Node::Reference {
                binding,
                output,
                canonical,
            } => {
                let frames = if let Some(frames) = self.bindings.get(binding) {
                    frames.clone()
                } else {
                    let plan = self.plans.get(binding).cloned().ok_or_else(|| {
                        Error::message(
                            ErrorKind::Internal,
                            format!("exec: binding {binding:?} was not committed"),
                        )
                    })?;
                    let BindingPlanKind::Derived { plan, .. } = &plan.kind else {
                        return Err(Error::message(
                            ErrorKind::Internal,
                            format!("exec: recursive binding {binding:?} was not committed"),
                        ));
                    };
                    self.execute_node(plan, &Env::new()).await?
                };
                Ok(frames
                    .iter()
                    .map(|frame| remap_canonical(output, canonical, frame, outer))
                    .collect())
            }
            Node::RecursiveReference {
                binding,
                output,
                canonical,
            } => Ok(self
                .frontier
                .get(binding)
                .into_iter()
                .flatten()
                .map(|frame| remap_canonical(output, canonical, frame, outer))
                .collect()),
        }
    }

    async fn execute_set_operation(
        &mut self,
        operation: SetOperation<'_>,
        outer: &Env,
    ) -> Result<Vec<Env>> {
        let left = self.execute_node(operation.left, outer).await?;
        let right = self.execute_node(operation.right, outer).await?;
        let mut right_counts = HashMap::<CanonicalRowKey, usize>::new();
        for frame in &right {
            *right_counts
                .entry(canonical_row_key(&operation.right_output.fields, frame))
                .or_default() += 1;
        }
        let mut emitted = std::collections::HashSet::new();
        let mut frames = Vec::new();
        for frame in left {
            let key = canonical_row_key(&operation.left_output.fields, &frame);
            let keep = if operation.quantifier == SetQuantifier::Distinct {
                if emitted.contains(&key) {
                    false
                } else {
                    let in_right = right_counts.get(&key).copied().unwrap_or_default() > 0;
                    let keep = operation.subtract != in_right;
                    if keep {
                        emitted.insert(key.clone());
                    }
                    keep
                }
            } else if let Some(count) = right_counts.get_mut(&key).filter(|count| **count > 0) {
                *count -= 1;
                !operation.subtract
            } else {
                operation.subtract
            };
            if keep {
                frames.push(remap_positional(
                    operation.output,
                    operation.left_output,
                    &frame,
                    outer,
                ));
            }
        }
        Ok(frames)
    }

    async fn execute_aggregate(
        &mut self,
        input: &Node,
        groups: &[bound::BoundGroupTerm],
        terms: &[bound::BoundAggregateTerm],
        outer: &Env,
    ) -> Result<Vec<Env>> {
        struct Group {
            values: Vec<Value>,
            accumulators: Vec<Accumulator>,
        }
        let input = self.execute_node(input, outer).await?;
        let mut by_key = HashMap::<Vec<u8>, Group>::new();
        let mut order = Vec::new();
        for frame in input {
            let values = groups
                .iter()
                .map(|group| evaluate(&group.expression, &frame).map_err(Into::into))
                .collect::<Result<Vec<_>>>()?;
            let key = codec::encode_tuple(&values)?;
            if !by_key.contains_key(&key) {
                order.push(key.clone());
                by_key.insert(
                    key.clone(),
                    Group {
                        values,
                        accumulators: (0..terms.len()).map(|_| Accumulator::default()).collect(),
                    },
                );
            }
            let group = by_key.get_mut(&key).expect("group inserted");
            for (term, accumulator) in terms.iter().zip(&mut group.accumulators) {
                if term.argument.is_none() {
                    accumulator.count += 1;
                    continue;
                }
                let value = evaluate(term.argument.as_ref().expect("checked"), &frame)?;
                if value.is_null() {
                    continue;
                }
                accumulator.count += 1;
                accumulator.accumulate(term.function, value)?;
            }
        }
        if groups.is_empty() && order.is_empty() {
            order.push(Vec::new());
            by_key.insert(
                Vec::new(),
                Group {
                    values: Vec::new(),
                    accumulators: (0..terms.len()).map(|_| Accumulator::default()).collect(),
                },
            );
        }
        order
            .into_iter()
            .map(|key| {
                let group = by_key.remove(&key).expect("ordered group exists");
                let mut frame = new_frame(outer);
                for (term, value) in groups.iter().zip(group.values) {
                    frame.set_scalar(term.slot, value);
                }
                for (term, accumulator) in terms.iter().zip(group.accumulators) {
                    frame.set_scalar(term.slot, accumulator.finish(term)?);
                }
                Ok(frame)
            })
            .collect()
    }

    async fn attach(
        &mut self,
        specification: &AttachSpec,
        frames: &mut [Env],
        outer: &Env,
    ) -> Result<()> {
        match specification.correlation.kind {
            CorrelationKind::Uncorrelated => {
                let datum = self.run_attach(specification, outer).await?;
                for frame in frames {
                    frame.insert(specification.slot, datum.clone());
                }
            }
            CorrelationKind::Key if !self.force_nested => {
                let mut groups = HashMap::<Vec<u8>, (Env, Vec<usize>)>::new();
                let mut order = Vec::new();
                for (index, frame) in frames.iter().enumerate() {
                    let values = specification
                        .correlation
                        .keys
                        .iter()
                        .map(|key| {
                            frame
                                .scalar_at(
                                    key.outer_slot,
                                    &key.inner_column,
                                    &crate::engine::lir::Type::catalog_scalar(
                                        scan_column_type(&specification.plan, &key.inner_column),
                                        true,
                                    ),
                                )
                                .map_err(Into::into)
                        })
                        .collect::<Result<Vec<_>>>()?;
                    let encoded = codec::encode_tuple(&values)?;
                    if !groups.contains_key(&encoded) {
                        let mut environment = new_frame(outer);
                        for (key, value) in specification.correlation.keys.iter().zip(values) {
                            environment.set_scalar(key.outer_slot, value);
                        }
                        order.push(encoded.clone());
                        groups.insert(encoded.clone(), (environment, Vec::new()));
                    }
                    groups
                        .get_mut(&encoded)
                        .expect("group inserted")
                        .1
                        .push(index);
                }
                for key in order {
                    let (environment, positions) = groups.remove(&key).expect("group exists");
                    let datum = self.run_attach(specification, &environment).await?;
                    for position in positions {
                        frames[position].insert(specification.slot, datum.clone());
                    }
                }
            }
            _ => {
                for frame in frames {
                    let datum = self.run_attach(specification, frame).await?;
                    frame.insert(specification.slot, datum);
                }
            }
        }
        Ok(())
    }

    async fn run_attach(&mut self, specification: &AttachSpec, outer: &Env) -> Result<Datum> {
        let frames = self.execute_node(&specification.plan, outer).await?;
        Ok(match specification.kind {
            CrossingKind::Exists => Datum::scalar(Value::Bool(!frames.is_empty())),
            CrossingKind::First => frames
                .first()
                .map(|frame| frame_to_object(&specification.output, frame))
                .unwrap_or(Datum::Null),
            CrossingKind::Scalar => frames
                .first()
                .map(|frame| frame_scalar(&specification.output, frame))
                .unwrap_or(Datum::Null),
            CrossingKind::Array => frames_to_array(&specification.output, &frames),
        })
    }

    async fn commit_recursive(
        &mut self,
        binding: &BindingPlan,
        anchor: &Node,
        step: &Node,
        step_output: &RowType,
        accumulation: RecursiveAccumulation,
    ) -> Result<Vec<Env>> {
        let anchor_slots = slots_by_name(&binding.output);
        let step_slots = slots_by_name(step_output);
        let canonical = &binding.output;
        let mut seen = (accumulation == RecursiveAccumulation::New)
            .then(|| CanonicalRowSet::new(canonical.fields.clone()));
        let mut result = Vec::new();
        let mut frontier = Vec::new();
        for frame in self.execute_node(anchor, &Env::new()).await? {
            let frame = project_canonical(canonical, &anchor_slots, &frame);
            if seen.as_mut().is_none_or(|seen| seen.insert(&frame)) {
                result.push(frame.clone());
                frontier.push(frame);
            }
        }
        let mut iteration = 0;
        while !frontier.is_empty() {
            if iteration >= self.limits.max_iterations {
                return Err(Error::message(
                    ErrorKind::RecursionLimit,
                    format!(
                        "exec: recursive binding {:?} exceeded {} iterations",
                        binding.name, self.limits.max_iterations
                    ),
                ));
            }
            iteration += 1;
            self.frontier.insert(binding.name.clone(), frontier);
            let produced = self.execute_node(step, &Env::new()).await?;
            let mut next = Vec::new();
            for frame in produced {
                let frame = project_canonical(canonical, &step_slots, &frame);
                if seen.as_mut().is_none_or(|seen| seen.insert(&frame)) {
                    next.push(frame);
                }
            }
            result.extend(next.iter().cloned());
            if result.len() > self.limits.max_rows {
                return Err(Error::message(
                    ErrorKind::RecursionLimit,
                    format!(
                        "exec: recursive binding {:?} exceeded {} rows",
                        binding.name, self.limits.max_rows
                    ),
                ));
            }
            frontier = next;
        }
        self.frontier.remove(&binding.name);
        Ok(result)
    }
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
                    _ => {
                        return Err(Error::message(
                            ErrorKind::Internal,
                            "exec: non-numeric aggregate argument",
                        ));
                    }
                }
            }
            AggregateFunction::Min | AggregateFunction::Max => {
                self.values += 1;
                if self
                    .minimum
                    .as_ref()
                    .is_none_or(|minimum| value.compare(minimum).is_ok_and(|o| o.is_lt()))
                {
                    self.minimum = Some(value.clone());
                }
                if self
                    .maximum
                    .as_ref()
                    .is_none_or(|maximum| value.compare(maximum).is_ok_and(|o| o.is_gt()))
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
                    .expect("aggregate output is scalar"),
            )
        };
        let value = match term.function {
            AggregateFunction::Count => Value::Int64(self.count),
            AggregateFunction::Sum if self.values == 0 => null(),
            AggregateFunction::Sum if term.value_type.kind == Kind::Int64 => {
                Value::Int64(i64::try_from(self.integer_sum).map_err(|_| {
                    Error::with_reason(
                        ErrorKind::Runtime,
                        super::ErrorReason::NumericOverflow,
                        "exec: integer overflow in sum",
                    )
                })?)
            }
            AggregateFunction::Sum => Value::Float64(self.float_sum),
            AggregateFunction::Average if self.values == 0 => null(),
            AggregateFunction::Average => Value::Float64(self.float_sum / self.values as f64),
            AggregateFunction::Min => self.minimum.unwrap_or_else(null),
            AggregateFunction::Max => self.maximum.unwrap_or_else(null),
        };
        if matches!(&value, Value::Float64(value) if !value.is_finite()) {
            return Err(Error::with_reason(
                ErrorKind::Runtime,
                super::ErrorReason::NumericOverflow,
                "exec: non-finite float aggregate result",
            ));
        }
        Ok(value)
    }
}

pub fn shape_frames(
    cardinality: RootCardinality,
    output: &RowType,
    frames: &[Env],
) -> Result<Datum> {
    match cardinality {
        RootCardinality::Many => Ok(frames_to_array(output, frames)),
        RootCardinality::First => Ok(frames
            .first()
            .map(|frame| frame_to_object(output, frame))
            .unwrap_or(Datum::Null)),
        RootCardinality::ExactlyOne => match frames {
            [frame] => Ok(frame_to_object(output, frame)),
            [] => Err(Error::with_reason(
                ErrorKind::Runtime,
                super::ErrorReason::CardinalityViolation,
                "exec: expected exactly one row, got none",
            )),
            _ => Err(Error::with_reason(
                ErrorKind::Runtime,
                super::ErrorReason::CardinalityViolation,
                "exec: expected exactly one row, got more",
            )),
        },
        RootCardinality::Scalar => Ok(frames
            .first()
            .map(|frame| frame_scalar(output, frame))
            .unwrap_or(Datum::Null)),
    }
}

pub(super) fn new_frame(outer: &Env) -> Env {
    outer.clone()
}

pub(super) fn merge_frames(left: &Env, right: &Env) -> Env {
    let mut output = left.clone();
    output.extend_from(right);
    output
}

pub(super) fn row_to_frame(
    relation: &bound::Relation,
    row: &crate::engine::lir::Row,
    outer: &Env,
) -> Env {
    let mut frame = new_frame(outer);
    for field in &relation.output().fields {
        if let Some(value) = row.get(&field.name) {
            frame.set_scalar(field.slot, value.clone());
        }
    }
    frame
}

fn frame_to_object(output: &RowType, frame: &Env) -> Datum {
    Datum::Object(
        output
            .fields
            .iter()
            .map(|field| ObjectField {
                name: field.name.clone(),
                datum: frame.get(field.slot).cloned().unwrap_or(Datum::Null),
            })
            .collect(),
    )
}

fn frame_scalar(output: &RowType, frame: &Env) -> Datum {
    output
        .fields
        .first()
        .and_then(|field| frame.get(field.slot))
        .cloned()
        .unwrap_or(Datum::Null)
}

fn frames_to_array(output: &RowType, frames: &[Env]) -> Datum {
    Datum::Array(
        frames
            .iter()
            .map(|frame| frame_to_object(output, frame))
            .collect(),
    )
}

fn remap_canonical(output: &RowType, canonical: &[SlotId], source: &Env, outer: &Env) -> Env {
    let mut frame = new_frame(outer);
    for (field, canonical) in output.fields.iter().zip(canonical) {
        if let Some(datum) = source.get(*canonical) {
            frame.insert(field.slot, datum.clone());
        }
    }
    frame
}

pub(super) fn remap_positional(
    output: &RowType,
    source_output: &RowType,
    source: &Env,
    outer: &Env,
) -> Env {
    let mut frame = new_frame(outer);
    for (field, source_field) in output.fields.iter().zip(&source_output.fields) {
        if let Some(datum) = source.get(source_field.slot) {
            frame.insert(field.slot, datum.clone());
        }
    }
    frame
}

pub(super) fn resolve_constant(value: &ConstValue, outer: &Env) -> Result<Value> {
    match value {
        ConstValue::Literal(value) => Ok(value.clone()),
        ConstValue::Outer(slot) => outer
            .get(*slot)
            .and_then(|datum| match datum {
                Datum::Scalar(value) => Some(value.clone()),
                Datum::Null => Some(Value::Null(crate::engine::catalog::model::ScalarType::Text)),
                _ => None,
            })
            .ok_or_else(|| {
                Error::message(
                    ErrorKind::Internal,
                    format!("exec: outer slot {} is not scalar", slot.0),
                )
            }),
    }
}

pub(super) fn scan_table(relation: &bound::Relation) -> &crate::engine::catalog::model::Table {
    let RelationNode::Scan { table, .. } = &relation.node else {
        unreachable!("physical access contains a scan relation")
    };
    table
}

fn scan_column_type(node: &Node, name: &str) -> crate::engine::catalog::model::ScalarType {
    match node {
        Node::PrimaryKeyGet { scan, .. }
        | Node::TableScan { scan, .. }
        | Node::IndexRangeScan { scan, .. } => scan_table(scan)
            .column(name)
            .map(|column| column.scalar_type)
            .unwrap_or(crate::engine::catalog::model::ScalarType::Text),
        Node::Filter { input, .. }
        | Node::Project { input, .. }
        | Node::Sort { input, .. }
        | Node::Slice { input, .. }
        | Node::Distinct { input, .. }
        | Node::Aggregate { input, .. }
        | Node::Attach { input, .. } => scan_column_type(input, name),
        _ => crate::engine::catalog::model::ScalarType::Text,
    }
}

fn slots_by_name(output: &RowType) -> HashMap<String, SlotId> {
    output
        .fields
        .iter()
        .map(|field| (field.name.clone(), field.slot))
        .collect()
}

fn project_canonical(output: &RowType, source: &HashMap<String, SlotId>, frame: &Env) -> Env {
    let mut projected = Env::new();
    for field in &output.fields {
        if let Some(datum) = source.get(&field.name).and_then(|slot| frame.get(*slot)) {
            projected.insert(field.slot, datum.clone());
        }
    }
    projected
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use async_trait::async_trait;
    use bytes::Bytes;

    use crate::engine::catalog::identity::{
        AccessGeneration, DefinitionGeneration, ExistenceGeneration, LogicalIndexId, SchemaId,
        ValueGeneration, WriteProtocolGeneration,
    };
    use crate::engine::catalog::model::{Column, Index, IndexState, ScalarType, Table};
    use crate::engine::exec::{ErrorReason, ReferenceExecutor};
    use crate::engine::kv::slatedb::Store;
    use crate::engine::kv::{Entry, KeyRange, Kv, KvIterator, KvView, TransactionalKv};
    use crate::engine::lir::bound::{BoundOrderTerm, Expr, ProjectField, Relation};
    use crate::engine::lir::{BinaryOp, Field, Type};
    use crate::engine::planner::{PlanOptions, plan_query};

    use super::*;

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
    fn float_aggregates_reject_non_finite_results() {
        for function in [AggregateFunction::Sum, AggregateFunction::Average] {
            let mut accumulator = Accumulator::default();
            for value in [f64::MAX, f64::MAX] {
                accumulator
                    .accumulate(function, Value::Float64(value))
                    .unwrap();
            }
            let error = accumulator
                .finish(&aggregate_term(function, Kind::Float64))
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Runtime);
            assert_eq!(error.reason(), ErrorReason::NumericOverflow);
        }
    }

    fn table() -> Table {
        Table {
            id: "t1".into(),
            schema_id: SchemaId::new(1).unwrap(),
            name: "tasks".into(),
            definition_generation: DefinitionGeneration::ZERO,
            existence_generation: ExistenceGeneration::ZERO,
            write_protocol_generation: WriteProtocolGeneration::ZERO,
            columns: ["id", "board_id", "status"]
                .into_iter()
                .enumerate()
                .map(|(index, name)| Column {
                    id: format!("c{}", index + 1).into(),
                    schema_id: SchemaId::new(index as u32 + 1).unwrap(),
                    name: name.into(),
                    value_generation: ValueGeneration::ZERO,
                    scalar_type: ScalarType::Text,
                    nullable: false,
                    format: String::new(),
                    insert_default: None,
                    missing_value: None,
                })
                .collect(),
            primary_key: vec!["id".into()],
            indexes: vec![Index {
                id: "i1".into(),
                logical_id: LogicalIndexId::from("board-status"),
                definition_generation: DefinitionGeneration::ZERO,
                access_generation: AccessGeneration::ZERO,
                state: IndexState::Ready,
                name: "tasks_board_status_idx".into(),
                columns: vec!["board_id".into(), "status".into()],
                column_ids: vec!["c2".into(), "c3".into()],
                unique: false,
            }],
            foreign_keys: Vec::new(),
            constraints: Vec::new(),
        }
    }

    fn scan(table: &Table, slots: [usize; 3], scope: &str) -> Relation {
        Relation::scan(
            table.clone(),
            scope,
            slots.into_iter().map(SlotId).collect(),
        )
    }

    fn expression(relation: &Relation, name: &str) -> Expr {
        let field = relation.output().lookup(name).unwrap();
        Expr::slot(field.slot, name, field.value_type.clone())
    }

    fn query(root: Relation, next_slot: usize) -> bound::Query {
        bound::Query {
            root,
            cardinality: RootCardinality::Many,
            bindings: Vec::new(),
            next_slot: SlotId(next_slot),
        }
    }

    async fn seed(store: &Store, table: &Table) {
        for (id, board, status) in [
            ("t1", "b1", "open"),
            ("t2", "b1", "done"),
            ("t3", "b2", "alpha"),
        ] {
            let row = crate::engine::lir::Row::from([
                ("id".into(), Value::Text(id.into())),
                ("board_id".into(), Value::Text(board.into())),
                ("status".into(), Value::Text(status.into())),
            ]);
            let primary_key = codec::encode_row_tuple(&row, &table.primary_key).unwrap();
            Kv::put(
                store,
                Bytes::from(codec::data_key(table, &primary_key)),
                Bytes::from(codec::marshal_row(table, &row).unwrap()),
            )
            .await
            .unwrap();
            let indexed = codec::encode_row_tuple(
                &row,
                &table
                    .index_column_names(&table.indexes[0])
                    .into_iter()
                    .map(str::to_owned)
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            Kv::put(
                store,
                Bytes::from(codec::index_key(
                    table,
                    &table.indexes[0].id,
                    &indexed,
                    &primary_key,
                )),
                Bytes::from(primary_key),
            )
            .await
            .unwrap();
        }
    }

    struct CountingView {
        store: Store,
        data_prefix: Vec<u8>,
        data_gets: AtomicUsize,
        scan_nexts: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl KvView for CountingView {
        async fn get(&self, key: &[u8]) -> crate::engine::kv::Result<Option<Bytes>> {
            if key.starts_with(&self.data_prefix) {
                self.data_gets.fetch_add(1, AtomicOrdering::Relaxed);
            }
            Kv::get(&self.store, key).await
        }

        async fn put(&self, key: Bytes, value: Bytes) -> crate::engine::kv::Result<()> {
            Kv::put(&self.store, key, value).await
        }

        async fn delete(&self, key: &[u8]) -> crate::engine::kv::Result<()> {
            Kv::delete(&self.store, key).await
        }

        async fn scan<'a>(
            &'a self,
            range: KeyRange,
        ) -> crate::engine::kv::Result<Box<dyn KvIterator + 'a>> {
            Ok(Box::new(CountingIterator {
                inner: Kv::scan(&self.store, range).await?,
                nexts: Arc::clone(&self.scan_nexts),
            }))
        }
    }

    struct CountingIterator {
        inner: Box<dyn KvIterator>,
        nexts: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl KvIterator for CountingIterator {
        async fn next(&mut self) -> crate::engine::kv::Result<Option<Entry>> {
            self.nexts.fetch_add(1, AtomicOrdering::Relaxed);
            self.inner.next().await
        }
    }

    #[tokio::test]
    async fn chosen_index_and_forced_scan_are_result_equivalent() {
        let table = table();
        let store = Store::memory("exec-plan-equivalence").await.unwrap();
        seed(&store, &table).await;
        let scan = scan(&table, [0, 1, 2], "t");
        let filtered = Relation::filter(
            scan.clone(),
            Expr::binary(
                BinaryOp::Eq,
                expression(&scan, "board_id"),
                Expr::literal(Value::Text("b1".into())),
            ),
        );
        let ordered = Relation::order(
            filtered,
            vec![
                BoundOrderTerm {
                    expression: expression(&scan, "status"),
                    descending: false,
                },
                BoundOrderTerm {
                    expression: expression(&scan, "id"),
                    descending: false,
                },
            ],
        );
        let bound = query(ordered, 3);
        let chosen = plan_query(&bound, PlanOptions::default());
        let oracle = plan_query(
            &bound,
            PlanOptions {
                full_scan_only: true,
            },
        );
        let chosen_result = {
            Executor::new(&store, Limits::default())
                .execute(&chosen)
                .await
                .unwrap()
        };
        let oracle_result = {
            Executor::new(&store, Limits::default())
                .execute(&oracle)
                .await
                .unwrap()
        };
        assert_eq!(chosen_result, oracle_result);
        let reference_result = ReferenceExecutor::new(&store, Limits::default())
            .execute(&bound)
            .await
            .unwrap();
        assert_eq!(chosen_result, reference_result);
        let Datum::Array(rows) = chosen_result else {
            panic!("many result must be an array")
        };
        assert_eq!(rows.len(), 2);
        assert!(matches!(
            &rows[0],
            Datum::Object(fields)
                if fields.iter().any(|field| field.name == "id" && field.datum == Datum::scalar(Value::Text("t2".into())))
        ));
    }

    #[tokio::test]
    async fn slice_stops_index_iteration_and_base_row_fetches_at_the_limit() {
        let table = table();
        let store = Store::memory("exec-pull-slice").await.unwrap();
        seed(&store, &table).await;
        let view = CountingView {
            store,
            data_prefix: codec::data_prefix(&table),
            data_gets: AtomicUsize::new(0),
            scan_nexts: Arc::new(AtomicUsize::new(0)),
        };
        let scan = scan(&table, [0, 1, 2], "t");
        let filtered = Relation::filter(
            scan.clone(),
            Expr::binary(
                BinaryOp::Eq,
                expression(&scan, "board_id"),
                Expr::literal(Value::Text("b1".into())),
            ),
        );
        let plan = plan_query(
            &query(Relation::slice(filtered, 0, Some(1)), 3),
            PlanOptions::default(),
        );
        assert!(matches!(plan.root, Node::Slice { .. }));

        let frames = Executor::new(&view, Limits::default())
            .run_frames(&plan)
            .await
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(view.scan_nexts.load(AtomicOrdering::Relaxed), 1);
        assert_eq!(view.data_gets.load(AtomicOrdering::Relaxed), 1);

        view.store.close().await.unwrap();
    }

    #[tokio::test]
    async fn key_correlated_attach_matches_nested_execution() {
        let table = table();
        let store = Store::memory("exec-correlated-attach").await.unwrap();
        seed(&store, &table).await;
        let outer = Relation::rows(
            "boards",
            vec![Field {
                name: "board_id".into(),
                slot: SlotId(0),
                value_type: Type::scalar(Kind::Text, false),
            }],
            vec![
                vec![Value::Text("b1".into())],
                vec![Value::Text("b1".into())],
                vec![Value::Text("b3".into())],
            ],
        );
        let inner_scan = scan(&table, [10, 11, 12], "tasks");
        let inner = Relation::filter(
            inner_scan.clone(),
            Expr::binary(
                BinaryOp::Eq,
                expression(&inner_scan, "board_id"),
                Expr::slot(
                    SlotId(0),
                    "boards.board_id",
                    Type::scalar(Kind::Text, false),
                ),
            ),
        );
        let root = Relation::project(
            outer,
            "result",
            vec![
                ProjectField {
                    name: "board_id".into(),
                    slot: SlotId(20),
                    expression: Expr::slot(
                        SlotId(0),
                        "boards.board_id",
                        Type::scalar(Kind::Text, false),
                    ),
                },
                ProjectField {
                    name: "has_tasks".into(),
                    slot: SlotId(21),
                    expression: Expr::exists(inner),
                },
            ],
        );
        let bound = query(root, 22);
        let plan = plan_query(&bound, PlanOptions::default());
        let batched = {
            Executor::new(&store, Limits::default())
                .execute(&plan)
                .await
                .unwrap()
        };
        let nested = {
            let mut executor = Executor::new(&store, Limits::default());
            executor.set_force_nested(true);
            executor.execute(&plan).await.unwrap()
        };
        assert_eq!(batched, nested);
        let reference = ReferenceExecutor::new(&store, Limits::default())
            .execute(&bound)
            .await
            .unwrap();
        assert_eq!(batched, reference);
        let Datum::Array(rows) = batched else {
            panic!("many result must be an array")
        };
        assert_eq!(rows.len(), 3);
        assert!(matches!(
            &rows[2],
            Datum::Object(fields)
                if fields.iter().any(|field| field.name == "has_tasks" && field.datum == Datum::scalar(Value::Bool(false)))
        ));
    }

    #[tokio::test]
    async fn reference_mutation_smoke_aggregates_use_exact_integer_sums_and_global_empty_rows() {
        let rows = Relation::rows(
            "numbers",
            vec![Field {
                name: "n".into(),
                slot: SlotId(0),
                value_type: Type::scalar(Kind::Int64, false),
            }],
            vec![vec![Value::Int64(2)], vec![Value::Int64(3)]],
        );
        let aggregate = Relation::aggregate(
            rows,
            Vec::new(),
            vec![
                bound::BoundAggregateTerm {
                    function: AggregateFunction::Count,
                    argument: None,
                    name: "count".into(),
                    slot: SlotId(1),
                    value_type: Type::scalar(Kind::Int64, false),
                },
                bound::BoundAggregateTerm {
                    function: AggregateFunction::Sum,
                    argument: Some(Expr::slot(SlotId(0), "n", Type::scalar(Kind::Int64, false))),
                    name: "sum".into(),
                    slot: SlotId(2),
                    value_type: Type::scalar(Kind::Int64, true),
                },
            ],
        );
        let bound = query(aggregate, 3);
        let plan = plan_query(&bound, PlanOptions::default());
        let store = Store::memory("exec-aggregate").await.unwrap();
        let result = {
            Executor::new(&store, Limits::default())
                .execute(&plan)
                .await
                .unwrap()
        };
        let reference = ReferenceExecutor::new(&store, Limits::default())
            .execute(&bound)
            .await
            .unwrap();
        assert_eq!(result, reference);
        assert!(matches!(
            result,
            Datum::Array(rows)
                if matches!(&rows[0], Datum::Object(fields)
                    if fields[0].datum == Datum::scalar(Value::Int64(2))
                        && fields[1].datum == Datum::scalar(Value::Int64(5)))
        ));
    }

    fn int_rows(scope: &str, slot: usize, values: &[i64]) -> Relation {
        Relation::rows(
            scope,
            vec![Field {
                name: "n".into(),
                slot: SlotId(slot),
                value_type: Type::scalar(Kind::Int64, false),
            }],
            values
                .iter()
                .map(|value| vec![Value::Int64(*value)])
                .collect(),
        )
    }

    #[tokio::test]
    async fn reference_mutation_smoke_bag_set_operations_consume_occurrences_positionally() {
        for (quantifier, intersect, except) in [
            (SetQuantifier::All, vec![1, 2], vec![1, 3]),
            (SetQuantifier::Distinct, vec![1, 2], vec![3]),
        ] {
            let left = int_rows("left", 0, &[1, 1, 2, 3]);
            let right = int_rows("right", 1, &[1, 2, 2]);
            let field = Field {
                name: "n".into(),
                slot: SlotId(2),
                value_type: Type::scalar(Kind::Int64, false),
            };
            let relations = [
                (
                    Relation::intersect(
                        left.clone(),
                        right.clone(),
                        quantifier,
                        "intersect",
                        vec![field.clone()],
                    ),
                    intersect,
                ),
                (
                    Relation::except(
                        left.clone(),
                        right.clone(),
                        quantifier,
                        "except",
                        vec![field.clone()],
                    ),
                    except,
                ),
            ];
            for (relation, expected) in relations {
                let bound = query(relation, 3);
                let plan = plan_query(&bound, PlanOptions::default());
                let store = Store::memory(&format!("exec-set-{quantifier:?}-{}", expected.len()))
                    .await
                    .unwrap();
                let frames = Executor::new(&store, Limits::default())
                    .run_frames(&plan)
                    .await
                    .unwrap();
                let reference = ReferenceExecutor::new(&store, Limits::default())
                    .run_frames(&bound)
                    .await
                    .unwrap();
                assert_eq!(frames, reference);
                assert_eq!(
                    frames
                        .iter()
                        .map(|frame| frame.get(SlotId(2)).cloned().unwrap())
                        .collect::<Vec<_>>(),
                    expected
                        .into_iter()
                        .map(|value| Datum::scalar(Value::Int64(value)))
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    #[tokio::test]
    async fn single_reference_replays_the_binding_commitment() {
        let body = int_rows("canonical", 0, &[4, 5]);
        let output = body.output().clone();
        let reference = Relation::reference(
            "numbers",
            "occurrence",
            vec![Field {
                name: "n".into(),
                slot: SlotId(1),
                value_type: Type::scalar(Kind::Int64, false),
            }],
            vec![SlotId(0)],
        );
        let bound = bound::Query {
            root: reference,
            cardinality: RootCardinality::Many,
            bindings: vec![bound::Binding {
                name: "numbers".into(),
                root: body,
                output,
                plan_sensitive: false,
                recursive: false,
                step: None,
                accumulation: None,
            }],
            next_slot: SlotId(2),
        };
        let plan = plan_query(&bound, PlanOptions::default());
        assert!(matches!(
            plan.bindings[0].kind,
            BindingPlanKind::Derived {
                strategy: BindingStrategy::Replay,
                ..
            }
        ));
        let store = Store::memory("exec-replay-binding").await.unwrap();
        let frames = Executor::new(&store, Limits::default())
            .run_frames(&plan)
            .await
            .unwrap();
        let reference = ReferenceExecutor::new(&store, Limits::default())
            .run_frames(&bound)
            .await
            .unwrap();
        assert_eq!(frames, reference);
        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.get(SlotId(1)).cloned().unwrap())
                .collect::<Vec<_>>(),
            vec![
                Datum::scalar(Value::Int64(4)),
                Datum::scalar(Value::Int64(5))
            ]
        );
    }

    #[tokio::test]
    async fn reference_mutation_smoke_recursive_binding_uses_the_frontier_and_reaches_a_fixpoint() {
        let anchor = int_rows("anchor", 0, &[1]);
        let recursive = Relation::recursive_reference(
            "walk",
            "frontier",
            vec![Field {
                name: "n".into(),
                slot: SlotId(1),
                value_type: Type::scalar(Kind::Int64, false),
            }],
            vec![SlotId(0)],
        );
        let filtered = Relation::filter(
            recursive,
            Expr::binary(
                BinaryOp::Lt,
                Expr::slot(SlotId(1), "n", Type::scalar(Kind::Int64, false)),
                Expr::literal(Value::Int64(3)),
            ),
        );
        let step = Relation::project(
            filtered,
            "next",
            vec![ProjectField {
                name: "n".into(),
                slot: SlotId(2),
                expression: Expr::binary(
                    BinaryOp::Add,
                    Expr::slot(SlotId(1), "n", Type::scalar(Kind::Int64, false)),
                    Expr::literal(Value::Int64(1)),
                ),
            }],
        );
        let root = Relation::reference(
            "walk",
            "result",
            vec![Field {
                name: "n".into(),
                slot: SlotId(3),
                value_type: Type::scalar(Kind::Int64, false),
            }],
            vec![SlotId(0)],
        );
        let bound = bound::Query {
            root,
            cardinality: RootCardinality::Many,
            bindings: vec![bound::Binding {
                name: "walk".into(),
                root: anchor.clone(),
                output: anchor.output().clone(),
                plan_sensitive: false,
                recursive: true,
                step: Some(step),
                accumulation: Some(RecursiveAccumulation::New),
            }],
            next_slot: SlotId(4),
        };
        let plan = plan_query(&bound, PlanOptions::default());
        assert!(matches!(
            &plan.bindings[0].kind,
            BindingPlanKind::Recursive {
                accumulation: RecursiveAccumulation::New,
                ..
            }
        ));
        let store = Store::memory("exec-recursive-binding").await.unwrap();
        let proof_limits = Limits {
            max_iterations: 16,
            max_rows: 64,
        };
        let frames = Executor::new(&store, proof_limits)
            .run_frames(&plan)
            .await
            .unwrap();
        let reference = ReferenceExecutor::new(&store, proof_limits)
            .run_frames(&bound)
            .await
            .unwrap();
        assert_eq!(frames, reference);
        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.get(SlotId(3)).cloned().unwrap())
                .collect::<Vec<_>>(),
            [1, 2, 3]
                .into_iter()
                .map(|value| Datum::scalar(Value::Int64(value)))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn reference_mutation_smoke_recursive_limits_are_configurable_in_both_executors() {
        let anchor = int_rows("anchor", 0, &[1]);
        let recursive = Relation::recursive_reference(
            "walk",
            "frontier",
            vec![Field {
                name: "n".into(),
                slot: SlotId(1),
                value_type: Type::scalar(Kind::Int64, false),
            }],
            vec![SlotId(0)],
        );
        let filtered = Relation::filter(
            recursive,
            Expr::binary(
                BinaryOp::Lt,
                Expr::slot(SlotId(1), "n", Type::scalar(Kind::Int64, false)),
                Expr::literal(Value::Int64(3)),
            ),
        );
        let step = Relation::project(
            filtered,
            "next",
            vec![ProjectField {
                name: "n".into(),
                slot: SlotId(2),
                expression: Expr::binary(
                    BinaryOp::Add,
                    Expr::slot(SlotId(1), "n", Type::scalar(Kind::Int64, false)),
                    Expr::literal(Value::Int64(1)),
                ),
            }],
        );
        let root = Relation::reference(
            "walk",
            "result",
            vec![Field {
                name: "n".into(),
                slot: SlotId(3),
                value_type: Type::scalar(Kind::Int64, false),
            }],
            vec![SlotId(0)],
        );
        let bound = bound::Query {
            root,
            cardinality: RootCardinality::Many,
            bindings: vec![bound::Binding {
                name: "walk".into(),
                root: anchor.clone(),
                output: anchor.output().clone(),
                plan_sensitive: false,
                recursive: true,
                step: Some(step),
                accumulation: Some(RecursiveAccumulation::New),
            }],
            next_slot: SlotId(4),
        };
        let plan = plan_query(&bound, PlanOptions::default());
        let store = Store::memory("exec-configurable-recursion-limits")
            .await
            .unwrap();

        let proof_limits = Limits {
            max_iterations: 16,
            max_rows: 64,
        };
        let default_frames = Executor::new(&store, proof_limits)
            .run_frames(&plan)
            .await
            .unwrap();
        assert_eq!(default_frames.len(), 3);

        let exact_limits = Limits {
            max_iterations: 3,
            max_rows: 3,
        };
        let exact_production = Executor::new(&store, exact_limits)
            .run_frames(&plan)
            .await
            .expect("the exact recursion limits are inclusive");
        let exact_reference = ReferenceExecutor::new(&store, exact_limits)
            .run_frames(&bound)
            .await
            .expect("the reference accepts the exact recursion limits");
        assert_eq!(exact_production, exact_reference);
        assert_eq!(exact_reference.len(), 3);

        for limits in [
            Limits {
                max_iterations: 1,
                max_rows: usize::MAX,
            },
            Limits {
                max_iterations: usize::MAX,
                max_rows: 2,
            },
        ] {
            let production_error = Executor::new(&store, limits)
                .run_frames(&plan)
                .await
                .unwrap_err();
            assert_eq!(production_error.reason(), ErrorReason::RecursionLimit);

            let reference_error = ReferenceExecutor::new(&store, limits)
                .run_frames(&bound)
                .await
                .unwrap_err();
            assert_eq!(reference_error.reason(), ErrorReason::RecursionLimit);
        }
    }
}
