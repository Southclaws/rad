//! Estimator evaluation over recorded statement cardinalities.
//!
//! Replay binds and plans canonical programs but never executes a physical
//! plan. Mutation payloads therefore cannot change data during evaluation.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::Serialize;

use crate::engine::exec::{Engine, PreparedStatementEstimate};
use crate::engine::planner::estimator::{Estimate, Estimator};
use crate::engine::planner::models::{
    DependencyStamp, KvResourceCost, KvResourceDistribution, PlannerStats, q_error_x100,
};

use super::statistics::CorpusExecution;

pub const DEFAULT_REPLAY_LIMIT: usize = 1_000;
pub const MAX_REPLAY_LIMIT: usize = 10_000;
const MIN_PLAN_PROFILE_EXECUTIONS: u64 = 3;

pub trait CandidateEstimator: Send + Sync {
    fn name(&self) -> &'static str;

    /// Return `None` when the candidate has no applicable evidence. The
    /// report then scores the active estimate as the candidate fallback.
    fn estimate(
        &self,
        statistics: &PlannerStats,
        statement: &PreparedStatementEstimate,
    ) -> Option<Estimate>;
}

pub struct FamilyFeedbackCandidate;

impl CandidateEstimator for FamilyFeedbackCandidate {
    fn name(&self) -> &'static str {
        "family_feedback_with_active_fallback"
    }

    fn estimate(
        &self,
        statistics: &PlannerStats,
        statement: &PreparedStatementEstimate,
    ) -> Option<Estimate> {
        let family = crate::engine::lir::fingerprint::query(&statement.query)
            .root
            .family;
        let stamp = DependencyStamp::of(&statement.plan.dependencies);
        Estimator::new(statistics).family_feedback(&family, stamp)
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CorpusReplayReport {
    pub corpus_digest: String,
    pub catalog_version: u64,
    pub catalog_hash: String,
    pub statistics_published_at_micros: u64,
    pub executions_considered: usize,
    pub executions_scored: usize,
    pub executions_without_actuals: usize,
    pub incompatible_executions: usize,
    pub statements_scored: usize,
    pub unmatched_statements: usize,
    pub candidate_evidence: usize,
    pub candidate_better: usize,
    pub candidate_equal: usize,
    pub candidate_worse: usize,
    pub baseline: EstimatorScore,
    pub candidate: EstimatorScore,
    pub observed_plan_regret: ObservedPlanRegretScore,
    pub observed_resource_cost: ObservedResourceCostScore,
    pub physical_cost: Option<crate::engine::planner::models::PhysicalCostModel>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EstimatorScore {
    pub estimator: &'static str,
    pub observations: usize,
    pub q_error_p50: f64,
    pub q_error_p95: f64,
    pub q_error_max: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservedPlanRegretScore {
    pub basis: &'static str,
    pub minimum_plan_executions: u64,
    pub statements_compared: usize,
    pub statements_without_comparison: usize,
    pub families_compared: usize,
    pub slowdown_p50: f64,
    pub slowdown_p95: f64,
    pub slowdown_max: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservedResourceCostScore {
    pub basis: &'static str,
    pub plan_profiles_considered: usize,
    pub plan_profiles_with_cost: usize,
    pub plan_profiles_without_cost: usize,
    pub cost: Option<KvResourceCost>,
}

struct PreparedProgram {
    statements: HashMap<String, PreparedStatementEstimate>,
}

pub async fn replay(
    engine: &Engine,
    statistics: Arc<PlannerStats>,
    executions: Vec<CorpusExecution>,
    candidate: &dyn CandidateEstimator,
    catalog_version: u64,
    catalog_hash: String,
) -> CorpusReplayReport {
    let corpus_digest = corpus_digest(&executions);
    let mut prepared = HashMap::<[u8; 16], Option<PreparedProgram>>::new();
    let mut baseline_errors = Vec::new();
    let mut candidate_errors = Vec::new();
    let mut executions_scored = 0;
    let mut executions_without_actuals = 0;
    let mut incompatible_executions = 0;
    let mut unmatched_statements = 0;
    let mut candidate_evidence = 0;
    let mut candidate_better = 0;
    let mut candidate_equal = 0;
    let mut candidate_worse = 0;
    let mut plan_regrets = Vec::new();
    let mut plan_families = HashSet::new();
    let mut resource_profile_keys = HashSet::new();
    let mut resource_profiles = KvResourceDistribution::default();
    let mut resource_profiles_considered = 0;
    let mut resource_profiles_with_cost = 0;

    for execution in &executions {
        if execution.outcomes.is_empty() {
            executions_without_actuals += 1;
            continue;
        }
        if let std::collections::hash_map::Entry::Vacant(entry) =
            prepared.entry(execution.content_hash)
        {
            let program = serde_json::from_slice(execution.canonical.as_slice())
                .ok()
                .and_then(|wire| crate::protocol::lower_pir(wire).ok());
            let estimates = match program {
                Some(program) => engine
                    .prepare_program_estimates(&program, statistics.clone())
                    .await
                    .ok(),
                None => None,
            };
            entry.insert(estimates.map(|estimates| {
                PreparedProgram {
                    statements: estimates
                        .into_iter()
                        .map(|estimate| (estimate.name.clone(), estimate))
                        .collect(),
                }
            }));
        }
        let Some(program) = prepared
            .get(&execution.content_hash)
            .and_then(Option::as_ref)
        else {
            incompatible_executions += 1;
            continue;
        };

        let mut scored = false;
        for outcome in &execution.outcomes {
            let Some(statement) = program.statements.get(&outcome.name) else {
                unmatched_statements += 1;
                continue;
            };
            scored = true;
            let baseline_error = q_error_x100(statement.active.cardinality, outcome.rows);
            let candidate_estimate = candidate.estimate(&statistics, statement);
            if candidate_estimate.is_some() {
                candidate_evidence += 1;
            }
            let candidate_error = q_error_x100(
                candidate_estimate.unwrap_or(statement.active).cardinality,
                outcome.rows,
            );
            baseline_errors.push(baseline_error);
            candidate_errors.push(candidate_error);
            match candidate_error.cmp(&baseline_error) {
                std::cmp::Ordering::Less => candidate_better += 1,
                std::cmp::Ordering::Equal => candidate_equal += 1,
                std::cmp::Ordering::Greater => candidate_worse += 1,
            }
            if let Some((family, regret)) = observed_plan_regret_x100(&statistics, statement) {
                plan_families.insert(family);
                plan_regrets.push(regret);
            }
            let (family, plan, access_stamp, profile) = active_plan_profile(&statistics, statement);
            if resource_profile_keys.insert((family, plan, access_stamp)) {
                resource_profiles_considered += 1;
                if let Some(profile) = profile
                    && profile.resources.cost().is_some()
                {
                    resource_profiles.merge(&profile.resources);
                    resource_profiles_with_cost += 1;
                }
            }
        }
        if scored {
            executions_scored += 1;
        } else {
            incompatible_executions += 1;
        }
    }

    CorpusReplayReport {
        corpus_digest,
        catalog_version,
        catalog_hash,
        statistics_published_at_micros: u64::try_from(statistics.published_at.as_micros())
            .unwrap_or(u64::MAX),
        executions_considered: executions.len(),
        executions_scored,
        executions_without_actuals,
        incompatible_executions,
        statements_scored: baseline_errors.len(),
        unmatched_statements,
        candidate_evidence,
        candidate_better,
        candidate_equal,
        candidate_worse,
        baseline: score("active", &mut baseline_errors),
        candidate: score(candidate.name(), &mut candidate_errors),
        observed_plan_regret: plan_regret_score(
            &mut plan_regrets,
            baseline_errors.len(),
            plan_families.len(),
        ),
        observed_resource_cost: ObservedResourceCostScore {
            basis: "retained_active_plan_profiles_for_replayed_statement_families",
            plan_profiles_considered: resource_profiles_considered,
            plan_profiles_with_cost: resource_profiles_with_cost,
            plan_profiles_without_cost: resource_profiles_considered
                .saturating_sub(resource_profiles_with_cost),
            cost: resource_profiles.cost(),
        },
        physical_cost: statistics.physical_cost.clone(),
    }
}

fn active_plan_profile<'a>(
    statistics: &'a PlannerStats,
    statement: &PreparedStatementEstimate,
) -> (
    crate::engine::lir::fingerprint::Fingerprint,
    crate::engine::lir::fingerprint::Fingerprint,
    u64,
    Option<&'a crate::engine::planner::models::PlanProfile>,
) {
    let family = crate::engine::lir::fingerprint::query(&statement.query)
        .root
        .family;
    let plan = statement.plan.fingerprint();
    let access_stamp = DependencyStamp::of(&statement.plan.dependencies).access;
    let profile = statistics.statement_models.get(&family).and_then(|model| {
        model
            .plan_profiles
            .iter()
            .find(|profile| profile.plan == plan && profile.access_stamp == access_stamp)
    });
    (family, plan, access_stamp, profile)
}

fn observed_plan_regret_x100(
    statistics: &PlannerStats,
    statement: &PreparedStatementEstimate,
) -> Option<(crate::engine::lir::fingerprint::Fingerprint, u64)> {
    let family = crate::engine::lir::fingerprint::query(&statement.query)
        .root
        .family;
    let model = statistics.statement_models.get(&family)?;
    let access_stamp = DependencyStamp::of(&statement.plan.dependencies).access;
    let active_plan = statement.plan.fingerprint();
    plan_profile_regret_x100(active_plan, access_stamp, &model.plan_profiles)
        .map(|regret| (family, regret))
}

fn plan_profile_regret_x100(
    active_plan: crate::engine::lir::fingerprint::Fingerprint,
    access_stamp: u64,
    profiles: &[crate::engine::planner::models::PlanProfile],
) -> Option<u64> {
    let active = profiles.iter().find(|profile| {
        profile.plan == active_plan
            && profile.access_stamp == access_stamp
            && profile.executions >= MIN_PLAN_PROFILE_EXECUTIONS
    })?;
    let row_class = active.rows_p50_upper_bound();
    let active_cost = active.execute_micros_p50_upper_bound();
    if active_cost == 0 {
        return None;
    }
    let alternatives: Vec<_> = profiles
        .iter()
        .filter(|profile| {
            profile.access_stamp == access_stamp
                && profile.executions >= MIN_PLAN_PROFILE_EXECUTIONS
                && profile.rows_p50_upper_bound() == row_class
                && profile.execute_micros_p50_upper_bound() > 0
        })
        .collect();
    if alternatives
        .iter()
        .map(|profile| profile.plan)
        .collect::<HashSet<_>>()
        .len()
        < 2
    {
        return None;
    }
    let best_cost = alternatives
        .iter()
        .map(|profile| profile.execute_micros_p50_upper_bound())
        .min()?;
    Some(active_cost.saturating_mul(100).div_ceil(best_cost).max(100))
}

fn corpus_digest(executions: &[CorpusExecution]) -> String {
    use sha2::{Digest as _, Sha256};

    let mut digest = Sha256::new();
    for execution in executions {
        digest.update(execution.content_hash);
        digest.update(execution.at_unix_micros.to_be_bytes());
        digest.update(execution.statements.to_be_bytes());
        digest.update((execution.outcomes.len() as u64).to_be_bytes());
        for outcome in &execution.outcomes {
            digest.update((outcome.name.len() as u64).to_be_bytes());
            digest.update(outcome.name.as_bytes());
            digest.update(outcome.rows.to_be_bytes());
        }
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn score(estimator: &'static str, errors: &mut [u64]) -> EstimatorScore {
    errors.sort_unstable();
    EstimatorScore {
        estimator,
        observations: errors.len(),
        q_error_p50: quantile(errors, 50) as f64 / 100.0,
        q_error_p95: quantile(errors, 95) as f64 / 100.0,
        q_error_max: errors.last().copied().unwrap_or(0) as f64 / 100.0,
    }
}

fn plan_regret_score(
    regrets: &mut [u64],
    statements_scored: usize,
    families_compared: usize,
) -> ObservedPlanRegretScore {
    regrets.sort_unstable();
    ObservedPlanRegretScore {
        basis: "historical_p50_execution_time_for_same_family_row_class_and_access_stamp",
        minimum_plan_executions: MIN_PLAN_PROFILE_EXECUTIONS,
        statements_compared: regrets.len(),
        statements_without_comparison: statements_scored.saturating_sub(regrets.len()),
        families_compared,
        slowdown_p50: quantile(regrets, 50) as f64 / 100.0,
        slowdown_p95: quantile(regrets, 95) as f64 / 100.0,
        slowdown_max: regrets.last().copied().unwrap_or(0) as f64 / 100.0,
    }
}

fn quantile(sorted: &[u64], percentile: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = percentile
        .saturating_mul(sorted.len())
        .div_ceil(100)
        .saturating_sub(1);
    sorted[rank.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::engine::exec::observe::ProgramStatementOutcome;
    use crate::engine::kv::slatedb::Store;
    use crate::engine::planner::models::{FeedbackModel, KvResourceSample, PlanProfile};

    use super::*;

    #[test]
    fn exact_quantiles_use_nearest_ranks() {
        let mut errors = vec![100, 200, 300, 400, 500];
        let score = score("test", &mut errors);
        assert_eq!(score.q_error_p50, 3.0);
        assert_eq!(score.q_error_p95, 5.0);
        assert_eq!(score.q_error_max, 5.0);
    }

    #[test]
    fn plan_regret_requires_comparable_plan_profiles() {
        let active_plan = crate::engine::lir::fingerprint::Fingerprint {
            canonicalization_version: 1,
            hash_algorithm: 1,
            digest: [1; 16],
        };
        let alternative_plan = crate::engine::lir::fingerprint::Fingerprint {
            digest: [2; 16],
            ..active_plan
        };
        let profile = |plan, access_stamp, rows, execute_micros, executions| {
            let mut profile = PlanProfile::new(plan, access_stamp);
            for _ in 0..executions {
                profile.record(rows, execute_micros);
            }
            profile
        };
        let active = profile(active_plan, 7, 10, 40, 3);

        assert_eq!(
            plan_profile_regret_x100(
                active_plan,
                7,
                &[active.clone(), profile(alternative_plan, 8, 10, 10, 3)]
            ),
            None
        );
        assert_eq!(
            plan_profile_regret_x100(
                active_plan,
                7,
                &[active.clone(), profile(alternative_plan, 7, 20, 10, 3)]
            ),
            None
        );
        assert_eq!(
            plan_profile_regret_x100(
                active_plan,
                7,
                &[active.clone(), profile(alternative_plan, 7, 10, 10, 2)]
            ),
            None
        );
        assert_eq!(
            plan_profile_regret_x100(
                active_plan,
                7,
                &[active, profile(alternative_plan, 7, 10, 10, 3)]
            ),
            Some(400)
        );
    }

    #[tokio::test]
    async fn replay_compares_a_candidate_with_recorded_actuals() {
        let wire = serde_json::from_str(
            r#"{
                "statements": [{
                    "kind": "query",
                    "name": "read",
                    "relation": {
                        "nodes": {
                            "rows": {
                                "kind": "rows",
                                "scope": "r",
                                "columns": [{"name": "id", "type": "int64"}],
                                "rows": [["1"]]
                            },
                            "filtered": {
                                "kind": "filter",
                                "input": "rows",
                                "predicate": {
                                    "kind": "lit",
                                    "value": {"type": "bool", "value": true}
                                }
                            },
                            "ordered": {
                                "kind": "order",
                                "input": "filtered",
                                "terms": [{
                                    "expr": {"kind": "col", "scope": "r", "column": "id"}
                                }]
                            }
                        },
                        "root": {"node": "ordered", "cardinality": "many"}
                    }
                }],
                "result": "read"
            }"#,
        )
        .unwrap();
        let (canonical, content_hash) =
            crate::engine::frontend::canonical_program_bytes(&wire).unwrap();
        let program = crate::protocol::lower_pir(wire).unwrap();
        let store = Arc::new(Store::memory("statistics-corpus-replay").await.unwrap());
        let engine = Engine::new(store);
        let empty = Arc::new(PlannerStats::empty());
        let prepared = engine
            .prepare_program_estimates(&program, empty)
            .await
            .unwrap();
        assert_eq!(prepared[0].active.cardinality, 1_000);
        let family = crate::engine::lir::fingerprint::query(&prepared[0].query)
            .root
            .family;
        let stamp = DependencyStamp::of(&prepared[0].plan.dependencies);
        let active_plan = prepared[0].plan.fingerprint();
        let mut alternative_plan = active_plan;
        alternative_plan.digest[15] ^= 1;
        let mut active_profile = PlanProfile::new(active_plan, stamp.access);
        let mut alternative_profile = PlanProfile::new(alternative_plan, stamp.access);
        for _ in 0..MIN_PLAN_PROFILE_EXECUTIONS {
            active_profile.record_with_resources(
                1,
                40,
                KvResourceSample {
                    gets: 2,
                    scans: 1,
                    iterated: 3,
                    bytes_read: 100,
                    ..KvResourceSample::default()
                },
            );
            alternative_profile.record(1, 10);
        }
        let mut statistics = PlannerStats::empty();
        let model = FeedbackModel {
            family,
            retained_executions: 20,
            exact_variants: 1,
            last_seen: Duration::ZERO,
            rows_p50_upper_bound: 1,
            rows_p95_upper_bound: 1,
            rows_max: 1,
            execute_micros_p50_upper_bound: 0,
            execute_micros_p95_upper_bound: 0,
            duration_ewma_micros: 0.0,
            plans: Vec::new(),
            plan_profiles: vec![active_profile, alternative_profile],
            resources: Default::default(),
            stamp,
            executions_with_estimate: 0,
            q_error_p50_upper_bound_x100: 0,
            q_error_p95_upper_bound_x100: 0,
            q_error_max_x100: 0,
        };
        statistics.feedback_models.insert(family, model.clone());
        statistics.statement_models.insert(family, model);

        let report = replay(
            &engine,
            Arc::new(statistics),
            vec![CorpusExecution {
                canonical,
                content_hash,
                at_unix_micros: 1,
                statements: 1,
                outcomes: vec![ProgramStatementOutcome {
                    name: "read".into(),
                    rows: 1,
                }],
            }],
            &FamilyFeedbackCandidate,
            7,
            "catalog-hash".into(),
        )
        .await;

        assert_eq!(report.executions_scored, 1);
        assert_eq!(report.catalog_version, 7);
        assert_eq!(report.corpus_digest.len(), 64);
        assert_eq!(report.statements_scored, 1);
        assert_eq!(report.candidate_evidence, 1);
        assert_eq!(report.candidate_better, 1);
        assert_eq!(report.baseline.q_error_max, 1_000.0);
        assert_eq!(report.candidate.q_error_max, 1.0);
        assert_eq!(report.observed_plan_regret.statements_compared, 1);
        assert_eq!(report.observed_plan_regret.families_compared, 1);
        assert_eq!(report.observed_plan_regret.slowdown_p50, 4.0);
        assert_eq!(report.observed_resource_cost.plan_profiles_considered, 1);
        assert_eq!(report.observed_resource_cost.plan_profiles_with_cost, 1);
        let cost = report.observed_resource_cost.cost.expect("resource cost");
        assert_eq!(cost.observed_executions, 3);
        assert_eq!(cost.gets.maximum, 2);
        assert_eq!(cost.scans.maximum, 1);
        assert_eq!(cost.iterated.maximum, 3);
        assert_eq!(cost.bytes_read.maximum, 100);
    }
}
