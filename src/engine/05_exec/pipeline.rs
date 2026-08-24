//! Pull operators for the streaming portion of a physical plan.

use async_recursion::async_recursion;
use async_trait::async_trait;

use crate::engine::kv::KvView;
use crate::engine::lir::bound::{self, RelationNode};
use crate::engine::lir::eval::{CanonicalRowSet, Env, evaluate_datum, evaluate_predicate};
use crate::engine::lir::{JoinKind, RowType, SetQuantifier, TriBool, Value};
use crate::engine::planner::physical::{Node, NodeKind, PhysicalField};

use super::frames::{
    merge as merge_frames, new_frame, remap_positional, row_to_frame, sort as sort_frames,
};
use std::sync::Arc;
use std::sync::atomic;

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
        | NodeKind::Intersect { left, right, .. }
        | NodeKind::Except { left, right, .. } => supports(left) && supports(right),
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
) -> Result<Vec<Env>> {
    let mut tallies = Vec::new();
    let mut operator = build(view, node, outer.clone(), &mut tallies, true).await?;
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
    Ok(frames)
}

pub(super) async fn execute(view: &dyn KvView, node: &Node, outer: &Env) -> Result<Vec<Env>> {
    let mut tallies = Vec::new();
    let mut operator = build(view, node, outer.clone(), &mut tallies, false).await?;
    let mut frames = Vec::new();
    while let Some(frame) = operator.next().await? {
        frames.push(frame);
    }
    Ok(frames)
}

#[async_recursion]
async fn build<'a>(
    view: &'a dyn KvView,
    node: &'a Node,
    outer: Env,
    tallies: &mut Vec<(Fingerprint, Tally)>,
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
            input: build(view, input, outer, tallies, measure).await?,
            predicate: predicate.clone(),
        }),
        NodeKind::Project { input, fields } => Box::new(Project {
            input: build(view, input, outer.clone(), tallies, measure).await?,
            fields: fields.clone(),
            outer,
        }),
        NodeKind::Sort { input, terms } => Box::new(Sort {
            input: Some(build(view, input, outer, tallies, measure).await?),
            terms: terms.clone(),
            frames: Vec::new(),
            position: 0,
        }),
        NodeKind::Slice {
            input,
            offset,
            limit,
        } => Box::new(Slice {
            input: build(view, input, outer, tallies, measure).await?,
            remaining_offset: *offset,
            remaining: *limit,
        }),
        NodeKind::Distinct { input, output } => Box::new(Distinct {
            input: build(view, input, outer, tallies, measure).await?,
            seen: CanonicalRowSet::new(output.fields.clone()),
        }),
        NodeKind::NestedLoopJoin {
            left,
            right,
            kind,
            on,
            right_output,
        } => Box::new(NestedLoopJoin {
            left: build(view, left, outer.clone(), tallies, measure).await?,
            right: Some(build(view, right, outer, tallies, measure).await?),
            kind: *kind,
            predicate: on.clone(),
            right_output: right_output.clone(),
            right_rows: Vec::new(),
            current_left: None,
            right_position: 0,
            matched: false,
        }),
        NodeKind::Concatenate {
            inputs,
            input_outputs,
            output,
        } => {
            let mut operators = Vec::with_capacity(inputs.len());
            for input in inputs {
                operators.push(build(view, input, outer.clone(), tallies, measure).await?);
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
            build(view, left, outer.clone(), tallies, measure).await?,
            build(view, right, outer.clone(), tallies, measure).await?,
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
            build(view, left, outer.clone(), tallies, measure).await?,
            build(view, right, outer.clone(), tallies, measure).await?,
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
    right_output: RowType,
    right_rows: Vec<Env>,
    current_left: Option<Env>,
    right_position: usize,
    matched: bool,
}

#[async_trait]
impl Operator for NestedLoopJoin<'_> {
    async fn next(&mut self) -> Result<Option<Env>> {
        if let Some(mut right) = self.right.take() {
            while let Some(frame) = right.next().await? {
                self.right_rows.push(frame);
            }
        }
        loop {
            if self.current_left.is_none() {
                let Some(left) = self.left.next().await? else {
                    return Ok(None);
                };
                self.current_left = Some(left);
                self.right_position = 0;
                self.matched = false;
            }
            while let Some(right) = self.right_rows.get(self.right_position) {
                self.right_position += 1;
                let merged = merge_frames(self.current_left.as_ref().expect("left row"), right);
                if evaluate_predicate(&self.predicate, &merged)? == TriBool::True {
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
