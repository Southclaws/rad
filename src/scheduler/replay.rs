//! Estimator evaluation over recorded statement cardinalities.
//!
//! Replay binds and plans canonical programs but never executes a physical
//! plan. Mutation payloads therefore cannot change data during evaluation.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::Serialize;

use crate::engine::exec::{Engine, PreparedStatementEstimate};
use crate::engine::planner::PlannerMode;
use crate::engine::planner::estimator::{Estimate, Estimator};
use crate::engine::planner::models::{
    DependencyStamp, KvResourceCost, KvResourceDistribution, PlannerStats, q_error_x100,
};
use crate::engine::planner::physical::{
    AccessCandidate, AccessDecisionBasis, JoinCandidate, JoinDecisionBasis, NodeKind, Plan,
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
    pub statistics_snapshot_identity: String,
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
    pub planner: PlannerReplayScore,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannerReplayScore {
    pub statements_compared: usize,
    pub structural_evidence_coverage: usize,
    pub cost_evidence_coverage: usize,
    pub plans_changed: usize,
    pub cost_dominance_decisions: usize,
    pub structural_fallback_decisions: usize,
    pub memo_expressions_changed: usize,
    pub memo_budget_stops: usize,
    pub equality_saturation_additional_alternatives: u64,
    pub structural_predicted_row_work: u64,
    pub cost_predicted_row_work: u64,
    pub structural_predicted_workload_regret: u64,
    pub cost_predicted_workload_regret: u64,
    pub decisions: Vec<PlannerReplayDecision>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannerReplayDecision {
    pub statement: String,
    pub structural_plan: crate::engine::lir::fingerprint::Fingerprint,
    pub cost_plan: crate::engine::lir::fingerprint::Fingerprint,
    pub changed: bool,
    pub structural_row_work: Option<u64>,
    pub cost_row_work: Option<u64>,
    pub cost_candidates: Vec<AccessCandidate>,
    pub cost_join_candidates: Vec<JoinCandidate>,
    pub structural_memo: MemoReplaySummary,
    pub cost_memo: MemoReplaySummary,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoReplaySummary {
    pub groups: u32,
    pub alternatives: u32,
    pub rule_applications: u32,
    pub planning_effort: u32,
    pub changed_expressions: usize,
    pub selected_proof_edges: usize,
    pub directed_alternatives: u64,
    pub saturated_alternatives: u64,
    pub additional_saturated_alternatives: u64,
    pub stop_reason: Option<crate::engine::planner::memo::MemoStopReason>,
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
    structural: HashMap<String, PreparedStatementEstimate>,
    cost: HashMap<String, PreparedStatementEstimate>,
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
    let mut replayed_plans = HashSet::new();
    let mut planner_decisions = Vec::new();
    let mut structural_evidence_coverage = 0;
    let mut cost_evidence_coverage = 0;
    let mut plans_changed = 0;
    let mut cost_dominance_decisions = 0;
    let mut structural_fallback_decisions = 0;
    let mut memo_expressions_changed = 0;
    let mut memo_budget_stops = 0;
    let mut equality_saturation_additional_alternatives = 0u64;
    let mut structural_predicted_row_work = 0u64;
    let mut cost_predicted_row_work = 0u64;
    let mut structural_predicted_workload_regret = 0u64;
    let mut cost_predicted_workload_regret = 0u64;

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
                Some(program) => {
                    let structural = engine
                        .prepare_program_estimates_with_mode(
                            &program,
                            statistics.clone(),
                            PlannerMode::Structural,
                        )
                        .await
                        .ok();
                    let cost = engine
                        .prepare_program_estimates_with_mode(
                            &program,
                            statistics.clone(),
                            PlannerMode::Cost,
                        )
                        .await
                        .ok();
                    structural.zip(cost)
                }
                None => None,
            };
            entry.insert(estimates.map(|(structural, cost)| {
                PreparedProgram {
                    structural: structural
                        .into_iter()
                        .map(|estimate| (estimate.name.clone(), estimate))
                        .collect(),
                    cost: cost
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
            let Some(statement) = program.structural.get(&outcome.name) else {
                unmatched_statements += 1;
                continue;
            };
            let Some(cost_statement) = program.cost.get(&outcome.name) else {
                unmatched_statements += 1;
                continue;
            };
            if replayed_plans.insert((execution.content_hash, outcome.name.clone())) {
                let structural_work = predicted_row_work(&statement.plan);
                let (cost_work, cost_candidates, cost_join_candidates) =
                    cost_plan_summary(&cost_statement.plan);
                structural_evidence_coverage += usize::from(structural_work.is_some());
                cost_evidence_coverage += usize::from(cost_work.is_some());
                if statement.plan.fingerprint() != cost_statement.plan.fingerprint() {
                    plans_changed += 1;
                }
                let structural_memo = memo_summary(&statement.plan);
                let cost_memo = memo_summary(&cost_statement.plan);
                memo_expressions_changed += cost_memo.changed_expressions;
                memo_budget_stops += usize::from(cost_memo.stop_reason.is_some());
                equality_saturation_additional_alternatives =
                    equality_saturation_additional_alternatives
                        .saturating_add(cost_memo.additional_saturated_alternatives);
                for candidate in cost_candidates.iter().filter(|candidate| candidate.chosen) {
                    match candidate.decision_basis {
                        Some(AccessDecisionBasis::CostDominance) => cost_dominance_decisions += 1,
                        Some(AccessDecisionBasis::Structural) => structural_fallback_decisions += 1,
                        _ => {}
                    }
                }
                for candidate in cost_join_candidates
                    .iter()
                    .filter(|candidate| candidate.chosen)
                {
                    match candidate.decision_basis {
                        Some(
                            JoinDecisionBasis::BoundedBuild | JoinDecisionBasis::CostDominance,
                        ) => cost_dominance_decisions += 1,
                        Some(JoinDecisionBasis::Structural) => structural_fallback_decisions += 1,
                        _ => {}
                    }
                }
                structural_predicted_row_work = structural_predicted_row_work
                    .saturating_add(structural_work.unwrap_or_default());
                cost_predicted_row_work =
                    cost_predicted_row_work.saturating_add(cost_work.unwrap_or_default());
                if let (Some(structural_work), Some(cost_work)) = (structural_work, cost_work) {
                    let family = crate::engine::lir::fingerprint::query(&statement.query)
                        .root
                        .family;
                    let weight = u64::from(statistics.frequency(&family)).max(1);
                    let best = structural_work.min(cost_work);
                    structural_predicted_workload_regret = structural_predicted_workload_regret
                        .saturating_add(
                            structural_work.saturating_sub(best).saturating_mul(weight),
                        );
                    cost_predicted_workload_regret = cost_predicted_workload_regret
                        .saturating_add(cost_work.saturating_sub(best).saturating_mul(weight));
                }
                planner_decisions.push(PlannerReplayDecision {
                    statement: outcome.name.clone(),
                    structural_plan: statement.plan.fingerprint(),
                    cost_plan: cost_statement.plan.fingerprint(),
                    changed: statement.plan.fingerprint() != cost_statement.plan.fingerprint(),
                    structural_row_work: structural_work,
                    cost_row_work: cost_work,
                    cost_candidates,
                    cost_join_candidates,
                    structural_memo,
                    cost_memo,
                });
            }
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
        statistics_snapshot_identity: statistics.snapshot_identity.clone(),
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
        planner: PlannerReplayScore {
            statements_compared: planner_decisions.len(),
            structural_evidence_coverage,
            cost_evidence_coverage,
            plans_changed,
            cost_dominance_decisions,
            structural_fallback_decisions,
            memo_expressions_changed,
            memo_budget_stops,
            equality_saturation_additional_alternatives,
            structural_predicted_row_work,
            cost_predicted_row_work,
            structural_predicted_workload_regret,
            cost_predicted_workload_regret,
            decisions: planner_decisions,
        },
    }
}

fn memo_summary(plan: &Plan) -> MemoReplaySummary {
    let directed_alternatives = plan
        .memo
        .roots
        .iter()
        .map(|root| u64::from(root.directed_alternatives))
        .sum::<u64>();
    let saturated_alternatives = plan
        .memo
        .roots
        .iter()
        .map(|root| u64::from(root.saturated_alternatives))
        .sum::<u64>();
    MemoReplaySummary {
        groups: plan.memo.usage.groups,
        alternatives: plan.memo.usage.alternatives,
        rule_applications: plan.memo.usage.rule_applications,
        planning_effort: plan.memo.usage.planning_effort,
        changed_expressions: plan
            .memo
            .roots
            .iter()
            .filter(|root| root.structural_expression != root.selected_expression)
            .count(),
        selected_proof_edges: plan
            .memo
            .roots
            .iter()
            .map(|root| root.selected_proof.len())
            .sum(),
        directed_alternatives,
        saturated_alternatives,
        additional_saturated_alternatives: saturated_alternatives
            .saturating_sub(directed_alternatives),
        stop_reason: plan.memo.stop_reason,
    }
}

fn predicted_row_work(plan: &Plan) -> Option<u64> {
    let (work, access, joins) = cost_plan_summary(plan);
    if access.is_empty() && joins.is_empty() {
        Some(0)
    } else {
        work
    }
}

fn cost_plan_summary(plan: &Plan) -> (Option<u64>, Vec<AccessCandidate>, Vec<JoinCandidate>) {
    let mut total = Some(0u64);
    let mut candidates = Vec::new();
    let mut join_candidates = Vec::new();
    plan.walk(&mut |node| {
        let access = match &node.kind {
            NodeKind::PrimaryKeyGet { access, .. }
            | NodeKind::TableScan { access, .. }
            | NodeKind::IndexRangeScan { access, .. } => Some(access),
            _ => None,
        };
        if let Some(access) = access {
            candidates.extend(access.candidates.clone());
            let work = access
                .candidates
                .iter()
                .find(|candidate| candidate.chosen)
                .and_then(|candidate| candidate.cost)
                .map(|cost| cost.logical_row_operations.central);
            total = total
                .zip(work)
                .map(|(total, work)| total.saturating_add(work));
        }
        let join = match &node.kind {
            NodeKind::NestedLoopJoin { decision, .. }
            | NodeKind::HashJoin { decision, .. }
            | NodeKind::IndexedLookupJoin { decision, .. } => Some(decision),
            _ => None,
        };
        if let Some(join) = join {
            join_candidates.extend(join.candidates.clone());
            let work = join
                .candidates
                .iter()
                .find(|candidate| candidate.chosen)
                .and_then(|candidate| candidate.cost)
                .map(|cost| cost.logical_row_operations.central);
            total = total
                .zip(work)
                .map(|(total, work)| total.saturating_add(work));
        }
    });
    (total, candidates, join_candidates)
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
            .prepare_program_estimates_with_mode(&program, empty, PlannerMode::Structural)
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
        assert_eq!(report.planner.structural_evidence_coverage, 1);
        assert_eq!(report.planner.cost_evidence_coverage, 1);
        assert_eq!(report.planner.memo_expressions_changed, 1);
        assert_eq!(report.planner.memo_budget_stops, 0);
        assert_eq!(report.planner.decisions[0].cost_memo.changed_expressions, 1);
        assert_eq!(
            report.planner.decisions[0].cost_memo.selected_proof_edges,
            1
        );
        assert_eq!(report.planner.structural_predicted_workload_regret, 0);
        assert_eq!(report.planner.cost_predicted_workload_regret, 0);
        let cost = report.observed_resource_cost.cost.expect("resource cost");
        assert_eq!(cost.observed_executions, 3);
        assert_eq!(cost.gets.maximum, 2);
        assert_eq!(cost.scans.maximum, 1);
        assert_eq!(cost.iterated.maximum, 3);
        assert_eq!(cost.bytes_read.maximum, 100);
    }
}
