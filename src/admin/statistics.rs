//! Read-only statistics diagnostics for the administration UI.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use serde::{Deserialize, Serialize};

use crate::scheduler::statistics::StatisticsRunner;

pub(super) fn router(runner: Option<Arc<StatisticsRunner>>) -> Router {
    Router::new()
        .route("/statistics", get(statistics))
        .route("/api/statistics/corpus", delete(erase_corpus))
        .route("/api/statistics/corpus/replay", post(replay_corpus))
        .with_state(runner)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayQuery {
    limit: Option<usize>,
}

async fn replay_corpus(
    State(runner): State<Option<Arc<StatisticsRunner>>>,
    Json(query): Json<ReplayQuery>,
) -> Response {
    let Some(runner) = runner else {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"detail": "the workload corpus is unavailable"})),
        )
            .into_response();
    };
    let limit = query
        .limit
        .unwrap_or(crate::scheduler::replay::DEFAULT_REPLAY_LIMIT);
    if limit == 0 || limit > crate::scheduler::replay::MAX_REPLAY_LIMIT {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "detail": format!(
                    "limit must be between 1 and {}",
                    crate::scheduler::replay::MAX_REPLAY_LIMIT
                ),
            })),
        )
            .into_response();
    }
    match runner.replay_corpus(limit).await {
        Ok(report) => Json(report).into_response(),
        Err(crate::scheduler::statistics::CorpusReplayError::Unavailable) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"detail": "the workload corpus is unavailable"})),
        )
            .into_response(),
        Err(crate::scheduler::statistics::CorpusReplayError::EngineUnavailable) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"detail": "the replay engine is unavailable"})),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"detail": error.to_string()})),
        )
            .into_response(),
    }
}

async fn erase_corpus(State(runner): State<Option<Arc<StatisticsRunner>>>) -> Response {
    let Some(runner) = runner else {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"detail": "this process does not own the workload corpus"})),
        )
            .into_response();
    };
    match runner.erase_corpus().await {
        Ok(deleted) => Json(serde_json::json!({"deleted": deleted})).into_response(),
        Err(crate::scheduler::statistics::CorpusEraseError::Unavailable) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"detail": "this process does not own the workload corpus"})),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"detail": error.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatisticsView {
    absorbed: u64,
    dropped: u64,
    evicted: u64,
    corpus: crate::engine::planner::models::CorpusCaptureStats,
    tracked_families: usize,
    models: Vec<ModelView>,
    synopses: Vec<crate::engine::planner::models::SynopsisModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    physical_cost: Option<crate::engine::planner::models::PhysicalCostModel>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelView {
    kind: &'static str,
    /// Family fingerprint: the relation shape with literals stripped.
    family: String,
    /// Approximate distinct literal-specific instances seen within this
    /// family.
    exact_variants: u64,
    /// Approximate number of times this family appeared anywhere in an
    /// executed query tree, from the frequency sketch. Over-counts on hash
    /// collision and halves periodically, so it tracks recent workload weight
    /// rather than a lifetime total.
    frequency: u32,
    /// Times this family was itself the measured relation or statement. Lower
    /// than `frequency` because a family counts once per appearance as a
    /// subtree but is only measured where a plan boundary exposes its rows.
    retained_executions: u64,
    /// Of those, how many carried a planner estimate to score against.
    executions_with_estimate: u64,
    /// Row counts. The percentiles are histogram bucket bounds, so they are
    /// upper bounds rather than exact quantiles; `rowsMax` is exact.
    rows_p50_upper_bound: u64,
    rows_p95_upper_bound: u64,
    rows_max: u64,
    /// Symmetric multiplicative estimate error: 1.0 is perfect, 10.0 is wrong
    /// by a factor of ten in either direction. Percentiles are bucket bounds;
    /// the maximum is exact.
    q_error_p50_upper_bound: f64,
    q_error_p95_upper_bound: f64,
    q_error_max: f64,
    /// Execution latency. Percentiles are bucket bounds; absent when this
    /// family was measured only as a relation inside a statement, which
    /// records rows but not durations.
    #[serde(skip_serializing_if = "Option::is_none")]
    execute_micros_p50_upper_bound: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    execute_micros_p95_upper_bound: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ewma_micros: Option<u64>,
    /// Physical plans observed for this family, with how often each ran.
    plans: Vec<PlanView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    planning_value: Option<crate::engine::planner::models::PlanningValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resource_cost: Option<crate::engine::planner::models::KvResourceCost>,
}

#[derive(Serialize)]
struct PlanView {
    plan: String,
    executions: u64,
}

/// A process without a collector is not the same as a collector with nothing
/// to report, so the absent case is a status rather than a body of zeroes.
async fn statistics(State(runner): State<Option<Arc<StatisticsRunner>>>) -> Response {
    let Some(runner) = runner else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "detail": "this process runs no statistics collector; the writer collects",
            })),
        )
            .into_response();
    };
    let stats = runner.stats();
    let mut models: Vec<ModelView> = stats
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
            ModelView {
                kind,
                family: model.family.to_string(),
                exact_variants: model.exact_variants,
                frequency: stats.frequency(&model.family),
                retained_executions: model.retained_executions,
                executions_with_estimate: model.executions_with_estimate,
                rows_p50_upper_bound: model.rows_p50_upper_bound,
                rows_p95_upper_bound: model.rows_p95_upper_bound,
                rows_max: model.rows_max,
                q_error_p50_upper_bound: model.q_error_p50_upper_bound_x100 as f64 / 100.0,
                q_error_p95_upper_bound: model.q_error_p95_upper_bound_x100 as f64 / 100.0,
                q_error_max: model.q_error_max_x100 as f64 / 100.0,
                execute_micros_p50_upper_bound: timed
                    .then_some(model.execute_micros_p50_upper_bound),
                execute_micros_p95_upper_bound: timed
                    .then_some(model.execute_micros_p95_upper_bound),
                duration_ewma_micros: timed.then_some(model.duration_ewma_micros as u64),
                plans: model
                    .plans
                    .iter()
                    .map(|(plan, executions)| PlanView {
                        plan: plan.to_string(),
                        executions: *executions,
                    })
                    .collect(),
                planning_value: model.planning_value(stats.frequency(&model.family)),
                resource_cost: model.resources.cost(),
            }
        })
        .collect();
    models.sort_by_key(|model| std::cmp::Reverse(model.retained_executions));
    let mut synopses: Vec<_> = stats.synopsis_models.values().cloned().collect();
    synopses.sort_by_key(|synopsis| synopsis.table);
    Json(StatisticsView {
        absorbed: stats.absorbed,
        dropped: stats.dropped,
        evicted: stats.evicted,
        corpus: stats.corpus,
        tracked_families: models.len(),
        models,
        synopses,
        physical_cost: stats.physical_cost.clone(),
    })
    .into_response()
}
