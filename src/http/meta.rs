use super::generated::server::{
    GetHealthzResponse, GetInfoResponse, GetStatisticsResponse, MetaApi, TableListResponse,
};
use super::generated::types::{
    Access, DatabaseInfo, DatabaseInfoMode, Health, Statistics, StatisticsColumnGroupSynopsis,
    StatisticsColumnSynopsis, StatisticsCorpus, StatisticsCorpusMaintenance, StatisticsModel,
    StatisticsMostCommonColumnGroup, StatisticsMostCommonValue, StatisticsPhysicalCacheCost,
    StatisticsPhysicalCacheCostTier, StatisticsPhysicalCost, StatisticsPhysicalCostBasis,
    StatisticsPhysicalCostMetric, StatisticsPhysicalRequestCost,
    StatisticsPhysicalRequestCostClass, StatisticsPhysicalRequestCostServiceTier,
    StatisticsPhysicalTelemetryCapabilities, StatisticsPlan, StatisticsPlanningValue,
    StatisticsRelay, StatisticsRelayState, StatisticsResourceCost, StatisticsResourceCostBasis,
    StatisticsResourceMetric, StatisticsSynopsis, StatisticsSynopsisCoverage,
};
use super::{server, wire};
use crate::engine::catalog::model::Mode;

use super::server::Api;

#[async_trait::async_trait]
impl MetaApi for Api {
    async fn get_statistics(&self) -> GetStatisticsResponse {
        let Some(provider) = self.engine.statistics() else {
            // A reader gathers statistics only for its own planner and never
            // publishes, so "no collector here" is a distinct answer from
            // "collecting, nothing observed yet".
            let problem = super::problem::ResponseProblem::from_failure(
                crate::service::error::Failure::not_found(
                    crate::service::error::Stage::Preflight,
                    crate::service::error::NotFoundReason::NotFound,
                    "this instance runs no statistics collector".to_owned(),
                    None,
                ),
            );
            return GetStatisticsResponse::NotFound(problem.body);
        };
        let stats = provider.stats();
        let mut models: Vec<StatisticsModel> = stats
            .feedback_models
            .values()
            .map(|model| ("relation", model))
            .chain(
                stats
                    .statement_models
                    .values()
                    .map(|model| ("statement", model)),
            )
            .map(|(kind, model)| {
                let timed =
                    model.execute_micros_p95_upper_bound > 0 || model.duration_ewma_micros > 0.0;
                StatisticsModel {
                    kind: kind.to_owned(),
                    family: model.family.to_string(),
                    exact_variants: saturating(model.exact_variants),
                    frequency: i64::from(stats.frequency(&model.family)),
                    retained_executions: saturating(model.retained_executions),
                    executions_with_estimate: saturating(model.executions_with_estimate),
                    rows_p50_upper_bound: saturating(model.rows_p50_upper_bound),
                    rows_p95_upper_bound: saturating(model.rows_p95_upper_bound),
                    rows_max: saturating(model.rows_max),
                    q_error_p50_upper_bound: model.q_error_p50_upper_bound_x100 as f64 / 100.0,
                    q_error_p95_upper_bound: model.q_error_p95_upper_bound_x100 as f64 / 100.0,
                    q_error_max: model.q_error_max_x100 as f64 / 100.0,
                    execute_micros_p50_upper_bound: timed
                        .then(|| saturating(model.execute_micros_p50_upper_bound)),
                    execute_micros_p95_upper_bound: timed
                        .then(|| saturating(model.execute_micros_p95_upper_bound)),
                    duration_ewma_micros: timed
                        .then(|| saturating(model.duration_ewma_micros as u64)),
                    plans: model
                        .plans
                        .iter()
                        .map(|(plan, executions)| StatisticsPlan {
                            plan: plan.to_string(),
                            executions: saturating(*executions),
                        })
                        .collect(),
                    planning_value: model
                        .planning_value(stats.frequency(&model.family))
                        .map(planning_value_view),
                    resource_cost: model.resources.cost().map(resource_cost_view),
                }
            })
            .collect();
        models.sort_by_key(|model| std::cmp::Reverse(model.retained_executions));
        let mut synopses: Vec<StatisticsSynopsis> =
            stats.synopsis_models.values().map(synopsis_view).collect();
        synopses.sort_by_key(|synopsis| synopsis.table);
        GetStatisticsResponse::Ok(Statistics {
            absorbed: saturating(stats.absorbed),
            corpus: StatisticsCorpus {
                enabled: stats.corpus.enabled,
                captured: saturating(stats.corpus.captured),
                skipped_oversize: saturating(stats.corpus.skipped_oversize),
                dropped_queue: saturating(stats.corpus.dropped_queue),
                shed_pending: saturating(stats.corpus.shed_pending),
                maintenance: stats.corpus.maintenance.map(corpus_maintenance_view),
            },
            dropped: saturating(stats.dropped),
            evicted: saturating(stats.evicted),
            shed: saturating(stats.shed),
            relay: relay_view(provider.relay(), &stats),
            tracked_families: models.len() as i64,
            models,
            physical_cost: stats.physical_cost.as_ref().map(physical_cost_view),
            synopses,
        })
    }

    async fn get_info(&self) -> GetInfoResponse {
        let (revision, _, _) = match self.engine.schema_migration_snapshot().await {
            Ok(snapshot) => snapshot,
            Err(error) => return info_problem(server::engine_problem(&error)),
        };
        let schema_version = match i64::try_from(revision.version.get()) {
            Ok(version) => version,
            Err(error) => {
                return info_problem(server::internal_problem("encode schema version", error));
            }
        };
        GetInfoResponse::Ok(DatabaseInfo {
            access: if self.engine.is_read_only() {
                Access::Read
            } else {
                Access::Write
            },
            location: (!self.location.is_empty()).then(|| self.location.to_string()),
            mode: match self.mode {
                Mode::Direct => DatabaseInfoMode::Direct,
                Mode::Schema => DatabaseInfoMode::Schema,
            },
            schema_hash: revision.hash,
            schema_version,
            schema_version_at: (!revision.created_at.is_zero())
                .then(|| revision.created_at.as_datetime()),
        })
    }

    async fn get_healthz(&self) -> GetHealthzResponse {
        GetHealthzResponse::Ok(Health {
            access: if self.engine.is_read_only() {
                Access::Read
            } else {
                Access::Write
            },
            mode: match self.mode {
                Mode::Direct => "direct",
                Mode::Schema => "schema",
            }
            .into(),
            status: "ok".into(),
        })
    }

    async fn table_list(&self) -> TableListResponse {
        let (_, tables, _) = match self.engine.schema_migration_snapshot().await {
            Ok(snapshot) => snapshot,
            Err(error) => return table_list_problem(server::engine_problem(&error)),
        };
        TableListResponse::Ok(wire::table_list(&tables))
    }
}

fn synopsis_view(synopsis: &crate::engine::planner::models::SynopsisModel) -> StatisticsSynopsis {
    StatisticsSynopsis {
        table: i64::from(synopsis.table.get()),
        observed_rows: saturating(synopsis.observed_rows),
        coverage: match synopsis.coverage {
            crate::engine::planner::models::SynopsisCoverage::Complete => {
                StatisticsSynopsisCoverage::Complete
            }
            crate::engine::planner::models::SynopsisCoverage::PrefixLimit => {
                StatisticsSynopsisCoverage::PrefixLimit
            }
        },
        sample_size: saturating(synopsis.sample_size),
        changes_since_collection: saturating(synopsis.changes_since_collection),
        table_existence_generation: saturating(synopsis.table_existence_generation),
        collected_at_unix_micros: saturating(synopsis.collected_at_unix_micros),
        catalog_version: saturating(synopsis.catalog_version),
        columns: synopsis
            .columns
            .iter()
            .map(|column| StatisticsColumnSynopsis {
                column: i64::from(column.column.get()),
                value_generation: saturating(column.value_generation),
                null_fraction: column.null_fraction,
                null_count: saturating(column.null_count),
                distinct: saturating(column.distinct),
                distinct_is_exact: column.distinct_is_exact,
                average_width: saturating(column.average_width),
                minimum: column.minimum.clone(),
                maximum: column.maximum.clone(),
                most_common_values: column
                    .most_common_values
                    .iter()
                    .map(|common| StatisticsMostCommonValue {
                        value: common.value.to_string(),
                        frequency: saturating(common.frequency),
                        lower_frequency: saturating(common.lower_frequency()),
                        maximum_error: saturating(common.maximum_error),
                    })
                    .collect(),
            })
            .collect(),
        column_groups: synopsis
            .column_groups
            .iter()
            .map(|group| StatisticsColumnGroupSynopsis {
                columns: group
                    .columns
                    .iter()
                    .map(|column| i64::from(column.get()))
                    .collect(),
                value_generations: group
                    .value_generations
                    .iter()
                    .copied()
                    .map(saturating)
                    .collect(),
                null_count: saturating(group.null_count),
                distinct: saturating(group.distinct),
                distinct_is_exact: group.distinct_is_exact,
                most_common_values: group
                    .most_common_values
                    .iter()
                    .map(|common| StatisticsMostCommonColumnGroup {
                        values: common.values.iter().map(ToString::to_string).collect(),
                        frequency: saturating(common.frequency),
                        lower_frequency: saturating(common.lower_frequency()),
                        maximum_error: saturating(common.maximum_error),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn corpus_maintenance_view(
    maintenance: crate::engine::planner::models::CorpusMaintenanceStats,
) -> StatisticsCorpusMaintenance {
    StatisticsCorpusMaintenance {
        expired_executions: saturating(maintenance.expired_executions),
        pruned_executions: saturating(maintenance.pruned_executions),
        invalid_executions: saturating(maintenance.invalid_executions),
        pruned_programs: saturating(maintenance.pruned_programs),
        invalid_programs: saturating(maintenance.invalid_programs),
        erased_executions: saturating(maintenance.erased_executions),
        erased_programs: saturating(maintenance.erased_programs),
        retained_executions: saturating(maintenance.retained_executions),
        retained_programs: saturating(maintenance.retained_programs),
        retained_program_bytes: saturating(maintenance.retained_program_bytes),
    }
}

fn resource_cost_view(
    cost: crate::engine::planner::models::KvResourceCost,
) -> StatisticsResourceCost {
    StatisticsResourceCost {
        basis: StatisticsResourceCostBasis::LogicalKvWork,
        observed_executions: saturating(cost.observed_executions),
        gets: resource_metric_view(cost.gets),
        puts: resource_metric_view(cost.puts),
        deletes: resource_metric_view(cost.deletes),
        scans: resource_metric_view(cost.scans),
        iterated: resource_metric_view(cost.iterated),
        bytes_read: resource_metric_view(cost.bytes_read),
        bytes_written: resource_metric_view(cost.bytes_written),
    }
}

fn physical_cost_view(
    cost: &crate::engine::planner::models::PhysicalCostModel,
) -> StatisticsPhysicalCost {
    StatisticsPhysicalCost {
        backend: cost.backend.clone(),
        basis: StatisticsPhysicalCostBasis::BackendPhysicalTelemetry,
        telemetry_format: i64::from(cost.telemetry_format),
        capabilities: StatisticsPhysicalTelemetryCapabilities {
            request_latency: cost.capabilities.request_latency,
            request_bytes: cost.capabilities.request_bytes,
            request_concurrency: cost.capabilities.request_concurrency,
            cache_tiers: cost.capabilities.cache_tiers,
            access_locality: cost.capabilities.access_locality,
        },
        requests: cost
            .requests
            .iter()
            .map(|request| StatisticsPhysicalRequestCost {
                class: match request.class {
                    crate::engine::kv::telemetry::PhysicalRequestClass::Read => {
                        StatisticsPhysicalRequestCostClass::Read
                    }
                    crate::engine::kv::telemetry::PhysicalRequestClass::RangeRead => {
                        StatisticsPhysicalRequestCostClass::RangeRead
                    }
                    crate::engine::kv::telemetry::PhysicalRequestClass::MetadataRead => {
                        StatisticsPhysicalRequestCostClass::MetadataRead
                    }
                    crate::engine::kv::telemetry::PhysicalRequestClass::Write => {
                        StatisticsPhysicalRequestCostClass::Write
                    }
                    crate::engine::kv::telemetry::PhysicalRequestClass::Delete => {
                        StatisticsPhysicalRequestCostClass::Delete
                    }
                    crate::engine::kv::telemetry::PhysicalRequestClass::List => {
                        StatisticsPhysicalRequestCostClass::List
                    }
                },
                observed_requests: saturating(request.observed_requests),
                errors: saturating(request.errors),
                latency_micros: request.latency_micros.map(physical_metric_view),
                bytes: request.bytes.map(physical_metric_view),
                size_upper_bound: request.size_upper_bound.map(saturating),
                concurrency_upper_bound: request.concurrency_upper_bound.map(i64::from),
                service_tier: request.service_tier.map(|tier| match tier {
                    crate::engine::kv::telemetry::PhysicalServiceTier::Memory => {
                        StatisticsPhysicalRequestCostServiceTier::Memory
                    }
                    crate::engine::kv::telemetry::PhysicalServiceTier::Local => {
                        StatisticsPhysicalRequestCostServiceTier::Local
                    }
                    crate::engine::kv::telemetry::PhysicalServiceTier::Remote => {
                        StatisticsPhysicalRequestCostServiceTier::Remote
                    }
                }),
            })
            .collect(),
        caches: cost
            .caches
            .iter()
            .map(|cache| StatisticsPhysicalCacheCost {
                tier: match cache.tier {
                    crate::engine::kv::telemetry::PhysicalCacheTier::Memory => {
                        StatisticsPhysicalCacheCostTier::Memory
                    }
                    crate::engine::kv::telemetry::PhysicalCacheTier::Local => {
                        StatisticsPhysicalCacheCostTier::Local
                    }
                },
                accesses: saturating(cache.accesses),
                hits: saturating(cache.hits),
                hit_rate_ppm: saturating(cache.hit_rate_ppm),
            })
            .collect(),
    }
}

fn physical_metric_view(
    metric: crate::engine::planner::models::PhysicalCostMetric,
) -> StatisticsPhysicalCostMetric {
    StatisticsPhysicalCostMetric {
        p50_upper_bound: saturating(metric.p50_upper_bound),
        p95_upper_bound: saturating(metric.p95_upper_bound),
        maximum_upper_bound: saturating(metric.maximum_upper_bound),
    }
}

fn resource_metric_view(
    metric: crate::engine::planner::models::ResourceMetric,
) -> StatisticsResourceMetric {
    StatisticsResourceMetric {
        p50_upper_bound: saturating(metric.p50_upper_bound),
        p95_upper_bound: saturating(metric.p95_upper_bound),
        maximum: saturating(metric.maximum),
    }
}

fn planning_value_view(
    value: crate::engine::planner::models::PlanningValue,
) -> StatisticsPlanningValue {
    StatisticsPlanningValue {
        basis: value.basis.to_owned(),
        comparable_executions: saturating(value.comparable_executions),
        cost_difference_micros: saturating(value.cost_difference_micros),
        frequency: i64::from(value.frequency),
        minimum_executions: saturating(value.minimum_executions),
        observed_plan_variation_ppm: saturating(value.observed_plan_variation_ppm),
        row_count_class_upper_bound: saturating(value.row_count_class_upper_bound),
        score: saturating(value.score),
        uncertainty_ppm: saturating(value.uncertainty_ppm),
    }
}

fn info_problem(problem: super::problem::ResponseProblem) -> GetInfoResponse {
    GetInfoResponse::Default(problem.status, problem.body)
}

fn table_list_problem(problem: super::problem::ResponseProblem) -> TableListResponse {
    TableListResponse::Default(problem.status, problem.body)
}

/// Both directions of the observation relay.
///
/// The sending fields are absent rather than zero on an instance that
/// publishes to storage: a writer that never relays has no send count, and
/// reporting zero would read as a relay that is failing to send.
fn relay_view(
    report: crate::scheduler::statistics::RelayReport,
    stats: &crate::engine::planner::models::PlannerStats,
) -> StatisticsRelay {
    let receiving = report.receiving;
    let sending = report.sending;
    StatisticsRelay {
        state: sending.map(|counters| match counters.health() {
            crate::scheduler::relay::RelayHealth::Connected => StatisticsRelayState::Connected,
            crate::scheduler::relay::RelayHealth::Retrying => StatisticsRelayState::Retrying,
            crate::scheduler::relay::RelayHealth::Losing => StatisticsRelayState::Losing,
            crate::scheduler::relay::RelayHealth::Idle => StatisticsRelayState::Idle,
        }),
        sent: sending.map(|counters| saturating(counters.sent)),
        abandoned: sending.map(|counters| saturating(counters.abandoned)),
        rejected: sending.map(|counters| saturating(counters.rejected)),
        holding: sending.map(|counters| counters.holding),
        corpus_sent: sending.map(|counters| saturating(counters.corpus_sent)),
        corpus_withheld: sending.map(|counters| saturating(counters.corpus_withheld)),
        received: saturating(receiving.accepted),
        received_already_applied: saturating(receiving.already_admitted),
        received_rejected: saturating(receiving.format_mismatch),
        received_saturated: saturating(receiving.saturated),
        received_corpus_oversize: saturating(receiving.corpus_oversize),
        sources: receiving.sources as i64,
        merged_observations: saturating(stats.relayed),
        corpus_adopted: saturating(stats.relayed_corpus),
        refused_stale_families: saturating(stats.relayed_stale),
    }
}

/// Counts are unsigned internally and signed on the wire; a count large enough
/// to overflow is already meaningless, so it saturates rather than wrapping.
fn saturating(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
