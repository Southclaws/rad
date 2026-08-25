//! Cardinality estimation for equality joins.
//!
//! The central estimate adapts the two-table factorization in Wu et al.,
//! "FactorJoin: A New Cardinality Estimation Framework for Join Queries,"
//! SIGMOD 2023: <https://doi.org/10.1145/3588721>. It is an estimate,
//! not a bound.
//!
//! Guaranteed upper bounds use degree statistics from Deeds et al.,
//! "SafeBound: A Practical System for Generating Cardinality Bounds," SIGMOD
//! 2023: <https://doi.org/10.1145/3588907>, and the norm inequalities from
//! Zhang et al., "LpBound: Pessimistic Cardinality Estimation using
//! l_p-Norms of Degree Sequences," SIGMOD 2025:
//! <https://doi.org/10.1145/3725321>. The estimator reports a bound only when
//! complete statistics and current generations make the inequality valid.

use std::time::Duration;

use crate::engine::catalog::model::{Column, Table};
use crate::engine::lir::bound::{Expr, Relation, RelationNode};
use crate::engine::lir::{BinaryOp, JoinKind, SlotId};

use super::estimator::{Estimate, EstimateBoundSource, EstimateInterval, EstimateSource};
use super::models::{
    DEGREE_SEQUENCE_FORMAT_VERSION, DegreeSequenceNorms, DegreeSequenceSynopsis, PlannerStats,
    SynopsisCoverage, SynopsisModel, SynopsisValue,
};

struct JoinKey<'a> {
    left: &'a Column,
    right: &'a Column,
}

struct Distribution {
    rows: u64,
    null_count: u64,
    distinct: u64,
    distinct_is_exact: bool,
    common: Vec<Frequency>,
    degree_norms: Option<DegreeSequenceNorms>,
}

struct Frequency {
    values: Vec<SynopsisValue>,
    frequency: u64,
    maximum_error: u64,
}

impl Frequency {
    fn midpoint(&self) -> u64 {
        let lower = self.frequency.saturating_sub(self.maximum_error);
        lower.saturating_add(self.frequency.saturating_sub(lower) / 2)
    }
}

pub(super) fn estimate(
    stats: &PlannerStats,
    left: &Relation,
    right: &Relation,
    kind: JoinKind,
    on: &Expr,
) -> Option<Estimate> {
    if let Some(estimate) = super::join_graph_estimator::estimate(stats, left, right, kind, on) {
        return Some(estimate);
    }
    let RelationNode::Scan {
        table: left_table, ..
    } = &left.node
    else {
        return None;
    };
    let RelationNode::Scan {
        table: right_table, ..
    } = &right.node
    else {
        return None;
    };
    let keys = join_keys(left, right, left_table, right_table, on)?;
    let left_synopsis = current_synopsis(stats, left_table)?;
    let right_synopsis = current_synopsis(stats, right_table)?;
    let left_columns: Vec<_> = keys.iter().map(|key| key.left).collect();
    let right_columns: Vec<_> = keys.iter().map(|key| key.right).collect();
    let left_distribution = distribution(left_synopsis, left_table, &left_columns)?;
    let right_distribution = distribution(right_synopsis, right_table, &right_columns)?;
    let left_unique = key_is_unique(left_table, &left_columns);
    let right_unique = key_is_unique(right_table, &right_columns);
    let left_foreign_key = follows_foreign_key(left_table, right_table, &keys, false);
    let right_foreign_key = follows_foreign_key(right_table, left_table, &keys, true);

    let domains_complete =
        left_distribution.complete_domain() && right_distribution.complete_domain();
    let left_non_null = left_distribution
        .rows
        .saturating_sub(left_distribution.null_count);
    let right_non_null = right_distribution
        .rows
        .saturating_sub(right_distribution.null_count);
    let (inner, inner_exact_at_collection) = if left_non_null == 0 || right_non_null == 0 {
        (0, true)
    } else if left_foreign_key && right_unique {
        (left_non_null, true)
    } else if right_foreign_key && left_unique {
        (right_non_null, true)
    } else if domains_complete {
        (
            exact_inner_cardinality(&left_distribution, &right_distribution),
            true,
        )
    } else {
        (
            estimated_inner_cardinality(&left_distribution, &right_distribution),
            false,
        )
    };

    let (cardinality, exact_at_collection) = match kind {
        JoinKind::Inner => (inner, inner_exact_at_collection),
        JoinKind::Left if right_unique => (left_distribution.rows, true),
        JoinKind::Left if left_non_null == 0 || right_non_null == 0 => {
            (left_distribution.rows, true)
        }
        JoinKind::Left if domains_complete => (
            exact_left_cardinality(&left_distribution, &right_distribution),
            true,
        ),
        JoinKind::Left => {
            let matched_distinct = left_distribution.distinct.min(right_distribution.distinct);
            let matched_left = multiply_divide(
                left_distribution
                    .rows
                    .saturating_sub(left_distribution.null_count),
                matched_distinct,
                left_distribution.distinct,
            );
            let unmatched_left = left_distribution.rows.saturating_sub(matched_left);
            (inner.saturating_add(unmatched_left), false)
        }
    };

    let left_changes = left_synopsis.changes_since_collection;
    let right_changes = right_synopsis.changes_since_collection;
    let changes = left_changes.saturating_add(right_changes);
    let left_lower = left_distribution.rows.saturating_sub(left_changes);
    let left_upper = left_distribution.rows.saturating_add(left_changes);
    let right_upper = right_distribution.rows.saturating_add(right_changes);
    let (inner_upper, upper_source) = if right_unique {
        (left_upper, EstimateBoundSource::KeyConstraint)
    } else if left_unique {
        (right_upper, EstimateBoundSource::KeyConstraint)
    } else {
        let (collection_upper, source) = if domains_complete {
            (
                exact_inner_cardinality(&left_distribution, &right_distribution),
                EstimateBoundSource::CompleteDomain,
            )
        } else if let Some(upper) = degree_sequence_upper(&left_distribution, &right_distribution) {
            (upper, EstimateBoundSource::DegreeSequenceNorms)
        } else {
            (
                multiply(left_non_null, right_non_null),
                EstimateBoundSource::CartesianProduct,
            )
        };
        (
            widen_inner_upper(
                collection_upper,
                left_distribution.rows,
                right_distribution.rows,
                left_changes,
                right_changes,
            ),
            source,
        )
    };
    let (upper, upper_source) = match kind {
        JoinKind::Inner => (inner_upper, upper_source),
        JoinKind::Left if right_unique => (left_upper, EstimateBoundSource::KeyConstraint),
        JoinKind::Left => {
            let degree_upper = inner_upper.saturating_add(left_upper);
            let cartesian_upper = multiply(left_upper, right_upper.max(1));
            if degree_upper < cartesian_upper {
                (degree_upper, upper_source)
            } else {
                (cartesian_upper, EstimateBoundSource::CartesianProduct)
            }
        }
    };
    let lower = if kind == JoinKind::Left {
        left_lower
    } else {
        0
    };
    let cardinality = cardinality.clamp(lower.min(upper), upper);
    let interval = if exact_at_collection && changes == 0 {
        EstimateInterval::Exact
    } else {
        EstimateInterval::AttributedRange {
            lower_bound: lower,
            upper_bound: upper,
            lower_source: EstimateBoundSource::Structural,
            central_source: if exact_at_collection {
                if domains_complete {
                    EstimateBoundSource::CompleteDomain
                } else {
                    EstimateBoundSource::KeyConstraint
                }
            } else {
                EstimateBoundSource::FactorizedDistribution
            },
            upper_source,
            drift_widened: changes > 0,
        }
    };
    Some(Estimate {
        cardinality,
        interval,
        source: EstimateSource::Join,
        sample_size: left_synopsis.sample_size.min(right_synopsis.sample_size),
        changes_since_collection: changes,
        age: synopsis_age(stats, left_synopsis).max(synopsis_age(stats, right_synopsis)),
    })
}

fn degree_sequence_upper(left: &Distribution, right: &Distribution) -> Option<u64> {
    let left = left.degree_norms?;
    let right = right.degree_norms?;
    Some(
        multiply(left.l1, right.l_infinity)
            .min(multiply(left.l_infinity, right.l1))
            .min(multiply(left.l2_upper, right.l2_upper)),
    )
}

fn widen_inner_upper(
    collection_upper: u64,
    left_rows: u64,
    right_rows: u64,
    left_changes: u64,
    right_changes: u64,
) -> u64 {
    collection_upper
        .saturating_add(multiply(
            left_changes,
            right_rows.saturating_add(right_changes),
        ))
        .saturating_add(multiply(right_changes, left_rows))
}

impl Distribution {
    fn complete_domain(&self) -> bool {
        if !self.distinct_is_exact || self.distinct as usize != self.common.len() {
            return false;
        }
        if self.common.iter().any(|common| common.maximum_error != 0) {
            return false;
        }
        let total = self
            .common
            .iter()
            .map(|common| common.frequency)
            .fold(0u64, u64::saturating_add);
        total == self.rows.saturating_sub(self.null_count)
            && self.common.iter().enumerate().all(|(index, common)| {
                self.common[index + 1..]
                    .iter()
                    .all(|candidate| candidate.values != common.values)
            })
    }
}

fn current_synopsis<'a>(stats: &'a PlannerStats, table: &Table) -> Option<&'a SynopsisModel> {
    let synopsis = stats.synopsis_models.get(&table.schema_id)?;
    (synopsis.coverage == SynopsisCoverage::Complete
        && synopsis.table_existence_generation == table.existence_generation.get())
    .then_some(synopsis)
}

fn distribution(
    synopsis: &SynopsisModel,
    table: &Table,
    columns: &[&Column],
) -> Option<Distribution> {
    if columns.len() == 1 {
        let catalog_column = columns[0];
        let column = synopsis
            .columns
            .iter()
            .find(|column| column.column == catalog_column.schema_id)?;
        if column.value_generation != catalog_column.value_generation.get()
            || column.null_count > synopsis.observed_rows
        {
            return None;
        }
        let non_null = synopsis.observed_rows.saturating_sub(column.null_count);
        let common = column
            .most_common_values
            .iter()
            .map(|common| Frequency {
                values: vec![common.value.clone()],
                frequency: common.frequency,
                maximum_error: common.maximum_error,
            })
            .collect::<Vec<_>>();
        valid_frequencies(&common, non_null).then_some(Distribution {
            rows: synopsis.observed_rows,
            null_count: column.null_count,
            distinct: column.distinct.min(non_null),
            distinct_is_exact: column.distinct_is_exact,
            common,
            degree_norms: valid_degree_norms(
                column.degree_sequence.as_ref(),
                &[catalog_column.value_generation.get()],
                synopsis.observed_rows,
                non_null,
                column.distinct,
                column.distinct_is_exact,
            ),
        })
    } else {
        let requested: Vec<_> = columns.iter().map(|column| column.schema_id).collect();
        let mut canonical = requested.clone();
        canonical.sort_unstable();
        if !super::models::declares_column_group(table, &canonical) {
            return None;
        }
        let group = synopsis
            .column_groups
            .iter()
            .find(|group| group.columns == canonical)?;
        if group.value_generations.len() != group.columns.len()
            || group.null_count > synopsis.observed_rows
            || !group
                .columns
                .iter()
                .zip(&group.value_generations)
                .all(|(column_id, generation)| {
                    table
                        .columns
                        .iter()
                        .find(|column| column.schema_id == *column_id)
                        .is_some_and(|column| column.value_generation.get() == *generation)
                })
        {
            return None;
        }
        let positions = requested
            .iter()
            .map(|column| {
                group
                    .columns
                    .iter()
                    .position(|candidate| candidate == column)
            })
            .collect::<Option<Vec<_>>>()?;
        let common = group
            .most_common_values
            .iter()
            .map(|common| {
                if common.values.len() != group.columns.len() {
                    return None;
                }
                Some(Frequency {
                    values: positions
                        .iter()
                        .map(|position| common.values[*position].clone())
                        .collect(),
                    frequency: common.frequency,
                    maximum_error: common.maximum_error,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let non_null = synopsis.observed_rows.saturating_sub(group.null_count);
        valid_frequencies(&common, non_null).then_some(Distribution {
            rows: synopsis.observed_rows,
            null_count: group.null_count,
            distinct: group.distinct.min(non_null),
            distinct_is_exact: group.distinct_is_exact,
            common,
            degree_norms: valid_degree_norms(
                group.degree_sequence.as_ref(),
                &group.value_generations,
                synopsis.observed_rows,
                non_null,
                group.distinct,
                group.distinct_is_exact,
            ),
        })
    }
}

fn valid_degree_norms(
    sequence: Option<&DegreeSequenceSynopsis>,
    value_generations: &[u64],
    rows: u64,
    non_null_rows: u64,
    distinct: u64,
    distinct_is_exact: bool,
) -> Option<DegreeSequenceNorms> {
    let sequence = sequence?;
    let norms = sequence.norms;
    (sequence.format_version == DEGREE_SEQUENCE_FORMAT_VERSION
        && sequence.coverage == SynopsisCoverage::Complete
        && sequence.sample_size == rows
        && sequence.collected_row_count == rows
        && sequence.non_null_rows == non_null_rows
        && sequence.value_generations == value_generations
        && sequence.distinct_is_exact
        && distinct_is_exact
        && sequence.distinct_values == distinct
        && norms.exact
        && norms.l1 == non_null_rows
        && norms.l_infinity <= norms.l2_upper
        && norms.l2_upper <= norms.l1)
        .then_some(norms)
}

fn valid_frequencies(common: &[Frequency], rows: u64) -> bool {
    common.iter().all(|common| {
        common.maximum_error <= common.frequency
            && common.frequency <= rows
            && !common.values.is_empty()
    })
}

fn join_keys<'a>(
    left: &Relation,
    right: &Relation,
    left_table: &'a Table,
    right_table: &'a Table,
    on: &Expr,
) -> Option<Vec<JoinKey<'a>>> {
    let mut keys = Vec::new();
    for conjunct in super::analysis::conjuncts(on) {
        let Expr::Binary {
            op: BinaryOp::Eq,
            left: expression_left,
            right: expression_right,
            ..
        } = conjunct
        else {
            return None;
        };
        let (Expr::SlotRef { slot: first, .. }, Expr::SlotRef { slot: second, .. }) =
            (&**expression_left, &**expression_right)
        else {
            return None;
        };
        let (left_slot, right_slot) =
            if left.produced().contains(*first) && right.produced().contains(*second) {
                (*first, *second)
            } else if left.produced().contains(*second) && right.produced().contains(*first) {
                (*second, *first)
            } else {
                return None;
            };
        let left_column = column_for_slot(left, left_table, left_slot)?;
        let right_column = column_for_slot(right, right_table, right_slot)?;
        if let Some(existing) = keys.iter().find(|key: &&JoinKey<'_>| {
            key.left.schema_id == left_column.schema_id
                || key.right.schema_id == right_column.schema_id
        }) {
            if existing.left.schema_id != left_column.schema_id
                || existing.right.schema_id != right_column.schema_id
            {
                return None;
            }
            continue;
        }
        keys.push(JoinKey {
            left: left_column,
            right: right_column,
        });
    }
    keys.sort_by_key(|key| key.left.schema_id);
    (!keys.is_empty()).then_some(keys)
}

fn column_for_slot<'a>(relation: &Relation, table: &'a Table, slot: SlotId) -> Option<&'a Column> {
    let field = relation
        .output()
        .fields
        .iter()
        .find(|field| field.slot == slot)?;
    table.column(&field.name)
}

fn key_is_unique(table: &Table, columns: &[&Column]) -> bool {
    named_columns_match(table, &table.primary_key, columns)
        || table.indexes.iter().any(|index| {
            index.unique
                && index.is_ready()
                && named_columns_match(table, &table.index_column_names(index), columns)
        })
}

fn named_columns_match<S: AsRef<str>>(table: &Table, names: &[S], columns: &[&Column]) -> bool {
    if names.len() != columns.len() {
        return false;
    }
    let mut named: Vec<_> = names
        .iter()
        .filter_map(|name| table.column(name.as_ref()).map(|column| column.schema_id))
        .collect();
    let mut wanted: Vec<_> = columns.iter().map(|column| column.schema_id).collect();
    named.sort_unstable();
    wanted.sort_unstable();
    named == wanted
}

fn follows_foreign_key(from: &Table, to: &Table, keys: &[JoinKey<'_>], reverse: bool) -> bool {
    from.foreign_keys.iter().any(|foreign_key| {
        foreign_key.ref_table_id == to.id
            && foreign_key.columns.len() == keys.len()
            && foreign_key.ref_columns.len() == keys.len()
            && foreign_key
                .columns
                .iter()
                .zip(&foreign_key.ref_columns)
                .all(|(from_name, to_name)| {
                    keys.iter().any(|key| {
                        let (from_column, to_column) = if reverse {
                            (key.right, key.left)
                        } else {
                            (key.left, key.right)
                        };
                        from_column.name == *from_name && to_column.name == *to_name
                    })
                })
    })
}

fn exact_inner_cardinality(left: &Distribution, right: &Distribution) -> u64 {
    left.common
        .iter()
        .filter_map(|left| {
            right
                .common
                .iter()
                .find(|right| right.values == left.values)
                .map(|right| multiply(left.frequency, right.frequency))
        })
        .fold(0u64, u64::saturating_add)
}

fn exact_left_cardinality(left: &Distribution, right: &Distribution) -> u64 {
    let rows = left
        .common
        .iter()
        .map(|left| {
            right
                .common
                .iter()
                .find(|right| right.values == left.values)
                .map_or(left.frequency, |right| {
                    multiply(left.frequency, right.frequency)
                })
        })
        .fold(0u64, u64::saturating_add);
    rows.saturating_add(left.null_count)
}

fn estimated_inner_cardinality(left: &Distribution, right: &Distribution) -> u64 {
    if left.distinct == 0 || right.distinct == 0 {
        return 0;
    }
    let left_rows = left.rows.saturating_sub(left.null_count);
    let right_rows = right.rows.saturating_sub(right.null_count);
    let estimate = multiply_divide(left_rows, right_rows, left.distinct.max(right.distinct));
    let expected_common = divide_product(
        left_rows,
        right_rows,
        u128::from(left.distinct) * u128::from(right.distinct),
    );
    let correction = left.common.iter().fold(0i128, |correction, left_common| {
        let Some(right_common) = right
            .common
            .iter()
            .find(|right_common| right_common.values == left_common.values)
        else {
            return correction;
        };
        let actual = multiply(left_common.midpoint(), right_common.midpoint());
        correction
            .saturating_add(i128::from(actual))
            .saturating_sub(i128::from(expected_common))
    });
    let maximum = multiply(left_rows, right_rows);
    i128::from(estimate)
        .saturating_add(correction)
        .clamp(0, i128::from(maximum)) as u64
}

fn multiply(left: u64, right: u64) -> u64 {
    u64::try_from(u128::from(left) * u128::from(right)).unwrap_or(u64::MAX)
}

fn multiply_divide(left: u64, right: u64, denominator: u64) -> u64 {
    divide_product(left, right, u128::from(denominator))
}

fn divide_product(left: u64, right: u64, denominator: u128) -> u64 {
    if denominator == 0 {
        return 0;
    }
    u64::try_from((u128::from(left) * u128::from(right)) / denominator).unwrap_or(u64::MAX)
}

fn synopsis_age(stats: &PlannerStats, synopsis: &SynopsisModel) -> Duration {
    stats
        .published_at
        .saturating_sub(Duration::from_micros(synopsis.collected_at_unix_micros))
}

#[cfg(test)]
mod tests {
    use crate::engine::catalog::identity::{
        AccessGeneration, DefinitionGeneration, ExistenceGeneration, LogicalIndexId, SchemaId,
        StorageGeneration, ValueGeneration, WriteProtocolGeneration,
    };
    use crate::engine::catalog::model::{Column, ForeignKey, Index, IndexState, ScalarType, Table};
    use crate::engine::lir::bound;
    use crate::engine::lir::{JoinKind, RootCardinality, SlotId, Type};
    use crate::engine::planner::models::{
        ColumnGroupSynopsis, ColumnSynopsis, DEGREE_SEQUENCE_FORMAT_VERSION, DegreeSequenceNorms,
        DegreeSequenceSegment, DegreeSequenceSynopsis, MostCommonColumnGroup, MostCommonValue,
        SynopsisCoverage, SynopsisModel, SynopsisValue,
    };

    use super::*;

    fn table(physical: &str, schema: u32, column_base: u32) -> Table {
        let columns = ["id", "key", "region", "status"]
            .into_iter()
            .enumerate()
            .map(|(offset, name)| Column {
                id: format!("{physical}-{name}").into(),
                schema_id: SchemaId::new(column_base + offset as u32).unwrap(),
                name: name.into(),
                value_generation: ValueGeneration::from(offset as u64 + 3),
                scalar_type: ScalarType::Text,
                nullable: false,
                format: String::new(),
                insert_default: None,
                missing_value: None,
            })
            .collect();
        Table {
            id: physical.into(),
            schema_id: SchemaId::new(schema).unwrap(),
            name: physical.into(),
            definition_generation: DefinitionGeneration::ZERO,
            existence_generation: ExistenceGeneration::from(2),
            write_protocol_generation: WriteProtocolGeneration::ZERO,
            storage_generation: StorageGeneration::INITIAL,
            columns,
            primary_key: vec!["id".into()],
            indexes: vec![Index {
                id: format!("{physical}-region-status-index").into(),
                logical_id: LogicalIndexId::from(format!("{physical}-region-status")),
                definition_generation: DefinitionGeneration::ZERO,
                access_generation: AccessGeneration::from(1),
                state: IndexState::Ready,
                name: format!("{physical}_region_status_idx"),
                columns: vec!["region".into(), "status".into()],
                column_ids: vec![
                    format!("{physical}-region").into(),
                    format!("{physical}-status").into(),
                ],
                unique: false,
            }],
            foreign_keys: Vec::new(),
            constraints: Vec::new(),
        }
    }

    fn scan(table: Table, scope: &str, first_slot: usize) -> bound::Relation {
        bound::Relation::scan(
            table,
            scope,
            (first_slot..first_slot + 4).map(SlotId).collect(),
        )
    }

    fn column(relation: &bound::Relation, name: &str) -> bound::Expr {
        let field = relation.output().lookup(name).unwrap();
        bound::Expr::slot(
            field.slot,
            format!("{name}.{}", field.slot.0),
            Type::scalar(crate::engine::lir::Kind::Text, false),
        )
    }

    fn join(left: Table, right: Table, pairs: &[(&str, &str)], kind: JoinKind) -> bound::Relation {
        let left = scan(left, "left", 0);
        let right = scan(right, "right", 10);
        let mut predicates = pairs.iter().map(|(left_name, right_name)| {
            bound::Expr::binary(
                BinaryOp::Eq,
                column(&left, left_name),
                column(&right, right_name),
            )
        });
        let first = predicates.next().unwrap();
        let on = predicates.fold(first, |left, right| {
            bound::Expr::binary(BinaryOp::And, left, right)
        });
        bound::Relation::join(left, right, kind, on)
    }

    fn column_synopsis(
        table: &Table,
        name: &str,
        rows: u64,
        null_count: u64,
        distinct: u64,
        exact: bool,
        common: &[(&str, u64)],
    ) -> ColumnSynopsis {
        let column = table.column(name).unwrap();
        ColumnSynopsis {
            column: column.schema_id,
            value_generation: column.value_generation.get(),
            null_fraction: if rows == 0 {
                0.0
            } else {
                null_count as f64 / rows as f64
            },
            null_count,
            distinct,
            distinct_is_exact: exact,
            average_width: 4,
            maximum_width: Some(4),
            minimum: None,
            maximum: None,
            most_common_values: common
                .iter()
                .map(|(value, frequency)| MostCommonValue {
                    value: SynopsisValue::Text((*value).into()),
                    frequency: *frequency,
                    maximum_error: 0,
                })
                .collect(),
            range_distribution: None,
            degree_sequence: None,
        }
    }

    fn group_synopsis(
        table: &Table,
        columns: &[&str],
        common: &[(&[&str], u64)],
    ) -> ColumnGroupSynopsis {
        let mut columns: Vec<_> = columns
            .iter()
            .map(|name| table.column(name).unwrap())
            .collect();
        columns.sort_by_key(|column| column.schema_id);
        ColumnGroupSynopsis {
            columns: columns.iter().map(|column| column.schema_id).collect(),
            value_generations: columns
                .iter()
                .map(|column| column.value_generation.get())
                .collect(),
            null_count: 0,
            distinct: common.len() as u64,
            distinct_is_exact: true,
            most_common_values: common
                .iter()
                .map(|(values, frequency)| MostCommonColumnGroup {
                    values: values
                        .iter()
                        .map(|value| SynopsisValue::Text((*value).into()))
                        .collect(),
                    frequency: *frequency,
                    maximum_error: 0,
                })
                .collect(),
            degree_sequence: None,
        }
    }

    fn degree_synopsis(
        table: &Table,
        name: &str,
        rows: u64,
        frequencies: &[u64],
        l2_upper: u64,
    ) -> DegreeSequenceSynopsis {
        let column = table.column(name).unwrap();
        DegreeSequenceSynopsis {
            format_version: DEGREE_SEQUENCE_FORMAT_VERSION,
            coverage: SynopsisCoverage::Complete,
            sample_size: rows,
            value_generations: vec![column.value_generation.get()],
            collected_row_count: rows,
            non_null_rows: rows,
            distinct_values: frequencies.len() as u64,
            distinct_is_exact: true,
            norms: DegreeSequenceNorms {
                l1: rows,
                l2_upper,
                l_infinity: frequencies.first().copied().unwrap_or(0),
                exact: true,
            },
            segments: frequencies
                .iter()
                .enumerate()
                .map(|(rank, frequency)| DegreeSequenceSegment {
                    rank_start: rank as u64,
                    rank_end: rank as u64 + 1,
                    frequency_upper: *frequency,
                })
                .collect(),
        }
    }

    fn group_degree_synopsis(
        table: &Table,
        names: &[&str],
        rows: u64,
        frequencies: &[u64],
        l2_upper: u64,
    ) -> DegreeSequenceSynopsis {
        let mut sequence = degree_synopsis(table, names[0], rows, frequencies, l2_upper);
        let mut columns: Vec<_> = names
            .iter()
            .map(|name| table.column(name).unwrap())
            .collect();
        columns.sort_by_key(|column| column.schema_id);
        sequence.value_generations = columns
            .iter()
            .map(|column| column.value_generation.get())
            .collect();
        sequence
    }

    fn synopsis(
        table: &Table,
        rows: u64,
        columns: Vec<ColumnSynopsis>,
        column_groups: Vec<ColumnGroupSynopsis>,
    ) -> SynopsisModel {
        SynopsisModel {
            table: table.schema_id,
            observed_rows: rows,
            coverage: SynopsisCoverage::Complete,
            sample_size: rows,
            changes_since_collection: 0,
            table_existence_generation: table.existence_generation.get(),
            collected_at_unix_micros: 0,
            catalog_version: 1,
            columns,
            column_groups,
            predicate_conditioned_degrees: Vec::new(),
        }
    }

    fn stats(left: SynopsisModel, right: SynopsisModel) -> PlannerStats {
        let mut stats = PlannerStats::empty();
        stats.synopsis_models.insert(left.table, left);
        stats.synopsis_models.insert(right.table, right);
        stats
    }

    #[test]
    fn complete_mcv_domains_estimate_skewed_inner_and_left_joins_exactly() {
        let left = table("left", 10, 11);
        let right = table("right", 20, 21);
        let stats = stats(
            synopsis(
                &left,
                100,
                vec![column_synopsis(
                    &left,
                    "key",
                    100,
                    0,
                    2,
                    true,
                    &[("a", 90), ("b", 10)],
                )],
                Vec::new(),
            ),
            synopsis(
                &right,
                100,
                vec![column_synopsis(
                    &right,
                    "key",
                    100,
                    0,
                    2,
                    true,
                    &[("a", 10), ("b", 90)],
                )],
                Vec::new(),
            ),
        );

        let inner = join(
            left.clone(),
            right.clone(),
            &[("key", "key")],
            JoinKind::Inner,
        );
        let estimate = super::estimate(
            &stats,
            match &inner.node {
                RelationNode::Join { left, .. } => left,
                _ => unreachable!(),
            },
            match &inner.node {
                RelationNode::Join { right, .. } => right,
                _ => unreachable!(),
            },
            JoinKind::Inner,
            match &inner.node {
                RelationNode::Join { on, .. } => on,
                _ => unreachable!(),
            },
        )
        .unwrap();
        assert_eq!(estimate.source, EstimateSource::Join);
        assert_eq!(estimate.cardinality, 1_800);
        assert_eq!(estimate.interval, EstimateInterval::Exact);

        let left_join = join(left, right, &[("key", "key")], JoinKind::Left);
        let estimate =
            crate::engine::planner::estimator::Estimator::new(&stats).bound_relation(&left_join);
        assert_eq!(estimate.cardinality, 1_800);
        assert_eq!(estimate.interval, EstimateInterval::Exact);
    }

    #[test]
    fn ndv_estimation_reports_uncertainty_without_uniform_domain_proof() {
        let left = table("left", 10, 11);
        let right = table("right", 20, 21);
        let stats = stats(
            synopsis(
                &left,
                1_000,
                vec![column_synopsis(&left, "key", 1_000, 0, 100, true, &[])],
                Vec::new(),
            ),
            synopsis(
                &right,
                2_000,
                vec![column_synopsis(&right, "key", 2_000, 0, 200, true, &[])],
                Vec::new(),
            ),
        );
        let relation = join(left, right, &[("key", "key")], JoinKind::Inner);

        let estimate =
            crate::engine::planner::estimator::Estimator::new(&stats).bound_relation(&relation);
        assert_eq!(estimate.cardinality, 10_000);
        assert_eq!(estimate.source, EstimateSource::Join);
        assert_eq!(
            estimate.interval,
            EstimateInterval::AttributedRange {
                lower_bound: 0,
                upper_bound: 2_000_000,
                lower_source: EstimateBoundSource::Structural,
                central_source: EstimateBoundSource::FactorizedDistribution,
                upper_source: EstimateBoundSource::CartesianProduct,
                drift_widened: false,
            }
        );
    }

    #[test]
    fn degree_norms_bound_a_heavy_key_join() {
        let left = table("left", 10, 11);
        let right = table("right", 20, 21);
        let mut left_column = column_synopsis(&left, "key", 1_000, 0, 101, true, &[]);
        left_column.degree_sequence = Some(degree_synopsis(
            &left,
            "key",
            1_000,
            &[900]
                .into_iter()
                .chain(std::iter::repeat_n(1, 100))
                .collect::<Vec<_>>(),
            901,
        ));
        let mut right_column = column_synopsis(&right, "key", 1_000, 0, 101, true, &[]);
        right_column.degree_sequence = Some(degree_synopsis(
            &right,
            "key",
            1_000,
            &[500]
                .into_iter()
                .chain(std::iter::repeat_n(5, 100))
                .collect::<Vec<_>>(),
            503,
        ));
        let mut stats = stats(
            synopsis(&left, 1_000, vec![left_column], Vec::new()),
            synopsis(&right, 1_000, vec![right_column], Vec::new()),
        );
        let relation = join(left.clone(), right, &[("key", "key")], JoinKind::Inner);

        let estimate =
            crate::engine::planner::estimator::Estimator::new(&stats).bound_relation(&relation);
        assert_eq!(estimate.cardinality, 9_900);
        assert_eq!(
            estimate.interval,
            EstimateInterval::AttributedRange {
                lower_bound: 0,
                upper_bound: 453_203,
                lower_source: EstimateBoundSource::Structural,
                central_source: EstimateBoundSource::FactorizedDistribution,
                upper_source: EstimateBoundSource::DegreeSequenceNorms,
                drift_widened: false,
            }
        );
        let json = serde_json::to_value(estimate).unwrap();
        assert_eq!(
            json["interval"]["central_source"],
            "factorized_distribution"
        );
        assert_eq!(json["interval"]["upper_source"], "degree_sequence_norms");

        stats
            .synopsis_models
            .get_mut(&left.schema_id)
            .unwrap()
            .changes_since_collection = 3;
        let estimate =
            crate::engine::planner::estimator::Estimator::new(&stats).bound_relation(&relation);
        assert!(matches!(
            estimate.interval,
            EstimateInterval::AttributedRange {
                upper_bound: 456_203,
                upper_source: EstimateBoundSource::DegreeSequenceNorms,
                drift_widened: true,
                ..
            }
        ));
        stats
            .synopsis_models
            .get_mut(&left.schema_id)
            .unwrap()
            .changes_since_collection = 0;

        let sequence = stats
            .synopsis_models
            .get_mut(&left.schema_id)
            .unwrap()
            .columns[0]
            .degree_sequence
            .as_mut()
            .unwrap();
        sequence.value_generations[0] -= 1;
        let estimate =
            crate::engine::planner::estimator::Estimator::new(&stats).bound_relation(&relation);
        assert!(matches!(
            estimate.interval,
            EstimateInterval::AttributedRange {
                upper_bound: 1_000_000,
                upper_source: EstimateBoundSource::CartesianProduct,
                ..
            }
        ));
    }

    #[test]
    fn self_join_uses_the_same_degree_norm_with_two_scopes() {
        let table = table("items", 10, 11);
        let mut column = column_synopsis(&table, "key", 100, 0, 2, true, &[]);
        column.degree_sequence = Some(degree_synopsis(&table, "key", 100, &[90, 10], 91));
        let synopsis = synopsis(&table, 100, vec![column], Vec::new());
        let mut stats = PlannerStats::empty();
        stats.synopsis_models.insert(table.schema_id, synopsis);
        let relation = join(table.clone(), table, &[("key", "key")], JoinKind::Inner);

        let estimate =
            crate::engine::planner::estimator::Estimator::new(&stats).bound_relation(&relation);
        assert!(matches!(
            estimate.interval,
            EstimateInterval::AttributedRange {
                upper_bound: 8_281,
                upper_source: EstimateBoundSource::DegreeSequenceNorms,
                ..
            }
        ));
    }

    #[test]
    fn complete_disjoint_domains_have_an_exact_zero_join() {
        let left = table("left", 10, 11);
        let right = table("right", 20, 21);
        let stats = stats(
            synopsis(
                &left,
                10,
                vec![column_synopsis(&left, "key", 10, 0, 1, true, &[("a", 10)])],
                Vec::new(),
            ),
            synopsis(
                &right,
                20,
                vec![column_synopsis(&right, "key", 20, 0, 1, true, &[("b", 20)])],
                Vec::new(),
            ),
        );
        let relation = join(left, right, &[("key", "key")], JoinKind::Inner);
        let estimate =
            crate::engine::planner::estimator::Estimator::new(&stats).bound_relation(&relation);
        assert_eq!(estimate.cardinality, 0);
        assert_eq!(estimate.interval, EstimateInterval::Exact);
    }

    #[test]
    fn shared_mcv_bounds_refine_the_uniform_join_estimate() {
        let left = table("left", 10, 11);
        let right = table("right", 20, 21);
        let mut left_synopsis = synopsis(
            &left,
            1_000,
            vec![column_synopsis(
                &left,
                "key",
                1_000,
                0,
                100,
                true,
                &[("hot", 100)],
            )],
            Vec::new(),
        );
        left_synopsis.columns[0].most_common_values[0].maximum_error = 20;
        let mut right_synopsis = synopsis(
            &right,
            2_000,
            vec![column_synopsis(
                &right,
                "key",
                2_000,
                0,
                200,
                true,
                &[("hot", 50)],
            )],
            Vec::new(),
        );
        right_synopsis.columns[0].most_common_values[0].maximum_error = 10;
        let stats = stats(left_synopsis, right_synopsis);
        let relation = join(left, right, &[("key", "key")], JoinKind::Inner);

        let estimate =
            crate::engine::planner::estimator::Estimator::new(&stats).bound_relation(&relation);
        assert_eq!(estimate.cardinality, 13_950);
        assert_eq!(estimate.source, EstimateSource::Join);
        assert!(matches!(
            estimate.interval,
            EstimateInterval::AttributedRange {
                central_source: EstimateBoundSource::FactorizedDistribution,
                ..
            }
        ));
    }

    #[test]
    fn complete_domains_count_unmatched_left_rows_once() {
        let left = table("left", 10, 11);
        let right = table("right", 20, 21);
        let stats = stats(
            synopsis(
                &left,
                100,
                vec![column_synopsis(
                    &left,
                    "key",
                    100,
                    0,
                    2,
                    true,
                    &[("a", 90), ("b", 10)],
                )],
                Vec::new(),
            ),
            synopsis(
                &right,
                2,
                vec![column_synopsis(&right, "key", 2, 0, 1, true, &[("a", 2)])],
                Vec::new(),
            ),
        );
        let relation = join(left, right, &[("key", "key")], JoinKind::Left);

        let estimate =
            crate::engine::planner::estimator::Estimator::new(&stats).bound_relation(&relation);
        assert_eq!(estimate.cardinality, 190);
        assert_eq!(estimate.interval, EstimateInterval::Exact);
    }

    #[test]
    fn composite_join_keys_use_column_groups_in_join_key_order() {
        let left = table("left", 10, 11);
        let right = table("right", 20, 21);
        let stats = stats(
            synopsis(
                &left,
                100,
                Vec::new(),
                vec![group_synopsis(
                    &left,
                    &["region", "status"],
                    &[(&["eu", "open"], 50), (&["us", "closed"], 50)],
                )],
            ),
            synopsis(
                &right,
                100,
                Vec::new(),
                vec![group_synopsis(
                    &right,
                    &["region", "status"],
                    &[(&["open", "eu"], 10), (&["closed", "us"], 90)],
                )],
            ),
        );
        let relation = join(
            left,
            right,
            &[("region", "status"), ("status", "region")],
            JoinKind::Inner,
        );

        let estimate =
            crate::engine::planner::estimator::Estimator::new(&stats).bound_relation(&relation);
        assert_eq!(estimate.cardinality, 5_000);
        assert_eq!(estimate.interval, EstimateInterval::Exact);
    }

    #[test]
    fn composite_degree_norms_bound_a_join_without_complete_domains() {
        let left = table("left", 10, 11);
        let right = table("right", 20, 21);
        let mut left_group = group_synopsis(&left, &["region", "status"], &[]);
        left_group.distinct = 2;
        left_group.degree_sequence = Some(group_degree_synopsis(
            &left,
            &["region", "status"],
            100,
            &[90, 10],
            91,
        ));
        let mut right_group = group_synopsis(&right, &["region", "status"], &[]);
        right_group.distinct = 2;
        right_group.degree_sequence = Some(group_degree_synopsis(
            &right,
            &["region", "status"],
            100,
            &[50, 50],
            71,
        ));
        let stats = stats(
            synopsis(&left, 100, Vec::new(), vec![left_group]),
            synopsis(&right, 100, Vec::new(), vec![right_group]),
        );
        let relation = join(
            left,
            right,
            &[("region", "region"), ("status", "status")],
            JoinKind::Inner,
        );
        let estimate =
            crate::engine::planner::estimator::Estimator::new(&stats).bound_relation(&relation);
        assert!(matches!(
            estimate.interval,
            EstimateInterval::AttributedRange {
                upper_bound: 5_000,
                upper_source: EstimateBoundSource::DegreeSequenceNorms,
                ..
            }
        ));
    }

    #[test]
    fn unique_and_foreign_key_constraints_produce_exact_cardinality() {
        let mut left = table("left", 10, 11);
        let right = table("right", 20, 21);
        left.foreign_keys.push(ForeignKey {
            id: "left-right-fk".into(),
            name: "left_right_fk".into(),
            columns: vec!["key".into()],
            ref_table_id: right.id.clone(),
            ref_columns: vec!["id".into()],
        });
        let stats = stats(
            synopsis(
                &left,
                100,
                vec![column_synopsis(&left, "key", 100, 0, 80, true, &[])],
                Vec::new(),
            ),
            synopsis(
                &right,
                80,
                vec![column_synopsis(&right, "id", 80, 0, 80, true, &[])],
                Vec::new(),
            ),
        );

        let inner = join(
            left.clone(),
            right.clone(),
            &[("key", "id")],
            JoinKind::Inner,
        );
        let estimate =
            crate::engine::planner::estimator::Estimator::new(&stats).bound_relation(&inner);
        assert_eq!(estimate.cardinality, 100);
        assert_eq!(estimate.interval, EstimateInterval::Exact);

        let left_join = join(left, right, &[("key", "id")], JoinKind::Left);
        let estimate =
            crate::engine::planner::estimator::Estimator::new(&stats).bound_relation(&left_join);
        assert_eq!(estimate.cardinality, 100);
        assert_eq!(estimate.interval, EstimateInterval::Exact);
    }

    #[test]
    fn composite_foreign_keys_use_declared_groups_without_secondary_indexes() {
        let mut left = table("left", 10, 11);
        let mut right = table("right", 20, 21);
        left.indexes.clear();
        right.indexes.clear();
        right.primary_key = vec!["region".into(), "status".into()];
        left.foreign_keys.push(ForeignKey {
            id: "left-right-fk".into(),
            name: "left_right_fk".into(),
            columns: vec!["region".into(), "status".into()],
            ref_table_id: right.id.clone(),
            ref_columns: vec!["region".into(), "status".into()],
        });
        let mut left_group = group_synopsis(&left, &["region", "status"], &[]);
        left_group.distinct = 80;
        let mut right_group = group_synopsis(&right, &["region", "status"], &[]);
        right_group.distinct = 80;
        let stats = stats(
            synopsis(&left, 100, Vec::new(), vec![left_group]),
            synopsis(&right, 80, Vec::new(), vec![right_group]),
        );
        let relation = join(
            left,
            right,
            &[("region", "region"), ("status", "status")],
            JoinKind::Inner,
        );

        let estimate =
            crate::engine::planner::estimator::Estimator::new(&stats).bound_relation(&relation);
        assert_eq!(estimate.cardinality, 100);
        assert_eq!(estimate.interval, EstimateInterval::Exact);
    }

    #[test]
    fn drift_widens_join_bounds_and_stale_columns_remove_the_candidate() {
        let left = table("left", 10, 11);
        let right = table("right", 20, 21);
        let mut left_synopsis = synopsis(
            &left,
            100,
            vec![column_synopsis(
                &left,
                "key",
                100,
                0,
                2,
                true,
                &[("a", 90), ("b", 10)],
            )],
            Vec::new(),
        );
        left_synopsis.changes_since_collection = 3;
        let right_synopsis = synopsis(
            &right,
            100,
            vec![column_synopsis(
                &right,
                "key",
                100,
                0,
                2,
                true,
                &[("a", 10), ("b", 90)],
            )],
            Vec::new(),
        );
        let mut stats = stats(left_synopsis, right_synopsis);
        let relation = join(
            left.clone(),
            right.clone(),
            &[("key", "key")],
            JoinKind::Inner,
        );
        let estimate =
            crate::engine::planner::estimator::Estimator::new(&stats).bound_relation(&relation);
        assert_eq!(estimate.cardinality, 1_800);
        assert_eq!(estimate.changes_since_collection, 3);
        assert_eq!(
            estimate.interval,
            EstimateInterval::AttributedRange {
                lower_bound: 0,
                upper_bound: 2_100,
                lower_source: EstimateBoundSource::Structural,
                central_source: EstimateBoundSource::CompleteDomain,
                upper_source: EstimateBoundSource::CompleteDomain,
                drift_widened: true,
            }
        );

        stats
            .synopsis_models
            .get_mut(&left.schema_id)
            .unwrap()
            .columns[0]
            .value_generation -= 1;
        assert_eq!(
            crate::engine::planner::estimator::Estimator::new(&stats)
                .bound_relation(&relation)
                .source,
            EstimateSource::Structural
        );
    }

    #[test]
    fn residual_join_predicates_use_a_structural_product_bound() {
        let left = table("left", 10, 11);
        let right = table("right", 20, 21);
        let left_scan = scan(left.clone(), "left", 0);
        let right_scan = scan(right.clone(), "right", 10);
        let on = bound::Expr::binary(
            BinaryOp::Gte,
            column(&left_scan, "key"),
            column(&right_scan, "key"),
        );
        let relation = bound::Relation::join(left_scan, right_scan, JoinKind::Inner, on);
        let stats = stats(
            synopsis(
                &left,
                10,
                vec![column_synopsis(&left, "key", 10, 0, 2, true, &[])],
                Vec::new(),
            ),
            synopsis(
                &right,
                10,
                vec![column_synopsis(&right, "key", 10, 0, 2, true, &[])],
                Vec::new(),
            ),
        );

        assert_eq!(
            crate::engine::planner::estimator::Estimator::new(&stats)
                .bound_relation(&relation)
                .source,
            EstimateSource::Structural
        );
    }

    #[test]
    fn join_estimates_reach_the_plan_observability_surface() {
        let left = table("left", 10, 11);
        let right = table("right", 20, 21);
        let stats = stats(
            synopsis(
                &left,
                10,
                vec![column_synopsis(&left, "key", 10, 0, 1, true, &[("a", 10)])],
                Vec::new(),
            ),
            synopsis(
                &right,
                5,
                vec![column_synopsis(&right, "key", 5, 0, 1, true, &[("a", 5)])],
                Vec::new(),
            ),
        );
        let root = join(left, right, &[("key", "key")], JoinKind::Inner);
        let query = bound::Query {
            root,
            cardinality: RootCardinality::Many,
            bindings: Vec::new(),
            next_slot: SlotId(20),
        };
        let planned = crate::engine::planner::plan_query_with_context(
            &query,
            crate::engine::planner::PlanOptions::default(),
            crate::engine::planner::PlanningContext {
                statistics: Some(&stats),
            },
        );
        let mut view = crate::engine::planner::explain::PlanView::new(&planned.plan);
        view.annotate_estimates(&stats, planned.estimate, &query, &planned.plan);
        let json = serde_json::to_value(view).unwrap();

        assert_eq!(json["estimates"][0]["source"], "join");
        assert_eq!(json["estimates"][0]["cardinality"], 50);
        assert!(
            json["estimates"]
                .as_array()
                .unwrap()
                .iter()
                .any(|estimate| {
                    estimate["target"] == "relation" && estimate["relation"].is_string()
                })
        );
    }
}
