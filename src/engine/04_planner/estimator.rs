//! Cardinality estimation: candidate estimators arbitrated over the model
//! repository. There is no fixed precedence. Arbitration weighs uncertainty,
//! drift, age, and sample size. Every candidate can be absent. The planner and
//! EXPLAIN consume [`Estimate`]s; they never touch model structures.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use crate::engine::lir::fingerprint::Fingerprint;

use super::models::{
    ColumnSynopsis, PlannerStats, RANGE_DISTRIBUTION_FORMAT_VERSION, RangeDistribution,
    RangeDistributionBucket, SynopsisCountBounds, SynopsisCoverage, SynopsisModel, SynopsisValue,
};

/// Read access to the current published model snapshot. Implemented by the
/// collection runner; the engine holds it behind this seam so the planner
/// never depends on the scheduler.
pub trait StatisticsProvider: Send + Sync {
    fn planning_stats(&self) -> Arc<PlannerStats>;

    fn diagnostic_stats(&self) -> Arc<PlannerStats> {
        self.planning_stats()
    }

    /// How the observation relay is faring, for reporting only. The planner
    /// never consults it: whether evidence arrived by relay or was observed
    /// locally does not change what it means.
    fn relay(&self) -> crate::scheduler::statistics::RelayReport {
        crate::scheduler::statistics::RelayReport::default()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EstimateSource {
    Structural,
    FamilyFeedback,
    Synopsis,
    Distribution,
    Mcv,
    ColumnGroup,
    Join,
    Heuristic,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EstimateBoundSource {
    Structural,
    CompleteDomain,
    KeyConstraint,
    FactorizedDistribution,
    DegreeSequenceNorms,
    PredicateConditionedDistribution,
    PredicateConditionedDegreeNorms,
    MultiwayDegreeNorms,
    PredicateConditionedMultiwayDegreeNorms,
    CartesianProduct,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EstimateInterval {
    Exact,
    Confidence {
        lower_bound: u64,
        upper_bound: u64,
        confidence_level: f64,
    },
    LowerBound {
        lower_bound: u64,
    },
    Range {
        lower_bound: u64,
        upper_bound: u64,
    },
    AttributedRange {
        lower_bound: u64,
        upper_bound: u64,
        lower_source: EstimateBoundSource,
        central_source: EstimateBoundSource,
        upper_source: EstimateBoundSource,
        drift_widened: bool,
    },
    Unknown,
}

/// A cardinality estimate with explicit uncertainty semantics.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Estimate {
    pub cardinality: u64,
    pub interval: EstimateInterval,
    pub source: EstimateSource,
    pub sample_size: u64,
    pub changes_since_collection: u64,
    #[serde(serialize_with = "serialize_age_micros")]
    pub age: Duration,
}

fn serialize_age_micros<S: serde::Serializer>(
    age: &Duration,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_u64(age.as_micros() as u64)
}

pub fn for_query(stats: &PlannerStats, query: &crate::engine::lir::bound::Query) -> Estimate {
    Estimator::new(stats).bound_relation(&query.root)
}

pub struct Estimator<'a> {
    stats: &'a PlannerStats,
}

impl<'a> Estimator<'a> {
    pub fn new(stats: &'a PlannerStats) -> Self {
        Self { stats }
    }

    /// Arbitrated estimate for a relation family, optionally grounded by the
    /// table its root scans. Feedback gathered under a different semantic
    /// stamp is withheld rather than aged: the shape is the same but what
    /// it denotes is not.
    pub fn relation(
        &self,
        _family: &Fingerprint,
        table: Option<&crate::engine::catalog::model::Table>,
        _stamp: super::models::DependencyStamp,
    ) -> Estimate {
        let mut candidates = Vec::with_capacity(2);
        if let Some(table) = table
            && let Some(estimate) = self.scan_for_table(table)
        {
            candidates.push(estimate);
        }
        candidates.push(Self::heuristic());
        arbitrate(candidates).expect("the heuristic candidate is present")
    }

    pub fn family_feedback(
        &self,
        family: &Fingerprint,
        stamp: super::models::DependencyStamp,
    ) -> Option<Estimate> {
        let model = self.stats.feedback_models.get(family)?;
        if model.stamp.semantic != stamp.semantic {
            return None;
        }
        Some(Estimate {
            cardinality: model.rows_p50_upper_bound,
            interval: EstimateInterval::Unknown,
            source: EstimateSource::FamilyFeedback,
            sample_size: model.retained_executions,
            changes_since_collection: 0,
            age: self.stats.published_at.saturating_sub(model.last_seen),
        })
    }

    pub fn bound_relation(&self, relation: &crate::engine::lir::bound::Relation) -> Estimate {
        use crate::engine::lir::bound::RelationNode;

        if let Some(constraints) = super::analysis::extract_constraints(relation) {
            let table = constraints.scan.scan_table();
            if !table.primary_key.is_empty()
                && table.primary_key.iter().all(|column| {
                    constraints
                        .columns
                        .get(column)
                        .and_then(|domain| domain.equality.as_ref())
                        .is_some()
                })
            {
                return Estimate {
                    cardinality: 1,
                    interval: EstimateInterval::Range {
                        lower_bound: 0,
                        upper_bound: 1,
                    },
                    source: EstimateSource::Structural,
                    sample_size: 0,
                    changes_since_collection: 0,
                    age: Duration::ZERO,
                };
            }
        }
        if let Some(equalities) = literal_equalities(relation) {
            if let Some(estimate) = self.column_group_equality(&equalities) {
                return estimate;
            }
            if let Some(estimate) = self.mcv_equality(&equalities) {
                return estimate;
            }
        }
        if let Some(estimate) = self.literal_range(relation) {
            return estimate;
        }
        if let Some((table, column)) = literal_null_test(relation)
            && let Some(estimate) = self.null_count(table, column)
        {
            return estimate;
        }

        match &relation.node {
            RelationNode::Scan { table, .. } => {
                self.scan_for_table(table).unwrap_or_else(Self::heuristic)
            }
            RelationNode::Rows { values, .. } => Self::exact(values.len() as u64),
            RelationNode::Project { input, .. } | RelationNode::Order { input, .. } => {
                self.bound_relation(input)
            }
            RelationNode::Slice {
                input,
                offset,
                limit,
            } => {
                let input = self.bound_relation(input);
                let after_offset = input.cardinality.saturating_sub(*offset as u64);
                let cardinality = limit
                    .map(|limit| after_offset.min(limit as u64))
                    .unwrap_or(after_offset);
                let interval = match (input.interval, limit) {
                    (EstimateInterval::Exact, _) => EstimateInterval::Exact,
                    (EstimateInterval::LowerBound { lower_bound }, Some(limit))
                        if lower_bound >= (*offset as u64).saturating_add(*limit as u64) =>
                    {
                        EstimateInterval::Exact
                    }
                    (_, Some(limit)) => EstimateInterval::Range {
                        lower_bound: 0,
                        upper_bound: *limit as u64,
                    },
                    _ => EstimateInterval::Unknown,
                };
                Estimate {
                    cardinality,
                    interval,
                    source: EstimateSource::Structural,
                    sample_size: input.sample_size,
                    changes_since_collection: input.changes_since_collection,
                    age: input.age,
                }
            }
            RelationNode::Aggregate { groups, .. } if groups.is_empty() => Self::exact(1),
            RelationNode::Concatenate { inputs, .. } => {
                let estimates: Vec<_> = inputs
                    .iter()
                    .map(|input| self.bound_relation(input))
                    .collect();
                if estimates
                    .iter()
                    .all(|estimate| estimate.interval == EstimateInterval::Exact)
                {
                    Self::exact(
                        estimates
                            .iter()
                            .map(|estimate| estimate.cardinality)
                            .fold(0u64, u64::saturating_add),
                    )
                } else {
                    Self::heuristic()
                }
            }
            RelationNode::Join {
                left,
                right,
                kind,
                on,
            } => super::join_estimator::estimate(self.stats, left, right, *kind, on)
                .unwrap_or_else(|| self.structural_join_bound(left, right, *kind)),
            _ => Self::heuristic(),
        }
    }

    fn literal_range(&self, relation: &crate::engine::lir::bound::Relation) -> Option<Estimate> {
        let constraints = super::analysis::extract_constraints(relation)?;
        if constraints.columns.len() != 1 {
            return None;
        }
        let (column, domain) = constraints.columns.iter().next()?;
        if domain.equality.is_some() || (domain.lower.is_none() && domain.upper.is_none()) {
            return None;
        }
        self.range_for_column(
            constraints.scan.scan_table(),
            column,
            domain.lower.as_ref(),
            domain.upper.as_ref(),
        )
    }

    pub(super) fn scan_for_table(
        &self,
        table: &crate::engine::catalog::model::Table,
    ) -> Option<Estimate> {
        let synopsis = self.stats.synopsis_models.get(&table.schema_id)?;
        if synopsis.table_existence_generation != table.existence_generation.get() {
            return None;
        }
        Some(self.synopsis_estimate(synopsis))
    }

    pub(super) fn access_prefix(
        &self,
        table: &crate::engine::catalog::model::Table,
        columns: &[&str],
        values: &[super::analysis::ConstValue],
        range: Option<&super::physical::RangeSpec>,
    ) -> Option<Estimate> {
        if columns.len() != values.len() || (columns.is_empty() && range.is_none()) {
            return None;
        }
        let values = columns
            .iter()
            .zip(values)
            .map(|(name, value)| {
                let column = table.columns.iter().find(|column| column.name == *name)?;
                let super::analysis::ConstValue::Literal(value) = value else {
                    return None;
                };
                Some((column, value))
            })
            .collect::<Option<Vec<_>>>()?;
        let equality_estimate = if values.is_empty() {
            None
        } else {
            let equalities = LiteralEqualities { table, values };
            let mut estimate = self
                .column_group_equality(&equalities)
                .or_else(|| self.mcv_equality(&equalities));
            if estimate.is_none() && equalities.values.len() > 1 {
                let first = LiteralEqualities {
                    table,
                    values: vec![equalities.values[0]],
                };
                estimate = self.mcv_equality(&first).map(as_upper_bound);
            }
            estimate
        };
        let range_estimate = range.and_then(|range| {
            self.range_for_column(
                table,
                &range.column,
                range.lower.as_ref(),
                range.upper.as_ref(),
            )
        });
        match (equality_estimate, range_estimate, range.is_some()) {
            (Some(equality), Some(range), true) => Some(intersect_upper_bounds(equality, range)),
            (Some(equality), None, true) => Some(as_upper_bound(equality)),
            (Some(equality), _, false) => Some(equality),
            (None, Some(range), true) => Some(range),
            _ => None,
        }
    }

    fn range_for_column(
        &self,
        table: &crate::engine::catalog::model::Table,
        column_name: &str,
        lower: Option<&super::analysis::RangeBound>,
        upper: Option<&super::analysis::RangeBound>,
    ) -> Option<Estimate> {
        let catalog_column = table.column(column_name)?;
        if range_is_empty(lower, upper)? {
            return Some(Estimate {
                cardinality: 0,
                interval: EstimateInterval::Exact,
                source: EstimateSource::Structural,
                sample_size: 0,
                changes_since_collection: 0,
                age: Duration::ZERO,
            });
        }
        let synopsis = self.stats.synopsis_models.get(&table.schema_id)?;
        if synopsis.table_existence_generation != table.existence_generation.get() {
            return None;
        }
        let column = synopsis.columns.iter().find(|column| {
            column.column == catalog_column.schema_id
                && column.value_generation == catalog_column.value_generation.get()
        })?;
        let distribution = column.range_distribution.as_ref()?;
        if !valid_range_distribution(synopsis, column, distribution) {
            return None;
        }
        let bounds = distribution_range_bounds(distribution, lower, upper)?;
        let changes = synopsis.changes_since_collection;
        let lower_bound = bounds.lower_bound.saturating_sub(changes);
        let table_upper = synopsis.observed_rows.saturating_add(changes);
        let upper_bound = bounds
            .upper_bound
            .map(|upper| upper.saturating_add(changes).min(table_upper));
        let cardinality = upper_bound.map_or(lower_bound, |upper| {
            lower_bound.saturating_add(upper.saturating_sub(lower_bound) / 2)
        });
        let interval = match upper_bound {
            Some(upper) if upper == lower_bound => EstimateInterval::Exact,
            Some(upper_bound) => EstimateInterval::Range {
                lower_bound,
                upper_bound,
            },
            None => EstimateInterval::LowerBound { lower_bound },
        };
        Some(Estimate {
            cardinality,
            interval,
            source: EstimateSource::Distribution,
            sample_size: distribution.sample_size,
            changes_since_collection: changes,
            age: self
                .stats
                .published_at
                .saturating_sub(Duration::from_micros(synopsis.collected_at_unix_micros)),
        })
    }

    pub(super) fn average_row_width(
        &self,
        table: &crate::engine::catalog::model::Table,
    ) -> Option<u64> {
        let synopsis = self.stats.synopsis_models.get(&table.schema_id)?;
        if synopsis.table_existence_generation != table.existence_generation.get() {
            return None;
        }
        table.columns.iter().try_fold(0u64, |total, column| {
            let model = synopsis.columns.iter().find(|model| {
                model.column == column.schema_id
                    && model.value_generation == column.value_generation.get()
            })?;
            Some(total.saturating_add(model.average_width))
        })
    }

    pub(super) fn maximum_row_width(
        &self,
        table: &crate::engine::catalog::model::Table,
    ) -> Option<u64> {
        let synopsis = self.stats.synopsis_models.get(&table.schema_id)?;
        if synopsis.table_existence_generation != table.existence_generation.get()
            || synopsis.coverage != SynopsisCoverage::Complete
            || synopsis.changes_since_collection != 0
        {
            return None;
        }
        table.columns.iter().try_fold(0u64, |total, column| {
            self.maximum_column_width(table, &column.name)
                .map(|width| total.saturating_add(width))
        })
    }

    pub(super) fn maximum_column_width(
        &self,
        table: &crate::engine::catalog::model::Table,
        column_name: &str,
    ) -> Option<u64> {
        let catalog_column = table.column(column_name)?;
        let synopsis = self.stats.synopsis_models.get(&table.schema_id)?;
        if synopsis.table_existence_generation != table.existence_generation.get()
            || synopsis.coverage != SynopsisCoverage::Complete
            || synopsis.changes_since_collection != 0
        {
            return None;
        }
        synopsis
            .columns
            .iter()
            .find(|model| {
                model.column == catalog_column.schema_id
                    && model.value_generation == catalog_column.value_generation.get()
            })
            .and_then(|model| model.maximum_width)
    }

    fn null_count(
        &self,
        table: &crate::engine::catalog::model::Table,
        column: &crate::engine::catalog::model::Column,
    ) -> Option<Estimate> {
        let synopsis = self.stats.synopsis_models.get(&table.schema_id)?;
        if synopsis.table_existence_generation != table.existence_generation.get()
            || synopsis.coverage != SynopsisCoverage::Complete
        {
            return None;
        }
        let column = synopsis.columns.iter().find(|candidate| {
            candidate.column == column.schema_id
                && candidate.value_generation == column.value_generation.get()
        })?;
        let changes = synopsis.changes_since_collection;
        Some(Estimate {
            cardinality: column.null_count,
            interval: if changes == 0 {
                EstimateInterval::Exact
            } else {
                EstimateInterval::Range {
                    lower_bound: column.null_count.saturating_sub(changes),
                    upper_bound: column.null_count.saturating_add(changes),
                }
            },
            source: EstimateSource::Synopsis,
            sample_size: synopsis.sample_size,
            changes_since_collection: changes,
            age: self
                .stats
                .published_at
                .saturating_sub(Duration::from_micros(synopsis.collected_at_unix_micros)),
        })
    }

    fn mcv_equality(&self, equalities: &LiteralEqualities<'_>) -> Option<Estimate> {
        let [(catalog_column, value)] = equalities.values.as_slice() else {
            return None;
        };
        let table = equalities.table;
        let synopsis = self.stats.synopsis_models.get(&table.schema_id)?;
        if synopsis.table_existence_generation != table.existence_generation.get()
            || synopsis.coverage != SynopsisCoverage::Complete
        {
            return None;
        }
        let column = synopsis
            .columns
            .iter()
            .find(|synopsis| synopsis.column == catalog_column.schema_id)?;
        if column.value_generation != catalog_column.value_generation.get() {
            return None;
        }
        if column.most_common_values.is_empty() {
            return None;
        }

        let changes = synopsis.changes_since_collection;
        let age = self
            .stats
            .published_at
            .saturating_sub(Duration::from_micros(synopsis.collected_at_unix_micros));
        if let Some(common) = column
            .most_common_values
            .iter()
            .find(|common| common.value.storage_eq(value))
        {
            let lower = common.lower_frequency();
            let upper = common.frequency;
            let cardinality = lower.saturating_add(upper.saturating_sub(lower) / 2);
            let interval = if common.maximum_error == 0 && changes == 0 {
                EstimateInterval::Exact
            } else {
                EstimateInterval::Range {
                    lower_bound: lower.saturating_sub(changes),
                    upper_bound: upper.saturating_add(changes),
                }
            };
            return Some(Estimate {
                cardinality,
                interval,
                source: EstimateSource::Mcv,
                sample_size: synopsis.sample_size,
                changes_since_collection: changes,
                age,
            });
        }

        let complete_domain = column.distinct_is_exact
            && column.distinct as usize == column.most_common_values.len()
            && column
                .most_common_values
                .iter()
                .all(|common| common.maximum_error == 0);
        if complete_domain {
            return Some(Estimate {
                cardinality: 0,
                interval: if changes == 0 {
                    EstimateInterval::Exact
                } else {
                    EstimateInterval::Range {
                        lower_bound: 0,
                        upper_bound: changes,
                    }
                },
                source: EstimateSource::Mcv,
                sample_size: synopsis.sample_size,
                changes_since_collection: changes,
                age,
            });
        }

        let non_null = synopsis.observed_rows.saturating_sub(column.null_count);
        let known = column
            .most_common_values
            .iter()
            .map(|common| common.lower_frequency())
            .fold(0u64, u64::saturating_add);
        let remaining_rows = non_null.saturating_sub(known);
        let remaining_distinct = column
            .distinct
            .saturating_sub(column.most_common_values.len() as u64)
            .max(1);
        let cardinality = remaining_rows.div_ceil(remaining_distinct);
        let untracked_upper = column
            .most_common_values
            .iter()
            .map(|common| common.frequency)
            .min()
            .unwrap_or(non_null)
            .max(cardinality);
        Some(Estimate {
            cardinality,
            interval: EstimateInterval::Range {
                lower_bound: 0,
                upper_bound: untracked_upper.saturating_add(changes),
            },
            source: EstimateSource::Mcv,
            sample_size: synopsis.sample_size,
            changes_since_collection: changes,
            age,
        })
    }

    fn column_group_equality(&self, equalities: &LiteralEqualities<'_>) -> Option<Estimate> {
        if equalities.values.len() < 2 {
            return None;
        }
        let table = equalities.table;
        let synopsis = self.stats.synopsis_models.get(&table.schema_id)?;
        if synopsis.table_existence_generation != table.existence_generation.get()
            || synopsis.coverage != SynopsisCoverage::Complete
        {
            return None;
        }
        let group = synopsis.column_groups.iter().find(|group| {
            group.columns.len() == equalities.values.len()
                && group.columns.iter().all(|column| {
                    equalities
                        .values
                        .iter()
                        .any(|(candidate, _)| candidate.schema_id == *column)
                })
        })?;
        if group.value_generations.len() != group.columns.len()
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
            || !super::models::declares_column_group(table, &group.columns)
            || group.most_common_values.is_empty()
        {
            return None;
        }
        let values: Vec<_> = group
            .columns
            .iter()
            .map(|column| {
                equalities
                    .values
                    .iter()
                    .find(|(candidate, _)| candidate.schema_id == *column)
                    .map(|(_, value)| *value)
            })
            .collect::<Option<_>>()?;
        let changes = synopsis.changes_since_collection;
        let age = self
            .stats
            .published_at
            .saturating_sub(Duration::from_micros(synopsis.collected_at_unix_micros));
        if let Some(common) = group.most_common_values.iter().find(|common| {
            common.values.len() == values.len()
                && common
                    .values
                    .iter()
                    .zip(&values)
                    .all(|(left, right)| left.storage_eq(right))
        }) {
            let lower = common.lower_frequency();
            let upper = common.frequency;
            return Some(Estimate {
                cardinality: lower.saturating_add(upper.saturating_sub(lower) / 2),
                interval: if common.maximum_error == 0 && changes == 0 {
                    EstimateInterval::Exact
                } else {
                    EstimateInterval::Range {
                        lower_bound: lower.saturating_sub(changes),
                        upper_bound: upper.saturating_add(changes),
                    }
                },
                source: EstimateSource::ColumnGroup,
                sample_size: synopsis.sample_size,
                changes_since_collection: changes,
                age,
            });
        }

        let complete_domain = group.distinct_is_exact
            && group.distinct as usize == group.most_common_values.len()
            && group
                .most_common_values
                .iter()
                .all(|common| common.maximum_error == 0);
        if complete_domain {
            return Some(Estimate {
                cardinality: 0,
                interval: if changes == 0 {
                    EstimateInterval::Exact
                } else {
                    EstimateInterval::Range {
                        lower_bound: 0,
                        upper_bound: changes,
                    }
                },
                source: EstimateSource::ColumnGroup,
                sample_size: synopsis.sample_size,
                changes_since_collection: changes,
                age,
            });
        }

        let non_null = synopsis.observed_rows.saturating_sub(group.null_count);
        let known = group
            .most_common_values
            .iter()
            .map(|common| common.lower_frequency())
            .fold(0u64, u64::saturating_add);
        let remaining_rows = non_null.saturating_sub(known);
        let remaining_distinct = group
            .distinct
            .saturating_sub(group.most_common_values.len() as u64)
            .max(1);
        let cardinality = remaining_rows.div_ceil(remaining_distinct);
        let untracked_upper = group
            .most_common_values
            .iter()
            .map(|common| common.frequency)
            .min()
            .unwrap_or(non_null)
            .max(cardinality);
        Some(Estimate {
            cardinality,
            interval: EstimateInterval::Range {
                lower_bound: 0,
                upper_bound: untracked_upper.saturating_add(changes),
            },
            source: EstimateSource::ColumnGroup,
            sample_size: synopsis.sample_size,
            changes_since_collection: changes,
            age,
        })
    }

    fn synopsis_estimate(&self, synopsis: &SynopsisModel) -> Estimate {
        let interval = match (synopsis.coverage, synopsis.changes_since_collection) {
            (SynopsisCoverage::Complete, 0) => EstimateInterval::Exact,
            (SynopsisCoverage::Complete, changes) => EstimateInterval::Range {
                lower_bound: synopsis.observed_rows.saturating_sub(changes),
                upper_bound: synopsis.observed_rows.saturating_add(changes),
            },
            (SynopsisCoverage::PrefixLimit, changes) => EstimateInterval::LowerBound {
                lower_bound: synopsis.observed_rows.saturating_sub(changes),
            },
        };
        Estimate {
            cardinality: synopsis.observed_rows,
            interval,
            source: EstimateSource::Synopsis,
            sample_size: synopsis.sample_size,
            changes_since_collection: synopsis.changes_since_collection,
            age: self
                .stats
                .published_at
                .saturating_sub(Duration::from_micros(synopsis.collected_at_unix_micros)),
        }
    }

    pub fn heuristic() -> Estimate {
        Estimate {
            cardinality: 1000,
            interval: EstimateInterval::Unknown,
            source: EstimateSource::Heuristic,
            sample_size: 0,
            changes_since_collection: 0,
            age: Duration::ZERO,
        }
    }

    fn structural_join_bound(
        &self,
        left: &crate::engine::lir::bound::Relation,
        right: &crate::engine::lir::bound::Relation,
        kind: crate::engine::lir::JoinKind,
    ) -> Estimate {
        let left = self.bound_relation(left);
        let right = self.bound_relation(right);
        let Some((left_lower, left_upper)) = finite_bounds(left) else {
            return Self::heuristic();
        };
        let Some((_, right_upper)) = finite_bounds(right) else {
            return Self::heuristic();
        };
        let product_upper = left_upper.saturating_mul(right_upper);
        let (lower_bound, upper_bound, cardinality) = match kind {
            crate::engine::lir::JoinKind::Inner => (
                0,
                product_upper,
                left.cardinality.saturating_mul(right.cardinality),
            ),
            crate::engine::lir::JoinKind::Left => (
                left_lower,
                product_upper.max(left_upper),
                left.cardinality
                    .saturating_mul(right.cardinality)
                    .max(left.cardinality),
            ),
        };
        if upper_bound == 0 || lower_bound == upper_bound {
            return Self::exact(upper_bound);
        }
        Estimate {
            cardinality: cardinality.clamp(lower_bound, upper_bound),
            interval: EstimateInterval::AttributedRange {
                lower_bound,
                upper_bound,
                lower_source: EstimateBoundSource::Structural,
                central_source: EstimateBoundSource::CartesianProduct,
                upper_source: EstimateBoundSource::CartesianProduct,
                drift_widened: false,
            },
            source: EstimateSource::Structural,
            sample_size: left.sample_size.min(right.sample_size),
            changes_since_collection: left
                .changes_since_collection
                .saturating_add(right.changes_since_collection),
            age: left.age.max(right.age),
        }
    }

    fn exact(cardinality: u64) -> Estimate {
        Estimate {
            cardinality,
            interval: EstimateInterval::Exact,
            source: EstimateSource::Structural,
            sample_size: 0,
            changes_since_collection: 0,
            age: Duration::ZERO,
        }
    }
}

fn finite_bounds(estimate: Estimate) -> Option<(u64, u64)> {
    match estimate.interval {
        EstimateInterval::Exact => Some((estimate.cardinality, estimate.cardinality)),
        EstimateInterval::Range {
            lower_bound,
            upper_bound,
        }
        | EstimateInterval::AttributedRange {
            lower_bound,
            upper_bound,
            ..
        } => Some((lower_bound, upper_bound)),
        EstimateInterval::Confidence { .. }
        | EstimateInterval::LowerBound { .. }
        | EstimateInterval::Unknown => None,
    }
}

fn valid_range_distribution(
    synopsis: &SynopsisModel,
    column: &ColumnSynopsis,
    distribution: &RangeDistribution,
) -> bool {
    if distribution.format_version != RANGE_DISTRIBUTION_FORMAT_VERSION
        || distribution.coverage != synopsis.coverage
        || distribution.sample_size != synopsis.sample_size
        || distribution.value_generation != column.value_generation
        || distribution.collected_row_count != synopsis.observed_rows
        || column.null_count > synopsis.observed_rows
    {
        return false;
    }
    let mut rows = 0u64;
    let mut previous: Option<&SynopsisValue> = None;
    for bucket in &distribution.buckets {
        if !valid_distribution_bucket(bucket, distribution.coverage) {
            return false;
        }
        if previous.is_some_and(|upper| {
            upper
                .compare(&bucket.lower)
                .is_none_or(|ordering| !ordering.is_lt())
        }) {
            return false;
        }
        previous = Some(&bucket.upper);
        let Some(total_rows) = rows.checked_add(bucket.rows.lower_bound) else {
            return false;
        };
        rows = total_rows;
        if bucket.cumulative_rows.lower_bound != rows
            || !valid_count_bounds(bucket.cumulative_rows, distribution.coverage)
        {
            return false;
        }
    }
    rows == synopsis.observed_rows.saturating_sub(column.null_count)
}

fn valid_distribution_bucket(bucket: &RangeDistributionBucket, coverage: SynopsisCoverage) -> bool {
    let Some(ordering) = bucket.lower.compare(&bucket.upper) else {
        return false;
    };
    if ordering.is_gt()
        || !valid_count_bounds(bucket.rows, coverage)
        || !valid_count_bounds(bucket.lower_endpoint_rows, coverage)
        || !valid_count_bounds(bucket.upper_endpoint_rows, coverage)
        || bucket.rows.lower_bound == 0
        || bucket.lower_endpoint_rows.lower_bound == 0
        || bucket.upper_endpoint_rows.lower_bound == 0
        || bucket.lower_endpoint_rows.lower_bound > bucket.rows.lower_bound
        || bucket.upper_endpoint_rows.lower_bound > bucket.rows.lower_bound
    {
        return false;
    }
    if ordering.is_eq() {
        bucket.lower_endpoint_rows == bucket.rows && bucket.upper_endpoint_rows == bucket.rows
    } else {
        bucket
            .lower_endpoint_rows
            .lower_bound
            .saturating_add(bucket.upper_endpoint_rows.lower_bound)
            <= bucket.rows.lower_bound
    }
}

fn valid_count_bounds(bounds: SynopsisCountBounds, coverage: SynopsisCoverage) -> bool {
    match coverage {
        SynopsisCoverage::Complete => bounds.upper_bound == Some(bounds.lower_bound),
        SynopsisCoverage::PrefixLimit => bounds.upper_bound.is_none(),
    }
}

fn range_is_empty(
    lower: Option<&super::analysis::RangeBound>,
    upper: Option<&super::analysis::RangeBound>,
) -> Option<bool> {
    let (Some(lower), Some(upper)) = (lower, upper) else {
        return Some(false);
    };
    Some(match lower.value.compare(&upper.value).ok()? {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Equal => !lower.inclusive || !upper.inclusive,
        std::cmp::Ordering::Less => false,
    })
}

fn distribution_range_bounds(
    distribution: &RangeDistribution,
    lower: Option<&super::analysis::RangeBound>,
    upper: Option<&super::analysis::RangeBound>,
) -> Option<SynopsisCountBounds> {
    let mut result = SynopsisCountBounds::exact(0);
    for bucket in &distribution.buckets {
        let bounds = bucket_range_bounds(bucket, lower, upper)?;
        result.lower_bound = result.lower_bound.saturating_add(bounds.lower_bound);
        result.upper_bound = match (result.upper_bound, bounds.upper_bound) {
            (Some(left), Some(right)) => Some(left.saturating_add(right)),
            _ => None,
        };
    }
    Some(result)
}

fn bucket_range_bounds(
    bucket: &RangeDistributionBucket,
    lower: Option<&super::analysis::RangeBound>,
    upper: Option<&super::analysis::RangeBound>,
) -> Option<SynopsisCountBounds> {
    let lower_satisfies = endpoint_satisfies(&bucket.lower, lower, upper)?;
    let upper_satisfies = endpoint_satisfies(&bucket.upper, lower, upper)?;
    if lower_satisfies && upper_satisfies {
        return Some(bucket.rows);
    }
    if bucket_is_outside_range(bucket, lower, upper)? {
        return Some(SynopsisCountBounds::exact(0));
    }
    let same_endpoint = bucket.lower.compare(&bucket.upper)?.is_eq();
    let mut lower_bound = 0u64;
    let mut known_outside = 0u64;
    for (satisfies, endpoint) in [
        (lower_satisfies, bucket.lower_endpoint_rows),
        (upper_satisfies, bucket.upper_endpoint_rows),
    ]
    .into_iter()
    .take(if same_endpoint { 1 } else { 2 })
    {
        if satisfies {
            lower_bound = lower_bound.saturating_add(endpoint.lower_bound);
        } else {
            known_outside = known_outside.saturating_add(endpoint.lower_bound);
        }
    }
    Some(SynopsisCountBounds {
        lower_bound,
        upper_bound: bucket
            .rows
            .upper_bound
            .map(|rows| rows.saturating_sub(known_outside.min(rows))),
    })
}

fn endpoint_satisfies(
    value: &SynopsisValue,
    lower: Option<&super::analysis::RangeBound>,
    upper: Option<&super::analysis::RangeBound>,
) -> Option<bool> {
    let above_lower = if let Some(bound) = lower {
        let ordering = value.storage_compare(&bound.value)?;
        ordering.is_gt() || (ordering.is_eq() && bound.inclusive)
    } else {
        true
    };
    let below_upper = if let Some(bound) = upper {
        let ordering = value.storage_compare(&bound.value)?;
        ordering.is_lt() || (ordering.is_eq() && bound.inclusive)
    } else {
        true
    };
    Some(above_lower && below_upper)
}

fn bucket_is_outside_range(
    bucket: &RangeDistributionBucket,
    lower: Option<&super::analysis::RangeBound>,
    upper: Option<&super::analysis::RangeBound>,
) -> Option<bool> {
    let below = if let Some(lower) = lower {
        let ordering = bucket.upper.storage_compare(&lower.value)?;
        ordering.is_lt() || (ordering.is_eq() && !lower.inclusive)
    } else {
        false
    };
    let above = if let Some(upper) = upper {
        let ordering = bucket.lower.storage_compare(&upper.value)?;
        ordering.is_gt() || (ordering.is_eq() && !upper.inclusive)
    } else {
        false
    };
    Some(below || above)
}

fn intersect_upper_bounds(left: Estimate, right: Estimate) -> Estimate {
    let left_upper = estimate_upper_bound(left);
    let right_upper = estimate_upper_bound(right);
    let (source, upper_bound) = match (left_upper, right_upper) {
        (Some(left_upper), Some(right_upper)) if right_upper < left_upper => {
            (right.source, right_upper)
        }
        (Some(left_upper), Some(_)) | (Some(left_upper), None) => (left.source, left_upper),
        (None, Some(right_upper)) => (right.source, right_upper),
        (None, None) => return as_upper_bound(left),
    };
    Estimate {
        cardinality: left.cardinality.min(right.cardinality).min(upper_bound),
        interval: if upper_bound == 0 {
            EstimateInterval::Exact
        } else {
            EstimateInterval::Range {
                lower_bound: 0,
                upper_bound,
            }
        },
        source,
        sample_size: left.sample_size.min(right.sample_size),
        changes_since_collection: left
            .changes_since_collection
            .max(right.changes_since_collection),
        age: left.age.max(right.age),
    }
}

fn estimate_upper_bound(estimate: Estimate) -> Option<u64> {
    match estimate.interval {
        EstimateInterval::Exact => Some(estimate.cardinality),
        EstimateInterval::Confidence { upper_bound, .. }
        | EstimateInterval::Range { upper_bound, .. }
        | EstimateInterval::AttributedRange { upper_bound, .. } => Some(upper_bound),
        EstimateInterval::LowerBound { .. } | EstimateInterval::Unknown => None,
    }
}

fn as_upper_bound(mut estimate: Estimate) -> Estimate {
    let upper_bound = match estimate.interval {
        EstimateInterval::Exact => estimate.cardinality,
        EstimateInterval::Confidence { upper_bound, .. }
        | EstimateInterval::Range { upper_bound, .. }
        | EstimateInterval::AttributedRange { upper_bound, .. } => upper_bound,
        EstimateInterval::LowerBound { .. } | EstimateInterval::Unknown => {
            estimate.interval = EstimateInterval::Unknown;
            return estimate;
        }
    };
    estimate.cardinality = estimate.cardinality.min(upper_bound);
    estimate.interval = EstimateInterval::Range {
        lower_bound: 0,
        upper_bound,
    };
    estimate
}

struct LiteralEqualities<'a> {
    table: &'a crate::engine::catalog::model::Table,
    values: Vec<(
        &'a crate::engine::catalog::model::Column,
        &'a crate::engine::lir::Value,
    )>,
}

fn literal_equalities(
    relation: &crate::engine::lir::bound::Relation,
) -> Option<LiteralEqualities<'_>> {
    use crate::engine::lir::BinaryOp;
    use crate::engine::lir::bound::{Expr, RelationNode};

    let RelationNode::Filter { input, predicate } = &relation.node else {
        return None;
    };
    let RelationNode::Scan { table, .. } = &input.node else {
        return None;
    };
    let mut values: Vec<(
        &crate::engine::catalog::model::Column,
        &crate::engine::lir::Value,
    )> = Vec::new();
    for conjunct in super::analysis::conjuncts(predicate) {
        let Expr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
            ..
        } = conjunct
        else {
            return None;
        };
        let (slot, value) = match (&**left, &**right) {
            (Expr::SlotRef { slot, .. }, Expr::Literal(value)) if !value.is_null() => {
                (*slot, value)
            }
            (Expr::Literal(value), Expr::SlotRef { slot, .. }) if !value.is_null() => {
                (*slot, value)
            }
            _ => return None,
        };
        let field = input
            .output()
            .fields
            .iter()
            .find(|field| field.slot == slot)?;
        let column = table
            .columns
            .iter()
            .find(|column| column.name == field.name)?;
        if let Some((_, existing)) = values
            .iter()
            .find(|(candidate, _)| candidate.schema_id == column.schema_id)
        {
            if !existing.storage_eq(value) {
                return None;
            }
            continue;
        }
        values.push((column, value));
    }
    values.sort_by_key(|(column, _)| column.schema_id);
    (!values.is_empty()).then_some(LiteralEqualities { table, values })
}

fn literal_null_test(
    relation: &crate::engine::lir::bound::Relation,
) -> Option<(
    &crate::engine::catalog::model::Table,
    &crate::engine::catalog::model::Column,
)> {
    use crate::engine::lir::UnaryOp;
    use crate::engine::lir::bound::{Expr, RelationNode};

    let RelationNode::Filter { input, predicate } = &relation.node else {
        return None;
    };
    let RelationNode::Scan { table, .. } = &input.node else {
        return None;
    };
    let Expr::Unary {
        op: UnaryOp::IsNull,
        expression,
        ..
    } = predicate
    else {
        return None;
    };
    let Expr::SlotRef { slot, .. } = &**expression else {
        return None;
    };
    let field = input
        .output()
        .fields
        .iter()
        .find(|field| field.slot == *slot)?;
    let column = table
        .columns
        .iter()
        .find(|column| column.name == field.name)?;
    Some((table, column))
}

pub fn arbitrate(candidates: impl IntoIterator<Item = Estimate>) -> Option<Estimate> {
    candidates
        .into_iter()
        .max_by(|left, right| evidence_rank(left).cmp(&evidence_rank(right)))
}

fn evidence_rank(estimate: &Estimate) -> (u8, u64, std::cmp::Reverse<Duration>, u64) {
    let uncertainty = match estimate.interval {
        EstimateInterval::Exact => 4,
        EstimateInterval::Confidence { .. } => 3,
        EstimateInterval::Range { .. } | EstimateInterval::AttributedRange { .. } => 2,
        EstimateInterval::LowerBound { .. } => 1,
        EstimateInterval::Unknown => 0,
    };
    (
        uncertainty,
        estimate
            .sample_size
            .saturating_sub(estimate.changes_since_collection),
        std::cmp::Reverse(estimate.age),
        estimate.sample_size,
    )
}

#[cfg(test)]
mod tests {
    use crate::engine::catalog::identity::SchemaId;
    use crate::engine::lir::fingerprint::{CANONICALIZATION_VERSION, HASH_SHA256_128};
    use crate::engine::lir::{BinaryOp, Value};
    use crate::engine::planner::test_support::{column, scan};

    use super::super::models::{
        ColumnGroupSynopsis, ColumnSynopsis, FeedbackModel, MostCommonColumnGroup, MostCommonValue,
        SynopsisModel, SynopsisValue,
    };
    use super::*;

    fn fingerprint(seed: u8) -> Fingerprint {
        Fingerprint {
            canonicalization_version: CANONICALIZATION_VERSION,
            hash_algorithm: HASH_SHA256_128,
            digest: [seed; 16],
        }
    }

    fn stamp(seed: u64) -> super::super::models::DependencyStamp {
        super::super::models::DependencyStamp {
            semantic: seed,
            access: seed,
        }
    }

    fn feedback(family: Fingerprint, executions: u64, rows_p50: u64) -> FeedbackModel {
        FeedbackModel {
            family,
            retained_executions: executions,
            exact_variants: 1,
            last_seen: Duration::from_secs(1),
            rows_p50_upper_bound: rows_p50,
            rows_p95_upper_bound: rows_p50 * 2,
            rows_max: rows_p50 * 2,
            execute_micros_p50_upper_bound: 100,
            execute_micros_p95_upper_bound: 200,
            duration_ewma_micros: 120.0,
            plans: Vec::new(),
            plan_profiles: Vec::new(),
            resources: Default::default(),
            stamp: stamp(1),
            executions_with_estimate: 0,
            q_error_p50_upper_bound_x100: 0,
            q_error_p95_upper_bound_x100: 0,
            q_error_max_x100: 0,
        }
    }

    fn synopsis(table: SchemaId, row_count: u64, coverage: SynopsisCoverage) -> SynopsisModel {
        SynopsisModel {
            table,
            observed_rows: row_count,
            coverage,
            sample_size: row_count,
            changes_since_collection: 0,
            table_existence_generation: 2,
            collected_at_unix_micros: 0,
            catalog_version: 1,
            columns: Vec::new(),
            column_groups: Vec::new(),
            predicate_conditioned_degrees: Vec::new(),
        }
    }

    fn synopsis_estimate(stats: &PlannerStats, table: SchemaId) -> Estimate {
        Estimator::new(stats).synopsis_estimate(&stats.synopsis_models[&table])
    }

    fn equality_filter(column_name: &str, value: &str) -> crate::engine::lir::bound::Relation {
        let input = scan();
        let predicate = crate::engine::lir::bound::Expr::binary(
            BinaryOp::Eq,
            column(&input, column_name),
            crate::engine::lir::bound::Expr::literal(Value::Text(value.into())),
        );
        crate::engine::lir::bound::Relation::filter(input, predicate)
    }

    fn mcv_synopsis(coverage: SynopsisCoverage) -> SynopsisModel {
        let table = crate::engine::planner::test_support::table();
        let mut synopsis = synopsis(table.schema_id, 1000, coverage);
        synopsis.sample_size = 1000;
        synopsis.columns = vec![ColumnSynopsis {
            column: table.column("status").unwrap().schema_id,
            value_generation: table.column("status").unwrap().value_generation.get(),
            null_fraction: 0.0,
            null_count: 0,
            distinct: 3,
            distinct_is_exact: true,
            average_width: 5,
            maximum_width: Some(7),
            minimum: Some("\"closed\"".into()),
            maximum: Some("\"pending\"".into()),
            most_common_values: vec![
                MostCommonValue {
                    value: SynopsisValue::Text("open".into()),
                    frequency: 700,
                    maximum_error: 0,
                },
                MostCommonValue {
                    value: SynopsisValue::Text("closed".into()),
                    frequency: 200,
                    maximum_error: 0,
                },
                MostCommonValue {
                    value: SynopsisValue::Text("pending".into()),
                    frequency: 100,
                    maximum_error: 0,
                },
            ],
            range_distribution: None,
            degree_sequence: None,
        }];
        synopsis
    }

    fn range_synopsis(coverage: SynopsisCoverage) -> SynopsisModel {
        let table = crate::engine::planner::test_support::table();
        let column = table.column("board_id").unwrap();
        let bounds = |value| match coverage {
            SynopsisCoverage::Complete => SynopsisCountBounds::exact(value),
            SynopsisCoverage::PrefixLimit => SynopsisCountBounds::lower(value),
        };
        let bucket = |lower: &str, upper: &str, rows, cumulative_rows, lower_rows, upper_rows| {
            RangeDistributionBucket {
                lower: SynopsisValue::Text(lower.into()),
                upper: SynopsisValue::Text(upper.into()),
                rows: bounds(rows),
                cumulative_rows: bounds(cumulative_rows),
                lower_endpoint_rows: bounds(lower_rows),
                upper_endpoint_rows: bounds(upper_rows),
            }
        };
        let mut synopsis = synopsis(table.schema_id, 1_000, coverage);
        synopsis.columns.push(ColumnSynopsis {
            column: column.schema_id,
            value_generation: column.value_generation.get(),
            null_fraction: 0.0,
            null_count: 0,
            distinct: 5,
            distinct_is_exact: true,
            average_width: 1,
            maximum_width: Some(1),
            minimum: Some("\"a\"".into()),
            maximum: Some("\"e\"".into()),
            most_common_values: Vec::new(),
            range_distribution: Some(RangeDistribution {
                format_version: RANGE_DISTRIBUTION_FORMAT_VERSION,
                coverage,
                sample_size: 1_000,
                value_generation: column.value_generation.get(),
                collected_row_count: 1_000,
                buckets: vec![
                    bucket("a", "a", 100, 100, 100, 100),
                    bucket("b", "d", 500, 600, 100, 100),
                    bucket("e", "e", 400, 1_000, 400, 400),
                ],
            }),
            degree_sequence: None,
        });
        synopsis
    }

    fn range_filter(
        lower: (&str, bool),
        upper: (&str, bool),
    ) -> crate::engine::lir::bound::Relation {
        let input = scan();
        let lower = crate::engine::lir::bound::Expr::binary(
            if lower.1 { BinaryOp::Gte } else { BinaryOp::Gt },
            column(&input, "board_id"),
            crate::engine::lir::bound::Expr::literal(Value::Text(lower.0.into())),
        );
        let upper = crate::engine::lir::bound::Expr::binary(
            if upper.1 { BinaryOp::Lte } else { BinaryOp::Lt },
            column(&input, "board_id"),
            crate::engine::lir::bound::Expr::literal(Value::Text(upper.0.into())),
        );
        crate::engine::lir::bound::Relation::filter(
            input,
            crate::engine::lir::bound::Expr::binary(BinaryOp::And, lower, upper),
        )
    }

    fn column_group_filter(board: &str, status: &str) -> crate::engine::lir::bound::Relation {
        let input = scan();
        let predicate = crate::engine::lir::bound::Expr::binary(
            BinaryOp::And,
            crate::engine::lir::bound::Expr::binary(
                BinaryOp::Eq,
                column(&input, "status"),
                crate::engine::lir::bound::Expr::literal(Value::Text(status.into())),
            ),
            crate::engine::lir::bound::Expr::binary(
                BinaryOp::Eq,
                crate::engine::lir::bound::Expr::literal(Value::Text(board.into())),
                column(&input, "board_id"),
            ),
        );
        crate::engine::lir::bound::Relation::filter(input, predicate)
    }

    fn column_group_synopsis(coverage: SynopsisCoverage) -> SynopsisModel {
        let table = crate::engine::planner::test_support::table();
        let board = table.column("board_id").unwrap();
        let status = table.column("status").unwrap();
        let mut synopsis = synopsis(table.schema_id, 1000, coverage);
        synopsis.column_groups = vec![ColumnGroupSynopsis {
            columns: vec![board.schema_id, status.schema_id],
            value_generations: vec![board.value_generation.get(), status.value_generation.get()],
            null_count: 0,
            distinct: 4,
            distinct_is_exact: true,
            most_common_values: vec![
                MostCommonColumnGroup {
                    values: vec![
                        SynopsisValue::Text("b1".into()),
                        SynopsisValue::Text("open".into()),
                    ],
                    frequency: 600,
                    maximum_error: 0,
                },
                MostCommonColumnGroup {
                    values: vec![
                        SynopsisValue::Text("b2".into()),
                        SynopsisValue::Text("closed".into()),
                    ],
                    frequency: 200,
                    maximum_error: 0,
                },
                MostCommonColumnGroup {
                    values: vec![
                        SynopsisValue::Text("b1".into()),
                        SynopsisValue::Text("closed".into()),
                    ],
                    frequency: 100,
                    maximum_error: 0,
                },
                MostCommonColumnGroup {
                    values: vec![
                        SynopsisValue::Text("b2".into()),
                        SynopsisValue::Text("open".into()),
                    ],
                    frequency: 100,
                    maximum_error: 0,
                },
            ],
            degree_sequence: None,
        }];
        synopsis
    }

    #[test]
    fn feedback_is_diagnostic_until_predicate_distributions_exist() {
        let relation = scan();
        let table = relation.scan_table();
        let family = fingerprint(1);
        let mut stats = PlannerStats::empty();
        stats.synopsis_models.insert(
            table.schema_id,
            synopsis(table.schema_id, 500_000, SynopsisCoverage::Complete),
        );

        // A table synopsis remains the source because the family does not
        // preserve the predicate-value distribution.
        stats
            .feedback_models
            .insert(family, feedback(family, 2, 127));
        let estimate = Estimator::new(&stats).relation(&family, Some(table), stamp(1));
        assert_eq!(estimate.source, EstimateSource::Synopsis);
        assert_eq!(estimate.cardinality, 500_000);
        assert_eq!(estimate.interval, EstimateInterval::Exact);

        // Feedback stays diagnostic because one family can contain different
        // predicate values with different cardinalities.
        stats
            .feedback_models
            .insert(family, feedback(family, 18_421, 127));
        let estimate = Estimator::new(&stats).relation(&family, Some(table), stamp(1));
        assert_eq!(estimate.source, EstimateSource::Synopsis);
        assert_eq!(estimate.cardinality, 500_000);
        let feedback = Estimator::new(&stats)
            .family_feedback(&family, stamp(1))
            .expect("feedback model");
        assert_eq!(feedback.source, EstimateSource::FamilyFeedback);
        assert_eq!(feedback.interval, EstimateInterval::Unknown);

        // Nothing known uses an estimate with unknown uncertainty.
        let unknown = fingerprint(9);
        let estimate = Estimator::new(&stats).relation(&unknown, None, stamp(1));
        assert_eq!(estimate.source, EstimateSource::Heuristic);
        assert_eq!(estimate.interval, EstimateInterval::Unknown);
    }

    #[test]
    fn feedback_is_withheld_when_the_semantic_stamp_moved() {
        let relation = scan();
        let table = relation.scan_table();
        let family = fingerprint(1);
        let mut stats = PlannerStats::empty();
        stats
            .feedback_models
            .insert(family, feedback(family, 18_421, 127));
        stats.synopsis_models.insert(
            table.schema_id,
            synopsis(table.schema_id, 500_000, SynopsisCoverage::Complete),
        );

        let estimator = Estimator::new(&stats);
        assert_eq!(
            estimator.family_feedback(&family, stamp(1)).unwrap().source,
            EstimateSource::FamilyFeedback
        );

        // A replaced column or redefined table leaves the shape identical
        // while changing what it denotes; the accumulated distribution no
        // longer describes it at any sample size.
        let moved = estimator.relation(&family, Some(table), stamp(2));
        assert_eq!(moved.source, EstimateSource::Synopsis);
        assert!(estimator.family_feedback(&family, stamp(2)).is_none());

        // With no synopsis either, arbitration falls to the heuristic
        // rather than reaching for evidence it must not trust.
        stats.synopsis_models.clear();
        assert_eq!(
            Estimator::new(&stats)
                .relation(&family, Some(table), stamp(2))
                .source,
            EstimateSource::Heuristic
        );
    }

    #[test]
    fn prefix_synopses_report_only_a_lower_bound() {
        let table = SchemaId::new(3).unwrap();
        let mut stats = PlannerStats::empty();
        stats
            .synopsis_models
            .insert(table, synopsis(table, 1000, SynopsisCoverage::PrefixLimit));
        let estimate = synopsis_estimate(&stats, table);
        assert_eq!(
            estimate.interval,
            EstimateInterval::LowerBound { lower_bound: 1000 }
        );
    }

    #[test]
    fn writes_widen_synopsis_bounds_without_guessing_the_mutation_kind() {
        let table = SchemaId::new(3).unwrap();
        let mut stats = PlannerStats::empty();
        let mut complete = synopsis(table, 1000, SynopsisCoverage::Complete);
        complete.changes_since_collection = 25;
        stats.synopsis_models.insert(table, complete);

        let estimate = synopsis_estimate(&stats, table);
        assert_eq!(estimate.cardinality, 1000);
        assert_eq!(estimate.changes_since_collection, 25);
        assert_eq!(
            estimate.interval,
            EstimateInterval::Range {
                lower_bound: 975,
                upper_bound: 1025,
            }
        );

        let mut prefix = synopsis(table, 100, SynopsisCoverage::PrefixLimit);
        prefix.changes_since_collection = 25;
        stats.synopsis_models.insert(table, prefix);
        assert_eq!(
            synopsis_estimate(&stats, table).interval,
            EstimateInterval::LowerBound { lower_bound: 75 }
        );
    }

    #[test]
    fn a_recreated_table_does_not_use_the_old_synopsis() {
        let relation = scan();
        let table = relation.scan_table().schema_id;
        let mut stats = PlannerStats::empty();
        let mut stale = synopsis(table, 1000, SynopsisCoverage::Complete);
        stale.table_existence_generation = 1;
        stats.synopsis_models.insert(table, stale);

        let estimate = Estimator::new(&stats).bound_relation(&relation);
        assert_eq!(estimate.source, EstimateSource::Heuristic);
    }

    #[test]
    fn arbitration_uses_effective_evidence_and_age_without_source_precedence() {
        let candidate = |source, sample_size, changes, age| Estimate {
            cardinality: sample_size,
            interval: EstimateInterval::Range {
                lower_bound: 0,
                upper_bound: sample_size,
            },
            source,
            sample_size,
            changes_since_collection: changes,
            age,
        };
        let older = candidate(
            EstimateSource::FamilyFeedback,
            100,
            20,
            Duration::from_secs(60),
        );
        let younger = candidate(EstimateSource::Synopsis, 90, 10, Duration::from_secs(1));
        assert_eq!(
            arbitrate([older, younger]).unwrap().source,
            EstimateSource::Synopsis
        );

        let stronger = candidate(
            EstimateSource::FamilyFeedback,
            101,
            20,
            Duration::from_secs(60),
        );
        assert_eq!(
            arbitrate([stronger, younger]).unwrap().source,
            EstimateSource::FamilyFeedback
        );
    }

    #[test]
    fn complete_primary_key_equality_has_a_zero_to_one_bound() {
        let scan = scan();
        let predicate = crate::engine::lir::bound::Expr::binary(
            BinaryOp::Eq,
            column(&scan, "id"),
            crate::engine::lir::bound::Expr::literal(Value::Text("task-1".into())),
        );
        let relation = crate::engine::lir::bound::Relation::filter(scan, predicate);

        let stats = PlannerStats::empty();
        let estimate = Estimator::new(&stats).bound_relation(&relation);
        assert_eq!(estimate.cardinality, 1);
        assert_eq!(estimate.source, EstimateSource::Structural);
        assert_eq!(
            estimate.interval,
            EstimateInterval::Range {
                lower_bound: 0,
                upper_bound: 1,
            }
        );
    }

    #[test]
    fn complete_mcv_statistics_estimate_literal_equalities() {
        let table = crate::engine::planner::test_support::table();
        let mut stats = PlannerStats::empty();
        stats
            .synopsis_models
            .insert(table.schema_id, mcv_synopsis(SynopsisCoverage::Complete));

        let hot = Estimator::new(&stats).bound_relation(&equality_filter("status", "open"));
        assert_eq!(hot.source, EstimateSource::Mcv);
        assert_eq!(hot.cardinality, 700);
        assert_eq!(hot.interval, EstimateInterval::Exact);

        let absent = Estimator::new(&stats).bound_relation(&equality_filter("status", "unknown"));
        assert_eq!(absent.source, EstimateSource::Mcv);
        assert_eq!(absent.cardinality, 0);
        assert_eq!(absent.interval, EstimateInterval::Exact);
    }

    #[test]
    fn complete_range_distribution_estimates_exact_and_partial_buckets() {
        let table = crate::engine::planner::test_support::table();
        let mut stats = PlannerStats::empty();
        stats
            .synopsis_models
            .insert(table.schema_id, range_synopsis(SynopsisCoverage::Complete));

        let exact = Estimator::new(&stats).bound_relation(&range_filter(("b", true), ("d", true)));
        assert_eq!(exact.source, EstimateSource::Distribution);
        assert_eq!(exact.cardinality, 500);
        assert_eq!(exact.interval, EstimateInterval::Exact);

        let partial =
            Estimator::new(&stats).bound_relation(&range_filter(("b", false), ("d", true)));
        assert_eq!(partial.cardinality, 250);
        assert_eq!(
            partial.interval,
            EstimateInterval::Range {
                lower_bound: 100,
                upper_bound: 400,
            }
        );
    }

    #[test]
    fn range_distribution_handles_empty_ranges_and_drift() {
        let table = crate::engine::planner::test_support::table();
        let mut synopsis = range_synopsis(SynopsisCoverage::Complete);
        synopsis.changes_since_collection = 5;
        let mut stats = PlannerStats::empty();
        stats.synopsis_models.insert(table.schema_id, synopsis);

        let drifted =
            Estimator::new(&stats).bound_relation(&range_filter(("b", true), ("d", true)));
        assert_eq!(drifted.cardinality, 500);
        assert_eq!(
            drifted.interval,
            EstimateInterval::Range {
                lower_bound: 495,
                upper_bound: 505,
            }
        );

        let empty = Estimator::new(&stats).bound_relation(&range_filter(("d", false), ("d", true)));
        assert_eq!(empty.cardinality, 0);
        assert_eq!(empty.interval, EstimateInterval::Exact);
        assert_eq!(empty.source, EstimateSource::Structural);
    }

    #[test]
    fn range_distribution_estimates_one_sided_and_type_boundary_ranges() {
        let table = crate::engine::planner::test_support::table();
        let mut stats = PlannerStats::empty();
        stats
            .synopsis_models
            .insert(table.schema_id, range_synopsis(SynopsisCoverage::Complete));
        let estimator = Estimator::new(&stats);
        let lower = super::super::analysis::RangeBound {
            value: Value::Text("e".into()),
            inclusive: true,
        };
        let upper = super::super::analysis::RangeBound {
            value: Value::Text("b".into()),
            inclusive: false,
        };
        assert_eq!(
            estimator
                .range_for_column(&table, "board_id", Some(&lower), None)
                .unwrap()
                .interval,
            EstimateInterval::Exact
        );
        assert_eq!(
            estimator
                .range_for_column(&table, "board_id", None, Some(&upper))
                .unwrap()
                .cardinality,
            100
        );

        let bucket = RangeDistributionBucket {
            lower: SynopsisValue::Int64(i64::MIN),
            upper: SynopsisValue::Int64(i64::MAX),
            rows: SynopsisCountBounds::exact(2),
            cumulative_rows: SynopsisCountBounds::exact(2),
            lower_endpoint_rows: SynopsisCountBounds::exact(1),
            upper_endpoint_rows: SynopsisCountBounds::exact(1),
        };
        let boundary = super::super::analysis::RangeBound {
            value: Value::Int64(i64::MIN),
            inclusive: true,
        };
        assert_eq!(
            bucket_range_bounds(&bucket, Some(&boundary), Some(&boundary)),
            Some(SynopsisCountBounds::exact(1))
        );
    }

    #[test]
    fn prefix_range_distribution_keeps_only_a_lower_bound() {
        let table = crate::engine::planner::test_support::table();
        let mut stats = PlannerStats::empty();
        stats.synopsis_models.insert(
            table.schema_id,
            range_synopsis(SynopsisCoverage::PrefixLimit),
        );

        let estimate =
            Estimator::new(&stats).bound_relation(&range_filter(("b", true), ("d", true)));
        assert_eq!(estimate.source, EstimateSource::Distribution);
        assert_eq!(estimate.cardinality, 500);
        assert_eq!(
            estimate.interval,
            EstimateInterval::LowerBound { lower_bound: 500 }
        );
    }

    #[test]
    fn range_distribution_requires_current_complete_metadata() {
        let table = crate::engine::planner::test_support::table();
        let mut synopsis = range_synopsis(SynopsisCoverage::Complete);
        synopsis.columns[0]
            .range_distribution
            .as_mut()
            .unwrap()
            .value_generation = synopsis.columns[0].value_generation.saturating_sub(1);
        let mut stats = PlannerStats::empty();
        stats.synopsis_models.insert(table.schema_id, synopsis);

        let estimate =
            Estimator::new(&stats).bound_relation(&range_filter(("b", true), ("d", true)));
        assert_eq!(estimate.source, EstimateSource::Heuristic);
        assert_eq!(estimate.interval, EstimateInterval::Unknown);
    }

    #[test]
    fn range_distribution_arithmetic_saturates() {
        let bucket = |value| RangeDistributionBucket {
            lower: SynopsisValue::Int64(value),
            upper: SynopsisValue::Int64(value),
            rows: SynopsisCountBounds::exact(u64::MAX),
            cumulative_rows: SynopsisCountBounds::exact(u64::MAX),
            lower_endpoint_rows: SynopsisCountBounds::exact(u64::MAX),
            upper_endpoint_rows: SynopsisCountBounds::exact(u64::MAX),
        };
        let distribution = RangeDistribution {
            format_version: RANGE_DISTRIBUTION_FORMAT_VERSION,
            coverage: SynopsisCoverage::Complete,
            sample_size: u64::MAX,
            value_generation: 1,
            collected_row_count: u64::MAX,
            buckets: vec![bucket(0), bucket(1)],
        };
        assert_eq!(
            distribution_range_bounds(&distribution, None, None),
            Some(SynopsisCountBounds::exact(u64::MAX))
        );
    }

    #[test]
    fn complete_column_statistics_estimate_null_predicates() {
        let table = crate::engine::planner::test_support::table();
        let mut synopsis = mcv_synopsis(SynopsisCoverage::Complete);
        synopsis.columns[0].null_count = 800;
        synopsis.columns[0].null_fraction = 0.8;
        let mut stats = PlannerStats::empty();
        stats.synopsis_models.insert(table.schema_id, synopsis);
        let input = scan();
        let relation = crate::engine::lir::bound::Relation::filter(
            input.clone(),
            crate::engine::lir::bound::Expr::unary(
                crate::engine::lir::UnaryOp::IsNull,
                column(&input, "status"),
            ),
        );
        let estimate = Estimator::new(&stats).bound_relation(&relation);
        assert_eq!(estimate.cardinality, 800);
        assert_eq!(estimate.interval, EstimateInterval::Exact);
        assert_eq!(estimate.source, EstimateSource::Synopsis);
    }

    #[test]
    fn mcv_error_and_table_drift_widen_the_estimate() {
        let table = crate::engine::planner::test_support::table();
        let mut synopsis = mcv_synopsis(SynopsisCoverage::Complete);
        synopsis.changes_since_collection = 5;
        synopsis.columns[0].most_common_values[0].maximum_error = 20;
        let mut stats = PlannerStats::empty();
        stats.synopsis_models.insert(table.schema_id, synopsis);

        let estimate = Estimator::new(&stats).bound_relation(&equality_filter("status", "open"));
        assert_eq!(estimate.source, EstimateSource::Mcv);
        assert_eq!(estimate.cardinality, 690);
        assert_eq!(
            estimate.interval,
            EstimateInterval::Range {
                lower_bound: 675,
                upper_bound: 705,
            }
        );
    }

    #[test]
    fn prefix_mcv_statistics_do_not_claim_full_table_selectivity() {
        let table = crate::engine::planner::test_support::table();
        let mut stats = PlannerStats::empty();
        stats
            .synopsis_models
            .insert(table.schema_id, mcv_synopsis(SynopsisCoverage::PrefixLimit));

        let estimate = Estimator::new(&stats).bound_relation(&equality_filter("status", "open"));
        assert_eq!(estimate.source, EstimateSource::Heuristic);
        assert_eq!(estimate.interval, EstimateInterval::Unknown);
    }

    #[test]
    fn mcv_statistics_do_not_cross_a_column_value_generation() {
        let table = crate::engine::planner::test_support::table();
        let mut synopsis = mcv_synopsis(SynopsisCoverage::Complete);
        synopsis.columns[0].value_generation =
            synopsis.columns[0].value_generation.saturating_sub(1);
        let mut stats = PlannerStats::empty();
        stats.synopsis_models.insert(table.schema_id, synopsis);

        let estimate = Estimator::new(&stats).bound_relation(&equality_filter("status", "open"));
        assert_eq!(estimate.source, EstimateSource::Heuristic);
    }

    #[test]
    fn complete_column_group_statistics_estimate_combined_equalities() {
        let table = crate::engine::planner::test_support::table();
        let mut stats = PlannerStats::empty();
        stats.synopsis_models.insert(
            table.schema_id,
            column_group_synopsis(SynopsisCoverage::Complete),
        );

        let correlated = Estimator::new(&stats).bound_relation(&column_group_filter("b1", "open"));
        assert_eq!(correlated.source, EstimateSource::ColumnGroup);
        assert_eq!(correlated.cardinality, 600);
        assert_eq!(correlated.interval, EstimateInterval::Exact);

        let absent = Estimator::new(&stats).bound_relation(&column_group_filter("b3", "open"));
        assert_eq!(absent.source, EstimateSource::ColumnGroup);
        assert_eq!(absent.cardinality, 0);
        assert_eq!(absent.interval, EstimateInterval::Exact);
    }

    #[test]
    fn column_group_error_and_table_drift_widen_the_estimate() {
        let table = crate::engine::planner::test_support::table();
        let mut synopsis = column_group_synopsis(SynopsisCoverage::Complete);
        synopsis.changes_since_collection = 5;
        synopsis.column_groups[0].most_common_values[0].maximum_error = 20;
        let mut stats = PlannerStats::empty();
        stats.synopsis_models.insert(table.schema_id, synopsis);

        let estimate = Estimator::new(&stats).bound_relation(&column_group_filter("b1", "open"));
        assert_eq!(estimate.source, EstimateSource::ColumnGroup);
        assert_eq!(estimate.cardinality, 590);
        assert_eq!(
            estimate.interval,
            EstimateInterval::Range {
                lower_bound: 575,
                upper_bound: 605,
            }
        );
    }

    #[test]
    fn column_group_statistics_require_complete_current_evidence() {
        let table = crate::engine::planner::test_support::table();
        let mut prefix = column_group_synopsis(SynopsisCoverage::PrefixLimit);
        let mut stats = PlannerStats::empty();
        stats
            .synopsis_models
            .insert(table.schema_id, prefix.clone());
        assert_eq!(
            Estimator::new(&stats)
                .bound_relation(&column_group_filter("b1", "open"))
                .source,
            EstimateSource::Heuristic
        );

        prefix.coverage = SynopsisCoverage::Complete;
        prefix.column_groups[0].value_generations[0] =
            prefix.column_groups[0].value_generations[0].saturating_sub(1);
        stats.synopsis_models.insert(table.schema_id, prefix);
        assert_eq!(
            Estimator::new(&stats)
                .bound_relation(&column_group_filter("b1", "open"))
                .source,
            EstimateSource::Heuristic
        );
    }

    #[test]
    fn column_group_statistics_do_not_ignore_residual_predicates() {
        let table = crate::engine::planner::test_support::table();
        let input = scan();
        let predicate = crate::engine::lir::bound::Expr::binary(
            BinaryOp::And,
            crate::engine::lir::bound::Expr::binary(
                BinaryOp::And,
                crate::engine::lir::bound::Expr::binary(
                    BinaryOp::Eq,
                    column(&input, "board_id"),
                    crate::engine::lir::bound::Expr::literal(Value::Text("b1".into())),
                ),
                crate::engine::lir::bound::Expr::binary(
                    BinaryOp::Eq,
                    column(&input, "status"),
                    crate::engine::lir::bound::Expr::literal(Value::Text("open".into())),
                ),
            ),
            crate::engine::lir::bound::Expr::binary(
                BinaryOp::Gte,
                column(&input, "id"),
                crate::engine::lir::bound::Expr::literal(Value::Text("task-10".into())),
            ),
        );
        let relation = crate::engine::lir::bound::Relation::filter(input, predicate);
        let mut stats = PlannerStats::empty();
        stats.synopsis_models.insert(
            table.schema_id,
            column_group_synopsis(SynopsisCoverage::Complete),
        );

        assert_eq!(
            Estimator::new(&stats).bound_relation(&relation).source,
            EstimateSource::Heuristic
        );
    }
}
