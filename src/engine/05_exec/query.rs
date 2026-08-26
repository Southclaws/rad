//! Production physical-query executor.
//!
//! Streaming nodes run through the pull pipeline. Blocking and stateful nodes
//! retain a semantics-first materialised implementation while their pull forms
//! are introduced incrementally. The independent reference interpreter lives
//! outside this executor.

use std::collections::HashMap;
use std::time::Instant;

use async_recursion::async_recursion;
use bytes::Bytes;
use num_bigint::BigInt;
use tracing::Instrument as _;

use crate::engine::catalog::store::admit_catalog_dependencies;
use crate::engine::kv::key_encoding::prefix_end;
use crate::engine::kv::{KeyRange, KvView};
use crate::engine::lir::bound::{self, RelationNode};
use crate::engine::lir::eval::{
    CanonicalRowSet, Env, evaluate, evaluate_datum, evaluate_predicate,
};
use crate::engine::lir::{
    AggregateFunction, Datum, Kind, RecursiveAccumulation, RowType, SetQuantifier, SlotId, TriBool,
    Value,
};
use crate::engine::planner::analysis::EquiJoinKey;
use crate::engine::planner::analysis::{ConstValue, CorrelationKind};
use crate::engine::planner::physical::{
    AttachSpec, BindingPlan, BindingPlanKind, BindingStrategy, CrossingKind, Node, NodeKind, Plan,
};

use super::codec;
use super::frames::{
    frame_scalar, frame_to_object, frames_to_array, merge as merge_frames, new_frame,
    remap_canonical, remap_positional, row_to_frame, shape_frames, sort,
};
use super::row_store;
use super::set;
use super::{Error, ErrorKind, Result};

const EARLY_RECURSIVE_ITERATION_SPANS: usize = 16;

pub(super) const fn operator_name(kind: &NodeKind) -> &'static str {
    match kind {
        NodeKind::PrimaryKeyGet { .. } => "PrimaryKeyGet",
        NodeKind::TableScan { .. } => "TableScan",
        NodeKind::Rows(_) => "Rows",
        NodeKind::IndexRangeScan { .. } => "IndexRangeScan",
        NodeKind::Filter { .. } => "Filter",
        NodeKind::Attach { .. } => "Attach",
        NodeKind::Project { .. } => "Project",
        NodeKind::Sort { .. } => "Sort",
        NodeKind::Slice { .. } => "Slice",
        NodeKind::Reference { .. } => "Reference",
        NodeKind::RecursiveReference { .. } => "RecursiveReference",
        NodeKind::Distinct { .. } => "Distinct",
        NodeKind::NestedLoopJoin { .. } => "NestedLoopJoin",
        NodeKind::HashJoin { .. } => "HashJoin",
        NodeKind::IndexedLookupJoin { .. } => "IndexedLookupJoin",
        NodeKind::JoinGraphChoice { .. } => "JoinGraphChoice",
        NodeKind::ShreddedYannakakisJoin { .. } => "ShreddedYannakakisJoin",
        NodeKind::PredicateTransferJoin { .. } => "PredicateTransferJoin",
        NodeKind::PredicateTransferInput { .. } => "PredicateTransferInput",
        NodeKind::Concatenate { .. } => "Concatenate",
        NodeKind::Intersect { .. } => "Intersect",
        NodeKind::Except { .. } => "Except",
        NodeKind::Aggregate { .. } => "Aggregate",
    }
}

fn duration_micros(duration: std::time::Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

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
    measure: bool,
    measure_operators: bool,
    /// Rows produced by each attributed node, in completion order. Only
    /// nodes the planner could attribute appear, and only where execution
    /// already materializes their output.
    measured: Vec<(crate::engine::lir::fingerprint::Fingerprint, u64)>,
    join_measurements: Vec<super::observe::JoinOperatorMeasurement>,
    operator_measurements: Vec<super::observe::OperatorMeasurement>,
    kv_counters: Option<&'a super::observe::KvCounters>,
    next_operator_id: u32,
    current_operator_id: Option<u32>,
    current_operator_span: Option<tracing::Span>,
    predicate_transfer_inputs: Vec<Vec<Vec<Env>>>,
}

struct SetPlan<'a> {
    left: &'a Node,
    right: &'a Node,
    quantifier: SetQuantifier,
    subtract: bool,
    left_output: &'a RowType,
    right_output: &'a RowType,
    output: &'a RowType,
}

#[derive(Clone, Copy)]
struct RecursiveExecution<'a> {
    binding: &'a BindingPlan,
    anchor: &'a Node,
    step: &'a Node,
    step_output: &'a RowType,
    accumulation: RecursiveAccumulation,
}

#[derive(Default)]
struct StoragePruningMeasurement {
    candidates: u64,
    ranges: u64,
    scans: u64,
    empty_scans: u64,
}

struct StoragePruningDecision {
    pruning: row_store::ScanPruning,
    applied_ranges: u64,
}

#[derive(Clone, Copy, Default)]
struct RecursiveWork {
    rows_examined: u64,
    logical_row_operations: u64,
    storage_reads: u64,
    storage_bytes: u64,
}

#[derive(Default)]
struct RecursiveMeasurement {
    iterations: usize,
    output_rows: u64,
    duplicate_rows: u64,
    peak_frontier_rows: u64,
    peak_retained_bytes: u64,
    anchor_rows: Option<u64>,
    anchor_duration_us: Option<u64>,
    anchor_work: RecursiveWork,
    step_work: RecursiveWork,
}

impl RecursiveWork {
    fn add(&mut self, work: Self) {
        self.rows_examined = self.rows_examined.saturating_add(work.rows_examined);
        self.logical_row_operations = self
            .logical_row_operations
            .saturating_add(work.logical_row_operations);
        self.storage_reads = self.storage_reads.saturating_add(work.storage_reads);
        self.storage_bytes = self.storage_bytes.saturating_add(work.storage_bytes);
    }
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
            measure: false,
            measure_operators: false,
            measured: Vec::new(),
            join_measurements: Vec::new(),
            operator_measurements: Vec::new(),
            kv_counters: None,
            next_operator_id: 0,
            current_operator_id: None,
            current_operator_span: None,
            predicate_transfer_inputs: Vec::new(),
        }
    }

    pub fn enable_measurements(&mut self) {
        self.measure = true;
    }

    pub fn enable_operator_measurements(&mut self) {
        self.measure_operators = true;
    }

    pub fn observe_kv_work(&mut self, counters: &'a super::observe::KvCounters) {
        self.kv_counters = Some(counters);
    }

    fn kv_work(&self) -> super::observe::KvWork {
        self.kv_counters
            .map(super::observe::KvCounters::snapshot)
            .unwrap_or_default()
    }

    fn recursive_work_since(
        &self,
        operator_start: usize,
        kv_before: super::observe::KvWork,
    ) -> RecursiveWork {
        let operators = &self.operator_measurements[operator_start..];
        let kv = self.kv_work().delta_since(kv_before);
        RecursiveWork {
            rows_examined: operators
                .iter()
                .filter(|measurement| {
                    matches!(
                        measurement.operator,
                        "PrimaryKeyGet" | "TableScan" | "IndexRangeScan"
                    )
                })
                .map(|measurement| measurement.output_rows)
                .fold(0, u64::saturating_add),
            logical_row_operations: operators
                .iter()
                .map(|measurement| measurement.output_rows)
                .fold(0, u64::saturating_add),
            storage_reads: kv.gets.saturating_add(kv.iterated),
            storage_bytes: kv.bytes_read,
        }
    }

    /// Row counts charged to the logical relations they implement.
    pub fn measured(&self) -> &[(crate::engine::lir::fingerprint::Fingerprint, u64)] {
        &self.measured
    }

    pub fn join_measurements(&self) -> &[super::observe::JoinOperatorMeasurement] {
        &self.join_measurements
    }

    pub fn operator_measurements(&self) -> Vec<super::observe::OperatorMeasurement> {
        let mut measurements = self.operator_measurements.clone();
        measurements.sort_by_key(|measurement| measurement.operator_id);
        let positions = measurements
            .iter()
            .enumerate()
            .map(|(position, measurement)| (measurement.operator_id, position))
            .collect::<HashMap<_, _>>();
        for position in 0..measurements.len() {
            let Some(parent_id) = measurements[position].parent_operator_id else {
                continue;
            };
            let Some(&parent) = positions.get(&parent_id) else {
                continue;
            };
            let child_rows = measurements[position].output_rows;
            let child_complete = measurements[position].complete;
            let child_open = measurements[position].open_micros;
            let child_duration = measurements[position].inclusive_micros;
            measurements[parent].input_rows =
                measurements[parent].input_rows.saturating_add(child_rows);
            measurements[parent].input_complete &= child_complete;
            measurements[parent].open_micros =
                measurements[parent].open_micros.saturating_sub(child_open);
            measurements[parent].exclusive_micros = measurements[parent]
                .exclusive_micros
                .saturating_sub(child_duration);
        }
        measurements
    }

    /// Test seam proving key-correlated batching and nested evaluation agree.
    pub fn set_force_nested(&mut self, force_nested: bool) {
        self.force_nested = force_nested;
    }

    /// Program execution uses this to expose earlier statement results.
    pub fn seed_bindings(&mut self, bindings: HashMap<String, Vec<Env>>) {
        self.bindings = bindings;
    }

    /// Rows a materialized binding produced. Replayed bindings never
    /// accumulate a full result, so they have no count to report.
    pub fn binding_cardinality(&self, name: &str) -> Option<u64> {
        self.bindings.get(name).map(|frames| frames.len() as u64)
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
            return self.execute_kind(node, outer).await;
        }
        if self.measure_operators {
            let operator_id = self.next_operator_id;
            self.next_operator_id = self.next_operator_id.saturating_add(1);
            let parent_operator_id = self.current_operator_id.replace(operator_id);
            let runtime_span = super::observe::OperatorRuntimeSpan::new(
                operator_id,
                parent_operator_id,
                operator_name(&node.kind),
                node.attribution,
                self.current_operator_span.as_ref(),
            );
            let operator_span = runtime_span.span().clone();
            let parent_operator_span = self.current_operator_span.replace(operator_span.clone());
            let started = Instant::now();
            let result = self
                .execute_kind(node, outer)
                .instrument(operator_span)
                .await;
            self.current_operator_id = parent_operator_id;
            self.current_operator_span = parent_operator_span;
            let output_rows = result.as_ref().map_or(0, |frames| frames.len() as u64);
            let inclusive_micros = duration_micros(started.elapsed());
            let measurement = super::observe::OperatorMeasurement {
                operator_id,
                parent_operator_id,
                operator: operator_name(&node.kind),
                relation_fingerprint: node.attribution,
                open_micros: 0,
                inclusive_micros,
                exclusive_micros: inclusive_micros,
                calls: 1,
                input_rows: 0,
                output_rows,
                input_complete: true,
                complete: result.is_ok(),
            };
            runtime_span.record(&measurement, result.is_err());
            self.operator_measurements.push(measurement);
            let frames = result?;
            if self.measure
                && let Some(attribution) = node.attribution
            {
                self.measured.push((attribution, frames.len() as u64));
            }
            return Ok(frames);
        }
        let frames = self.execute_kind(node, outer).await?;
        if self.measure
            && let Some(attribution) = node.attribution
        {
            self.measured.push((attribution, frames.len() as u64));
        }
        Ok(frames)
    }

    async fn execute_scan_with_pruning(
        &mut self,
        node: &Node,
        outer: &Env,
        pruning: &row_store::ScanPruning,
    ) -> Result<Vec<Env>> {
        let frames = match &node.kind {
            NodeKind::TableScan {
                scan,
                decode_columns,
                ..
            } => {
                let mut iterator = row_store::scan_table_with_pruning(
                    self.view,
                    scan.scan_table(),
                    decode_columns,
                    Some(pruning),
                )
                .await?;
                let mut frames = Vec::new();
                while let Some(row) = iterator.next().await? {
                    frames.push(row_to_frame(scan, &row, outer));
                }
                frames
            }
            NodeKind::IndexRangeScan {
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
                    Vec::new()
                } else {
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
                    let mut iterator = row_store::scan_index_range_with_pruning(
                        self.view,
                        scan.scan_table(),
                        index,
                        &equality_prefix,
                        range,
                        decode_columns,
                        Some(pruning),
                    )
                    .await?;
                    let mut frames = Vec::new();
                    while let Some(row) = iterator.next().await? {
                        frames.push(row_to_frame(scan, &row, outer));
                    }
                    frames
                }
            }
            _ => return self.execute_node(node, outer).await,
        };
        if self.measure
            && let Some(attribution) = node.attribution
        {
            self.measured.push((attribution, frames.len() as u64));
        }
        Ok(frames)
    }

    async fn execute_predicate_transfer_inputs(
        &mut self,
        inputs: &[Node],
        schedule: &crate::engine::planner::physical::PredicateTransferSchedule,
        outer: &Env,
    ) -> Result<(Vec<Vec<Env>>, StoragePruningMeasurement)> {
        let mut loaded = vec![None; inputs.len()];
        let mut ranges = super::predicate_transfer::StorageRangeSchedule::new(inputs.len());
        let mut measurement = StoragePruningMeasurement::default();
        for &input in &schedule.forward.order {
            let Some(node) = inputs.get(input) else {
                return Err(Error::message(
                    ErrorKind::Internal,
                    "exec: invalid predicate transfer input",
                ));
            };
            if loaded[input].is_some() {
                return Err(Error::message(
                    ErrorKind::Internal,
                    "exec: duplicate predicate transfer input",
                ));
            }
            let decision = storage_pruning(node, &schedule.forward, ranges.ranges(input), outer)?;
            measurement.candidates = measurement
                .candidates
                .saturating_add(ranges.ranges(input).len() as u64);
            let rows = if let Some(decision) = decision {
                measurement.ranges = measurement.ranges.saturating_add(decision.applied_ranges);
                measurement.scans = measurement.scans.saturating_add(1);
                measurement.empty_scans = measurement.empty_scans.saturating_add(u64::from(
                    matches!(decision.pruning, row_store::ScanPruning::Empty),
                ));
                self.execute_scan_with_pruning(node, outer, &decision.pruning)
                    .await?
            } else {
                self.execute_node(node, outer).await?
            };
            ranges.observe(input, &rows, &schedule.forward)?;
            loaded[input] = Some(rows);
        }
        for (input, node) in inputs.iter().enumerate() {
            if loaded[input].is_none() {
                loaded[input] = Some(self.execute_node(node, outer).await?);
            }
        }
        Ok((
            loaded
                .into_iter()
                .map(|rows| rows.expect("predicate transfer input loaded"))
                .collect(),
            measurement,
        ))
    }

    #[async_recursion]
    async fn execute_kind(&mut self, node: &Node, outer: &Env) -> Result<Vec<Env>> {
        if super::pipeline::supports(node) {
            return if self.measure || self.measure_operators {
                super::pipeline::execute_measured(
                    self.view,
                    node,
                    outer,
                    &mut self.measured,
                    &mut self.join_measurements,
                    &mut self.operator_measurements,
                    &mut self.next_operator_id,
                    self.current_operator_id,
                    self.measure_operators,
                )
                .await
            } else {
                super::pipeline::execute(self.view, node, outer, &mut self.join_measurements).await
            };
        }
        match &node.kind {
            NodeKind::PrimaryKeyGet {
                scan,
                key,
                decode_columns,
                ..
            } => {
                let table = scan.scan_table();
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
            NodeKind::TableScan {
                scan,
                decode_columns,
                ..
            } => Ok(
                row_store::scan_table_columns(self.view, scan.scan_table(), decode_columns)
                    .await?
                    .iter()
                    .map(|row| row_to_frame(scan, row, outer))
                    .collect(),
            ),
            NodeKind::IndexRangeScan {
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
                    scan.scan_table(),
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
            NodeKind::Rows(relation) => {
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
            NodeKind::Filter { input, predicate } => Ok(self
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
            NodeKind::Attach {
                input,
                specifications,
            } => {
                let mut frames = self.execute_node(input, outer).await?;
                for specification in specifications {
                    self.attach(specification, &mut frames, outer).await?;
                }
                Ok(frames)
            }
            NodeKind::Project { input, fields } => self
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
            NodeKind::Sort { input, terms } => {
                let mut frames = self.execute_node(input, outer).await?;
                sort(&mut frames, terms)?;
                Ok(frames)
            }
            NodeKind::Slice {
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
            NodeKind::NestedLoopJoin {
                left,
                right,
                kind,
                on,
                right_output,
                ..
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
            NodeKind::HashJoin { .. } => {
                unreachable!("hash joins run in the pull pipeline")
            }
            NodeKind::IndexedLookupJoin {
                left,
                right,
                kind,
                on,
                keys,
                right_output,
                ..
            } => {
                let left = self.execute_node(left, outer).await?;
                let mut output = Vec::new();
                let mut measurement = super::observe::JoinOperatorMeasurement {
                    operator: "IndexedLookupJoin",
                    ..Default::default()
                };
                for left in left {
                    measurement.probe_rows = measurement.probe_rows.saturating_add(1);
                    let Some(_) = super::pipeline::join_key(&left, keys, true)? else {
                        if *kind == crate::engine::lir::JoinKind::Left {
                            output.push(pad_join_left(left, right_output));
                        }
                        continue;
                    };
                    measurement.lookup_requests = measurement.lookup_requests.saturating_add(1);
                    let right = self.execute_node(right, &left).await?;
                    measurement.peak_retained_bytes = measurement.peak_retained_bytes.max(
                        right
                            .iter()
                            .map(super::pipeline::frame_retained_bytes)
                            .fold(0u64, u64::saturating_add),
                    );
                    let mut matched = false;
                    for right in right {
                        let merged = merge_frames(&left, &right);
                        if evaluate_indexed_join_predicate(on, keys, &merged, &mut measurement)?
                            == TriBool::True
                        {
                            matched = true;
                            output.push(merged);
                        }
                    }
                    if *kind == crate::engine::lir::JoinKind::Left && !matched {
                        output.push(pad_join_left(left, right_output));
                    }
                }
                if self.measure {
                    self.join_measurements.push(measurement);
                }
                Ok(output)
            }
            NodeKind::JoinGraphChoice { input, .. } => self.execute_node(input, outer).await,
            NodeKind::ShreddedYannakakisJoin {
                inputs,
                edges,
                root_input,
                memory_limit_bytes,
                ..
            } => {
                let mut input_rows = Vec::with_capacity(inputs.len());
                for input in inputs {
                    input_rows.push(self.execute_node(input, outer).await?);
                }
                let (output, measurement) = super::shredded_join::execute(
                    input_rows,
                    edges,
                    *root_input,
                    *memory_limit_bytes,
                )?;
                if self.measure {
                    self.join_measurements.push(measurement);
                }
                Ok(output)
            }
            NodeKind::PredicateTransferJoin {
                inputs,
                schedule,
                join_plan,
                bits_per_key,
                hash_functions,
                runtime_policy,
                memory_limit_bytes,
                ..
            } => {
                let (input_rows, storage) = self
                    .execute_predicate_transfer_inputs(inputs, schedule, outer)
                    .await?;
                let (input_rows, mut measurement) = super::predicate_transfer::execute(
                    input_rows,
                    schedule,
                    *bits_per_key,
                    *hash_functions,
                    runtime_policy,
                    *memory_limit_bytes,
                    self.measure,
                )?;
                measurement.filter_storage_range_candidates = storage.candidates;
                measurement.filter_storage_ranges_applied = storage.ranges;
                measurement.filter_storage_scans_pruned = storage.scans;
                measurement.filter_storage_empty_scans = storage.empty_scans;
                if self.measure {
                    self.join_measurements.push(measurement);
                }
                self.predicate_transfer_inputs.push(input_rows);
                let result = self.execute_node(join_plan, outer).await;
                self.predicate_transfer_inputs.pop();
                result
            }
            NodeKind::PredicateTransferInput { input, .. } => self
                .predicate_transfer_inputs
                .last()
                .and_then(|inputs| inputs.get(*input))
                .cloned()
                .ok_or_else(|| {
                    Error::message(
                        ErrorKind::Internal,
                        "exec: missing predicate transfer input",
                    )
                }),
            NodeKind::Concatenate {
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
            NodeKind::Intersect {
                left,
                right,
                quantifier,
                left_output,
                right_output,
                output,
            } => {
                self.execute_set_operation(
                    SetPlan {
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
            NodeKind::Except {
                left,
                right,
                quantifier,
                left_output,
                right_output,
                output,
            } => {
                self.execute_set_operation(
                    SetPlan {
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
            NodeKind::Distinct { input, output } => {
                let mut seen = CanonicalRowSet::new(output.fields.clone());
                Ok(self
                    .execute_node(input, outer)
                    .await?
                    .into_iter()
                    .filter(|frame| seen.insert(frame))
                    .collect())
            }
            NodeKind::Aggregate {
                input,
                groups,
                terms,
            } => self.execute_aggregate(input, groups, terms, outer).await,
            NodeKind::Reference {
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
            NodeKind::RecursiveReference {
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
        operation: SetPlan<'_>,
        outer: &Env,
    ) -> Result<Vec<Env>> {
        let left = self.execute_node(operation.left, outer).await?;
        let right = self.execute_node(operation.right, outer).await?;
        let mut state = set::State::new(operation.quantifier, operation.subtract);
        for frame in &right {
            state.add_right(operation.right_output, frame);
        }
        let mut frames = Vec::new();
        for frame in left {
            if state.keep_left(operation.left_output, &frame) {
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
        let span = tracing::debug_span!(
            target: "rad::telemetry",
            "rad.recursive.execute",
            otel.name = "rad.recursive.execute",
            otel.kind = "internal",
            rad.recursive.binding = %binding.name,
            rad.recursive.accumulation = ?accumulation,
            rad.recursive.iterations = tracing::field::Empty,
            rad.recursive.output_rows = tracing::field::Empty,
            rad.recursive.duplicate_rows = tracing::field::Empty,
            rad.recursive.peak_frontier_rows = tracing::field::Empty,
            rad.recursive.peak_retained_bytes = tracing::field::Empty,
            rad.recursive.anchor_rows = tracing::field::Empty,
            rad.recursive.anchor_duration_us = tracing::field::Empty,
            rad.recursive.anchor_rows_examined = tracing::field::Empty,
            rad.recursive.anchor_logical_row_operations = tracing::field::Empty,
            rad.recursive.anchor_storage_reads = tracing::field::Empty,
            rad.recursive.anchor_storage_bytes = tracing::field::Empty,
            rad.recursive.step_rows_examined = tracing::field::Empty,
            rad.recursive.step_logical_row_operations = tracing::field::Empty,
            rad.recursive.step_storage_reads = tracing::field::Empty,
            rad.recursive.step_storage_bytes = tracing::field::Empty,
            rad.status = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
        );
        let execution = RecursiveExecution {
            binding,
            anchor,
            step,
            step_output,
            accumulation,
        };
        let mut measurement = RecursiveMeasurement::default();
        let result = self
            .commit_recursive_inner(execution, &span, &mut measurement)
            .instrument(span.clone())
            .await;
        self.frontier.remove(&binding.name);
        record_recursive_measurement(&span, &measurement);
        span.record(
            "rad.status",
            if result.is_ok() { "success" } else { "error" },
        );
        if result.is_err() {
            span.record("otel.status_code", "ERROR");
        }
        result
    }

    async fn commit_recursive_inner(
        &mut self,
        execution: RecursiveExecution<'_>,
        recursive_span: &tracing::Span,
        measurement: &mut RecursiveMeasurement,
    ) -> Result<Vec<Env>> {
        let RecursiveExecution {
            binding,
            anchor,
            step,
            step_output,
            accumulation,
        } = execution;
        let anchor_slots = slots_by_name(&binding.output);
        let step_slots = slots_by_name(step_output);
        let canonical = &binding.output;
        let mut seen = (accumulation == RecursiveAccumulation::New)
            .then(|| CanonicalRowSet::new(canonical.fields.clone()));
        let diagnostics = !recursive_span.is_disabled();
        let mut result = Vec::new();
        let mut frontier = Vec::new();
        let anchor_span = tracing::debug_span!(
            target: "rad::telemetry",
            parent: recursive_span,
            "rad.recursive.anchor",
            otel.name = "rad.recursive.anchor",
            otel.kind = "internal",
            rad.recursive.output_rows = tracing::field::Empty,
            rad.recursive.rows_examined = tracing::field::Empty,
            rad.recursive.logical_row_operations = tracing::field::Empty,
            rad.recursive.storage_reads = tracing::field::Empty,
            rad.recursive.storage_bytes = tracing::field::Empty,
            rad.status = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
        );
        let anchor_operator_start = self.operator_measurements.len();
        let anchor_kv_before = if diagnostics {
            self.kv_work()
        } else {
            super::observe::KvWork::default()
        };
        let anchor_started = diagnostics.then(Instant::now);
        let anchor_frames = self
            .execute_node(anchor, &Env::new())
            .instrument(anchor_span.clone())
            .await;
        let anchor_duration = anchor_started.map(|started| started.elapsed());
        let anchor_work = if diagnostics {
            self.recursive_work_since(anchor_operator_start, anchor_kv_before)
        } else {
            RecursiveWork::default()
        };
        record_recursive_work(&anchor_span, anchor_work);
        if anchor_frames.is_err() {
            anchor_span.record("rad.status", "error");
            anchor_span.record("otel.status_code", "ERROR");
        }
        if let Some(duration) = anchor_duration {
            measurement.anchor_duration_us = Some(duration_micros(duration));
        }
        measurement.anchor_work = anchor_work;
        let anchor_frames = anchor_frames?;
        anchor_span.record("rad.recursive.output_rows", anchor_frames.len() as u64);
        anchor_span.record("rad.status", "success");
        measurement.anchor_rows = Some(anchor_frames.len() as u64);
        let mut duplicate_rows = 0_u64;
        let mut result_retained_bytes = 0_u64;
        let mut frontier_retained_bytes = 0_u64;
        let mut seen_retained_bytes = 0_u64;
        for frame in anchor_frames {
            let frame = project_canonical(canonical, &anchor_slots, &frame);
            if seen.as_mut().is_none_or(|seen| seen.insert(&frame)) {
                let retained_bytes = if diagnostics {
                    super::pipeline::frame_retained_bytes(&frame)
                } else {
                    0
                };
                result_retained_bytes = result_retained_bytes.saturating_add(retained_bytes);
                frontier_retained_bytes = frontier_retained_bytes.saturating_add(retained_bytes);
                if seen.is_some() {
                    seen_retained_bytes = seen_retained_bytes.saturating_add(retained_bytes);
                }
                result.push(frame.clone());
                frontier.push(frame);
            } else {
                duplicate_rows = duplicate_rows.saturating_add(1);
            }
        }
        let mut iteration = 0;
        let mut peak_frontier_rows = frontier.len() as u64;
        let mut peak_retained_bytes = result_retained_bytes
            .saturating_add(frontier_retained_bytes)
            .saturating_add(seen_retained_bytes);
        let mut step_work = RecursiveWork::default();
        measurement.output_rows = result.len() as u64;
        measurement.duplicate_rows = duplicate_rows;
        measurement.peak_frontier_rows = peak_frontier_rows;
        measurement.peak_retained_bytes = peak_retained_bytes;
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
            let input_rows = frontier.len() as u64;
            measurement.iterations = iteration;
            self.frontier.insert(binding.name.clone(), frontier);
            let iteration_span = if trace_recursive_iteration(iteration) {
                tracing::debug_span!(
                    target: "rad::telemetry",
                    parent: recursive_span,
                    "rad.recursive.iteration",
                    otel.name = "rad.recursive.iteration",
                    otel.kind = "internal",
                    rad.recursive.iteration = iteration,
                    rad.recursive.input_rows = input_rows,
                    rad.recursive.produced_rows = tracing::field::Empty,
                    rad.recursive.accepted_rows = tracing::field::Empty,
                    rad.recursive.duplicate_rows = tracing::field::Empty,
                    rad.recursive.accumulated_rows = tracing::field::Empty,
                    rad.recursive.rows_examined = tracing::field::Empty,
                    rad.recursive.logical_row_operations = tracing::field::Empty,
                    rad.recursive.storage_reads = tracing::field::Empty,
                    rad.recursive.storage_bytes = tracing::field::Empty,
                    rad.status = tracing::field::Empty,
                    otel.status_code = tracing::field::Empty,
                )
            } else {
                tracing::Span::none()
            };
            let operator_start = self.operator_measurements.len();
            let kv_before = if diagnostics {
                self.kv_work()
            } else {
                super::observe::KvWork::default()
            };
            let produced = self
                .execute_node(step, &Env::new())
                .instrument(iteration_span.clone())
                .await;
            let iteration_work = if diagnostics {
                self.recursive_work_since(operator_start, kv_before)
            } else {
                RecursiveWork::default()
            };
            step_work.add(iteration_work);
            record_recursive_work(&iteration_span, iteration_work);
            measurement.step_work = step_work;
            if produced.is_err() {
                iteration_span.record("rad.recursive.accumulated_rows", result.len() as u64);
                iteration_span.record("rad.status", "error");
                iteration_span.record("otel.status_code", "ERROR");
            }
            let produced = produced?;
            let produced_rows = produced.len() as u64;
            let mut next = Vec::new();
            let input_frontier_retained_bytes = frontier_retained_bytes;
            let mut next_retained_bytes = 0_u64;
            for frame in produced {
                let frame = project_canonical(canonical, &step_slots, &frame);
                if seen.as_mut().is_none_or(|seen| seen.insert(&frame)) {
                    let retained_bytes = if diagnostics {
                        super::pipeline::frame_retained_bytes(&frame)
                    } else {
                        0
                    };
                    next_retained_bytes = next_retained_bytes.saturating_add(retained_bytes);
                    if seen.is_some() {
                        seen_retained_bytes = seen_retained_bytes.saturating_add(retained_bytes);
                    }
                    next.push(frame);
                }
            }
            let accepted_rows = next.len() as u64;
            let iteration_duplicates = produced_rows.saturating_sub(accepted_rows);
            duplicate_rows = duplicate_rows.saturating_add(iteration_duplicates);
            peak_frontier_rows = peak_frontier_rows.max(accepted_rows);
            iteration_span.record("rad.recursive.produced_rows", produced_rows);
            iteration_span.record("rad.recursive.accepted_rows", accepted_rows);
            iteration_span.record("rad.recursive.duplicate_rows", iteration_duplicates);
            result.extend(next.iter().cloned());
            result_retained_bytes = result_retained_bytes.saturating_add(next_retained_bytes);
            peak_retained_bytes = peak_retained_bytes.max(
                result_retained_bytes
                    .saturating_add(input_frontier_retained_bytes)
                    .saturating_add(next_retained_bytes)
                    .saturating_add(seen_retained_bytes),
            );
            frontier_retained_bytes = next_retained_bytes;
            iteration_span.record("rad.recursive.accumulated_rows", result.len() as u64);
            iteration_span.record("rad.status", "success");
            measurement.output_rows = result.len() as u64;
            measurement.duplicate_rows = duplicate_rows;
            measurement.peak_frontier_rows = peak_frontier_rows;
            measurement.peak_retained_bytes = peak_retained_bytes;
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
        Ok(result)
    }
}

fn record_recursive_measurement(span: &tracing::Span, measurement: &RecursiveMeasurement) {
    span.record("rad.recursive.iterations", measurement.iterations);
    span.record("rad.recursive.output_rows", measurement.output_rows);
    span.record("rad.recursive.duplicate_rows", measurement.duplicate_rows);
    span.record(
        "rad.recursive.peak_frontier_rows",
        measurement.peak_frontier_rows,
    );
    span.record(
        "rad.recursive.peak_retained_bytes",
        measurement.peak_retained_bytes,
    );
    if let Some(anchor_rows) = measurement.anchor_rows {
        span.record("rad.recursive.anchor_rows", anchor_rows);
    }
    if let Some(anchor_duration_us) = measurement.anchor_duration_us {
        span.record("rad.recursive.anchor_duration_us", anchor_duration_us);
    }
    record_recursive_anchor_work(span, measurement.anchor_work);
    record_recursive_step_work(span, measurement.step_work);
}

fn record_recursive_work(span: &tracing::Span, work: RecursiveWork) {
    span.record("rad.recursive.rows_examined", work.rows_examined);
    span.record(
        "rad.recursive.logical_row_operations",
        work.logical_row_operations,
    );
    span.record("rad.recursive.storage_reads", work.storage_reads);
    span.record("rad.recursive.storage_bytes", work.storage_bytes);
}

fn record_recursive_anchor_work(span: &tracing::Span, work: RecursiveWork) {
    span.record("rad.recursive.anchor_rows_examined", work.rows_examined);
    span.record(
        "rad.recursive.anchor_logical_row_operations",
        work.logical_row_operations,
    );
    span.record("rad.recursive.anchor_storage_reads", work.storage_reads);
    span.record("rad.recursive.anchor_storage_bytes", work.storage_bytes);
}

fn record_recursive_step_work(span: &tracing::Span, work: RecursiveWork) {
    span.record("rad.recursive.step_rows_examined", work.rows_examined);
    span.record(
        "rad.recursive.step_logical_row_operations",
        work.logical_row_operations,
    );
    span.record("rad.recursive.step_storage_reads", work.storage_reads);
    span.record("rad.recursive.step_storage_bytes", work.storage_bytes);
}

fn trace_recursive_iteration(iteration: usize) -> bool {
    iteration <= EARLY_RECURSIVE_ITERATION_SPANS || iteration.is_power_of_two()
}

fn storage_pruning(
    node: &Node,
    pass: &crate::engine::planner::physical::PredicateTransferPass,
    ranges: &[super::predicate_transfer::StorageRange],
    outer: &Env,
) -> Result<Option<StoragePruningDecision>> {
    if ranges.is_empty() {
        return Ok(None);
    }
    let (scan, storage_columns, prefix) = match &node.kind {
        NodeKind::TableScan { scan, .. } => (
            &**scan,
            scan.scan_table().primary_key.clone(),
            codec::data_prefix(scan.scan_table())?,
        ),
        NodeKind::IndexRangeScan {
            scan,
            index,
            equality_prefix,
            ..
        } => {
            let equality_prefix = equality_prefix
                .iter()
                .map(|constant| resolve_constant(constant, outer))
                .collect::<Result<Vec<_>>>()?;
            if equality_prefix.iter().any(Value::is_null) {
                return Ok(None);
            }
            let columns = scan
                .scan_table()
                .index_column_names(index)
                .into_iter()
                .skip(equality_prefix.len())
                .map(str::to_owned)
                .collect();
            let mut prefix = codec::index_prefix(scan.scan_table(), &index.id)?;
            prefix.extend_from_slice(&codec::encode_tuple(&equality_prefix)?);
            (&**scan, columns, prefix)
        }
        _ => return Ok(None),
    };
    let mut selected: Option<KeyRange> = None;
    let mut empty = false;
    let mut applied_ranges = 0u64;
    for range in ranges {
        let Some(edge) = pass.edges.get(range.edge_index()) else {
            return Err(Error::message(
                ErrorKind::Internal,
                "exec: invalid predicate transfer storage range",
            ));
        };
        let Some(target_columns) = edge_target_columns(scan, edge) else {
            continue;
        };
        if !storage_columns.starts_with(&target_columns) {
            continue;
        }
        applied_ranges = applied_ranges.saturating_add(1);
        let super::predicate_transfer::StorageRange::Bounded {
            minimum, maximum, ..
        } = range
        else {
            empty = true;
            break;
        };
        let mut start = prefix.clone();
        start.extend_from_slice(minimum);
        let mut end = prefix.clone();
        end.extend_from_slice(maximum);
        let range = KeyRange {
            start: Some(Bytes::from(start)),
            end: prefix_end(&end).map(Bytes::from),
        };
        selected = match selected.take() {
            Some(selected) => match intersect_key_ranges(selected, range) {
                Some(range) => Some(range),
                None => {
                    empty = true;
                    break;
                }
            },
            None => Some(range),
        };
    }
    if applied_ranges == 0 {
        return Ok(None);
    }
    Ok(Some(StoragePruningDecision {
        pruning: if empty {
            row_store::ScanPruning::Empty
        } else {
            row_store::ScanPruning::Range(selected.expect("an applied bounded range"))
        },
        applied_ranges,
    }))
}

fn edge_target_columns(
    scan: &bound::Relation,
    edge: &crate::engine::planner::physical::PredicateTransferEdge,
) -> Option<Vec<String>> {
    edge.keys
        .iter()
        .map(|key| {
            scan.output()
                .fields
                .iter()
                .find(|field| field.slot == key.right.slot)
                .map(|field| field.name.clone())
        })
        .collect()
}

fn intersect_key_ranges(first: KeyRange, second: KeyRange) -> Option<KeyRange> {
    let start = match (first.start, second.start) {
        (Some(first), Some(second)) => Some(first.max(second)),
        (first, second) => first.or(second),
    };
    let end = match (first.end, second.end) {
        (Some(first), Some(second)) => Some(first.min(second)),
        (Some(end), None) | (None, Some(end)) => Some(end),
        (None, None) => None,
    };
    if start
        .as_ref()
        .zip(end.as_ref())
        .is_some_and(|(start, end)| start >= end)
    {
        None
    } else {
        Some(KeyRange { start, end })
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
            AggregateFunction::Sum | AggregateFunction::Average if self.values == 0 => null(),
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

fn evaluate_indexed_join_predicate(
    predicate: &bound::Expr,
    keys: &[EquiJoinKey],
    frame: &Env,
    measurement: &mut super::observe::JoinOperatorMeasurement,
) -> Result<TriBool> {
    if let bound::Expr::Binary {
        op: crate::engine::lir::BinaryOp::And,
        left,
        right,
        ..
    } = predicate
    {
        let left = evaluate_indexed_join_predicate(left, keys, frame, measurement)?;
        if left == TriBool::False {
            return Ok(TriBool::False);
        }
        return Ok(left.and(evaluate_indexed_join_predicate(
            right,
            keys,
            frame,
            measurement,
        )?));
    }
    if super::pipeline::is_join_key_comparison(predicate, keys) {
        measurement.key_comparisons = measurement.key_comparisons.saturating_add(1);
    } else {
        measurement.residual_predicate_evaluations =
            measurement.residual_predicate_evaluations.saturating_add(1);
    }
    Ok(evaluate_predicate(predicate, frame)?)
}

fn pad_join_left(mut left: Env, right_output: &RowType) -> Env {
    for field in &right_output.fields {
        left.insert(field.slot, Datum::Null);
    }
    left
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

fn scan_column_type(node: &Node, name: &str) -> crate::engine::catalog::model::ScalarType {
    match &node.kind {
        NodeKind::PrimaryKeyGet { scan, .. }
        | NodeKind::TableScan { scan, .. }
        | NodeKind::IndexRangeScan { scan, .. } => scan
            .scan_table()
            .column(name)
            .map(|column| column.scalar_type)
            .unwrap_or(crate::engine::catalog::model::ScalarType::Text),
        NodeKind::Filter { input, .. }
        | NodeKind::Project { input, .. }
        | NodeKind::Sort { input, .. }
        | NodeKind::Slice { input, .. }
        | NodeKind::Distinct { input, .. }
        | NodeKind::Aggregate { input, .. }
        | NodeKind::Attach { input, .. } => scan_column_type(input, name),
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
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use bytes::Bytes;
    use tracing::field::{Field as TracingField, Visit};
    use tracing::{Id, Subscriber};
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::Context;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::registry::LookupSpan;

    use crate::engine::catalog::identity::{
        AccessGeneration, DefinitionGeneration, ExistenceGeneration, LogicalIndexId, SchemaId,
        ValueGeneration, WriteProtocolGeneration,
    };
    use crate::engine::catalog::model::{Column, Index, IndexState, ScalarType, Table};
    use crate::engine::exec::{ErrorReason, ReferenceExecutor};
    use crate::engine::kv::slatedb::Store;
    use crate::engine::kv::{Entry, KeyRange, Kv, KvIterator, KvView, TransactionalKv};
    use crate::engine::lir::bound::{BoundOrderTerm, Expr, ProjectField, Relation};
    use crate::engine::lir::{BinaryOp, Field, RootCardinality, Type};
    use crate::engine::planner::models::PlannerStats;
    use crate::engine::planner::physical::{
        AccessQuantity, PredicateTransferPass, PredicateTransferSchedule,
    };
    use crate::engine::planner::{
        PlanOptions, PlannerMode, PlanningContext, plan_query, plan_query_with_context,
    };

    use super::*;

    #[derive(Clone, Default)]
    struct RecursiveSummaryCapture(Arc<Mutex<HashMap<&'static str, usize>>>);

    impl<S> Layer<S> for RecursiveSummaryCapture
    where
        S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    {
        fn on_record(
            &self,
            span: &Id,
            values: &tracing::span::Record<'_>,
            context: Context<'_, S>,
        ) {
            if context
                .metadata(span)
                .is_none_or(|metadata| metadata.name() != "rad.recursive.execute")
            {
                return;
            }
            values.record(&mut RecursiveSummaryVisitor(&self.0));
        }
    }

    struct RecursiveSummaryVisitor<'a>(&'a Mutex<HashMap<&'static str, usize>>);

    impl Visit for RecursiveSummaryVisitor<'_> {
        fn record_debug(&mut self, field: &TracingField, _value: &dyn std::fmt::Debug) {
            if field.name().starts_with("rad.recursive.") {
                let mut counts = self.0.lock().expect("recursive capture lock");
                *counts.entry(field.name()).or_default() += 1;
            }
        }
    }

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
            storage_generation: crate::engine::catalog::identity::StorageGeneration::INITIAL,
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
                Bytes::from(codec::data_key(table, &primary_key).unwrap()),
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
                Bytes::from(
                    codec::index_key(table, &table.indexes[0].id, &indexed, &primary_key).unwrap(),
                ),
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

    struct RecordingView {
        store: Store,
        ranges: Mutex<Vec<KeyRange>>,
        scan_entries: Arc<AtomicUsize>,
        seek_targets: Arc<Mutex<Vec<Bytes>>>,
    }

    #[async_trait]
    impl KvView for RecordingView {
        async fn get(&self, key: &[u8]) -> crate::engine::kv::Result<Option<Bytes>> {
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
            self.ranges
                .lock()
                .expect("recorded range lock poisoned")
                .push(range.clone());
            Ok(Box::new(RecordingIterator {
                inner: Kv::scan(&self.store, range).await?,
                entries: Arc::clone(&self.scan_entries),
                seek_targets: Arc::clone(&self.seek_targets),
            }))
        }
    }

    struct RecordingIterator {
        inner: Box<dyn KvIterator>,
        entries: Arc<AtomicUsize>,
        seek_targets: Arc<Mutex<Vec<Bytes>>>,
    }

    #[async_trait]
    impl KvIterator for RecordingIterator {
        async fn seek_forward(&mut self, next_key: &[u8]) -> crate::engine::kv::Result<()> {
            self.inner.seek_forward(next_key).await?;
            self.seek_targets
                .lock()
                .expect("recorded seek target lock poisoned")
                .push(Bytes::copy_from_slice(next_key));
            Ok(())
        }

        async fn next(&mut self) -> crate::engine::kv::Result<Option<Entry>> {
            let entry = self.inner.next().await?;
            if entry.is_some() {
                self.entries.fetch_add(1, AtomicOrdering::Relaxed);
            }
            Ok(entry)
        }
    }

    #[async_trait]
    impl KvIterator for CountingIterator {
        async fn seek_forward(&mut self, next_key: &[u8]) -> crate::engine::kv::Result<()> {
            self.inner.seek_forward(next_key).await
        }

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
                ..PlanOptions::default()
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
    async fn memo_distinct_extraction_matches_structural_and_reference_execution() {
        let table = table();
        let store = Store::memory("exec-memo-distinct").await.unwrap();
        seed(&store, &table).await;
        let scan = scan(&table, [0, 1, 2], "t");
        let distinct = Relation::distinct(Relation::distinct(Relation::distinct(scan.clone())));
        let ordered = Relation::order(
            distinct,
            vec![BoundOrderTerm {
                expression: expression(&scan, "id"),
                descending: false,
            }],
        );
        let bound = query(ordered, 3);
        let structural = plan_query(&bound, PlanOptions::default());
        let cost = plan_query(
            &bound,
            PlanOptions {
                mode: crate::engine::planner::PlannerMode::Cost,
                ..PlanOptions::default()
            },
        );
        let structural = Executor::new(&store, Limits::default())
            .execute(&structural)
            .await
            .unwrap();
        let cost = Executor::new(&store, Limits::default())
            .execute(&cost)
            .await
            .unwrap();
        let reference = ReferenceExecutor::new(&store, Limits::default())
            .execute(&bound)
            .await
            .unwrap();
        assert_eq!(cost, structural);
        assert_eq!(cost, reference);
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn slice_stops_index_iteration_and_base_row_fetches_at_the_limit() {
        let table = table();
        let store = Store::memory("exec-pull-slice").await.unwrap();
        seed(&store, &table).await;
        let view = CountingView {
            store,
            data_prefix: codec::data_prefix(&table).unwrap(),
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
        assert!(matches!(plan.root.kind, NodeKind::Slice { .. }));

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

    fn nullable_int_rows(scope: &str, slot: usize, values: &[Option<i64>]) -> Relation {
        Relation::rows(
            scope,
            vec![Field {
                name: "n".into(),
                slot: SlotId(slot),
                value_type: Type::scalar(Kind::Int64, true),
            }],
            values
                .iter()
                .map(|value| vec![value.map_or(Value::Null(ScalarType::Int64), Value::Int64)])
                .collect(),
        )
    }

    #[tokio::test]
    async fn shredded_yannakakis_matches_binary_and_reference_execution() {
        let input = |scope: &str, slot: usize| {
            Relation::rows(
                scope,
                vec![Field {
                    name: "key".into(),
                    slot: SlotId(slot),
                    value_type: Type::scalar(Kind::Int64, false),
                }],
                (0..16).map(|_| vec![Value::Int64(1)]).collect(),
            )
        };
        let equality = |left: usize, right: usize| {
            Expr::binary(
                BinaryOp::Eq,
                Expr::slot(SlotId(left), "key", Type::scalar(Kind::Int64, false)),
                Expr::slot(SlotId(right), "key", Type::scalar(Kind::Int64, false)),
            )
        };
        let first_second = Relation::join(
            input("first", 0),
            input("second", 1),
            crate::engine::lir::JoinKind::Inner,
            equality(0, 1),
        );
        let joined = Relation::join(
            first_second,
            input("third", 2),
            crate::engine::lir::JoinKind::Inner,
            equality(1, 2),
        );
        let aggregate = Relation::aggregate(
            joined,
            Vec::new(),
            vec![bound::BoundAggregateTerm {
                function: AggregateFunction::Count,
                argument: None,
                name: "count".into(),
                slot: SlotId(3),
                value_type: Type::scalar(Kind::Int64, false),
            }],
        );
        let bound = query(aggregate, 4);
        let statistics = PlannerStats::empty();
        let structural_plan = plan_query_with_context(
            &bound,
            PlanOptions::default(),
            PlanningContext {
                statistics: Some(&statistics),
            },
        )
        .plan;
        let cost_plan = plan_query_with_context(
            &bound,
            PlanOptions {
                mode: PlannerMode::Cost,
                ..PlanOptions::default()
            },
            PlanningContext {
                statistics: Some(&statistics),
            },
        )
        .plan;
        let mut selected = false;
        cost_plan.walk(&mut |node| {
            selected |= matches!(node.kind, NodeKind::ShreddedYannakakisJoin { .. });
        });
        assert!(selected);

        let store = Store::memory("exec-shredded-yannakakis").await.unwrap();
        let structural = Executor::new(&store, Limits::default())
            .execute(&structural_plan)
            .await
            .unwrap();
        let mut cost_executor = Executor::new(&store, Limits::default());
        cost_executor.enable_measurements();
        let cost = cost_executor.execute(&cost_plan).await.unwrap();
        let reference = ReferenceExecutor::new(&store, Limits::default())
            .execute(&bound)
            .await
            .unwrap();
        assert_eq!(cost, structural);
        assert_eq!(cost, reference);
        let measurement = cost_executor.join_measurements().last().unwrap();
        assert_eq!(measurement.operator, "ShreddedYannakakisJoin");
        assert_eq!(measurement.reduction_passes, 4);
        assert_eq!(measurement.expanded_rows, 4_096);
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn predicate_transfer_matches_binary_and_reference_execution() {
        let input = |scope: &str, slots: [usize; 2], values: [&str; 2]| {
            Relation::rows(
                scope,
                vec![
                    Field {
                        name: "first_key".into(),
                        slot: SlotId(slots[0]),
                        value_type: Type::scalar(Kind::Text, false),
                    },
                    Field {
                        name: "second_key".into(),
                        slot: SlotId(slots[1]),
                        value_type: Type::scalar(Kind::Text, false),
                    },
                ],
                (0..30)
                    .map(|_| vec![Value::Text(values[0].into()), Value::Text(values[1].into())])
                    .collect(),
            )
        };
        let equality = |left: usize, right: usize| {
            Expr::binary(
                BinaryOp::Eq,
                Expr::slot(SlotId(left), "join_key", Type::scalar(Kind::Text, false)),
                Expr::slot(SlotId(right), "join_key", Type::scalar(Kind::Text, false)),
            )
        };
        let first = input("first", [0, 1], ["a", "b"]);
        let second = input("second", [2, 3], ["b", "c"]);
        let third = input("third", [4, 5], ["c", "different"]);
        let first_second = Relation::join(
            first,
            second,
            crate::engine::lir::JoinKind::Inner,
            equality(1, 2),
        );
        let joined = Relation::join(
            first_second,
            third,
            crate::engine::lir::JoinKind::Inner,
            Expr::binary(BinaryOp::And, equality(3, 4), equality(0, 5)),
        );
        let aggregate = Relation::aggregate(
            joined,
            Vec::new(),
            vec![bound::BoundAggregateTerm {
                function: AggregateFunction::Count,
                argument: None,
                name: "count".into(),
                slot: SlotId(6),
                value_type: Type::scalar(Kind::Int64, false),
            }],
        );
        let bound = query(aggregate, 7);
        let statistics = PlannerStats::empty();
        let structural_plan = plan_query_with_context(
            &bound,
            PlanOptions::default(),
            PlanningContext {
                statistics: Some(&statistics),
            },
        )
        .plan;
        let cost_plan = plan_query_with_context(
            &bound,
            PlanOptions {
                mode: PlannerMode::Cost,
                ..PlanOptions::default()
            },
            PlanningContext {
                statistics: Some(&statistics),
            },
        )
        .plan;
        let mut selected = false;
        cost_plan.walk(&mut |node| {
            selected |= matches!(node.kind, NodeKind::PredicateTransferJoin { .. });
        });
        assert!(selected);

        let store = Store::memory("exec-predicate-transfer").await.unwrap();
        let structural = Executor::new(&store, Limits::default())
            .execute(&structural_plan)
            .await
            .unwrap();
        let mut cost_executor = Executor::new(&store, Limits::default());
        cost_executor.enable_measurements();
        let cost = cost_executor.execute(&cost_plan).await.unwrap();
        let reference = ReferenceExecutor::new(&store, Limits::default())
            .execute(&bound)
            .await
            .unwrap();

        assert_eq!(cost, structural);
        assert_eq!(cost, reference);
        let measurement = cost_executor
            .join_measurements()
            .iter()
            .find(|measurement| measurement.operator == "PredicateTransferJoin")
            .expect("predicate transfer measurement");
        assert_eq!(measurement.rows_before_reduction, 90);
        assert_eq!(measurement.rows_after_reduction, 0);
        assert_eq!(measurement.filter_rows_skipped, 90);
        assert_eq!(measurement.filter_input_scans, 6);
        assert_eq!(measurement.filter_repeated_scans, 3);
        assert_eq!(measurement.filter_false_positives, 0);
        assert!(measurement.filter_false_positive_measurement_complete);
        assert!(measurement.filter_insertions > 0);
        assert!(measurement.filter_min_max_checks > 0);
        assert!(measurement.filter_blocks_skipped > 0);
        assert!(measurement.filter_min_max_rows_skipped > 0);
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn predicate_transfer_narrows_a_primary_key_scan() {
        let table = table();
        let store = Store::memory("exec-predicate-transfer-storage")
            .await
            .unwrap();
        seed(&store, &table).await;
        let source = Relation::rows(
            "selected",
            vec![Field {
                name: "id".into(),
                slot: SlotId(0),
                value_type: Type::scalar(Kind::Text, false),
            }],
            vec![vec![Value::Text("t2".into())]],
        );
        let target = scan(&table, [1, 2, 3], "target");
        let on = Expr::binary(
            BinaryOp::Eq,
            expression(&source, "id"),
            expression(&target, "id"),
        );
        let joined = Relation::join(
            source.clone(),
            target.clone(),
            crate::engine::lir::JoinKind::Inner,
            on,
        );
        let mut plan = plan_query(&query(joined.clone(), 4), PlanOptions::default());
        let structural = plan.clone();
        let NodeKind::NestedLoopJoin {
            left,
            right,
            kind,
            on,
            keys,
            right_output,
            decision,
        } = plan.root.kind.clone()
        else {
            panic!("expected nested-loop join")
        };
        assert!(matches!(&left.kind, NodeKind::Rows(_)));
        assert!(matches!(&right.kind, NodeKind::TableScan { .. }));
        let join_plan = NodeKind::NestedLoopJoin {
            left: Box::new(
                NodeKind::PredicateTransferInput {
                    input: 0,
                    output: source.output().clone(),
                }
                .bare(),
            ),
            right: Box::new(
                NodeKind::PredicateTransferInput {
                    input: 1,
                    output: target.output().clone(),
                }
                .bare(),
            ),
            kind,
            on,
            keys: keys.clone(),
            right_output,
            decision,
        }
        .bare();
        plan.root = NodeKind::PredicateTransferJoin {
            inputs: vec![*left, *right],
            schedule: PredicateTransferSchedule {
                root_input: 1,
                forward: PredicateTransferPass {
                    order: vec![0, 1],
                    edges: vec![crate::engine::planner::physical::PredicateTransferEdge {
                        source: 0,
                        target: 1,
                        keys,
                    }],
                },
                backward: PredicateTransferPass {
                    order: Vec::new(),
                    edges: Vec::new(),
                },
                pruned_paths: 1,
            },
            join_plan: Box::new(join_plan),
            output: joined.output().clone(),
            bits_per_key: 20,
            hash_functions: 7,
            runtime_policy: crate::engine::planner::predicate_transfer::runtime_policy(),
            memory_limit_bytes: 1_000_000,
            logical_row_operations: AccessQuantity::exact(0),
            estimated_peak_retained_bytes: AccessQuantity::exact(0),
        }
        .bare();
        let view = RecordingView {
            store,
            ranges: Mutex::new(Vec::new()),
            scan_entries: Arc::new(AtomicUsize::new(0)),
            seek_targets: Arc::new(Mutex::new(Vec::new())),
        };
        let structural_rows = Executor::new(&view, Limits::default())
            .run_frames(&structural)
            .await
            .unwrap();
        let structural_entries = view.scan_entries.swap(0, AtomicOrdering::Relaxed);
        view.ranges
            .lock()
            .expect("recorded range lock poisoned")
            .clear();
        let mut executor = Executor::new(&view, Limits::default());
        executor.enable_measurements();
        let rows = executor.run_frames(&plan).await.unwrap();

        assert_eq!(rows, structural_rows);
        assert_eq!(rows.len(), 1);
        assert_eq!(structural_entries, 3);
        assert_eq!(view.scan_entries.load(AtomicOrdering::Relaxed), 1);
        let mut expected_start = codec::data_prefix(&table).unwrap();
        expected_start.extend_from_slice(&codec::encode_value(&Value::Text("t2".into())).unwrap());
        let data_prefix = codec::data_prefix(&table).unwrap();
        let data_ranges = view
            .ranges
            .lock()
            .expect("recorded range lock poisoned")
            .iter()
            .filter(|range| {
                range
                    .start
                    .as_deref()
                    .is_some_and(|start| start.starts_with(&data_prefix))
            })
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            data_ranges,
            vec![KeyRange {
                start: Some(Bytes::from(data_prefix)),
                end: prefix_end(&expected_start).map(Bytes::from),
            }]
        );
        assert_eq!(
            *view
                .seek_targets
                .lock()
                .expect("recorded seek target lock poisoned"),
            vec![Bytes::from(expected_start)]
        );
        let measurement = executor
            .join_measurements()
            .iter()
            .find(|measurement| measurement.operator == "PredicateTransferJoin")
            .expect("predicate transfer measurement");
        assert_eq!(measurement.filter_storage_range_candidates, 1);
        assert_eq!(measurement.filter_storage_ranges_applied, 1);
        assert_eq!(measurement.filter_storage_scans_pruned, 1);
        assert_eq!(measurement.filter_storage_empty_scans, 0);
        view.store.close().await.unwrap();
    }

    #[test]
    fn predicate_transfer_maps_the_next_index_prefix() {
        let table = table();
        let relation = scan(&table, [0, 1, 2], "target");
        let target = relation.output().lookup("board_id").unwrap().clone();
        let source = Field {
            name: "board_id".into(),
            slot: SlotId(10),
            value_type: target.value_type.clone(),
        };
        let node = NodeKind::IndexRangeScan {
            scan: Box::new(relation),
            index: table.indexes[0].clone(),
            equality_prefix: Vec::new(),
            range: None,
            decode_columns: table.columns.clone(),
            access: Default::default(),
        }
        .bare();
        let pass = PredicateTransferPass {
            order: vec![0, 1],
            edges: vec![crate::engine::planner::physical::PredicateTransferEdge {
                source: 0,
                target: 1,
                keys: vec![EquiJoinKey {
                    left: source,
                    right: target,
                }],
            }],
        };
        let encoded = codec::encode_value(&Value::Text("b1".into())).unwrap();
        let decision = storage_pruning(
            &node,
            &pass,
            &[
                crate::engine::exec::predicate_transfer::StorageRange::Bounded {
                    edge_index: 0,
                    minimum: encoded.clone(),
                    maximum: encoded.clone(),
                },
            ],
            &Env::new(),
        )
        .unwrap()
        .expect("index range pruning");
        let row_store::ScanPruning::Range(range) = decision.pruning else {
            panic!("expected bounded index range")
        };
        let mut start = codec::index_prefix(&table, &table.indexes[0].id).unwrap();
        start.extend_from_slice(&encoded);
        assert_eq!(
            range,
            KeyRange {
                start: Some(Bytes::from(start.clone())),
                end: prefix_end(&start).map(Bytes::from),
            }
        );
    }

    #[tokio::test]
    async fn hash_join_matches_nested_and_reference_for_duplicates_nulls_and_left_padding() {
        for kind in [
            crate::engine::lir::JoinKind::Inner,
            crate::engine::lir::JoinKind::Left,
        ] {
            let left = nullable_int_rows("left", 0, &[Some(1), Some(2), None]);
            let right = nullable_int_rows("right", 1, &[Some(1), Some(1), Some(3), None]);
            let on = Expr::binary(
                BinaryOp::Eq,
                Expr::slot(SlotId(0), "left.n", Type::scalar(Kind::Int64, true)),
                Expr::slot(SlotId(1), "right.n", Type::scalar(Kind::Int64, true)),
            );
            let relation = Relation::join(left.clone(), right.clone(), kind, on.clone());
            let bound = query(relation.clone(), 2);
            let nested_plan = plan_query(&bound, PlanOptions::default());
            let NodeKind::NestedLoopJoin {
                left: planned_left,
                right: planned_right,
                keys,
                right_output,
                ..
            } = nested_plan.root.kind.clone()
            else {
                panic!("expected nested-loop join")
            };
            let mut hash_plan = nested_plan.clone();
            hash_plan.root.kind = NodeKind::HashJoin {
                left: planned_left,
                right: planned_right,
                kind,
                on,
                keys,
                right_output,
                memory_limit_bytes: 1024,
                decision: Default::default(),
            };
            let store = Store::memory(&format!("exec-hash-join-{kind:?}"))
                .await
                .unwrap();
            let mut nested = Executor::new(&store, Limits::default());
            nested.enable_measurements();
            let nested_result = nested.execute(&nested_plan).await.unwrap();
            let mut hash = Executor::new(&store, Limits::default());
            hash.enable_measurements();
            let hash_result = hash.execute(&hash_plan).await.unwrap();
            let reference = ReferenceExecutor::new(&store, Limits::default())
                .execute(&bound)
                .await
                .unwrap();
            assert_eq!(hash_result, nested_result);
            assert_eq!(hash_result, reference);
            let hash_measurement = hash.join_measurements().last().unwrap();
            assert_eq!(hash_measurement.operator, "HashJoin");
            assert_eq!(hash_measurement.build_rows, 4);
            assert_eq!(hash_measurement.probe_rows, 3);
            assert!(hash_measurement.key_comparisons > 0);
            assert_eq!(hash_measurement.residual_predicate_evaluations, 0);
            assert!(hash_measurement.peak_retained_bytes > 0);
            assert_eq!(hash_measurement.spill_bytes, 0);
            let nested_measurement = nested.join_measurements().last().unwrap();
            assert_eq!(nested_measurement.build_rows, 4);
            assert_eq!(nested_measurement.probe_rows, 3);
            if kind == crate::engine::lir::JoinKind::Inner {
                let NodeKind::HashJoin {
                    memory_limit_bytes, ..
                } = &mut hash_plan.root.kind
                else {
                    unreachable!()
                };
                *memory_limit_bytes = 1;
                let error = Executor::new(&store, Limits::default())
                    .execute(&hash_plan)
                    .await
                    .unwrap_err();
                assert_eq!(error.kind(), ErrorKind::Runtime);
            }
        }
    }

    #[tokio::test]
    async fn indexed_lookup_join_matches_nested_and_reports_exact_requests() {
        let table = table();
        let left = Relation::rows(
            "left",
            vec![Field {
                name: "id".into(),
                slot: SlotId(10),
                value_type: Type::scalar(Kind::Text, true),
            }],
            vec![
                vec![Value::Text("t1".into())],
                vec![Value::Text("missing".into())],
                vec![Value::Null(ScalarType::Text)],
            ],
        );
        let right = scan(&table, [20, 21, 22], "right");
        let on = Expr::binary(
            BinaryOp::Eq,
            Expr::slot(SlotId(10), "left.id", Type::scalar(Kind::Text, true)),
            Expr::slot(SlotId(20), "right.id", Type::scalar(Kind::Text, false)),
        );
        let relation = Relation::join(
            left,
            right.clone(),
            crate::engine::lir::JoinKind::Left,
            on.clone(),
        );
        let bound = query(relation, 23);
        let nested_plan = plan_query(&bound, PlanOptions::default());
        let NodeKind::NestedLoopJoin {
            left: planned_left,
            right: planned_right,
            keys,
            right_output,
            ..
        } = nested_plan.root.kind.clone()
        else {
            panic!("expected nested-loop join")
        };
        let NodeKind::TableScan { decode_columns, .. } = planned_right.kind else {
            panic!("expected table scan")
        };
        let lookup = NodeKind::PrimaryKeyGet {
            scan: Box::new(right),
            key: vec![ConstValue::Outer(SlotId(10))],
            decode_columns,
            access: Default::default(),
        }
        .bare();
        let mut lookup_plan = nested_plan.clone();
        lookup_plan.root.kind = NodeKind::IndexedLookupJoin {
            left: planned_left,
            right: Box::new(lookup),
            kind: crate::engine::lir::JoinKind::Left,
            on,
            keys,
            right_output,
            decision: Default::default(),
        };
        let store = Store::memory("exec-indexed-lookup-join").await.unwrap();
        seed(&store, &table).await;
        let nested = Executor::new(&store, Limits::default())
            .execute(&nested_plan)
            .await
            .unwrap();
        let mut lookup = Executor::new(&store, Limits::default());
        lookup.enable_measurements();
        let lookup_result = lookup.execute(&lookup_plan).await.unwrap();
        let reference = ReferenceExecutor::new(&store, Limits::default())
            .execute(&bound)
            .await
            .unwrap();
        assert_eq!(lookup_result, nested);
        assert_eq!(lookup_result, reference);
        let measurement = lookup.join_measurements().last().unwrap();
        assert_eq!(measurement.operator, "IndexedLookupJoin");
        assert_eq!(measurement.probe_rows, 3);
        assert_eq!(measurement.lookup_requests, 2);
        assert_eq!(measurement.key_comparisons, 1);
        assert_eq!(measurement.residual_predicate_evaluations, 0);
        assert!(measurement.peak_retained_bytes > 0);
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

    #[tokio::test(flavor = "current_thread")]
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
        let summary = RecursiveSummaryCapture::default();
        let subscriber = tracing_subscriber::registry().with(summary.clone());
        let _subscriber = tracing::subscriber::set_default(subscriber);
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
        let counts = summary.0.lock().expect("recursive capture lock");
        for field in [
            "rad.recursive.iterations",
            "rad.recursive.output_rows",
            "rad.recursive.duplicate_rows",
            "rad.recursive.peak_frontier_rows",
            "rad.recursive.peak_retained_bytes",
            "rad.recursive.anchor_rows",
            "rad.recursive.anchor_duration_us",
            "rad.recursive.anchor_rows_examined",
            "rad.recursive.anchor_logical_row_operations",
            "rad.recursive.anchor_storage_reads",
            "rad.recursive.anchor_storage_bytes",
            "rad.recursive.step_rows_examined",
            "rad.recursive.step_logical_row_operations",
            "rad.recursive.step_storage_reads",
            "rad.recursive.step_storage_bytes",
        ] {
            assert_eq!(counts.get(field), Some(&1), "{field}");
        }
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
            let mut executor = Executor::new(&store, limits);
            let production_error = executor.run_frames(&plan).await.unwrap_err();
            assert_eq!(production_error.reason(), ErrorReason::RecursionLimit);
            assert!(!executor.frontier.contains_key("walk"));

            let reference_error = ReferenceExecutor::new(&store, limits)
                .run_frames(&bound)
                .await
                .unwrap_err();
            assert_eq!(reference_error.reason(), ErrorReason::RecursionLimit);
        }
    }
}
