//! Pull operators for the streaming portion of a physical plan.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use async_recursion::async_recursion;
use async_trait::async_trait;

use crate::engine::kv::KvView;
use crate::engine::lir::bound::{self, RelationNode};
use crate::engine::lir::eval::{
    CanonicalRowKey, CanonicalRowSet, Env, canonical_row_key, evaluate, evaluate_datum,
    evaluate_predicate,
};
use crate::engine::lir::{JoinKind, RowType, SetQuantifier, TriBool, Value};
use crate::engine::planner::physical::{Node, PhysicalField};

use super::query::{
    merge_frames, new_frame, remap_positional, resolve_constant, row_to_frame, scan_table,
};
use super::row_store::{self, RowIterator};
use super::{Error, ErrorKind, Result};

#[async_trait]
trait Operator: Send {
    async fn next(&mut self) -> Result<Option<Env>>;
}

pub(super) fn supports(node: &Node) -> bool {
    match node {
        Node::PrimaryKeyGet { .. }
        | Node::TableScan { .. }
        | Node::Rows(_)
        | Node::IndexRangeScan { .. } => true,
        Node::Filter { input, .. }
        | Node::Project { input, .. }
        | Node::Sort { input, .. }
        | Node::Slice { input, .. }
        | Node::Distinct { input, .. } => supports(input),
        Node::NestedLoopJoin { left, right, .. }
        | Node::Intersect { left, right, .. }
        | Node::Except { left, right, .. } => supports(left) && supports(right),
        Node::Concatenate { inputs, .. } => inputs.iter().all(supports),
        _ => false,
    }
}

pub(super) async fn execute(view: &dyn KvView, node: &Node, outer: &Env) -> Result<Vec<Env>> {
    let mut operator = build(view, node, outer.clone()).await?;
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
) -> Result<Box<dyn Operator + 'a>> {
    let operator: Box<dyn Operator + 'a> = match node {
        Node::PrimaryKeyGet {
            scan,
            key,
            decode_columns,
            ..
        } => {
            let table = scan_table(scan);
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
        Node::TableScan {
            scan,
            decode_columns,
            ..
        } => Box::new(RowScan {
            iterator: row_store::scan_table(view, scan_table(scan), decode_columns).await?,
            scan: (**scan).clone(),
            outer,
        }),
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
                    scan_table(scan),
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
        Node::Rows(relation) => Box::new(Rows {
            relation: relation.clone(),
            outer,
            position: 0,
        }),
        Node::Filter { input, predicate } => Box::new(Filter {
            input: build(view, input, outer).await?,
            predicate: predicate.clone(),
        }),
        Node::Project { input, fields } => Box::new(Project {
            input: build(view, input, outer.clone()).await?,
            fields: fields.clone(),
            outer,
        }),
        Node::Sort { input, terms } => Box::new(Sort {
            input: Some(build(view, input, outer).await?),
            terms: terms.clone(),
            frames: Vec::new(),
            position: 0,
        }),
        Node::Slice {
            input,
            offset,
            limit,
        } => Box::new(Slice {
            input: build(view, input, outer).await?,
            remaining_offset: *offset,
            remaining: *limit,
        }),
        Node::Distinct { input, output } => Box::new(Distinct {
            input: build(view, input, outer).await?,
            seen: CanonicalRowSet::new(output.fields.clone()),
        }),
        Node::NestedLoopJoin {
            left,
            right,
            kind,
            on,
            right_output,
        } => Box::new(NestedLoopJoin {
            left: build(view, left, outer.clone()).await?,
            right: Some(build(view, right, outer).await?),
            kind: *kind,
            predicate: on.clone(),
            right_output: right_output.clone(),
            right_rows: Vec::new(),
            current_left: None,
            right_position: 0,
            matched: false,
        }),
        Node::Concatenate {
            inputs,
            input_outputs,
            output,
        } => {
            let mut operators = Vec::with_capacity(inputs.len());
            for input in inputs {
                operators.push(build(view, input, outer.clone()).await?);
            }
            Box::new(Concatenate {
                inputs: operators,
                input_outputs: input_outputs.clone(),
                output: output.clone(),
                outer,
                position: 0,
            })
        }
        Node::Intersect {
            left,
            right,
            quantifier,
            left_output,
            right_output,
            output,
        } => Box::new(SetOperation::new(
            build(view, left, outer.clone()).await?,
            build(view, right, outer.clone()).await?,
            *quantifier,
            false,
            left_output.clone(),
            right_output.clone(),
            output.clone(),
            outer,
        )),
        Node::Except {
            left,
            right,
            quantifier,
            left_output,
            right_output,
            output,
        } => Box::new(SetOperation::new(
            build(view, left, outer.clone()).await?,
            build(view, right, outer.clone()).await?,
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
    Ok(operator)
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
            row_store::get_columns(self.view, scan_table(&self.scan), &self.key, &self.columns)
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

struct SetOperation<'a> {
    left: Box<dyn Operator + 'a>,
    right: Option<Box<dyn Operator + 'a>>,
    quantifier: SetQuantifier,
    subtract: bool,
    left_output: RowType,
    right_output: RowType,
    output: RowType,
    outer: Env,
    right_counts: HashMap<CanonicalRowKey, usize>,
    emitted: HashSet<CanonicalRowKey>,
}

impl<'a> SetOperation<'a> {
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
            quantifier,
            subtract,
            left_output,
            right_output,
            output,
            outer,
            right_counts: HashMap::new(),
            emitted: HashSet::new(),
        }
    }
}

#[async_trait]
impl Operator for SetOperation<'_> {
    async fn next(&mut self) -> Result<Option<Env>> {
        if let Some(mut right) = self.right.take() {
            while let Some(frame) = right.next().await? {
                *self
                    .right_counts
                    .entry(canonical_row_key(&self.right_output.fields, &frame))
                    .or_default() += 1;
            }
        }
        while let Some(frame) = self.left.next().await? {
            let key = canonical_row_key(&self.left_output.fields, &frame);
            let keep = if self.quantifier == SetQuantifier::Distinct {
                if self.emitted.contains(&key) {
                    false
                } else {
                    let in_right = self.right_counts.get(&key).copied().unwrap_or_default() > 0;
                    let keep = self.subtract != in_right;
                    if keep {
                        self.emitted.insert(key);
                    }
                    keep
                }
            } else if let Some(count) = self.right_counts.get_mut(&key).filter(|count| **count > 0)
            {
                *count -= 1;
                !self.subtract
            } else {
                self.subtract
            };
            if keep {
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

fn sort_frames(frames: &mut Vec<Env>, terms: &[bound::BoundOrderTerm]) -> Result<()> {
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
    let sorted = positions
        .into_iter()
        .map(|position| frames[position].clone())
        .collect();
    *frames = sorted;
    Ok(())
}
