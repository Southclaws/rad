//! Pull operators for the streaming portion of a physical plan.

use async_recursion::async_recursion;
use async_trait::async_trait;

use crate::engine::kv::KvView;
use crate::engine::lir::bound::{self, RelationNode};
use crate::engine::lir::eval::{CanonicalRowSet, Env, evaluate_datum, evaluate_predicate};
use crate::engine::lir::{Datum, JoinKind, RowType, SetQuantifier, TriBool, Value};
use crate::engine::planner::analysis::EquiJoinKey;
use crate::engine::planner::physical::{Node, NodeKind, PhysicalField};

use super::frames::{
    merge as merge_frames, new_frame, remap_positional, row_to_frame, sort as sort_frames,
};
use std::sync::Arc;
use std::sync::atomic;
use std::{collections::HashMap, hash::Hasher as _};

use crate::engine::lir::fingerprint::Fingerprint;

use super::query::resolve_constant;
use super::row_store::{self, RowIterator};
use super::set;
use super::{Error, ErrorKind, Result};

#[async_trait]
trait Operator: Send {
    async fn next(&mut self) -> Result<Option<Env>>;
}

pub(super) fn supports(node: &Node) -> bool {
    match &node.kind {
        NodeKind::PrimaryKeyGet { .. }
        | NodeKind::TableScan { .. }
        | NodeKind::Rows(_)
        | NodeKind::IndexRangeScan { .. } => true,
        NodeKind::Filter { input, .. }
        | NodeKind::Project { input, .. }
        | NodeKind::Sort { input, .. }
        | NodeKind::Slice { input, .. }
        | NodeKind::Distinct { input, .. } => supports(input),
        NodeKind::NestedLoopJoin { left, right, .. }
        | NodeKind::HashJoin { left, right, .. }
        | NodeKind::Intersect { left, right, .. }
        | NodeKind::Except { left, right, .. } => supports(left) && supports(right),
        NodeKind::JoinGraphChoice { .. }
        | NodeKind::ShreddedYannakakisJoin { .. }
        | NodeKind::PredicateTransferJoin { .. }
        | NodeKind::PredicateTransferInput { .. } => false,
        NodeKind::Concatenate { inputs, .. } => inputs.iter().all(supports),
        _ => false,
    }
}

/// Run one fused segment, tallying rows emitted by every node inside it that
/// carries an attribution.
pub(super) async fn execute_measured(
    view: &dyn KvView,
    node: &Node,
    outer: &Env,
    measured: &mut Vec<(Fingerprint, u64)>,
    join_measurements: &mut Vec<super::observe::JoinOperatorMeasurement>,
) -> Result<Vec<Env>> {
    let mut tallies = Vec::new();
    let mut join_tallies = Vec::new();
    let mut operator = build(
        view,
        node,
        outer.clone(),
        &mut tallies,
        &mut join_tallies,
        true,
    )
    .await?;
    let mut frames = Vec::new();
    while let Some(frame) = operator.next().await? {
        frames.push(frame);
    }
    drop(operator);
    // A node a downstream limit abandoned emitted fewer rows than it holds;
    // reporting that as its cardinality would bias every model beneath a
    // slice downwards, so only exhausted nodes are reported at all.
    measured.extend(tallies.into_iter().filter_map(|(family, tally)| {
        tally
            .exhausted
            .load(atomic::Ordering::Relaxed)
            .then(|| (family, tally.rows.load(atomic::Ordering::Relaxed)))
    }));
    join_measurements.extend(join_tallies.iter().map(JoinTally::snapshot));
    Ok(frames)
}

pub(super) async fn execute(
    view: &dyn KvView,
    node: &Node,
    outer: &Env,
    join_measurements: &mut Vec<super::observe::JoinOperatorMeasurement>,
) -> Result<Vec<Env>> {
    let mut tallies = Vec::new();
    let mut join_tallies = Vec::new();
    let mut operator = build(
        view,
        node,
        outer.clone(),
        &mut tallies,
        &mut join_tallies,
        false,
    )
    .await?;
    let mut frames = Vec::new();
    while let Some(frame) = operator.next().await? {
        frames.push(frame);
    }
    drop(operator);
    join_measurements.extend(join_tallies.iter().map(JoinTally::snapshot));
    Ok(frames)
}

#[async_recursion]
async fn build<'a>(
    view: &'a dyn KvView,
    node: &'a Node,
    outer: Env,
    tallies: &mut Vec<(Fingerprint, Tally)>,
    join_tallies: &mut Vec<JoinTally>,
    measure: bool,
) -> Result<Box<dyn Operator + 'a>> {
    let operator: Box<dyn Operator + 'a> = match &node.kind {
        NodeKind::PrimaryKeyGet {
            scan,
            key,
            decode_columns,
            ..
        } => {
            let table = scan.scan_table();
            let mut values = crate::engine::lir::Row::new();
            for (column, constant) in table.primary_key.iter().zip(key) {
                let value = resolve_constant(constant, &outer)?;
                if value.is_null() {
                    return Ok(Box::new(Empty) as Box<dyn Operator + 'a>);
                }
                values.insert(column.clone(), value);
            }
            Box::new(PrimaryKeyGet {
                view,
                scan: (**scan).clone(),
                key: values,
                columns: decode_columns.clone(),
                outer,
                done: false,
            })
        }
        NodeKind::TableScan {
            scan,
            decode_columns,
            ..
        } => Box::new(RowScan {
            iterator: row_store::scan_table(view, scan.scan_table(), decode_columns).await?,
            scan: (**scan).clone(),
            outer,
        }),
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
                .map(|constant| resolve_constant(constant, &outer))
                .collect::<Result<Vec<_>>>()?;
            if equality_prefix.iter().any(Value::is_null) {
                return Ok(Box::new(Empty) as Box<dyn Operator + 'a>);
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
            Box::new(RowScan {
                iterator: row_store::scan_index_range(
                    view,
                    scan.scan_table(),
                    index,
                    &equality_prefix,
                    range,
                    decode_columns,
                )
                .await?,
                scan: (**scan).clone(),
                outer,
            })
        }
        NodeKind::Rows(relation) => Box::new(Rows {
            relation: relation.clone(),
            outer,
            position: 0,
        }),
        NodeKind::Filter { input, predicate } => Box::new(Filter {
            input: build(view, input, outer, tallies, join_tallies, measure).await?,
            predicate: predicate.clone(),
        }),
        NodeKind::Project { input, fields } => Box::new(Project {
            input: build(view, input, outer.clone(), tallies, join_tallies, measure).await?,
            fields: fields.clone(),
            outer,
        }),
        NodeKind::Sort { input, terms } => Box::new(Sort {
            input: Some(build(view, input, outer, tallies, join_tallies, measure).await?),
            terms: terms.clone(),
            frames: Vec::new(),
            position: 0,
        }),
        NodeKind::Slice {
            input,
            offset,
            limit,
        } => Box::new(Slice {
            input: build(view, input, outer, tallies, join_tallies, measure).await?,
            remaining_offset: *offset,
            remaining: *limit,
        }),
        NodeKind::Distinct { input, output } => Box::new(Distinct {
            input: build(view, input, outer, tallies, join_tallies, measure).await?,
            seen: CanonicalRowSet::new(output.fields.clone()),
        }),
        NodeKind::NestedLoopJoin {
            left,
            right,
            kind,
            on,
            keys,
            right_output,
            ..
        } => {
            let tally = if measure {
                let tally = JoinTally::new("NestedLoopJoin");
                join_tallies.push(tally.clone());
                tally
            } else {
                JoinTally::disabled("NestedLoopJoin")
            };
            Box::new(NestedLoopJoin {
                left: build(view, left, outer.clone(), tallies, join_tallies, measure).await?,
                right: Some(build(view, right, outer, tallies, join_tallies, measure).await?),
                kind: *kind,
                predicate: on.clone(),
                keys: keys.clone(),
                right_output: right_output.clone(),
                right_rows: Vec::new(),
                current_left: None,
                right_position: 0,
                matched: false,
                retained_bytes: 0,
                tally,
            })
        }
        NodeKind::HashJoin {
            left,
            right,
            kind,
            on,
            keys,
            right_output,
            memory_limit_bytes,
            ..
        } => {
            let tally = if measure {
                let tally = JoinTally::new("HashJoin");
                join_tallies.push(tally.clone());
                tally
            } else {
                JoinTally::disabled("HashJoin")
            };
            Box::new(HashJoin {
                left: build(view, left, outer.clone(), tallies, join_tallies, measure).await?,
                right: Some(build(view, right, outer, tallies, join_tallies, measure).await?),
                kind: *kind,
                predicate: on.clone(),
                keys: keys.clone(),
                right_output: right_output.clone(),
                memory_limit_bytes: *memory_limit_bytes,
                entries: HashMap::new(),
                current_left: None,
                current_hash: 0,
                current_entry: 0,
                current_row: 0,
                matched: false,
                retained_bytes: 0,
                tally,
            })
        }
        NodeKind::Concatenate {
            inputs,
            input_outputs,
            output,
        } => {
            let mut operators = Vec::with_capacity(inputs.len());
            for input in inputs {
                operators
                    .push(build(view, input, outer.clone(), tallies, join_tallies, measure).await?);
            }
            Box::new(Concatenate {
                inputs: operators,
                input_outputs: input_outputs.clone(),
                output: output.clone(),
                outer,
                position: 0,
            })
        }
        NodeKind::Intersect {
            left,
            right,
            quantifier,
            left_output,
            right_output,
            output,
        } => Box::new(SetOperator::new(
            build(view, left, outer.clone(), tallies, join_tallies, measure).await?,
            build(view, right, outer.clone(), tallies, join_tallies, measure).await?,
            *quantifier,
            false,
            left_output.clone(),
            right_output.clone(),
            output.clone(),
            outer,
        )),
        NodeKind::Except {
            left,
            right,
            quantifier,
            left_output,
            right_output,
            output,
        } => Box::new(SetOperator::new(
            build(view, left, outer.clone(), tallies, join_tallies, measure).await?,
            build(view, right, outer.clone(), tallies, join_tallies, measure).await?,
            *quantifier,
            true,
            left_output.clone(),
            right_output.clone(),
            output.clone(),
            outer,
        )),
        _ => {
            return Err(Error::message(
                ErrorKind::Internal,
                "exec: unsupported node entered the pull pipeline",
            ));
        }
    };
    let Some(attribution) = node.attribution.filter(|_| measure) else {
        return Ok(operator);
    };
    let tally = Tally::default();
    tallies.push((attribution, tally.clone()));
    Ok(Box::new(Counting {
        inner: operator,
        tally,
    }))
}

/// Rows a node emitted, and whether it ran out of rows or was abandoned once
/// a downstream operator had enough.
#[derive(Clone, Default)]
struct Tally {
    rows: Arc<atomic::AtomicU64>,
    exhausted: Arc<atomic::AtomicBool>,
}

#[derive(Clone)]
struct JoinTally {
    operator: &'static str,
    enabled: bool,
    build_rows: Arc<atomic::AtomicU64>,
    probe_rows: Arc<atomic::AtomicU64>,
    lookup_requests: Arc<atomic::AtomicU64>,
    key_comparisons: Arc<atomic::AtomicU64>,
    residual_predicate_evaluations: Arc<atomic::AtomicU64>,
    peak_retained_bytes: Arc<atomic::AtomicU64>,
    spill_bytes: Arc<atomic::AtomicU64>,
}

impl JoinTally {
    fn new(operator: &'static str) -> Self {
        Self {
            operator,
            enabled: true,
            build_rows: Arc::new(atomic::AtomicU64::new(0)),
            probe_rows: Arc::new(atomic::AtomicU64::new(0)),
            lookup_requests: Arc::new(atomic::AtomicU64::new(0)),
            key_comparisons: Arc::new(atomic::AtomicU64::new(0)),
            residual_predicate_evaluations: Arc::new(atomic::AtomicU64::new(0)),
            peak_retained_bytes: Arc::new(atomic::AtomicU64::new(0)),
            spill_bytes: Arc::new(atomic::AtomicU64::new(0)),
        }
    }

    fn disabled(operator: &'static str) -> Self {
        let mut tally = Self::new(operator);
        tally.enabled = false;
        tally
    }

    fn add_build_row(&self) {
        if self.enabled {
            self.build_rows.fetch_add(1, atomic::Ordering::Relaxed);
        }
    }

    fn add_probe_row(&self) {
        if self.enabled {
            self.probe_rows.fetch_add(1, atomic::Ordering::Relaxed);
        }
    }

    fn add_key_comparison(&self) {
        if self.enabled {
            self.key_comparisons.fetch_add(1, atomic::Ordering::Relaxed);
        }
    }

    fn add_residual_predicate_evaluation(&self) {
        if self.enabled {
            self.residual_predicate_evaluations
                .fetch_add(1, atomic::Ordering::Relaxed);
        }
    }

    fn retain(&self, bytes: u64) {
        if self.enabled {
            self.peak_retained_bytes
                .fetch_max(bytes, atomic::Ordering::Relaxed);
        }
    }

    fn snapshot(&self) -> super::observe::JoinOperatorMeasurement {
        super::observe::JoinOperatorMeasurement {
            operator: self.operator,
            build_rows: self.build_rows.load(atomic::Ordering::Relaxed),
            probe_rows: self.probe_rows.load(atomic::Ordering::Relaxed),
            lookup_requests: self.lookup_requests.load(atomic::Ordering::Relaxed),
            key_comparisons: self.key_comparisons.load(atomic::Ordering::Relaxed),
            residual_predicate_evaluations: self
                .residual_predicate_evaluations
                .load(atomic::Ordering::Relaxed),
            peak_retained_bytes: self.peak_retained_bytes.load(atomic::Ordering::Relaxed),
            spill_bytes: self.spill_bytes.load(atomic::Ordering::Relaxed),
            reduction_passes: 0,
            rows_before_reduction: 0,
            rows_after_reduction: 0,
            dangling_rows_removed: 0,
            expanded_rows: 0,
            filter_rows_scanned: 0,
            filter_insertions: 0,
            filter_checks: 0,
            filter_false_positives: 0,
            filter_false_positive_measurement_complete: false,
            filter_rows_skipped: 0,
            filter_paths: 0,
            filter_pruned_paths: 0,
            filter_builds: 0,
            filter_shared_paths: 0,
            filter_build_cancellations: 0,
            filter_memory_cancellations: 0,
            filter_probe_cancellations: 0,
            filter_paths_canceled: 0,
            filter_blocks_scanned: 0,
            filter_blocks_skipped: 0,
            filter_min_max_checks: 0,
            filter_min_max_rows_skipped: 0,
            filter_input_scans: 0,
            filter_repeated_scans: 0,
            filter_bytes: 0,
            filter_storage_range_candidates: 0,
            filter_storage_ranges_applied: 0,
            filter_storage_scans_pruned: 0,
            filter_storage_empty_scans: 0,
        }
    }
}

/// Pass-through that tallies what its input emits. Present only for nodes the
/// planner attributed, and never a factor in choosing the pipeline: a segment
/// runs the same operators in the same order either way.
struct Counting<'a> {
    inner: Box<dyn Operator + 'a>,
    tally: Tally,
}

#[async_trait]
impl Operator for Counting<'_> {
    async fn next(&mut self) -> Result<Option<Env>> {
        let frame = self.inner.next().await?;
        match &frame {
            Some(_) => {
                self.tally.rows.fetch_add(1, atomic::Ordering::Relaxed);
            }
            None => self.tally.exhausted.store(true, atomic::Ordering::Relaxed),
        }
        Ok(frame)
    }
}

struct Empty;

#[async_trait]
impl Operator for Empty {
    async fn next(&mut self) -> Result<Option<Env>> {
        Ok(None)
    }
}

struct PrimaryKeyGet<'a> {
    view: &'a dyn KvView,
    scan: bound::Relation,
    key: crate::engine::lir::Row,
    columns: Vec<crate::engine::catalog::model::Column>,
    outer: Env,
    done: bool,
}

#[async_trait]
impl Operator for PrimaryKeyGet<'_> {
    async fn next(&mut self) -> Result<Option<Env>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        Ok(
            row_store::get_columns(self.view, self.scan.scan_table(), &self.key, &self.columns)
                .await?
                .map(|row| row_to_frame(&self.scan, &row, &self.outer)),
        )
    }
}

struct RowScan<'a> {
    iterator: Box<dyn RowIterator + 'a>,
    scan: bound::Relation,
    outer: Env,
}

#[async_trait]
impl Operator for RowScan<'_> {
    async fn next(&mut self) -> Result<Option<Env>> {
        Ok(self
            .iterator
            .next()
            .await?
            .map(|row| row_to_frame(&self.scan, &row, &self.outer)))
    }
}

struct Rows {
    relation: bound::Relation,
    outer: Env,
    position: usize,
}

#[async_trait]
impl Operator for Rows {
    async fn next(&mut self) -> Result<Option<Env>> {
        let RelationNode::Rows { values, .. } = &self.relation.node else {
            unreachable!()
        };
        let Some(values) = values.get(self.position) else {
            return Ok(None);
        };
        self.position += 1;
        let mut frame = new_frame(&self.outer);
        for (field, value) in self.relation.output().fields.iter().zip(values) {
            frame.set_scalar(field.slot, value.clone());
        }
        Ok(Some(frame))
    }
}

struct Filter<'a> {
    input: Box<dyn Operator + 'a>,
    predicate: bound::Expr,
}

#[async_trait]
impl Operator for Filter<'_> {
    async fn next(&mut self) -> Result<Option<Env>> {
        while let Some(frame) = self.input.next().await? {
            if evaluate_predicate(&self.predicate, &frame)? == TriBool::True {
                return Ok(Some(frame));
            }
        }
        Ok(None)
    }
}

struct Project<'a> {
    input: Box<dyn Operator + 'a>,
    fields: Vec<PhysicalField>,
    outer: Env,
}

#[async_trait]
impl Operator for Project<'_> {
    async fn next(&mut self) -> Result<Option<Env>> {
        let Some(input) = self.input.next().await? else {
            return Ok(None);
        };
        let mut output = new_frame(&self.outer);
        for field in &self.fields {
            output.insert(field.slot, evaluate_datum(&field.expression, &input)?);
        }
        Ok(Some(output))
    }
}

struct Slice<'a> {
    input: Box<dyn Operator + 'a>,
    remaining_offset: usize,
    remaining: Option<usize>,
}

#[async_trait]
impl Operator for Slice<'_> {
    async fn next(&mut self) -> Result<Option<Env>> {
        if self.remaining == Some(0) {
            return Ok(None);
        }
        while self.remaining_offset > 0 {
            if self.input.next().await?.is_none() {
                return Ok(None);
            }
            self.remaining_offset -= 1;
        }
        let frame = self.input.next().await?;
        if frame.is_some()
            && let Some(remaining) = &mut self.remaining
        {
            *remaining -= 1;
        }
        Ok(frame)
    }
}

struct Sort<'a> {
    input: Option<Box<dyn Operator + 'a>>,
    terms: Vec<bound::BoundOrderTerm>,
    frames: Vec<Env>,
    position: usize,
}

#[async_trait]
impl Operator for Sort<'_> {
    async fn next(&mut self) -> Result<Option<Env>> {
        if let Some(mut input) = self.input.take() {
            while let Some(frame) = input.next().await? {
                self.frames.push(frame);
            }
            sort_frames(&mut self.frames, &self.terms)?;
        }
        let frame = self.frames.get(self.position).cloned();
        self.position += usize::from(frame.is_some());
        Ok(frame)
    }
}

struct Distinct<'a> {
    input: Box<dyn Operator + 'a>,
    seen: CanonicalRowSet,
}

#[async_trait]
impl Operator for Distinct<'_> {
    async fn next(&mut self) -> Result<Option<Env>> {
        while let Some(frame) = self.input.next().await? {
            if self.seen.insert(&frame) {
                return Ok(Some(frame));
            }
        }
        Ok(None)
    }
}

struct Concatenate<'a> {
    inputs: Vec<Box<dyn Operator + 'a>>,
    input_outputs: Vec<RowType>,
    output: RowType,
    outer: Env,
    position: usize,
}

#[async_trait]
impl Operator for Concatenate<'_> {
    async fn next(&mut self) -> Result<Option<Env>> {
        while let Some(input) = self.inputs.get_mut(self.position) {
            if let Some(frame) = input.next().await? {
                return Ok(Some(remap_positional(
                    &self.output,
                    &self.input_outputs[self.position],
                    &frame,
                    &self.outer,
                )));
            }
            self.position += 1;
        }
        Ok(None)
    }
}

struct NestedLoopJoin<'a> {
    left: Box<dyn Operator + 'a>,
    right: Option<Box<dyn Operator + 'a>>,
    kind: JoinKind,
    predicate: bound::Expr,
    keys: Vec<EquiJoinKey>,
    right_output: RowType,
    right_rows: Vec<Env>,
    current_left: Option<Env>,
    right_position: usize,
    matched: bool,
    retained_bytes: u64,
    tally: JoinTally,
}

#[async_trait]
impl Operator for NestedLoopJoin<'_> {
    async fn next(&mut self) -> Result<Option<Env>> {
        if let Some(mut right) = self.right.take() {
            while let Some(frame) = right.next().await? {
                self.tally.add_build_row();
                self.retained_bytes = self
                    .retained_bytes
                    .saturating_add(frame_retained_bytes(&frame));
                self.tally.retain(self.retained_bytes);
                self.right_rows.push(frame);
            }
        }
        loop {
            if self.current_left.is_none() {
                let Some(left) = self.left.next().await? else {
                    return Ok(None);
                };
                self.tally.add_probe_row();
                self.current_left = Some(left);
                self.right_position = 0;
                self.matched = false;
            }
            while let Some(right) = self.right_rows.get(self.right_position) {
                self.right_position += 1;
                let merged = merge_frames(self.current_left.as_ref().expect("left row"), right);
                if evaluate_join_predicate(&self.predicate, &self.keys, &merged, &self.tally)?
                    == TriBool::True
                {
                    self.matched = true;
                    return Ok(Some(merged));
                }
            }
            let left = self.current_left.take().expect("left row");
            if self.kind == JoinKind::Left && !self.matched {
                let mut padded = left;
                for field in &self.right_output.fields {
                    padded.insert(field.slot, crate::engine::lir::Datum::Null);
                }
                return Ok(Some(padded));
            }
        }
    }
}

struct HashEntry {
    key: Vec<u8>,
    rows: Vec<Env>,
}

struct HashJoin<'a> {
    left: Box<dyn Operator + 'a>,
    right: Option<Box<dyn Operator + 'a>>,
    kind: JoinKind,
    predicate: bound::Expr,
    keys: Vec<EquiJoinKey>,
    right_output: RowType,
    memory_limit_bytes: u64,
    entries: HashMap<u64, Vec<HashEntry>>,
    current_left: Option<Env>,
    current_hash: u64,
    current_entry: usize,
    current_row: usize,
    matched: bool,
    retained_bytes: u64,
    tally: JoinTally,
}

#[async_trait]
impl Operator for HashJoin<'_> {
    async fn next(&mut self) -> Result<Option<Env>> {
        if let Some(mut right) = self.right.take() {
            while let Some(frame) = right.next().await? {
                self.tally.add_build_row();
                let Some(key) = join_key(&frame, &self.keys, false)? else {
                    continue;
                };
                let hash = hash_join_key(&key);
                let entries = self.entries.entry(hash).or_default();
                let mut matching = None;
                for (index, entry) in entries.iter().enumerate() {
                    self.tally.add_key_comparison();
                    if entry.key == key {
                        matching = Some(index);
                        break;
                    }
                }
                let index = matching.unwrap_or_else(|| {
                    self.retained_bytes = self.retained_bytes.saturating_add(key.len() as u64);
                    entries.push(HashEntry {
                        key,
                        rows: Vec::new(),
                    });
                    entries.len() - 1
                });
                self.retained_bytes = self
                    .retained_bytes
                    .saturating_add(frame_retained_bytes(&frame));
                if self.retained_bytes > self.memory_limit_bytes {
                    return Err(Error::message(
                        ErrorKind::Runtime,
                        format!(
                            "exec: hash join retained byte limit {} exceeded",
                            self.memory_limit_bytes
                        ),
                    ));
                }
                self.tally.retain(self.retained_bytes);
                entries[index].rows.push(frame);
            }
        }
        loop {
            if self.current_left.is_none() {
                let Some(left) = self.left.next().await? else {
                    return Ok(None);
                };
                self.tally.add_probe_row();
                let key = join_key(&left, &self.keys, true)?;
                self.current_hash = key.as_ref().map_or(0, |key| hash_join_key(key));
                self.current_entry = usize::MAX;
                if let Some(key) = key
                    && let Some(entries) = self.entries.get(&self.current_hash)
                {
                    for (index, entry) in entries.iter().enumerate() {
                        self.tally.add_key_comparison();
                        if entry.key == key {
                            self.current_entry = index;
                            break;
                        }
                    }
                }
                self.current_left = Some(left);
                self.current_row = 0;
                self.matched = false;
            }
            while self.current_entry != usize::MAX {
                let Some(right) = self
                    .entries
                    .get(&self.current_hash)
                    .and_then(|entries| entries.get(self.current_entry))
                    .and_then(|entry| entry.rows.get(self.current_row))
                else {
                    break;
                };
                self.current_row += 1;
                let merged = merge_frames(self.current_left.as_ref().expect("left row"), right);
                if evaluate_join_predicate(&self.predicate, &self.keys, &merged, &self.tally)?
                    == TriBool::True
                {
                    self.matched = true;
                    return Ok(Some(merged));
                }
            }
            let left = self.current_left.take().expect("left row");
            if self.kind == JoinKind::Left && !self.matched {
                let mut padded = left;
                for field in &self.right_output.fields {
                    padded.insert(field.slot, Datum::Null);
                }
                return Ok(Some(padded));
            }
        }
    }
}

fn evaluate_join_predicate(
    predicate: &bound::Expr,
    keys: &[EquiJoinKey],
    frame: &Env,
    tally: &JoinTally,
) -> Result<TriBool> {
    if let bound::Expr::Binary {
        op: crate::engine::lir::BinaryOp::And,
        left,
        right,
        ..
    } = predicate
    {
        let left = evaluate_join_predicate(left, keys, frame, tally)?;
        if left == TriBool::False {
            return Ok(TriBool::False);
        }
        return Ok(left.and(evaluate_join_predicate(right, keys, frame, tally)?));
    }
    if is_join_key_comparison(predicate, keys) {
        tally.add_key_comparison();
    } else {
        tally.add_residual_predicate_evaluation();
    }
    Ok(evaluate_predicate(predicate, frame)?)
}

pub(super) fn is_join_key_comparison(predicate: &bound::Expr, keys: &[EquiJoinKey]) -> bool {
    let bound::Expr::Binary {
        op: crate::engine::lir::BinaryOp::Eq,
        left,
        right,
        ..
    } = predicate
    else {
        return false;
    };
    let (
        bound::Expr::SlotRef {
            slot: left_slot, ..
        },
        bound::Expr::SlotRef {
            slot: right_slot, ..
        },
    ) = (&**left, &**right)
    else {
        return false;
    };
    keys.iter().any(|key| {
        (key.left.slot == *left_slot && key.right.slot == *right_slot)
            || (key.left.slot == *right_slot && key.right.slot == *left_slot)
    })
}

pub(super) fn join_key(frame: &Env, keys: &[EquiJoinKey], left: bool) -> Result<Option<Vec<u8>>> {
    let mut output = Vec::new();
    for key in keys {
        let field = if left { &key.left } else { &key.right };
        let value = frame.scalar_at(field.slot, &field.name, &field.value_type)?;
        if value.is_null() {
            return Ok(None);
        }
        match value {
            Value::Text(value) => {
                output.push(1);
                output.extend_from_slice(&(value.len() as u64).to_be_bytes());
                output.extend_from_slice(value.as_bytes());
            }
            Value::Int64(value) => {
                output.push(2);
                output.extend_from_slice(&value.to_be_bytes());
            }
            Value::Float64(value) => {
                output.push(3);
                let bits = if value == 0.0 {
                    0
                } else if value.is_nan() {
                    f64::NAN.to_bits()
                } else {
                    value.to_bits()
                };
                output.extend_from_slice(&bits.to_be_bytes());
            }
            Value::Bool(value) => output.extend_from_slice(&[4, u8::from(value)]),
            Value::Null(_) => unreachable!("null join keys return before encoding"),
        }
    }
    Ok(Some(output))
}

fn hash_join_key(key: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hasher.write(key);
    hasher.finish()
}

pub(super) fn frame_retained_bytes(frame: &Env) -> u64 {
    frame
        .iter()
        .map(|(_, datum)| datum_retained_bytes(datum))
        .fold(0u64, u64::saturating_add)
}

fn datum_retained_bytes(datum: &Datum) -> u64 {
    match datum {
        Datum::Null | Datum::Scalar(Value::Null(_)) => 0,
        Datum::Scalar(Value::Text(value)) => value.len() as u64,
        Datum::Scalar(Value::Int64(_) | Value::Float64(_)) => 8,
        Datum::Scalar(Value::Bool(_)) => 1,
        Datum::Array(values) => values
            .iter()
            .map(datum_retained_bytes)
            .fold(0u64, u64::saturating_add),
        Datum::Object(fields) => fields
            .iter()
            .map(|field| field.name.len() as u64 + datum_retained_bytes(&field.datum))
            .fold(0u64, u64::saturating_add),
    }
}

struct SetOperator<'a> {
    left: Box<dyn Operator + 'a>,
    right: Option<Box<dyn Operator + 'a>>,
    left_output: RowType,
    right_output: RowType,
    output: RowType,
    outer: Env,
    state: set::State,
}

impl<'a> SetOperator<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        left: Box<dyn Operator + 'a>,
        right: Box<dyn Operator + 'a>,
        quantifier: SetQuantifier,
        subtract: bool,
        left_output: RowType,
        right_output: RowType,
        output: RowType,
        outer: Env,
    ) -> Self {
        Self {
            left,
            right: Some(right),
            left_output,
            right_output,
            output,
            outer,
            state: set::State::new(quantifier, subtract),
        }
    }
}

#[async_trait]
impl Operator for SetOperator<'_> {
    async fn next(&mut self) -> Result<Option<Env>> {
        if let Some(mut right) = self.right.take() {
            while let Some(frame) = right.next().await? {
                self.state.add_right(&self.right_output, &frame);
            }
        }
        while let Some(frame) = self.left.next().await? {
            if self.state.keep_left(&self.left_output, &frame) {
                return Ok(Some(remap_positional(
                    &self.output,
                    &self.left_output,
                    &frame,
                    &self.outer,
                )));
            }
        }
        Ok(None)
    }
}
