//! Pull operators for the streaming portion of a physical plan.

use async_recursion::async_recursion;
use async_trait::async_trait;

use crate::engine::catalog::model::ScalarType;
use crate::engine::kv::KvView;
use crate::engine::lir::bound::{self, RelationNode};
use crate::engine::lir::eval::{
    CanonicalRowSet, Env, evaluate, evaluate_datum, evaluate_predicate,
};
use crate::engine::lir::{
    BinaryOp, Datum, JoinKind, Kind, RowType, SetQuantifier, SlotId, TriBool, Value,
};
use crate::engine::planner::analysis::EquiJoinKey;
use crate::engine::planner::physical::{
    MaterializationRepresentation, Node, NodeKind, PhysicalField,
};

use super::frames::{
    column_values_to_frame, merge as merge_frames, new_frame, remap_canonical, remap_positional,
    row_to_frame, scan_slots, sort as sort_frames,
};
use std::collections::HashMap;
use std::hash::{BuildHasher as _, BuildHasherDefault, Hasher};
use std::sync::Arc;
use std::sync::atomic;

use crate::engine::lir::fingerprint::Fingerprint;

use super::parallel::ExecutionGrant;
use super::parallel::ExecutionScheduleEvent;
use super::parallel::WorkRequest;
use super::query::resolve_constant;
use super::relation_cache::{
    CachedGroupedDimensionBuild, CachedGroupedDimensionBuildHandle, CachedGroupedDimensionEntry,
    CachedHashBuild, CachedHashBuildHandle, CachedHashEntry, CachedWork, MaterializationDomain,
};
use super::row_store::{self, RowIterator};
use super::set;
use super::{Error, ErrorKind, Result};
use std::time::Instant;
#[cfg(debug_assertions)]
use tracing::Instrument as _;

#[async_trait]
trait Operator: Send {
    async fn next(&mut self) -> Result<Option<Env>>;

    async fn next_batch(&mut self, limit: usize, output: &mut Vec<Env>) -> Result<()> {
        let target = output.len().saturating_add(limit);
        while output.len() < target {
            let Some(frame) = self.next().await? else {
                break;
            };
            output.push(frame);
        }
        Ok(())
    }

    fn enable_bounded_read_ahead(&mut self) {}

    fn decoded_slots(&self) -> Option<&[SlotId]> {
        None
    }

    async fn next_decoded_batch(
        &mut self,
        _limit: usize,
        _output: &mut Vec<super::codec::DecodedRow>,
    ) -> Result<()> {
        Err(Error::message(
            ErrorKind::Internal,
            "exec: operator does not provide decoded rows",
        ))
    }

    fn raw_decoder(&self) -> Option<super::codec::RowDecoder> {
        None
    }

    async fn next_raw_batch(
        &mut self,
        _limit: usize,
        _output: &mut Vec<bytes::Bytes>,
    ) -> Result<()> {
        Err(Error::message(
            ErrorKind::Internal,
            "exec: operator does not provide raw rows",
        ))
    }
}

pub(super) fn supports(node: &Node) -> bool {
    match &node.kind {
        NodeKind::PrimaryKeyGet { .. }
        | NodeKind::TableScan { .. }
        | NodeKind::Rows(_)
        | NodeKind::IndexRangeScan { .. }
        | NodeKind::RecursiveReference { .. } => true,
        NodeKind::Filter { input, .. }
        | NodeKind::Project { input, .. }
        | NodeKind::Sort { input, .. }
        | NodeKind::Slice { input, .. }
        | NodeKind::Distinct { input, .. }
        | NodeKind::Aggregate { input, .. } => supports(input),
        NodeKind::NestedLoopJoin { left, right, .. }
        | NodeKind::HashJoin { left, right, .. }
        | NodeKind::GroupedHashJoinAggregate {
            fact: left,
            dimension: right,
            ..
        }
        | NodeKind::Intersect { left, right, .. }
        | NodeKind::Except { left, right, .. } => supports(left) && supports(right),
        NodeKind::Concatenate { inputs, .. } => inputs.iter().all(supports),
        _ => false,
    }
}

/// Run one fused segment, tallying rows emitted by every node inside it that
/// carries an attribution.
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_measured(
    view: &dyn KvView,
    node: &Node,
    outer: &Env,
    frontier: &HashMap<String, Vec<Env>>,
    measured: &mut Vec<(Fingerprint, u64)>,
    join_measurements: &mut Vec<super::observe::JoinOperatorMeasurement>,
    operator_measurements: &mut Vec<super::observe::OperatorMeasurement>,
    next_operator_id: &mut u32,
    parent_operator_id: Option<u32>,
    measure_operators: bool,
    subrelation_cache: Option<&super::relation_cache::SubrelationCacheContext<'_>>,
    execution_grant: &ExecutionGrant,
) -> Result<Vec<Env>> {
    let mut tallies = Vec::new();
    let mut join_tallies = Vec::new();
    let mut operator_tallies = Vec::new();
    let mut operator = build(
        view,
        node,
        outer.clone(),
        &mut tallies,
        &mut join_tallies,
        true,
        &mut operator_tallies,
        next_operator_id,
        parent_operator_id,
        measure_operators,
        None,
        BuildResources {
            execution_grant: Some(execution_grant.clone()),
            subrelation_cache,
            frontier,
            bypass_materialization: false,
        },
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
    operator_measurements.extend(operator_tallies.iter().map(OperatorTally::snapshot));
    Ok(frames)
}

pub(super) async fn execute(
    view: &dyn KvView,
    node: &Node,
    outer: &Env,
    frontier: &HashMap<String, Vec<Env>>,
    join_measurements: &mut Vec<super::observe::JoinOperatorMeasurement>,
    subrelation_cache: Option<&super::relation_cache::SubrelationCacheContext<'_>>,
    execution_grant: &ExecutionGrant,
) -> Result<Vec<Env>> {
    let mut tallies = Vec::new();
    let mut join_tallies = Vec::new();
    let mut operator_tallies = Vec::new();
    let mut next_operator_id = 0;
    let mut operator = build(
        view,
        node,
        outer.clone(),
        &mut tallies,
        &mut join_tallies,
        false,
        &mut operator_tallies,
        &mut next_operator_id,
        None,
        false,
        None,
        BuildResources {
            execution_grant: Some(execution_grant.clone()),
            subrelation_cache,
            frontier,
            bypass_materialization: false,
        },
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

#[derive(Clone)]
struct BuildResources<'a> {
    execution_grant: Option<ExecutionGrant>,
    subrelation_cache: Option<&'a super::relation_cache::SubrelationCacheContext<'a>>,
    frontier: &'a HashMap<String, Vec<Env>>,
    bypass_materialization: bool,
}

#[allow(clippy::too_many_arguments)]
#[async_recursion]
async fn build<'a>(
    view: &'a dyn KvView,
    node: &'a Node,
    outer: Env,
    tallies: &mut Vec<(Fingerprint, Tally)>,
    join_tallies: &mut Vec<JoinTally>,
    measure: bool,
    operator_tallies: &mut Vec<OperatorTally>,
    next_operator_id: &mut u32,
    parent_operator_id: Option<u32>,
    measure_operators: bool,
    parent_operator_span: Option<&tracing::Span>,
    resources: BuildResources<'a>,
) -> Result<Box<dyn Operator + 'a>> {
    if node
        .materialization
        .as_ref()
        .is_some_and(|candidate| candidate.representation == MaterializationRepresentation::Rows)
        && !resources.bypass_materialization
        && resources.subrelation_cache.is_some()
    {
        return build_materialized(
            view,
            node,
            outer,
            tallies,
            join_tallies,
            measure,
            operator_tallies,
            next_operator_id,
            parent_operator_id,
            measure_operators,
            parent_operator_span,
            resources,
        )
        .await;
    }
    let execution_grant = resources;
    let operator_tally = measure_operators.then(|| {
        let operator_id = *next_operator_id;
        *next_operator_id = next_operator_id.saturating_add(1);
        let tally = OperatorTally::new(
            operator_id,
            parent_operator_id,
            super::query::operator_name(&node.kind),
            node.attribution,
            matches!(node.kind, NodeKind::IndexRangeScan { .. }),
        );
        operator_tallies.push(tally.clone());
        tally
    });
    let operator_span = operator_tally.as_ref().map(|tally| {
        super::observe::OperatorRuntimeSpan::new(
            tally.operator_id,
            tally.parent_operator_id,
            tally.operator,
            tally.relation_fingerprint,
            parent_operator_span,
        )
    });
    let child_parent_id = operator_tally
        .as_ref()
        .map_or(parent_operator_id, |tally| Some(tally.operator_id));
    let child_parent_span = operator_span
        .as_ref()
        .map(super::observe::OperatorRuntimeSpan::span)
        .or(parent_operator_span);
    let open_started = operator_tally.as_ref().map(|_| Instant::now());
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
                    return Ok(finish_operator(
                        operator_tally,
                        operator_span,
                        open_started,
                        Box::new(Empty),
                    ));
                }
                values.insert(column.clone(), value);
            }
            Box::new(PrimaryKeyGet {
                view,
                scan,
                key: values,
                columns: decode_columns,
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
            slots: scan_slots(scan, decode_columns)?,
            outer,
            values_batch: Vec::new(),
        }),
        NodeKind::IndexRangeScan {
            scan,
            index,
            equality_prefix,
            range,
            descending_prefix,
            descending_limit,
            decode_columns,
            ..
        } => {
            let equality_prefix = equality_prefix
                .iter()
                .map(|constant| resolve_constant(constant, &outer))
                .collect::<Result<Vec<_>>>()?;
            if equality_prefix.iter().any(Value::is_null) {
                return Ok(finish_operator(
                    operator_tally,
                    operator_span,
                    open_started,
                    Box::new(Empty),
                ));
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
                iterator: row_store::scan_index_range_observed(
                    view,
                    scan.scan_table(),
                    index,
                    &equality_prefix,
                    range,
                    *descending_prefix,
                    *descending_limit,
                    decode_columns,
                    operator_tally
                        .as_ref()
                        .and_then(|tally| tally.index_reads.clone()),
                )
                .await?,
                slots: scan_slots(scan, decode_columns)?,
                outer,
                values_batch: Vec::new(),
            })
        }
        NodeKind::Rows(relation) => Box::new(Rows {
            relation,
            outer,
            position: 0,
        }),
        NodeKind::RecursiveReference {
            binding,
            output,
            canonical,
        } => {
            // The recursive driver cannot replace the frontier while this
            // pipeline borrows it. Remap one row at a time so an iteration
            // does not allocate and retain a second frontier.
            Box::new(RecursiveRows {
                rows: execution_grant
                    .frontier
                    .get(binding)
                    .map(|rows| rows.iter()),
                output,
                canonical,
                outer,
            })
        }
        NodeKind::Filter { input, predicate } => Box::new(Filter {
            input: build(
                view,
                input,
                outer,
                tallies,
                join_tallies,
                measure,
                operator_tallies,
                next_operator_id,
                child_parent_id,
                measure_operators,
                child_parent_span,
                execution_grant.clone(),
            )
            .await?,
            predicate,
        }),
        NodeKind::Project { input, fields } => Box::new(Project {
            input: build(
                view,
                input,
                outer.clone(),
                tallies,
                join_tallies,
                measure,
                operator_tallies,
                next_operator_id,
                child_parent_id,
                measure_operators,
                child_parent_span,
                execution_grant.clone(),
            )
            .await?,
            fields,
            outer,
        }),
        NodeKind::Sort { input, terms } => {
            let mut input = build(
                view,
                input,
                outer,
                tallies,
                join_tallies,
                measure,
                operator_tallies,
                next_operator_id,
                child_parent_id,
                measure_operators,
                child_parent_span,
                execution_grant.clone(),
            )
            .await?;
            input.enable_bounded_read_ahead();
            Box::new(Sort {
                input: Some(input),
                terms,
                frames: Vec::new(),
                position: 0,
            })
        }
        NodeKind::Slice {
            input,
            offset,
            limit,
        } => Box::new(Slice {
            input: build(
                view,
                input,
                outer,
                tallies,
                join_tallies,
                measure,
                operator_tallies,
                next_operator_id,
                child_parent_id,
                measure_operators,
                child_parent_span,
                execution_grant.clone(),
            )
            .await?,
            remaining_offset: *offset,
            remaining: *limit,
        }),
        NodeKind::Distinct { input, output } => Box::new(Distinct {
            input: build(
                view,
                input,
                outer,
                tallies,
                join_tallies,
                measure,
                operator_tallies,
                next_operator_id,
                child_parent_id,
                measure_operators,
                child_parent_span,
                execution_grant.clone(),
            )
            .await?,
            seen: CanonicalRowSet::new(&output.fields),
        }),
        NodeKind::Aggregate {
            input,
            groups,
            terms,
        } => Box::new(Aggregate {
            input: Some(
                build(
                    view,
                    input,
                    outer.clone(),
                    tallies,
                    join_tallies,
                    measure,
                    operator_tallies,
                    next_operator_id,
                    child_parent_id,
                    measure_operators,
                    child_parent_span,
                    execution_grant.clone(),
                )
                .await?,
            ),
            groups,
            terms,
            outer,
            output: std::collections::VecDeque::new(),
            hash_builder: ahash::RandomState::new(),
            groups_are_slots: groups
                .iter()
                .all(|group| matches!(&group.expression, bound::Expr::SlotRef { .. })),
        }),
        NodeKind::GroupedHashJoinAggregate {
            fact,
            dimension,
            keys,
            fact_is_left,
            groups,
            terms,
            memory_limit_bytes,
            ..
        } => {
            let fact_operator = build(
                view,
                fact,
                outer.clone(),
                tallies,
                join_tallies,
                measure,
                operator_tallies,
                next_operator_id,
                child_parent_id,
                measure_operators,
                child_parent_span,
                execution_grant.clone(),
            )
            .await?;
            let cached_cardinality = if measure {
                dimension.attribution.map(|attribution| {
                    let tally = Tally::default();
                    tallies.push((attribution, tally.clone()));
                    tally
                })
            } else {
                None
            };
            let physical_key = match (
                execution_grant.subrelation_cache,
                dimension.materialization.as_ref(),
            ) {
                (Some(context), Some(candidate))
                    if matches!(
                        candidate.representation,
                        MaterializationRepresentation::GroupedHashJoinDimension { .. }
                    ) =>
                {
                    Some((
                        context,
                        context.root_key.for_physical_subrelation(
                            MaterializationDomain::GroupedHashJoinDimension,
                            candidate.exact,
                            &candidate.dependencies,
                            candidate.representation.identity_bytes(),
                        )?,
                    ))
                }
                _ => None,
            };
            let mut dimension_resources = execution_grant.clone();
            dimension_resources.bypass_materialization = true;
            let (dimensions, group_positions) = if let Some((context, key)) = physical_key {
                let materialization = super::RelationCacheMaterialization::GroupedHashJoinDimension;
                let relation = dimension
                    .materialization
                    .as_ref()
                    .expect("grouped dimension materialization is present")
                    .exact;
                context
                    .reach(super::EngineEvent::SubrelationCacheLookupStarted {
                        materialization,
                        relation,
                    })
                    .await;
                let mut result = context
                    .cache
                    .get_or_fill_grouped_dimension(key, || async {
                        context
                            .reach(super::EngineEvent::SubrelationCacheFillStarted {
                                materialization,
                                relation,
                            })
                            .await;
                        let before = context.counters.map(|counters| counters.snapshot());
                        let started = Instant::now();
                        let dimension = build(
                            view,
                            dimension,
                            outer.clone(),
                            tallies,
                            join_tallies,
                            measure,
                            operator_tallies,
                            next_operator_id,
                            child_parent_id,
                            measure_operators,
                            child_parent_span,
                            dimension_resources,
                        )
                        .await?;
                        let value = collect_grouped_dimension(
                            dimension,
                            keys,
                            *fact_is_left,
                            groups,
                            *memory_limit_bytes,
                        )
                        .await?;
                        let kv = context.counters.map_or_else(Default::default, |counters| {
                            counters
                                .snapshot()
                                .delta_since(before.expect("initial KV counters are present"))
                        });
                        context
                            .reach(super::EngineEvent::SubrelationCacheFillReady {
                                materialization,
                                relation,
                            })
                            .await;
                        Ok((
                            value,
                            CachedWork {
                                kv,
                                execution: started.elapsed(),
                            },
                        ))
                    })
                    .await;
                if let Ok(result) = &mut result {
                    context.reach_all(result.take_events()).await;
                }
                let lookup_result = match &result {
                    Ok(result)
                        if result.source == super::observe::StatementSource::RelationCache =>
                    {
                        super::RelationCacheLookupResult::Reused
                    }
                    Ok(_) => super::RelationCacheLookupResult::Filled,
                    Err(_) => super::RelationCacheLookupResult::Failed,
                };
                context
                    .reach(super::EngineEvent::SubrelationCacheLookupCompleted {
                        materialization,
                        relation,
                        result: lookup_result,
                    })
                    .await;
                let result = result?;
                let attach_started = Instant::now();
                let group_positions = GroupPositions::new(result.value.value().row_count);
                if result.source == super::observe::StatementSource::RelationCache {
                    if let Some(cardinality) = &cached_cardinality {
                        cardinality.rows.store(
                            result.value.value().input_row_count as u64,
                            atomic::Ordering::Relaxed,
                        );
                        cardinality.exhausted.store(true, atomic::Ordering::Relaxed);
                    }
                    crate::telemetry::relation_cache_materialization_attach(
                        MaterializationDomain::GroupedHashJoinDimension.as_str(),
                        attach_started.elapsed(),
                    );
                }
                (Some(result.value), Some(group_positions))
            } else {
                (None, None)
            };
            let dimension = if dimensions.is_none() {
                Some(
                    build(
                        view,
                        dimension,
                        outer.clone(),
                        tallies,
                        join_tallies,
                        measure,
                        operator_tallies,
                        next_operator_id,
                        child_parent_id,
                        measure_operators,
                        child_parent_span,
                        execution_grant.clone(),
                    )
                    .await?,
                )
            } else {
                None
            };
            Box::new(GroupedHashJoinAggregate {
                fact: Some(fact_operator),
                dimension,
                dimensions,
                group_positions,
                keys,
                fact_is_left: *fact_is_left,
                groups,
                terms,
                outer,
                output: std::collections::VecDeque::new(),
                memory_limit_bytes: *memory_limit_bytes,
            })
        }
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
                left: build(
                    view,
                    left,
                    outer.clone(),
                    tallies,
                    join_tallies,
                    measure,
                    operator_tallies,
                    next_operator_id,
                    child_parent_id,
                    measure_operators,
                    child_parent_span,
                    execution_grant.clone(),
                )
                .await?,
                right: Some(
                    build(
                        view,
                        right,
                        outer,
                        tallies,
                        join_tallies,
                        measure,
                        operator_tallies,
                        next_operator_id,
                        child_parent_id,
                        measure_operators,
                        child_parent_span,
                        execution_grant.clone(),
                    )
                    .await?,
                ),
                kind: *kind,
                predicate: on,
                keys,
                right_output,
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
            let left_operator = build(
                view,
                left,
                outer.clone(),
                tallies,
                join_tallies,
                measure,
                operator_tallies,
                next_operator_id,
                child_parent_id,
                measure_operators,
                child_parent_span,
                execution_grant.clone(),
            )
            .await?;
            let cached_cardinality = if measure {
                right.attribution.map(|attribution| {
                    let tally = Tally::default();
                    tallies.push((attribution, tally.clone()));
                    tally
                })
            } else {
                None
            };
            let physical_key = match (
                execution_grant.subrelation_cache,
                right.materialization.as_ref(),
            ) {
                (Some(context), Some(candidate))
                    if matches!(
                        candidate.representation,
                        MaterializationRepresentation::HashJoinBuild { .. }
                    ) =>
                {
                    Some((
                        context,
                        context.root_key.for_physical_subrelation(
                            MaterializationDomain::HashJoinBuild,
                            candidate.exact,
                            &candidate.dependencies,
                            candidate.representation.identity_bytes(),
                        )?,
                    ))
                }
                _ => None,
            };
            let mut right_resources = execution_grant.clone();
            right_resources.bypass_materialization = true;
            let cached_build = if let Some((context, key)) = physical_key {
                let materialization = super::RelationCacheMaterialization::HashJoinBuild;
                let relation = right
                    .materialization
                    .as_ref()
                    .expect("hash-build materialization is present")
                    .exact;
                context
                    .reach(super::EngineEvent::SubrelationCacheLookupStarted {
                        materialization,
                        relation,
                    })
                    .await;
                let mut result = context
                    .cache
                    .get_or_fill_hash_build(key, || async {
                        context
                            .reach(super::EngineEvent::SubrelationCacheFillStarted {
                                materialization,
                                relation,
                            })
                            .await;
                        let before = context.counters.map(|counters| counters.snapshot());
                        let started = Instant::now();
                        let right = build(
                            view,
                            right,
                            outer.clone(),
                            tallies,
                            join_tallies,
                            measure,
                            operator_tallies,
                            next_operator_id,
                            child_parent_id,
                            measure_operators,
                            child_parent_span,
                            right_resources,
                        )
                        .await?;
                        let value = collect_hash_build(
                            right,
                            keys,
                            right_output,
                            *memory_limit_bytes,
                            &tally,
                        )
                        .await?;
                        let kv = context.counters.map_or_else(Default::default, |counters| {
                            counters
                                .snapshot()
                                .delta_since(before.expect("initial KV counters are present"))
                        });
                        context
                            .reach(super::EngineEvent::SubrelationCacheFillReady {
                                materialization,
                                relation,
                            })
                            .await;
                        Ok((
                            value,
                            CachedWork {
                                kv,
                                execution: started.elapsed(),
                            },
                        ))
                    })
                    .await;
                if let Ok(result) = &mut result {
                    context.reach_all(result.take_events()).await;
                }
                let lookup_result = match &result {
                    Ok(result)
                        if result.source == super::observe::StatementSource::RelationCache =>
                    {
                        super::RelationCacheLookupResult::Reused
                    }
                    Ok(_) => super::RelationCacheLookupResult::Filled,
                    Err(_) => super::RelationCacheLookupResult::Failed,
                };
                context
                    .reach(super::EngineEvent::SubrelationCacheLookupCompleted {
                        materialization,
                        relation,
                        result: lookup_result,
                    })
                    .await;
                let result = result?;
                let attach_started = Instant::now();
                let bound_build = Arc::new(BoundHashBuild::new(result.value, right_output));
                if result.source == super::observe::StatementSource::RelationCache {
                    let value = bound_build.value.value();
                    tally.add_build_rows(value.input_row_count);
                    tally.retain(value.execution_retained_bytes);
                    if let Some(cardinality) = &cached_cardinality {
                        cardinality
                            .rows
                            .store(value.input_row_count as u64, atomic::Ordering::Relaxed);
                        cardinality.exhausted.store(true, atomic::Ordering::Relaxed);
                    }
                    crate::telemetry::relation_cache_materialization_attach(
                        MaterializationDomain::HashJoinBuild.as_str(),
                        attach_started.elapsed(),
                    );
                }
                Some(bound_build)
            } else {
                None
            };
            let right = if cached_build.is_none() {
                Some(
                    build(
                        view,
                        right,
                        outer,
                        tallies,
                        join_tallies,
                        measure,
                        operator_tallies,
                        next_operator_id,
                        child_parent_id,
                        measure_operators,
                        child_parent_span,
                        execution_grant.clone(),
                    )
                    .await?,
                )
            } else {
                None
            };
            Box::new(HashJoin {
                left: left_operator,
                right,
                kind: *kind,
                residual_predicate: (!join_predicate_is_keys(on, keys)).then_some(on),
                keys,
                right_output,
                memory_limit_bytes: *memory_limit_bytes,
                build: cached_build,
                current_left: None,
                current_hash: 0,
                current_entry: 0,
                current_row: 0,
                matched: false,
                tally,
                execution_grant: execution_grant
                    .execution_grant
                    .unwrap_or_else(ExecutionGrant::serial),
                parallel_output: std::collections::VecDeque::new(),
                left_exhausted: false,
                parallel_batch_sequence: 0,
                serial_probe_rows_until_retry: 0,
                parallel_plan: None,
            })
        }
        NodeKind::Concatenate {
            inputs,
            input_outputs,
            output,
        } => {
            let mut operators = Vec::with_capacity(inputs.len());
            for input in inputs {
                operators.push(
                    build(
                        view,
                        input,
                        outer.clone(),
                        tallies,
                        join_tallies,
                        measure,
                        operator_tallies,
                        next_operator_id,
                        child_parent_id,
                        measure_operators,
                        child_parent_span,
                        execution_grant.clone(),
                    )
                    .await?,
                );
            }
            Box::new(Concatenate {
                inputs: operators,
                input_outputs,
                output,
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
            build(
                view,
                left,
                outer.clone(),
                tallies,
                join_tallies,
                measure,
                operator_tallies,
                next_operator_id,
                child_parent_id,
                measure_operators,
                child_parent_span,
                execution_grant.clone(),
            )
            .await?,
            build(
                view,
                right,
                outer.clone(),
                tallies,
                join_tallies,
                measure,
                operator_tallies,
                next_operator_id,
                child_parent_id,
                measure_operators,
                child_parent_span,
                execution_grant.clone(),
            )
            .await?,
            *quantifier,
            false,
            left_output,
            right_output,
            output,
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
            build(
                view,
                left,
                outer.clone(),
                tallies,
                join_tallies,
                measure,
                operator_tallies,
                next_operator_id,
                child_parent_id,
                measure_operators,
                child_parent_span,
                execution_grant.clone(),
            )
            .await?,
            build(
                view,
                right,
                outer.clone(),
                tallies,
                join_tallies,
                measure,
                operator_tallies,
                next_operator_id,
                child_parent_id,
                measure_operators,
                child_parent_span,
                execution_grant.clone(),
            )
            .await?,
            *quantifier,
            true,
            left_output,
            right_output,
            output,
            outer,
        )),
        _ => {
            return Err(Error::message(
                ErrorKind::Internal,
                "exec: unsupported node entered the pull pipeline",
            ));
        }
    };
    if let (Some(tally), Some(started)) = (&operator_tally, open_started) {
        tally
            .open_nanos
            .store(duration_nanos(started.elapsed()), atomic::Ordering::Relaxed);
    };
    let operator: Box<dyn Operator + 'a> =
        if let Some(attribution) = node.attribution.filter(|_| measure) {
            let tally = Tally::default();
            tallies.push((attribution, tally.clone()));
            Box::new(Counting {
                inner: operator,
                tally,
            })
        } else {
            operator
        };
    Ok(match (operator_tally, operator_span) {
        (Some(tally), Some(runtime_span)) => Box::new(MeasuredOperator {
            inner: operator,
            tally,
            runtime_span,
            failed: false,
        }),
        (None, None) => operator,
        _ => unreachable!("operator tally and span are created together"),
    })
}

#[allow(clippy::too_many_arguments)]
async fn build_materialized<'a>(
    view: &'a dyn KvView,
    node: &'a Node,
    outer: Env,
    tallies: &mut Vec<(Fingerprint, Tally)>,
    join_tallies: &mut Vec<JoinTally>,
    measure: bool,
    operator_tallies: &mut Vec<OperatorTally>,
    next_operator_id: &mut u32,
    parent_operator_id: Option<u32>,
    measure_operators: bool,
    parent_operator_span: Option<&tracing::Span>,
    resources: BuildResources<'a>,
) -> Result<Box<dyn Operator + 'a>> {
    let candidate = node
        .materialization
        .as_ref()
        .expect("materialization candidate is present");
    let context = resources
        .subrelation_cache
        .expect("subrelation cache context is present");
    // The root key and this candidate come from one physical plan and one
    // pinned view. Selecting a subset of the root vector keeps this lookup on
    // that view without a second catalog or data-generation read.
    let key = context
        .root_key
        .for_subrelation(candidate.exact, &candidate.dependencies)?;
    let mut fill_resources = resources.clone();
    fill_resources.bypass_materialization = true;
    let lookup_started = measure_operators.then(Instant::now);
    let materialization = super::RelationCacheMaterialization::Rows;
    let relation = candidate.exact;
    context
        .reach(super::EngineEvent::SubrelationCacheLookupStarted {
            materialization,
            relation,
        })
        .await;
    let mut result = context
        .cache
        .get_or_fill(key, &candidate.output, || async {
            context
                .reach(super::EngineEvent::SubrelationCacheFillStarted {
                    materialization,
                    relation,
                })
                .await;
            let before = context.counters.map(|counters| counters.snapshot());
            let started = Instant::now();
            let mut operator = build(
                view,
                node,
                outer,
                tallies,
                join_tallies,
                measure,
                operator_tallies,
                next_operator_id,
                parent_operator_id,
                measure_operators,
                parent_operator_span,
                fill_resources,
            )
            .await?;
            let mut frames = Vec::new();
            while let Some(frame) = operator.next().await? {
                frames.push(frame);
            }
            drop(operator);
            let kv = context.counters.map_or_else(Default::default, |counters| {
                counters
                    .snapshot()
                    .delta_since(before.expect("initial KV counters are present"))
            });
            context
                .reach(super::EngineEvent::SubrelationCacheFillReady {
                    materialization,
                    relation,
                })
                .await;
            Ok((
                frames,
                CachedWork {
                    kv,
                    execution: started.elapsed(),
                },
            ))
        })
        .await;
    if let Ok(result) = &mut result {
        context.reach_all(result.take_events()).await;
    }
    let lookup_result = match &result {
        Ok(result) if result.source == super::observe::StatementSource::RelationCache => {
            super::RelationCacheLookupResult::Reused
        }
        Ok(_) => super::RelationCacheLookupResult::Filled,
        Err(_) => super::RelationCacheLookupResult::Failed,
    };
    context
        .reach(super::EngineEvent::SubrelationCacheLookupCompleted {
            materialization,
            relation,
            result: lookup_result,
        })
        .await;
    let result = result?;
    let source = result.source;
    let restore_started =
        (source == super::observe::StatementSource::RelationCache).then(Instant::now);
    let frames = result.into_frames(&candidate.output);
    if let Some(restore_started) = restore_started {
        crate::telemetry::relation_cache_materialization_restore(
            MaterializationDomain::SubrelationRowsV1.as_str(),
            restore_started.elapsed(),
        );
    }
    let mut operator: Box<dyn Operator + 'a> = Box::new(MaterializedRows {
        rows: frames.into_iter(),
    });
    if source == super::observe::StatementSource::RelationCache {
        if measure && let Some(attribution) = node.attribution {
            let tally = Tally::default();
            tallies.push((attribution, tally.clone()));
            operator = Box::new(Counting {
                inner: operator,
                tally,
            });
        }
        if measure_operators {
            let operator_id = *next_operator_id;
            *next_operator_id = next_operator_id.saturating_add(1);
            let tally = OperatorTally::new(
                operator_id,
                parent_operator_id,
                "CachedRelation",
                node.attribution,
                false,
            );
            tally.open_nanos.store(
                duration_nanos(
                    lookup_started
                        .expect("measured materialization lookup has a timer")
                        .elapsed(),
                ),
                atomic::Ordering::Relaxed,
            );
            let runtime_span = super::observe::OperatorRuntimeSpan::new(
                operator_id,
                parent_operator_id,
                "CachedRelation",
                node.attribution,
                parent_operator_span,
            );
            operator_tallies.push(tally.clone());
            operator = finish_operator(Some(tally), Some(runtime_span), None, operator);
        }
    }
    Ok(operator)
}

fn finish_operator<'a>(
    tally: Option<OperatorTally>,
    runtime_span: Option<super::observe::OperatorRuntimeSpan>,
    started: Option<Instant>,
    operator: Box<dyn Operator + 'a>,
) -> Box<dyn Operator + 'a> {
    let Some(tally) = tally else {
        return operator;
    };
    let runtime_span = runtime_span.expect("operator tally and span are created together");
    if let Some(started) = started {
        tally
            .open_nanos
            .store(duration_nanos(started.elapsed()), atomic::Ordering::Relaxed);
    }
    Box::new(MeasuredOperator {
        inner: operator,
        tally,
        runtime_span,
        failed: false,
    })
}

fn duration_nanos(duration: std::time::Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

/// Rows a node emitted, and whether it ran out of rows or was abandoned once
/// a downstream operator had enough.
#[derive(Clone, Default)]
struct Tally {
    rows: Arc<atomic::AtomicU64>,
    exhausted: Arc<atomic::AtomicBool>,
}

#[derive(Clone)]
struct OperatorTally {
    operator_id: u32,
    parent_operator_id: Option<u32>,
    operator: &'static str,
    relation_fingerprint: Option<Fingerprint>,
    open_nanos: Arc<atomic::AtomicU64>,
    next_nanos: Arc<atomic::AtomicU64>,
    calls: Arc<atomic::AtomicU64>,
    rows: Arc<atomic::AtomicU64>,
    complete: Arc<atomic::AtomicBool>,
    index_reads: Option<row_store::IndexReadTally>,
}

impl OperatorTally {
    fn new(
        operator_id: u32,
        parent_operator_id: Option<u32>,
        operator: &'static str,
        relation_fingerprint: Option<Fingerprint>,
        measure_index_reads: bool,
    ) -> Self {
        Self {
            operator_id,
            parent_operator_id,
            operator,
            relation_fingerprint,
            open_nanos: Arc::new(atomic::AtomicU64::new(0)),
            next_nanos: Arc::new(atomic::AtomicU64::new(0)),
            calls: Arc::new(atomic::AtomicU64::new(0)),
            rows: Arc::new(atomic::AtomicU64::new(0)),
            complete: Arc::new(atomic::AtomicBool::new(false)),
            index_reads: measure_index_reads.then(row_store::IndexReadTally::default),
        }
    }

    fn snapshot(&self) -> super::observe::OperatorMeasurement {
        let open_nanos = self.open_nanos.load(atomic::Ordering::Relaxed);
        let next_nanos = self.next_nanos.load(atomic::Ordering::Relaxed);
        let inclusive_micros = open_nanos.saturating_add(next_nanos) / 1_000;
        super::observe::OperatorMeasurement {
            operator_id: self.operator_id,
            parent_operator_id: self.parent_operator_id,
            operator: self.operator,
            relation_fingerprint: self.relation_fingerprint,
            open_micros: open_nanos / 1_000,
            inclusive_micros,
            exclusive_micros: inclusive_micros,
            calls: self.calls.load(atomic::Ordering::Relaxed),
            input_rows: 0,
            output_rows: self.rows.load(atomic::Ordering::Relaxed),
            input_complete: true,
            complete: self.complete.load(atomic::Ordering::Relaxed),
            index_entries_visited: self
                .index_reads
                .as_ref()
                .map_or(0, row_store::IndexReadTally::entries),
            index_base_row_reads: self
                .index_reads
                .as_ref()
                .map_or(0, row_store::IndexReadTally::reads),
            index_peak_base_row_read_concurrency: self
                .index_reads
                .as_ref()
                .map_or(0, row_store::IndexReadTally::peak_active),
        }
    }
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

    fn add_build_rows(&self, rows: usize) {
        if self.enabled {
            self.build_rows
                .fetch_add(rows as u64, atomic::Ordering::Relaxed);
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

struct MeasuredOperator<'a> {
    inner: Box<dyn Operator + 'a>,
    tally: OperatorTally,
    runtime_span: super::observe::OperatorRuntimeSpan,
    failed: bool,
}

#[async_trait]
impl Operator for MeasuredOperator<'_> {
    async fn next(&mut self) -> Result<Option<Env>> {
        let started = Instant::now();
        #[cfg(debug_assertions)]
        let result = self
            .inner
            .next()
            .instrument(self.runtime_span.span().clone())
            .await;
        #[cfg(not(debug_assertions))]
        let result = self.inner.next().await;
        self.tally
            .next_nanos
            .fetch_add(duration_nanos(started.elapsed()), atomic::Ordering::Relaxed);
        self.tally.calls.fetch_add(1, atomic::Ordering::Relaxed);
        match &result {
            Ok(Some(_)) => {
                self.tally.rows.fetch_add(1, atomic::Ordering::Relaxed);
            }
            Ok(None) => self.tally.complete.store(true, atomic::Ordering::Relaxed),
            Err(_) => self.failed = true,
        }
        result
    }

    async fn next_batch(&mut self, limit: usize, output: &mut Vec<Env>) -> Result<()> {
        let initial = output.len();
        let started = Instant::now();
        #[cfg(debug_assertions)]
        let result = self
            .inner
            .next_batch(limit, output)
            .instrument(self.runtime_span.span().clone())
            .await;
        #[cfg(not(debug_assertions))]
        let result = self.inner.next_batch(limit, output).await;
        let rows = output.len().saturating_sub(initial) as u64;
        self.tally
            .next_nanos
            .fetch_add(duration_nanos(started.elapsed()), atomic::Ordering::Relaxed);
        self.tally.calls.fetch_add(
            rows.saturating_add(u64::from(rows < limit as u64)),
            atomic::Ordering::Relaxed,
        );
        self.tally.rows.fetch_add(rows, atomic::Ordering::Relaxed);
        match &result {
            Ok(()) if rows < limit as u64 => {
                self.tally.complete.store(true, atomic::Ordering::Relaxed);
            }
            Ok(()) => {}
            Err(_) => self.failed = true,
        }
        result
    }

    fn enable_bounded_read_ahead(&mut self) {
        self.inner.enable_bounded_read_ahead();
    }

    fn decoded_slots(&self) -> Option<&[SlotId]> {
        self.inner.decoded_slots()
    }

    async fn next_decoded_batch(
        &mut self,
        limit: usize,
        output: &mut Vec<super::codec::DecodedRow>,
    ) -> Result<()> {
        let initial = output.len();
        let started = Instant::now();
        #[cfg(debug_assertions)]
        let result = self
            .inner
            .next_decoded_batch(limit, output)
            .instrument(self.runtime_span.span().clone())
            .await;
        #[cfg(not(debug_assertions))]
        let result = self.inner.next_decoded_batch(limit, output).await;
        let rows = output.len().saturating_sub(initial) as u64;
        self.tally
            .next_nanos
            .fetch_add(duration_nanos(started.elapsed()), atomic::Ordering::Relaxed);
        self.tally.calls.fetch_add(
            rows.saturating_add(u64::from(rows < limit as u64)),
            atomic::Ordering::Relaxed,
        );
        self.tally.rows.fetch_add(rows, atomic::Ordering::Relaxed);
        match &result {
            Ok(()) if rows < limit as u64 => {
                self.tally.complete.store(true, atomic::Ordering::Relaxed);
            }
            Ok(()) => {}
            Err(_) => self.failed = true,
        }
        result
    }

    fn raw_decoder(&self) -> Option<super::codec::RowDecoder> {
        self.inner.raw_decoder()
    }

    async fn next_raw_batch(&mut self, limit: usize, output: &mut Vec<bytes::Bytes>) -> Result<()> {
        let initial = output.len();
        let started = Instant::now();
        #[cfg(debug_assertions)]
        let result = self
            .inner
            .next_raw_batch(limit, output)
            .instrument(self.runtime_span.span().clone())
            .await;
        #[cfg(not(debug_assertions))]
        let result = self.inner.next_raw_batch(limit, output).await;
        let rows = output.len().saturating_sub(initial) as u64;
        self.tally
            .next_nanos
            .fetch_add(duration_nanos(started.elapsed()), atomic::Ordering::Relaxed);
        self.tally.calls.fetch_add(
            rows.saturating_add(u64::from(rows < limit as u64)),
            atomic::Ordering::Relaxed,
        );
        self.tally.rows.fetch_add(rows, atomic::Ordering::Relaxed);
        match &result {
            Ok(()) if rows < limit as u64 => {
                self.tally.complete.store(true, atomic::Ordering::Relaxed);
            }
            Ok(()) => {}
            Err(_) => self.failed = true,
        }
        result
    }
}

impl Drop for MeasuredOperator<'_> {
    fn drop(&mut self) {
        self.runtime_span
            .record(&self.tally.snapshot(), self.failed);
    }
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

    async fn next_batch(&mut self, limit: usize, output: &mut Vec<Env>) -> Result<()> {
        let initial = output.len();
        let result = self.inner.next_batch(limit, output).await;
        let rows = output.len().saturating_sub(initial) as u64;
        self.tally.rows.fetch_add(rows, atomic::Ordering::Relaxed);
        if result.is_ok() && rows < limit as u64 {
            self.tally.exhausted.store(true, atomic::Ordering::Relaxed);
        }
        result
    }

    fn enable_bounded_read_ahead(&mut self) {
        self.inner.enable_bounded_read_ahead();
    }

    fn decoded_slots(&self) -> Option<&[SlotId]> {
        self.inner.decoded_slots()
    }

    async fn next_decoded_batch(
        &mut self,
        limit: usize,
        output: &mut Vec<super::codec::DecodedRow>,
    ) -> Result<()> {
        let initial = output.len();
        let result = self.inner.next_decoded_batch(limit, output).await;
        let rows = output.len().saturating_sub(initial) as u64;
        self.tally.rows.fetch_add(rows, atomic::Ordering::Relaxed);
        if result.is_ok() && rows < limit as u64 {
            self.tally.exhausted.store(true, atomic::Ordering::Relaxed);
        }
        result
    }

    fn raw_decoder(&self) -> Option<super::codec::RowDecoder> {
        self.inner.raw_decoder()
    }

    async fn next_raw_batch(&mut self, limit: usize, output: &mut Vec<bytes::Bytes>) -> Result<()> {
        let initial = output.len();
        let result = self.inner.next_raw_batch(limit, output).await;
        let rows = output.len().saturating_sub(initial) as u64;
        self.tally.rows.fetch_add(rows, atomic::Ordering::Relaxed);
        if result.is_ok() && rows < limit as u64 {
            self.tally.exhausted.store(true, atomic::Ordering::Relaxed);
        }
        result
    }
}

struct Empty;

struct MaterializedRows {
    rows: std::vec::IntoIter<Env>,
}

struct RecursiveRows<'a> {
    rows: Option<std::slice::Iter<'a, Env>>,
    output: &'a RowType,
    canonical: &'a [SlotId],
    outer: Env,
}

#[async_trait]
impl Operator for MaterializedRows {
    async fn next(&mut self) -> Result<Option<Env>> {
        Ok(self.rows.next())
    }

    async fn next_batch(&mut self, limit: usize, output: &mut Vec<Env>) -> Result<()> {
        output.extend(self.rows.by_ref().take(limit));
        Ok(())
    }
}

#[async_trait]
impl Operator for RecursiveRows<'_> {
    async fn next(&mut self) -> Result<Option<Env>> {
        Ok(self
            .rows
            .as_mut()
            .and_then(Iterator::next)
            .map(|frame| remap_canonical(self.output, self.canonical, frame, &self.outer)))
    }
}

#[async_trait]
impl Operator for Empty {
    async fn next(&mut self) -> Result<Option<Env>> {
        Ok(None)
    }
}

struct PrimaryKeyGet<'a> {
    view: &'a dyn KvView,
    scan: &'a bound::Relation,
    key: crate::engine::lir::Row,
    columns: &'a [crate::engine::catalog::model::Column],
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
            row_store::get_columns(self.view, self.scan.scan_table(), &self.key, self.columns)
                .await?
                .map(|row| row_to_frame(self.scan, &row, &self.outer)),
        )
    }
}

struct RowScan<'a> {
    iterator: Box<dyn RowIterator + 'a>,
    slots: smallvec::SmallVec<[SlotId; 8]>,
    outer: Env,
    values_batch: Vec<super::codec::DecodedRow>,
}

#[async_trait]
impl Operator for RowScan<'_> {
    async fn next(&mut self) -> Result<Option<Env>> {
        Ok(self
            .iterator
            .next()
            .await?
            .map(|values| column_values_to_frame(&self.slots, values, &self.outer)))
    }

    async fn next_batch(&mut self, limit: usize, output: &mut Vec<Env>) -> Result<()> {
        self.values_batch.clear();
        self.iterator
            .next_batch(limit, &mut self.values_batch)
            .await?;
        output.extend(
            self.values_batch
                .drain(..)
                .map(|values| column_values_to_frame(&self.slots, values, &self.outer)),
        );
        Ok(())
    }

    fn enable_bounded_read_ahead(&mut self) {
        self.iterator.enable_bounded_read_ahead();
    }

    fn decoded_slots(&self) -> Option<&[SlotId]> {
        Some(&self.slots)
    }

    async fn next_decoded_batch(
        &mut self,
        limit: usize,
        output: &mut Vec<super::codec::DecodedRow>,
    ) -> Result<()> {
        self.iterator.next_batch(limit, output).await
    }

    fn raw_decoder(&self) -> Option<super::codec::RowDecoder> {
        self.iterator.raw_decoder()
    }

    async fn next_raw_batch(&mut self, limit: usize, output: &mut Vec<bytes::Bytes>) -> Result<()> {
        self.iterator.next_raw_batch(limit, output).await
    }
}

struct Rows<'a> {
    relation: &'a bound::Relation,
    outer: Env,
    position: usize,
}

#[async_trait]
impl Operator for Rows<'_> {
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
    predicate: &'a bound::Expr,
}

#[async_trait]
impl Operator for Filter<'_> {
    async fn next(&mut self) -> Result<Option<Env>> {
        while let Some(frame) = self.input.next().await? {
            if evaluate_predicate(self.predicate, &frame)? == TriBool::True {
                return Ok(Some(frame));
            }
        }
        Ok(None)
    }

    fn enable_bounded_read_ahead(&mut self) {
        self.input.enable_bounded_read_ahead();
    }
}

struct Project<'a> {
    input: Box<dyn Operator + 'a>,
    fields: &'a [PhysicalField],
    outer: Env,
}

#[async_trait]
impl Operator for Project<'_> {
    async fn next(&mut self) -> Result<Option<Env>> {
        let Some(input) = self.input.next().await? else {
            return Ok(None);
        };
        let mut output = new_frame(&self.outer);
        for field in self.fields {
            output.insert(field.slot, evaluate_datum(&field.expression, &input)?);
        }
        Ok(Some(output))
    }

    fn enable_bounded_read_ahead(&mut self) {
        self.input.enable_bounded_read_ahead();
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
    terms: &'a [bound::BoundOrderTerm],
    frames: Vec<Env>,
    position: usize,
}

#[async_trait]
impl Operator for Sort<'_> {
    async fn next(&mut self) -> Result<Option<Env>> {
        if let Some(mut input) = self.input.take() {
            #[cfg(debug_assertions)]
            {
                let span = tracing::debug_span!(
                    target: "rad::telemetry",
                    "rad.debug.sort.collect",
                    otel.name = "rad.debug.sort.collect",
                    otel.kind = "internal",
                    rad.debug.sort.rows = tracing::field::Empty,
                    rad.status = tracing::field::Empty,
                    otel.status_code = tracing::field::Empty,
                );
                let result = collect_sort_input(&mut *input, &mut self.frames)
                    .instrument(span.clone())
                    .await;
                span.record("rad.debug.sort.rows", self.frames.len() as u64);
                span.record(
                    "rad.status",
                    if result.is_ok() { "success" } else { "error" },
                );
                if result.is_err() {
                    span.record("otel.status_code", "ERROR");
                }
                result?;
            }
            #[cfg(not(debug_assertions))]
            collect_sort_input(&mut *input, &mut self.frames).await?;
            #[cfg(debug_assertions)]
            let sort_span = tracing::debug_span!(
                target: "rad::telemetry",
                "rad.debug.sort.compare",
                otel.name = "rad.debug.sort.compare",
                otel.kind = "internal",
                rad.debug.sort.rows = self.frames.len() as u64,
                rad.debug.sort.terms = self.terms.len() as u64,
                rad.status = tracing::field::Empty,
                otel.status_code = tracing::field::Empty,
            );
            #[cfg(debug_assertions)]
            let _entered = sort_span.enter();
            sort_frames(&mut self.frames, self.terms)?;
            #[cfg(debug_assertions)]
            sort_span.record("rad.status", "success");
        }
        let frame = self.frames.get(self.position).cloned();
        self.position += usize::from(frame.is_some());
        Ok(frame)
    }
}

async fn collect_sort_input(input: &mut dyn Operator, frames: &mut Vec<Env>) -> Result<()> {
    while let Some(frame) = input.next().await? {
        frames.push(frame);
    }
    Ok(())
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

    fn enable_bounded_read_ahead(&mut self) {
        self.input.enable_bounded_read_ahead();
    }
}

#[derive(Default)]
struct AggregateGroup {
    values: Vec<Value>,
    accumulators: Vec<super::query::Accumulator>,
}

struct Aggregate<'a> {
    input: Option<Box<dyn Operator + 'a>>,
    groups: &'a [bound::BoundGroupTerm],
    terms: &'a [bound::BoundAggregateTerm],
    outer: Env,
    output: std::collections::VecDeque<Env>,
    hash_builder: ahash::RandomState,
    groups_are_slots: bool,
}

#[async_trait]
impl Operator for Aggregate<'_> {
    async fn next(&mut self) -> Result<Option<Env>> {
        if let Some(mut input) = self.input.take() {
            let mut by_hash = IdentityHashMap::<Vec<AggregateGroup>>::default();
            let mut order = Vec::new();
            let mut input_batch = Vec::with_capacity(1_024);
            loop {
                input_batch.clear();
                input.next_batch(1_024, &mut input_batch).await?;
                if input_batch.is_empty() {
                    break;
                }
                for frame in input_batch.drain(..) {
                    accumulate_aggregate_frame(
                        &frame,
                        self.groups,
                        self.terms,
                        self.groups_are_slots,
                        &self.hash_builder,
                        &mut by_hash,
                        &mut order,
                    )?;
                }
            }
            if self.groups.is_empty() && order.is_empty() {
                order.push((0, 0));
                by_hash.insert(
                    0,
                    vec![AggregateGroup {
                        values: Vec::new(),
                        accumulators: (0..self.terms.len())
                            .map(|_| super::query::Accumulator::default())
                            .collect(),
                    }],
                );
            }
            self.output.reserve(order.len());
            for (hash, position) in order {
                let group = by_hash
                    .get_mut(&hash)
                    .and_then(|bucket| bucket.get_mut(position))
                    .map(std::mem::take)
                    .expect("ordered group exists");
                let mut frame = new_frame(&self.outer);
                for (term, value) in self.groups.iter().zip(group.values) {
                    frame.set_scalar(term.slot, value);
                }
                for (term, accumulator) in self.terms.iter().zip(group.accumulators) {
                    frame.set_scalar(term.slot, accumulator.finish(term)?);
                }
                self.output.push_back(frame);
            }
        }
        Ok(self.output.pop_front())
    }
}

fn accumulate_aggregate_frame(
    frame: &Env,
    groups: &[bound::BoundGroupTerm],
    terms: &[bound::BoundAggregateTerm],
    groups_are_slots: bool,
    hash_builder: &ahash::RandomState,
    by_hash: &mut IdentityHashMap<Vec<AggregateGroup>>,
    order: &mut Vec<(u64, usize)>,
) -> Result<()> {
    let slot_values = groups_are_slots
        .then(|| aggregate_slot_values(groups, frame))
        .transpose()?;
    let evaluated = (!groups_are_slots)
        .then(|| {
            groups
                .iter()
                .map(|group| evaluate(&group.expression, frame).map_err(Into::into))
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?;
    let hash = aggregate_group_hash(slot_values.as_deref(), evaluated.as_deref(), hash_builder);
    let bucket = by_hash.entry(hash).or_default();
    let position = bucket.iter().position(|group| {
        aggregate_group_matches(&group.values, slot_values.as_deref(), evaluated.as_deref())
    });
    let position = match position {
        Some(position) => position,
        None => {
            let values = match evaluated {
                Some(values) => values,
                None => slot_values
                    .as_ref()
                    .expect("slot aggregate values")
                    .iter()
                    .zip(groups)
                    .map(|(value, group)| {
                        (*value).cloned().unwrap_or_else(|| {
                            let bound::Expr::SlotRef { value_type, .. } = &group.expression else {
                                unreachable!("slot aggregate group")
                            };
                            Value::Null(
                                value_type
                                    .kind
                                    .catalog_type()
                                    .expect("aggregate group slot is scalar"),
                            )
                        })
                    })
                    .collect(),
            };
            bucket.push(AggregateGroup {
                values,
                accumulators: (0..terms.len())
                    .map(|_| super::query::Accumulator::default())
                    .collect(),
            });
            let position = bucket.len() - 1;
            order.push((hash, position));
            position
        }
    };
    let group = &mut bucket[position];
    for (term, accumulator) in terms.iter().zip(&mut group.accumulators) {
        accumulator.observe(term, frame)?;
    }
    Ok(())
}

fn aggregate_group_hash(
    slot_values: Option<&[Option<&Value>]>,
    evaluated: Option<&[Value]>,
    hash_builder: &ahash::RandomState,
) -> u64 {
    if let Some(values) = evaluated {
        hash_scalar_values(
            values
                .iter()
                .map(|value| (!value.is_null()).then_some(value)),
            hash_builder,
        )
    } else {
        hash_scalar_values(
            slot_values.expect("slot aggregate values").iter().copied(),
            hash_builder,
        )
    }
}

fn aggregate_group_matches(
    stored: &[Value],
    slot_values: Option<&[Option<&Value>]>,
    evaluated: Option<&[Value]>,
) -> bool {
    for (index, stored) in stored.iter().enumerate() {
        let candidate = if let Some(values) = evaluated {
            (!values[index].is_null()).then_some(&values[index])
        } else {
            slot_values.expect("slot aggregate values")[index]
        };
        let matches = match candidate {
            None => stored.is_null(),
            Some(_) if stored.is_null() => false,
            Some(candidate) => stored
                .compare(candidate)
                .is_ok_and(|ordering| ordering == std::cmp::Ordering::Equal),
        };
        if !matches {
            return false;
        }
    }
    true
}

fn aggregate_slot_values<'a>(
    groups: &[bound::BoundGroupTerm],
    frame: &'a Env,
) -> Result<smallvec::SmallVec<[Option<&'a Value>; 4]>> {
    groups
        .iter()
        .map(|group| aggregate_slot_value(group, frame))
        .collect()
}

fn aggregate_slot_value<'a>(
    group: &bound::BoundGroupTerm,
    frame: &'a Env,
) -> Result<Option<&'a Value>> {
    let bound::Expr::SlotRef {
        slot,
        name,
        value_type,
    } = &group.expression
    else {
        unreachable!("aggregate slot fast path requires a slot reference")
    };
    frame
        .scalar_ref_at(*slot, name, value_type)
        .map_err(Into::into)
}

enum DecodedAggregateInput {
    CountAll,
    Slot(usize),
    Product {
        left: usize,
        right: usize,
        kind: Kind,
    },
}

struct DecodedFactAccess {
    join: smallvec::SmallVec<[usize; 4]>,
    aggregates: Vec<DecodedAggregateInput>,
}

enum DecodedJoinValues<'a> {
    One(super::codec::DecodedValueRef<'a>),
    Many(smallvec::SmallVec<[super::codec::DecodedValueRef<'a>; 4]>),
}

impl DecodedJoinValues<'_> {
    fn hash(&self, hash_builder: &ahash::RandomState) -> u64 {
        match self {
            Self::One(value) => {
                hash_decoded_scalar_values(std::iter::once(Some(*value)), hash_builder)
            }
            Self::Many(values) => {
                hash_decoded_scalar_values(values.iter().copied().map(Some), hash_builder)
            }
        }
    }

    fn matches(&self, stored: &[Value]) -> Result<bool> {
        match self {
            Self::One(value) => {
                debug_assert_eq!(stored.len(), 1);
                join_decoded_value_equal(&stored[0], *value)
            }
            Self::Many(values) => join_decoded_values_equal(stored, values),
        }
    }
}

impl DecodedFactAccess {
    fn new(
        slots: &[SlotId],
        keys: &[EquiJoinKey],
        fact_is_left: bool,
        terms: &[bound::BoundAggregateTerm],
    ) -> Option<Self> {
        let position = |slot: SlotId| slots.iter().position(|candidate| *candidate == slot);
        let join = keys
            .iter()
            .map(|key| {
                position(if fact_is_left {
                    key.left.slot
                } else {
                    key.right.slot
                })
            })
            .collect::<Option<_>>()?;
        let aggregates = terms
            .iter()
            .map(|term| match term.argument.as_ref() {
                None => Some(DecodedAggregateInput::CountAll),
                Some(bound::Expr::SlotRef { slot, .. }) => {
                    position(*slot).map(DecodedAggregateInput::Slot)
                }
                Some(bound::Expr::Binary {
                    op: BinaryOp::Mul,
                    left,
                    right,
                    value_type,
                }) if value_type.kind.is_numeric() => {
                    let bound::Expr::SlotRef {
                        slot: left_slot, ..
                    } = left.as_ref()
                    else {
                        return None;
                    };
                    let bound::Expr::SlotRef {
                        slot: right_slot, ..
                    } = right.as_ref()
                    else {
                        return None;
                    };
                    Some(DecodedAggregateInput::Product {
                        left: position(*left_slot)?,
                        right: position(*right_slot)?,
                        kind: value_type.kind,
                    })
                }
                _ => None,
            })
            .collect::<Option<_>>()?;
        Some(Self { join, aggregates })
    }

    fn join_values<'a>(
        &self,
        row: &'a super::codec::DecodedRow,
    ) -> Option<smallvec::SmallVec<[&'a Value; 4]>> {
        self.join
            .iter()
            .map(|position| row.get(*position).filter(|value| !value.is_null()))
            .collect()
    }

    fn observe(
        &self,
        row: &super::codec::DecodedRow,
        terms: &[bound::BoundAggregateTerm],
        accumulators: &mut [super::query::Accumulator],
    ) -> Result<()> {
        for ((term, input), accumulator) in terms.iter().zip(&self.aggregates).zip(accumulators) {
            match input {
                DecodedAggregateInput::CountAll => {
                    accumulator.observe_value(term, None)?;
                }
                DecodedAggregateInput::Slot(position) => {
                    let value = row.get(*position).filter(|value| !value.is_null());
                    accumulator.observe_ref(term.function, value)?;
                }
                DecodedAggregateInput::Product { left, right, kind } => {
                    let left = row.get(*left).filter(|value| !value.is_null());
                    let right = row.get(*right).filter(|value| !value.is_null());
                    let value = left
                        .zip(right)
                        .map(|(left, right)| {
                            super::query::checked_numeric_product(left, right, *kind)
                        })
                        .transpose()?;
                    accumulator.observe_ref(term.function, value.as_ref())?;
                }
            }
        }
        Ok(())
    }

    fn join_values_ref<'a>(
        &self,
        row: &super::codec::DecodedRowRef<'a>,
    ) -> Option<DecodedJoinValues<'a>> {
        if let [position] = self.join.as_slice() {
            return row
                .get(*position)
                .copied()
                .filter(|value| !value.is_null())
                .map(DecodedJoinValues::One);
        }
        self.join
            .iter()
            .map(|position| row.get(*position).copied().filter(|value| !value.is_null()))
            .collect::<Option<_>>()
            .map(DecodedJoinValues::Many)
    }

    fn observe_ref(
        &self,
        row: &super::codec::DecodedRowRef<'_>,
        terms: &[bound::BoundAggregateTerm],
        accumulators: &mut [super::query::Accumulator],
    ) -> Result<()> {
        for ((term, input), accumulator) in terms.iter().zip(&self.aggregates).zip(accumulators) {
            match input {
                DecodedAggregateInput::CountAll => {
                    accumulator.observe_value(term, None)?;
                }
                DecodedAggregateInput::Slot(position) => {
                    let value = row
                        .get(*position)
                        .copied()
                        .filter(|value| !value.is_null())
                        .map(super::codec::DecodedValueRef::to_value);
                    accumulator.observe_ref(term.function, value.as_ref())?;
                }
                DecodedAggregateInput::Product { left, right, kind } => {
                    let left = row
                        .get(*left)
                        .copied()
                        .filter(|value| !value.is_null())
                        .map(super::codec::DecodedValueRef::to_value);
                    let right = row
                        .get(*right)
                        .copied()
                        .filter(|value| !value.is_null())
                        .map(super::codec::DecodedValueRef::to_value);
                    let value = left
                        .as_ref()
                        .zip(right.as_ref())
                        .map(|(left, right)| {
                            super::query::checked_numeric_product(left, right, *kind)
                        })
                        .transpose()?;
                    accumulator.observe_ref(term.function, value.as_ref())?;
                }
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct IdentityHasher(u64);

impl Hasher for IdentityHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0 = bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }
}

type IdentityHashMap<V> = HashMap<u64, V, BuildHasherDefault<IdentityHasher>>;

// One page bounds the allocation for a sparse probe. A dense probe allocates
// less than one unused page, while the empty index has 256 times fewer cells.
const MATERIALIZATION_STATE_PAGE_ROWS: usize = 256;

fn materialization_state_page_count(row_count: usize) -> usize {
    row_count.div_ceil(MATERIALIZATION_STATE_PAGE_ROWS)
}

struct GroupPositions {
    pages: Box<[Option<GroupPositionPage>]>,
}

type GroupPositionPage = Box<[Option<usize>; MATERIALIZATION_STATE_PAGE_ROWS]>;

impl GroupPositions {
    fn new(row_count: usize) -> Self {
        let pages = (0..materialization_state_page_count(row_count))
            .map(|_| None)
            .collect();
        Self { pages }
    }

    fn get(&self, index: usize) -> Option<usize> {
        let page = index / MATERIALIZATION_STATE_PAGE_ROWS;
        let offset = index % MATERIALIZATION_STATE_PAGE_ROWS;
        self.pages
            .get(page)
            .and_then(Option::as_deref)
            .and_then(|positions| positions.get(offset))
            .copied()
            .flatten()
    }

    fn set(&mut self, index: usize, position: usize) {
        let page = index / MATERIALIZATION_STATE_PAGE_ROWS;
        let offset = index % MATERIALIZATION_STATE_PAGE_ROWS;
        let positions = self.pages[page]
            .get_or_insert_with(|| Box::new([None; MATERIALIZATION_STATE_PAGE_ROWS]));
        positions[offset] = Some(position);
    }

    #[cfg(test)]
    fn initialized_page_count(&self) -> usize {
        self.pages.iter().filter(|page| page.is_some()).count()
    }
}

struct GroupedHashJoinAggregate<'a> {
    fact: Option<Box<dyn Operator + 'a>>,
    dimension: Option<Box<dyn Operator + 'a>>,
    dimensions: Option<CachedGroupedDimensionBuildHandle>,
    group_positions: Option<GroupPositions>,
    keys: &'a [EquiJoinKey],
    fact_is_left: bool,
    groups: &'a [bound::BoundGroupTerm],
    terms: &'a [bound::BoundAggregateTerm],
    outer: Env,
    output: std::collections::VecDeque<Env>,
    memory_limit_bytes: u64,
}

fn ensure_aggregate_group_for_dimension(
    entry: &CachedGroupedDimensionEntry,
    terms: &[bound::BoundAggregateTerm],
    final_groups: &mut IdentityHashMap<Vec<AggregateGroup>>,
    order: &mut Vec<(u64, usize)>,
    retained_bytes: &mut u64,
    memory_limit_bytes: u64,
) -> Result<usize> {
    let hash = entry.group_hash;
    let bucket = final_groups.entry(hash).or_default();
    let position = bucket
        .iter()
        .position(|group| aggregate_group_matches(&group.values, None, Some(&entry.group_values)));
    let position = match position {
        Some(position) => position,
        None => {
            *retained_bytes = retained_bytes.saturating_add(256);
            if *retained_bytes > memory_limit_bytes {
                return Err(Error::message(
                    ErrorKind::Runtime,
                    format!(
                        "exec: grouped hash join aggregate retained byte limit {memory_limit_bytes} exceeded"
                    ),
                ));
            }
            bucket.push(AggregateGroup {
                values: entry.group_values.clone(),
                accumulators: (0..terms.len())
                    .map(|_| super::query::Accumulator::default())
                    .collect(),
            });
            let position = bucket.len() - 1;
            order.push((hash, position));
            position
        }
    };
    Ok(position)
}

fn aggregate_group_for_dimension<'a>(
    entry: &CachedGroupedDimensionEntry,
    group_positions: &mut GroupPositions,
    terms: &[bound::BoundAggregateTerm],
    final_groups: &'a mut IdentityHashMap<Vec<AggregateGroup>>,
    order: &mut Vec<(u64, usize)>,
    retained_bytes: &mut u64,
    memory_limit_bytes: u64,
) -> Result<&'a mut AggregateGroup> {
    let position = match group_positions.get(entry.position) {
        Some(position) => position,
        None => {
            let position = ensure_aggregate_group_for_dimension(
                entry,
                terms,
                final_groups,
                order,
                retained_bytes,
                memory_limit_bytes,
            )?;
            group_positions.set(entry.position, position);
            position
        }
    };
    Ok(&mut final_groups
        .get_mut(&entry.group_hash)
        .expect("aggregate group exists")[position])
}

async fn collect_grouped_dimension(
    mut dimension: Box<dyn Operator + '_>,
    keys: &[EquiJoinKey],
    fact_is_left: bool,
    groups: &[bound::BoundGroupTerm],
    memory_limit_bytes: u64,
) -> Result<CachedGroupedDimensionBuild> {
    let hash_builder = ahash::RandomState::new();
    let mut entries = HashMap::<u64, Vec<CachedGroupedDimensionEntry>>::new();
    let mut retained_bytes = 0u64;
    let mut row_count = 0usize;
    let mut input_row_count = 0usize;
    while let Some(frame) = dimension.next().await? {
        input_row_count = input_row_count.saturating_add(1);
        let Some(values) = join_value_refs(&frame, keys, !fact_is_left)? else {
            continue;
        };
        let hash = hash_scalar_values(values.iter().copied().map(Some), &hash_builder);
        retained_bytes = retained_bytes.saturating_add(256);
        if retained_bytes > memory_limit_bytes {
            return Err(Error::message(
                ErrorKind::Runtime,
                format!(
                    "exec: grouped hash join aggregate retained byte limit {memory_limit_bytes} exceeded"
                ),
            ));
        }
        let group_values = groups
            .iter()
            .map(|group| evaluate(&group.expression, &frame).map_err(Into::into))
            .collect::<Result<Vec<_>>>()?;
        let group_hash = aggregate_group_hash(None, Some(&group_values), &hash_builder);
        entries
            .entry(hash)
            .or_default()
            .push(CachedGroupedDimensionEntry {
                key: values.into_iter().cloned().collect(),
                group_values,
                group_hash,
                position: row_count,
            });
        row_count = row_count.saturating_add(1);
    }
    Ok(CachedGroupedDimensionBuild {
        hash_builder,
        entries,
        row_count,
        input_row_count,
        execution_retained_bytes: retained_bytes,
    })
}

#[async_trait]
impl Operator for GroupedHashJoinAggregate<'_> {
    async fn next(&mut self) -> Result<Option<Env>> {
        if let Some(mut fact) = self.fact.take() {
            if self.dimensions.is_none() {
                let dimension = self
                    .dimension
                    .take()
                    .expect("grouped hash join aggregate has a dimension input");
                let dimensions = collect_grouped_dimension(
                    dimension,
                    self.keys,
                    self.fact_is_left,
                    self.groups,
                    self.memory_limit_bytes,
                )
                .await?;
                self.group_positions = Some(GroupPositions::new(dimensions.row_count));
                self.dimensions = Some(CachedGroupedDimensionBuildHandle::uncached(dimensions));
            }
            let dimensions = self
                .dimensions
                .as_ref()
                .expect("grouped dimension build is present")
                .value();
            let mut retained_bytes = dimensions.execution_retained_bytes;
            let mut group_positions = self
                .group_positions
                .take()
                .expect("grouped dimension positions are present");

            let mut final_groups = IdentityHashMap::<Vec<AggregateGroup>>::default();
            let mut order = Vec::new();
            let decoded_access = fact.decoded_slots().and_then(|slots| {
                DecodedFactAccess::new(slots, self.keys, self.fact_is_left, self.terms)
            });
            let raw_access = decoded_access.as_ref().zip(fact.raw_decoder());
            if let Some((decoded_access, decoder)) = raw_access {
                let mut fact_batch = Vec::with_capacity(1_024);
                loop {
                    fact_batch.clear();
                    fact.next_raw_batch(1_024, &mut fact_batch).await?;
                    if fact_batch.is_empty() {
                        break;
                    }
                    for raw in fact_batch.drain(..) {
                        let row = decoder.decode_ref(&raw)?;
                        let Some(values) = decoded_access.join_values_ref(&row) else {
                            continue;
                        };
                        let dimension_hash = values.hash(&dimensions.hash_builder);
                        let Some(entries) = dimensions.entries.get(&dimension_hash) else {
                            continue;
                        };
                        for entry in entries {
                            if !values.matches(&entry.key)? {
                                continue;
                            }
                            let group = aggregate_group_for_dimension(
                                entry,
                                &mut group_positions,
                                self.terms,
                                &mut final_groups,
                                &mut order,
                                &mut retained_bytes,
                                self.memory_limit_bytes,
                            )?;
                            decoded_access.observe_ref(
                                &row,
                                self.terms,
                                &mut group.accumulators,
                            )?;
                        }
                    }
                }
            } else if let Some(decoded_access) = decoded_access {
                let mut fact_batch = Vec::with_capacity(1_024);
                loop {
                    fact_batch.clear();
                    fact.next_decoded_batch(1_024, &mut fact_batch).await?;
                    if fact_batch.is_empty() {
                        break;
                    }
                    for row in fact_batch.drain(..) {
                        let Some(values) = decoded_access.join_values(&row) else {
                            continue;
                        };
                        let dimension_hash = hash_scalar_values(
                            values.iter().copied().map(Some),
                            &dimensions.hash_builder,
                        );
                        let Some(entries) = dimensions.entries.get(&dimension_hash) else {
                            continue;
                        };
                        for entry in entries {
                            if !join_values_equal(&entry.key, &values)? {
                                continue;
                            }
                            let group = aggregate_group_for_dimension(
                                entry,
                                &mut group_positions,
                                self.terms,
                                &mut final_groups,
                                &mut order,
                                &mut retained_bytes,
                                self.memory_limit_bytes,
                            )?;
                            decoded_access.observe(&row, self.terms, &mut group.accumulators)?;
                        }
                    }
                }
            } else {
                let mut fact_batch = Vec::with_capacity(1_024);
                loop {
                    fact_batch.clear();
                    fact.next_batch(1_024, &mut fact_batch).await?;
                    if fact_batch.is_empty() {
                        break;
                    }
                    for frame in fact_batch.drain(..) {
                        let Some(values) = join_value_refs(&frame, self.keys, self.fact_is_left)?
                        else {
                            continue;
                        };
                        let dimension_hash = hash_scalar_values(
                            values.iter().copied().map(Some),
                            &dimensions.hash_builder,
                        );
                        let Some(entries) = dimensions.entries.get(&dimension_hash) else {
                            continue;
                        };
                        for entry in entries {
                            if !join_values_equal(&entry.key, &values)? {
                                continue;
                            }
                            let group = aggregate_group_for_dimension(
                                entry,
                                &mut group_positions,
                                self.terms,
                                &mut final_groups,
                                &mut order,
                                &mut retained_bytes,
                                self.memory_limit_bytes,
                            )?;
                            for (term, accumulator) in
                                self.terms.iter().zip(&mut group.accumulators)
                            {
                                accumulator.observe(term, &frame)?;
                            }
                        }
                    }
                }
            }

            self.output.reserve(order.len());
            for (hash, position) in order {
                let group = final_groups
                    .get_mut(&hash)
                    .and_then(|bucket| bucket.get_mut(position))
                    .map(std::mem::take)
                    .expect("ordered group exists");
                let mut frame = new_frame(&self.outer);
                for (term, value) in self.groups.iter().zip(group.values) {
                    frame.set_scalar(term.slot, value);
                }
                for (term, accumulator) in self.terms.iter().zip(group.accumulators) {
                    frame.set_scalar(term.slot, accumulator.finish(term)?);
                }
                self.output.push_back(frame);
            }
        }
        Ok(self.output.pop_front())
    }
}

struct Concatenate<'a> {
    inputs: Vec<Box<dyn Operator + 'a>>,
    input_outputs: &'a [RowType],
    output: &'a RowType,
    outer: Env,
    position: usize,
}

#[async_trait]
impl Operator for Concatenate<'_> {
    async fn next(&mut self) -> Result<Option<Env>> {
        while let Some(input) = self.inputs.get_mut(self.position) {
            if let Some(frame) = input.next().await? {
                return Ok(Some(remap_positional(
                    self.output,
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
    predicate: &'a bound::Expr,
    keys: &'a [EquiJoinKey],
    right_output: &'a RowType,
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
                let left = self.current_left.as_ref().expect("left row");
                if evaluate_join_predicate(self.predicate, self.keys, left, right, &self.tally)?
                    == TriBool::True
                {
                    self.matched = true;
                    return Ok(Some(merge_frames(left, right)));
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

struct BoundHashBuild {
    value: CachedHashBuildHandle,
    slots: Box<[SlotId]>,
    // A page keeps each restored frame at a stable address. Concurrent probes
    // can share the frame after its complete initialization.
    rows: Box<[std::sync::OnceLock<BoundHashBuildPage>]>,
}

type BoundHashBuildPage = Box<[std::sync::OnceLock<Box<Env>>; MATERIALIZATION_STATE_PAGE_ROWS]>;

impl BoundHashBuild {
    fn new(value: CachedHashBuildHandle, output: &RowType) -> Self {
        let rows = (0..materialization_state_page_count(value.value().rows.len()))
            .map(|_| std::sync::OnceLock::new())
            .collect();
        Self {
            value,
            slots: output.fields.iter().map(|field| field.slot).collect(),
            rows,
        }
    }

    fn row(&self, index: usize) -> &Env {
        let page_index = index / MATERIALIZATION_STATE_PAGE_ROWS;
        let page_offset = index % MATERIALIZATION_STATE_PAGE_ROWS;
        let page = self.rows[page_index]
            .get_or_init(|| Box::new(std::array::from_fn(|_| std::sync::OnceLock::new())));
        page[page_offset].get_or_init(|| {
            let mut frame = Env::new();
            frame.set_datums(&self.slots, self.value.value().rows[index].iter().cloned());
            Box::new(frame)
        })
    }

    #[cfg(test)]
    fn initialized_page_count(&self) -> usize {
        self.rows.iter().filter(|page| page.get().is_some()).count()
    }
}

struct HashJoin<'a> {
    left: Box<dyn Operator + 'a>,
    right: Option<Box<dyn Operator + 'a>>,
    kind: JoinKind,
    residual_predicate: Option<&'a bound::Expr>,
    keys: &'a [EquiJoinKey],
    right_output: &'a RowType,
    memory_limit_bytes: u64,
    build: Option<Arc<BoundHashBuild>>,
    current_left: Option<Env>,
    current_hash: u64,
    current_entry: usize,
    current_row: usize,
    matched: bool,
    tally: JoinTally,
    execution_grant: ExecutionGrant,
    parallel_output: std::collections::VecDeque<Env>,
    left_exhausted: bool,
    parallel_batch_sequence: u64,
    serial_probe_rows_until_retry: usize,
    parallel_plan: Option<Arc<HashProbePlan>>,
}

async fn collect_hash_build(
    mut right: Box<dyn Operator + '_>,
    keys: &[EquiJoinKey],
    right_output: &RowType,
    memory_limit_bytes: u64,
    tally: &JoinTally,
) -> Result<CachedHashBuild> {
    let hash_builder = ahash::RandomState::new();
    let mut entries = HashMap::<u64, Vec<CachedHashEntry>>::new();
    let mut rows = Vec::new();
    let mut input_row_count = 0usize;
    let mut retained_bytes = 0u64;
    while let Some(frame) = right.next().await? {
        tally.add_build_row();
        input_row_count = input_row_count.saturating_add(1);
        let Some(values) = join_value_refs(&frame, keys, false)? else {
            continue;
        };
        let hash = hash_scalar_values(values.iter().copied().map(Some), &hash_builder);
        let bucket = entries.entry(hash).or_default();
        let mut matching = None;
        for (index, entry) in bucket.iter().enumerate() {
            tally.add_key_comparison();
            if join_values_equal(&entry.key, &values)? {
                matching = Some(index);
                break;
            }
        }
        let index = matching.unwrap_or_else(|| {
            bucket.push(CachedHashEntry {
                key: values.into_iter().cloned().collect(),
                rows: Vec::new(),
            });
            bucket.len() - 1
        });
        retained_bytes = retained_bytes.saturating_add(frame_retained_bytes(&frame));
        if retained_bytes > memory_limit_bytes {
            return Err(Error::message(
                ErrorKind::Runtime,
                format!("exec: hash join retained byte limit {memory_limit_bytes} exceeded"),
            ));
        }
        tally.retain(retained_bytes);
        let row = rows.len();
        rows.push(super::relation_cache::positional_row(right_output, &frame));
        bucket[index].rows.push(row);
    }
    Ok(CachedHashBuild {
        hash_builder,
        entries,
        rows,
        input_row_count,
        execution_retained_bytes: retained_bytes,
    })
}

impl<'a> HashJoin<'a> {
    async fn build(&mut self, right: Box<dyn Operator + 'a>) -> Result<()> {
        let build = collect_hash_build(
            right,
            self.keys,
            self.right_output,
            self.memory_limit_bytes,
            &self.tally,
        )
        .await?;
        self.build = Some(Arc::new(BoundHashBuild::new(
            CachedHashBuildHandle::uncached(build),
            self.right_output,
        )));
        Ok(())
    }

    async fn fill_parallel_output(&mut self) -> Result<bool> {
        const ROWS_PER_MORSEL: usize = 512;
        const MAX_MORSELS: usize = 64;

        let lease = self.execution_grant.try_lease(WorkRequest {
            operator: "HashJoin",
            morsels: MAX_MORSELS,
        });
        let width = lease.width();
        if width == 1 {
            self.serial_probe_rows_until_retry = ROWS_PER_MORSEL;
            return Ok(false);
        }

        let mut left_rows = Vec::with_capacity(width.saturating_mul(ROWS_PER_MORSEL));
        while left_rows.len() < left_rows.capacity() {
            let Some(left) = self.left.next().await? else {
                self.left_exhausted = true;
                break;
            };
            left_rows.push(left);
        }
        if left_rows.is_empty() {
            return Ok(true);
        }

        let sequence = self.parallel_batch_sequence;
        self.parallel_batch_sequence = self.parallel_batch_sequence.saturating_add(1);
        let width = width.min(left_rows.len());
        self.execution_grant
            .reach(ExecutionScheduleEvent::Prepared {
                operator: "HashJoin",
                sequence,
                rows: left_rows.len(),
                width,
            })
            .await;
        crate::telemetry::execution_parallel_batch("HashJoin", width, left_rows.len());
        let chunk_rows = left_rows.len().div_ceil(width);
        let mut left_rows = left_rows.into_iter();
        let mut helpers = lease.into_helpers().into_iter();
        let mut tasks = Vec::with_capacity(width);
        // Blocking tasks require owned metadata with a static lifetime. One
        // shared copy serves all worker batches. Serial probes borrow the plan.
        let parallel_plan = Arc::clone(self.parallel_plan.get_or_insert_with(|| {
            Arc::new(HashProbePlan {
                residual_predicate: self.residual_predicate.cloned(),
                keys: self.keys.to_vec(),
                right_output: self.right_output.clone(),
            })
        }));
        for index in 0..width {
            let rows = left_rows.by_ref().take(chunk_rows).collect::<Vec<_>>();
            if rows.is_empty() {
                break;
            }
            let build = self
                .build
                .as_ref()
                .expect("hash join build is present")
                .clone();
            let kind = self.kind;
            let plan = Arc::clone(&parallel_plan);
            let tally = self.tally.clone();
            let permit = (index > 0).then(|| helpers.next()).flatten();
            tasks.push(tokio::task::spawn_blocking(move || {
                let _permit = permit;
                probe_hash_rows(HashProbeInput {
                    rows,
                    build,
                    kind,
                    plan,
                    tally,
                })
            }));
        }
        drop(helpers);

        let completed = futures::future::join_all(tasks).await;
        self.execution_grant
            .reach(ExecutionScheduleEvent::Completed {
                operator: "HashJoin",
                sequence,
            })
            .await;
        let mut published = 0;
        for task in completed {
            let rows = task.map_err(|_| {
                Error::message(
                    ErrorKind::Internal,
                    "exec: parallel hash join worker failed",
                )
            })??;
            published += rows.len();
            self.parallel_output.extend(rows);
        }
        self.execution_grant
            .reach(ExecutionScheduleEvent::Published {
                operator: "HashJoin",
                sequence,
                rows: published,
            })
            .await;
        Ok(true)
    }
}

#[async_trait]
impl<'a> Operator for HashJoin<'a> {
    async fn next(&mut self) -> Result<Option<Env>> {
        if let Some(right) = self.right.take() {
            #[cfg(debug_assertions)]
            {
                let span = tracing::debug_span!(
                    target: "rad::telemetry",
                    "rad.debug.hash_join.build",
                    otel.name = "rad.debug.hash_join.build",
                    otel.kind = "internal",
                    rad.debug.join.build_rows = tracing::field::Empty,
                    rad.debug.join.retained_bytes = tracing::field::Empty,
                    rad.debug.join.hash_buckets = tracing::field::Empty,
                    rad.status = tracing::field::Empty,
                    otel.status_code = tracing::field::Empty,
                );
                let result = self.build(right).instrument(span.clone()).await;
                span.record(
                    "rad.debug.join.build_rows",
                    self.tally.build_rows.load(atomic::Ordering::Relaxed),
                );
                span.record(
                    "rad.debug.join.retained_bytes",
                    self.build
                        .as_ref()
                        .map_or(0, |build| build.value.value().execution_retained_bytes),
                );
                span.record(
                    "rad.debug.join.hash_buckets",
                    self.build
                        .as_ref()
                        .map_or(0, |build| build.value.value().entries.len() as u64),
                );
                span.record(
                    "rad.status",
                    if result.is_ok() { "success" } else { "error" },
                );
                if result.is_err() {
                    span.record("otel.status_code", "ERROR");
                }
                result?;
            }
            #[cfg(not(debug_assertions))]
            self.build(right).await?;
        }
        loop {
            if let Some(frame) = self.parallel_output.pop_front() {
                return Ok(Some(frame));
            }
            if self.left_exhausted {
                return Ok(None);
            }
            if self.current_left.is_some() || self.serial_probe_rows_until_retry > 0 {
                break;
            }
            if !self.fill_parallel_output().await? {
                break;
            }
        }
        loop {
            if self.current_left.is_none() {
                let Some(left) = self.left.next().await? else {
                    return Ok(None);
                };
                self.serial_probe_rows_until_retry =
                    self.serial_probe_rows_until_retry.saturating_sub(1);
                self.tally.add_probe_row();
                let values = join_value_refs(&left, self.keys, true)?;
                let build = self.build.as_ref().expect("hash join build is present");
                self.current_hash = values.as_ref().map_or(0, |values| {
                    hash_scalar_values(
                        values.iter().copied().map(Some),
                        &build.value.value().hash_builder,
                    )
                });
                self.current_entry = usize::MAX;
                if let Some(values) = values.as_ref()
                    && let Some(entries) = build.value.value().entries.get(&self.current_hash)
                {
                    for (index, entry) in entries.iter().enumerate() {
                        self.tally.add_key_comparison();
                        if join_values_equal(&entry.key, values)? {
                            self.current_entry = index;
                            break;
                        }
                    }
                }
                drop(values);
                self.current_left = Some(left);
                self.current_row = 0;
                self.matched = false;
            }
            while self.current_entry != usize::MAX {
                let Some(right_index) = self
                    .build
                    .as_ref()
                    .expect("hash join build is present")
                    .value
                    .value()
                    .entries
                    .get(&self.current_hash)
                    .and_then(|entries| entries.get(self.current_entry))
                    .and_then(|entry| entry.rows.get(self.current_row))
                    .copied()
                else {
                    break;
                };
                self.current_row += 1;
                let last_match = self
                    .build
                    .as_ref()
                    .expect("hash join build is present")
                    .value
                    .value()
                    .entries
                    .get(&self.current_hash)
                    .and_then(|entries| entries.get(self.current_entry))
                    .is_some_and(|entry| self.current_row == entry.rows.len());
                let right = self
                    .build
                    .as_ref()
                    .expect("hash join build is present")
                    .row(right_index);
                let left = self.current_left.as_ref().expect("left row");
                if let Some(predicate) = self.residual_predicate
                    && crate::engine::lir::eval::evaluate_join_predicate(predicate, left, right)?
                        != TriBool::True
                {
                    continue;
                }
                self.matched = true;
                if last_match {
                    let mut left = self.current_left.take().expect("left row");
                    left.extend_from(right);
                    return Ok(Some(left));
                }
                return Ok(Some(merge_frames(left, right)));
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

struct HashProbeInput {
    rows: Vec<Env>,
    build: Arc<BoundHashBuild>,
    kind: JoinKind,
    plan: Arc<HashProbePlan>,
    tally: JoinTally,
}

struct HashProbePlan {
    residual_predicate: Option<bound::Expr>,
    keys: Vec<EquiJoinKey>,
    right_output: RowType,
}

fn probe_hash_rows(input: HashProbeInput) -> Result<Vec<Env>> {
    let mut output = Vec::with_capacity(input.rows.len());
    for mut left in input.rows {
        input.tally.add_probe_row();
        let values = join_value_refs(&left, &input.plan.keys, true)?;
        let mut matching = None;
        if let Some(values) = values.as_ref() {
            let hash = hash_scalar_values(
                values.iter().copied().map(Some),
                &input.build.value.value().hash_builder,
            );
            if let Some(entries) = input.build.value.value().entries.get(&hash) {
                for entry in entries {
                    input.tally.add_key_comparison();
                    if join_values_equal(&entry.key, values)? {
                        matching = Some(entry);
                        break;
                    }
                }
            }
        }
        drop(values);
        let mut matched = false;
        if let Some(entry) = matching {
            if let [right_index] = entry.rows.as_slice() {
                let right = input.build.row(*right_index);
                let accepted = if let Some(predicate) = &input.plan.residual_predicate {
                    crate::engine::lir::eval::evaluate_join_predicate(predicate, &left, right)?
                        == TriBool::True
                } else {
                    true
                };
                if accepted {
                    left.extend_from(right);
                    output.push(left);
                    continue;
                }
                if input.kind == JoinKind::Left {
                    for field in &input.plan.right_output.fields {
                        left.insert(field.slot, Datum::Null);
                    }
                    output.push(left);
                }
                continue;
            }
            for right_index in &entry.rows {
                let right = input.build.row(*right_index);
                if let Some(predicate) = &input.plan.residual_predicate
                    && crate::engine::lir::eval::evaluate_join_predicate(predicate, &left, right)?
                        != TriBool::True
                {
                    continue;
                }
                matched = true;
                output.push(merge_frames(&left, right));
            }
        }
        if input.kind == JoinKind::Left && !matched {
            let mut padded = left;
            for field in &input.plan.right_output.fields {
                padded.insert(field.slot, Datum::Null);
            }
            output.push(padded);
        }
    }
    Ok(output)
}

fn join_predicate_is_keys(predicate: &bound::Expr, keys: &[EquiJoinKey]) -> bool {
    if keys.is_empty() {
        return false;
    }
    if let bound::Expr::Binary {
        op: BinaryOp::And,
        left,
        right,
        ..
    } = predicate
    {
        return join_predicate_is_keys(left, keys) && join_predicate_is_keys(right, keys);
    }
    let bound::Expr::Binary {
        op: BinaryOp::Eq,
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

fn evaluate_join_predicate(
    predicate: &bound::Expr,
    keys: &[EquiJoinKey],
    left_frame: &Env,
    right_frame: &Env,
    tally: &JoinTally,
) -> Result<TriBool> {
    if !keys.is_empty() {
        for key in keys {
            tally.add_key_comparison();
            let result = crate::engine::lir::eval::evaluate_join_key_equality(
                key.left.slot,
                key.right.slot,
                left_frame,
                right_frame,
            )?;
            if result != TriBool::True {
                return Ok(result);
            }
        }
        return Ok(TriBool::True);
    }
    if let bound::Expr::Binary {
        op: BinaryOp::And,
        left,
        right,
        ..
    } = predicate
    {
        let left = evaluate_join_predicate(left, keys, left_frame, right_frame, tally)?;
        if left == TriBool::False {
            return Ok(TriBool::False);
        }
        return Ok(left.and(evaluate_join_predicate(
            right,
            keys,
            left_frame,
            right_frame,
            tally,
        )?));
    }
    tally.add_residual_predicate_evaluation();
    Ok(crate::engine::lir::eval::evaluate_join_predicate(
        predicate,
        left_frame,
        right_frame,
    )?)
}

fn write_scalar_hash(hasher: &mut impl Hasher, value: Option<&Value>) {
    let Some(value) = value else {
        hasher.write_u8(0);
        return;
    };
    match value {
        Value::Text(value) => {
            hasher.write_u8(1);
            hasher.write_usize(value.len());
            hasher.write(value.as_bytes());
        }
        Value::Int64(value) => {
            hasher.write_u8(2);
            hasher.write_i64(*value);
        }
        Value::Float64(value) => {
            hasher.write_u8(3);
            let bits = if *value == 0.0 {
                0
            } else if value.is_nan() {
                f64::NAN.to_bits()
            } else {
                value.to_bits()
            };
            hasher.write_u64(bits);
        }
        Value::Bool(value) => {
            hasher.write_u8(4);
            hasher.write_u8(u8::from(*value));
        }
        Value::Bytes(value) => {
            hasher.write_u8(5);
            hasher.write_usize(value.as_slice().len());
            hasher.write(value.as_slice());
        }
        Value::Null(_) => hasher.write_u8(0),
    }
}

fn write_decoded_scalar_hash(
    hasher: &mut impl Hasher,
    value: Option<super::codec::DecodedValueRef<'_>>,
) {
    let Some(value) = value else {
        hasher.write_u8(0);
        return;
    };
    match value {
        super::codec::DecodedValueRef::Text(value) => {
            hasher.write_u8(1);
            hasher.write_usize(value.len());
            hasher.write(value.as_bytes());
        }
        super::codec::DecodedValueRef::Int64(value) => {
            hasher.write_u8(2);
            hasher.write_i64(value);
        }
        super::codec::DecodedValueRef::Float64(value) => {
            hasher.write_u8(3);
            let bits = if value == 0.0 {
                0
            } else if value.is_nan() {
                f64::NAN.to_bits()
            } else {
                value.to_bits()
            };
            hasher.write_u64(bits);
        }
        super::codec::DecodedValueRef::Bool(value) => {
            hasher.write_u8(4);
            hasher.write_u8(u8::from(value));
        }
        super::codec::DecodedValueRef::Bytes(value, _) => {
            hasher.write_u8(5);
            hasher.write_usize(value.len());
            hasher.write(value);
        }
        super::codec::DecodedValueRef::Null(_) => hasher.write_u8(0),
    }
}

fn hash_scalar_values<'a>(
    values: impl IntoIterator<Item = Option<&'a Value>>,
    hash_builder: &ahash::RandomState,
) -> u64 {
    let mut hasher = hash_builder.build_hasher();
    for value in values {
        write_scalar_hash(&mut hasher, value);
    }
    hasher.finish()
}

fn hash_decoded_scalar_values<'a>(
    values: impl IntoIterator<Item = Option<super::codec::DecodedValueRef<'a>>>,
    hash_builder: &ahash::RandomState,
) -> u64 {
    let mut hasher = hash_builder.build_hasher();
    for value in values {
        write_decoded_scalar_hash(&mut hasher, value);
    }
    hasher.finish()
}

fn join_value_refs<'a>(
    frame: &'a Env,
    keys: &[EquiJoinKey],
    left: bool,
) -> Result<Option<smallvec::SmallVec<[&'a Value; 4]>>> {
    let mut values = smallvec::SmallVec::new();
    for key in keys {
        let field = if left { &key.left } else { &key.right };
        let Some(value) = frame.scalar_ref_at(field.slot, &field.name, &field.value_type)? else {
            return Ok(None);
        };
        values.push(value);
    }
    Ok(Some(values))
}

fn join_values_equal(stored: &[Value], candidate: &[&Value]) -> Result<bool> {
    debug_assert_eq!(stored.len(), candidate.len());
    for (stored, candidate) in stored.iter().zip(candidate) {
        let ordering = stored
            .compare(candidate)
            .map_err(|error| Error::message(ErrorKind::Internal, format!("exec: {error}")))?;
        if ordering != std::cmp::Ordering::Equal {
            return Ok(false);
        }
    }
    Ok(true)
}

fn join_decoded_values_equal(
    stored: &[Value],
    candidate: &[super::codec::DecodedValueRef<'_>],
) -> Result<bool> {
    debug_assert_eq!(stored.len(), candidate.len());
    for (stored, candidate) in stored.iter().zip(candidate) {
        if !join_decoded_value_equal(stored, *candidate)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn join_decoded_value_equal(
    stored: &Value,
    candidate: super::codec::DecodedValueRef<'_>,
) -> Result<bool> {
    match (stored, candidate) {
        (Value::Text(left), super::codec::DecodedValueRef::Text(right)) => Ok(left == right),
        (Value::Int64(left), super::codec::DecodedValueRef::Int64(right)) => Ok(*left == right),
        (Value::Float64(left), super::codec::DecodedValueRef::Float64(right)) => {
            Ok(*left == right || (left.is_nan() && right.is_nan()))
        }
        (Value::Bool(left), super::codec::DecodedValueRef::Bool(right)) => Ok(*left == right),
        (Value::Bytes(left), super::codec::DecodedValueRef::Bytes(right, _)) => {
            Ok(left.as_slice() == right)
        }
        (Value::Null(_), super::codec::DecodedValueRef::Null(_)) => Ok(false),
        (left, right) => Err(Error::message(
            ErrorKind::Internal,
            format!(
                "exec: cannot compare join values {:?} and {:?}",
                left.scalar_type(),
                decoded_scalar_type(right)
            ),
        )),
    }
}

fn decoded_scalar_type(value: super::codec::DecodedValueRef<'_>) -> ScalarType {
    match value {
        super::codec::DecodedValueRef::Text(_) => ScalarType::Text,
        super::codec::DecodedValueRef::Int64(_) => ScalarType::Int64,
        super::codec::DecodedValueRef::Float64(_) => ScalarType::Float64,
        super::codec::DecodedValueRef::Bool(_) => ScalarType::Bool,
        super::codec::DecodedValueRef::Bytes(_, _) => ScalarType::Bytes,
        super::codec::DecodedValueRef::Null(value_type) => value_type,
    }
}

pub(super) fn join_key(frame: &Env, keys: &[EquiJoinKey], left: bool) -> Result<Option<Vec<u8>>> {
    let mut output = Vec::new();
    for key in keys {
        let field = if left { &key.left } else { &key.right };
        let Some(value) = frame.scalar_ref_at(field.slot, &field.name, &field.value_type)? else {
            return Ok(None);
        };
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
                let bits = if *value == 0.0 {
                    0
                } else if value.is_nan() {
                    f64::NAN.to_bits()
                } else {
                    value.to_bits()
                };
                output.extend_from_slice(&bits.to_be_bytes());
            }
            Value::Bool(value) => output.extend_from_slice(&[4, u8::from(*value)]),
            Value::Bytes(value) => {
                output.push(5);
                output.extend_from_slice(&(value.as_slice().len() as u64).to_be_bytes());
                output.extend_from_slice(value.as_slice());
            }
            Value::Null(_) => unreachable!("null join keys return before encoding"),
        }
    }
    Ok(Some(output))
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
        Datum::Scalar(Value::Bytes(value)) => value.as_slice().len() as u64,
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
    left_output: &'a RowType,
    right_output: &'a RowType,
    output: &'a RowType,
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
        left_output: &'a RowType,
        right_output: &'a RowType,
        output: &'a RowType,
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
                self.state.add_right(self.right_output, &frame);
            }
        }
        while let Some(frame) = self.left.next().await? {
            if self.state.keep_left(self.left_output, &frame) {
                return Ok(Some(remap_positional(
                    self.output,
                    self.left_output,
                    &frame,
                    &self.outer,
                )));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::lir::{Field, Type};

    fn cached_hash_build(row_count: usize) -> BoundHashBuild {
        let rows = (0..row_count)
            .map(|value| vec![Datum::scalar(Value::Int64(value as i64))].into_boxed_slice())
            .collect();
        BoundHashBuild::new(
            CachedHashBuildHandle::uncached(CachedHashBuild {
                hash_builder: ahash::RandomState::new(),
                entries: HashMap::new(),
                rows,
                input_row_count: row_count,
                execution_retained_bytes: 0,
            }),
            &RowType {
                fields: vec![Field {
                    name: "value".into(),
                    slot: SlotId(3),
                    value_type: Type::scalar(Kind::Int64, false),
                }],
            },
        )
    }

    #[test]
    fn bound_hash_build_initializes_only_accessed_pages() {
        let build = cached_hash_build(1_025);
        assert_eq!(build.initialized_page_count(), 0);

        assert_eq!(
            build.row(0).get(SlotId(3)),
            Some(&Datum::scalar(Value::Int64(0)))
        );
        let first = build.row(255);
        assert!(std::ptr::eq(first, build.row(255)));
        assert_eq!(build.initialized_page_count(), 1);

        assert_eq!(
            build.row(256).get(SlotId(3)),
            Some(&Datum::scalar(Value::Int64(256)))
        );
        assert_eq!(build.initialized_page_count(), 2);

        assert_eq!(
            build.row(1_024).get(SlotId(3)),
            Some(&Datum::scalar(Value::Int64(1_024)))
        );
        assert_eq!(build.initialized_page_count(), 3);
    }

    #[test]
    fn bound_hash_build_initializes_one_frame_for_concurrent_probes() {
        let build = cached_hash_build(1_025);
        let addresses = std::thread::scope(|scope| {
            (0..8)
                .map(|_| scope.spawn(|| build.row(513) as *const Env as usize))
                .collect::<Vec<_>>()
                .into_iter()
                .map(|thread| thread.join().expect("probe thread succeeds"))
                .collect::<Vec<_>>()
        });

        assert!(addresses.iter().all(|address| *address == addresses[0]));
        assert_eq!(build.initialized_page_count(), 1);
    }

    #[test]
    fn group_positions_initializes_only_accessed_pages() {
        let mut positions = GroupPositions::new(1_025);
        assert_eq!(positions.initialized_page_count(), 0);
        assert_eq!(positions.get(700), None);

        positions.set(700, 11);
        positions.set(701, 12);
        assert_eq!(positions.get(700), Some(11));
        assert_eq!(positions.get(701), Some(12));
        assert_eq!(positions.initialized_page_count(), 1);

        positions.set(1_024, 13);
        assert_eq!(positions.get(1_024), Some(13));
        assert_eq!(positions.initialized_page_count(), 2);
    }
}
