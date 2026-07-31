//! Shared execution-frame mechanics.

use std::cmp::Ordering;

use crate::engine::lir::bound;
use crate::engine::lir::eval::{Env, evaluate};
use crate::engine::lir::{Datum, ObjectField, RootCardinality, Row, RowType, SlotId};

use super::{Error, ErrorKind, ErrorReason, Result};

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
                ErrorReason::CardinalityViolation,
                "exec: expected exactly one row, got none",
            )),
            _ => Err(Error::with_reason(
                ErrorKind::Runtime,
                ErrorReason::CardinalityViolation,
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

pub(super) fn merge(left: &Env, right: &Env) -> Env {
    let mut output = left.clone();
    output.extend_from(right);
    output
}

pub(super) fn row_to_frame(relation: &bound::Relation, row: &Row, outer: &Env) -> Env {
    let mut frame = new_frame(outer);
    for field in &relation.output().fields {
        if let Some(value) = row.get(&field.name) {
            frame.set_scalar(field.slot, value.clone());
        }
    }
    frame
}

pub(super) fn frame_to_object(output: &RowType, frame: &Env) -> Datum {
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

pub(super) fn remap_canonical(
    output: &RowType,
    canonical: &[SlotId],
    source: &Env,
    outer: &Env,
) -> Env {
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

pub(super) fn sort(frames: &mut Vec<Env>, terms: &[bound::BoundOrderTerm]) -> Result<()> {
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
    *frames = positions
        .into_iter()
        .map(|position| frames[position].clone())
        .collect();
    Ok(())
}

pub(super) fn frame_scalar(output: &RowType, frame: &Env) -> Datum {
    output
        .fields
        .first()
        .and_then(|field| frame.get(field.slot))
        .cloned()
        .unwrap_or(Datum::Null)
}

pub(super) fn frames_to_array(output: &RowType, frames: &[Env]) -> Datum {
    Datum::Array(
        frames
            .iter()
            .map(|frame| frame_to_object(output, frame))
            .collect(),
    )
}
