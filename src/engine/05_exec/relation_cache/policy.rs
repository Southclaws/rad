//! Bounded admission evidence for the relation cache.
//!
//! The complete `RelationCacheKey` remains the only lookup identity. The
//! policy uses compact dependency profiles only for performance evidence. A
//! profile collision can change an admission decision, but it cannot cause a
//! cache hit or expose a result from another dependency vector.
//!
//! One exact fingerprint can have several live dependency cohorts. An old
//! transaction can request an older cohort after a newer cohort is visible.
//! The evidence therefore retains cohorts by identity. It does not use one
//! mutable "current generation" record.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use smallvec::SmallVec;

use super::{
    CachedWork, DependencyGeneration, MaterializationDomain, RelationCacheKey,
    RelationCachePolicyConfig, RelationCachePolicyMode, RelationCachePrior,
    RelationCacheReuseAdmission,
};
use crate::engine::lir::fingerprint::Fingerprint;

const GENERATION_VALUE_LIMIT: usize = 64;
const RATE_SCALE: u64 = 1_000_000;

const TABLE_TAG: u8 = 1;
const COLUMN_TAG: u8 = 2;
const INDEX_TAG: u8 = 3;
const WRITE_PROTOCOL_TAG: u8 = 4;

const GET_WORK_UNITS: u64 = 256;
const SCAN_WORK_UNITS: u64 = 4 * 1024;
const SEEK_WORK_UNITS: u64 = 256;
const ITERATED_ROW_WORK_UNITS: u64 = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CohortToken {
    identity: PolicyIdentity,
    family_identity: FamilyIdentity,
    cohort: [u8; 16],
}

type PolicyIdentity = (MaterializationDomain, Fingerprint, [u8; 16]);
type FamilyIdentity = (MaterializationDomain, Fingerprint, [u8; 16]);

pub(super) struct RelationCachePolicy {
    shards: Box<[Mutex<PolicyShard>]>,
    family_shards: Option<Box<[Mutex<FamilyShard>]>>,
    config: RelationCachePolicyConfig,
}

impl RelationCachePolicy {
    pub(super) fn new(exact_limit: usize, config: RelationCachePolicyConfig) -> Self {
        let exact_limit = exact_limit.max(1);
        let shard_count = super::CACHE_SHARDS.min(exact_limit);
        let base_limit = exact_limit / shard_count;
        let extra = exact_limit % shard_count;
        // The per-shard limits add up to the exact limit. A skewed fingerprint
        // distribution can evict one shard early, but it cannot make the
        // evidence structure exceed the process limit.
        let shards = (0..shard_count)
            .map(|index| Mutex::new(PolicyShard::new(base_limit + usize::from(index < extra))))
            .collect::<Box<[_]>>();
        let family_shards =
            (config.reuse_admission == RelationCacheReuseAdmission::FamilyConversion).then(|| {
                (0..shard_count)
                    .map(|index| {
                        Mutex::new(FamilyShard::new(base_limit + usize::from(index < extra)))
                    })
                    .collect::<Box<[_]>>()
            });
        Self {
            shards,
            family_shards,
            config,
        }
    }

    pub(super) fn observe_request(&self, key: &RelationCacheKey, now: Duration) -> CohortToken {
        let profile = DependencyProfile::new(key);
        let representation = representation_profile(key);
        let identity = (key.domain, key.exact, representation);
        let token = CohortToken {
            identity,
            family_identity: (key.domain, key.family, representation),
            cohort: profile.cohort,
        };
        let events = self
            .shard(identity)
            .lock()
            .expect("relation cache policy lock poisoned")
            .observe_request(identity, profile, now, self.config);
        if let Some(cause) = events.transition {
            crate::telemetry::relation_cache_cohort_transition(
                token.identity.0.as_str(),
                self.config.mode.as_str(),
                cause.as_str(),
            );
        }
        for summary in events.cohort_summaries {
            summary.emit(token.identity.0, self.config.mode);
        }
        for cause in events.evidence_evictions {
            crate::telemetry::relation_cache_evidence_eviction(cause.as_str());
        }
        token
    }

    pub(super) fn observe_cache_hit(&self, token: CohortToken, work: CachedWork, now: Duration) {
        self.observe_success(token, SuccessSource::CacheHit, work, now);
    }

    pub(super) fn observe_coalesced_reuse(
        &self,
        token: CohortToken,
        work: CachedWork,
        now: Duration,
    ) {
        self.observe_success(token, SuccessSource::Coalesced, work, now);
    }

    pub(super) fn observe_residency(&self, token: CohortToken, admitted: bool) {
        if !admitted {
            return;
        }
        self.shard(token.identity)
            .lock()
            .expect("relation cache policy lock poisoned")
            .observe_residency(token);
    }

    pub(super) fn observe_fill(
        &self,
        token: CohortToken,
        work: CachedWork,
        result_bytes: usize,
        retained_bytes: usize,
        result_byte_limit: usize,
        now: Duration,
    ) -> AdmissionDecision {
        let work_units = deterministic_work(work);
        let observation = FillObservation {
            work_units,
            result_bytes,
            retained_bytes,
            result_byte_limit,
            now,
        };
        let family = self.family_estimate(token.family_identity, now);
        let fill = {
            let mut shard = self
                .shard(token.identity)
                .lock()
                .expect("relation cache policy lock poisoned");
            shard.observe_fill(token, observation, family, self.config)
        };
        self.observe_family(token.family_identity, fill.success.family_observation, now);
        let retained_bytes_u64 = u64::try_from(retained_bytes).unwrap_or(u64::MAX).max(1);
        let density = work_units as f64 / retained_bytes_u64 as f64;
        Self::emit_success(fill.success, token.identity.0, self.config.mode);
        crate::telemetry::relation_cache_policy_candidate(
            token.identity.0.as_str(),
            self.config.mode.as_str(),
            work_units,
            density,
        );
        crate::telemetry::relation_cache_policy_decision(
            token.identity.0.as_str(),
            self.config.mode.as_str(),
            fill.decision.outcome(),
            fill.decision.reason(),
            fill.decision.evidence_source(),
        );
        fill.decision
    }

    fn shard(&self, identity: PolicyIdentity) -> &Mutex<PolicyShard> {
        &self.shards[usize::from(identity.1.digest[0] ^ identity.2[0]) % self.shards.len()]
    }

    fn family_shard(&self, identity: FamilyIdentity) -> Option<&Mutex<FamilyShard>> {
        let shards = self.family_shards.as_ref()?;
        Some(&shards[usize::from(identity.1.digest[0] ^ identity.2[0]) % shards.len()])
    }

    fn family_estimate(&self, identity: FamilyIdentity, now: Duration) -> Option<FamilyEstimate> {
        if self.config.reuse_admission != RelationCacheReuseAdmission::FamilyConversion {
            return None;
        }
        self.family_shard(identity)?
            .lock()
            .expect("relation cache family policy lock poisoned")
            .estimate(identity, now, self.config)
    }

    fn observe_family(
        &self,
        identity: FamilyIdentity,
        observation: Option<FamilyObservation>,
        now: Duration,
    ) {
        if self.config.reuse_admission != RelationCacheReuseAdmission::FamilyConversion {
            return;
        }
        let Some(observation) = observation else {
            return;
        };
        let evicted = self
            .family_shard(identity)
            .expect("family policy shards are configured")
            .lock()
            .expect("relation cache family policy lock poisoned")
            .observe(identity, observation, now, self.config.rate_half_life);
        if evicted {
            crate::telemetry::relation_cache_evidence_eviction("family_capacity");
        }
    }

    fn observe_success(
        &self,
        token: CohortToken,
        source: SuccessSource,
        work: CachedWork,
        now: Duration,
    ) {
        let observation = SuccessObservation {
            source,
            work_units: deterministic_work(work),
            now,
            rate_half_life: self.config.rate_half_life,
        };
        let family = self.family_estimate(token.family_identity, now);
        let events = self
            .shard(token.identity)
            .lock()
            .expect("relation cache policy lock poisoned")
            .observe_success(token, observation, family, self.config);
        self.observe_family(token.family_identity, events.family_observation, now);
        Self::emit_success(events, token.identity.0, self.config.mode);
    }

    fn emit_success(
        events: SuccessEvents,
        domain: MaterializationDomain,
        mode: RelationCachePolicyMode,
    ) {
        if events.reuse_opportunity {
            crate::telemetry::relation_cache_reuse_opportunity();
        }
        if events.rejected_reuse {
            crate::telemetry::relation_cache_policy_rejected_reuse(domain.as_str(), mode.as_str());
        }
        if let Some(decision) = events.decision {
            crate::telemetry::relation_cache_policy_decision(
                domain.as_str(),
                mode.as_str(),
                decision.outcome(),
                decision.reason(),
                decision.evidence_source(),
            );
        }
    }

    pub(super) fn stats(&self) -> RelationCachePolicyStatistics {
        let mut result = RelationCachePolicyStatistics::default();
        let mut total_cohorts = CohortSamples::default();
        let mut query_cohorts = CohortSamples::default();
        let mut hash_build_cohorts = CohortSamples::default();
        let mut grouped_dimension_cohorts = CohortSamples::default();
        for shard in &self.shards {
            let shard = shard.lock().expect("relation cache policy lock poisoned");
            for (identity, exact) in &shard.exact {
                result.total.observe_exact(exact);
                result.for_domain_mut(identity.0).observe_exact(exact);
                total_cohorts.observe_exact(exact);
                match identity.0 {
                    MaterializationDomain::QueryResult => query_cohorts.observe_exact(exact),
                    MaterializationDomain::HashJoinBuild
                    | MaterializationDomain::SubrelationRowsV1 => {
                        hash_build_cohorts.observe_exact(exact)
                    }
                    MaterializationDomain::GroupedHashJoinDimension => {
                        grouped_dimension_cohorts.observe_exact(exact)
                    }
                }
            }
        }
        if let Some(family_shards) = &self.family_shards {
            for shard in family_shards {
                let shard = shard
                    .lock()
                    .expect("relation cache family policy lock poisoned");
                for (identity, family) in &shard.families {
                    result.total.observe_family(family);
                    result.for_domain_mut(identity.0).observe_family(family);
                }
            }
        }
        result.total_cohorts = total_cohorts.finish();
        result.query_cohorts = query_cohorts.finish();
        result.hash_build_cohorts = hash_build_cohorts.finish();
        result.grouped_dimension_cohorts = grouped_dimension_cohorts.finish();
        result
    }
}

fn representation_profile(key: &RelationCacheKey) -> [u8; 16] {
    let mut hash = Sha256::new();
    hash.update((key.representation.len() as u64).to_be_bytes());
    hash.update(&key.representation);
    let digest = hash.finalize();
    let mut profile = [0u8; 16];
    profile.copy_from_slice(&digest[..16]);
    profile
}

pub(super) fn deterministic_work(work: CachedWork) -> u64 {
    // These units compare candidates inside one process. They are not time or
    // storage bytes. Fixed weights keep the decision independent of host load,
    // object-cache state, and wall-clock precision. Logical bytes remain the
    // main input. Operation weights prevent an empty or narrow scan from
    // appearing free.
    work.kv
        .bytes_read
        .saturating_add(work.kv.gets.saturating_mul(GET_WORK_UNITS))
        .saturating_add(work.kv.scans.saturating_mul(SCAN_WORK_UNITS))
        .saturating_add(work.kv.forward_seeks.saturating_mul(SEEK_WORK_UNITS))
        .saturating_add(work.kv.iterated.saturating_mul(ITERATED_ROW_WORK_UNITS))
}

struct PolicyShard {
    exact_limit: usize,
    next_sequence: u64,
    exact: BTreeMap<PolicyIdentity, ExactEvidence>,
    recency: BTreeSet<(u64, PolicyIdentity)>,
}

impl PolicyShard {
    fn new(exact_limit: usize) -> Self {
        Self {
            exact_limit,
            next_sequence: 0,
            exact: BTreeMap::new(),
            recency: BTreeSet::new(),
        }
    }

    fn observe_request(
        &mut self,
        identity: PolicyIdentity,
        profile: DependencyProfile,
        now: Duration,
        config: RelationCachePolicyConfig,
    ) -> RequestEvents {
        // The sequence is local to this shard. It gives deterministic recency
        // without reading a clock. The process cannot reach the limit during
        // its useful lifetime, so overflow indicates corrupt policy state.
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .expect("relation cache policy sequence overflow");
        let sequence = self.next_sequence;
        let mut events = RequestEvents::default();
        if let Some(exact) = self.exact.get(&identity) {
            self.recency.remove(&(exact.last_seen_sequence, identity));
        } else {
            if self.exact.len() == self.exact_limit {
                let (_, evicted) = self
                    .recency
                    .pop_first()
                    .expect("policy recency has every exact entry");
                let evicted = self
                    .exact
                    .remove(&evicted)
                    .expect("policy recency points to an exact entry");
                events.cohort_summaries.extend(
                    evicted
                        .cohorts
                        .iter()
                        .filter(|cohort| !cohort.superseded)
                        .map(|cohort| {
                            cohort.summary(now, CohortCompletion::CensoredExactCapacity, None)
                        }),
                );
                events
                    .evidence_evictions
                    .push(EvidenceEviction::ExactCapacity);
            }
            self.exact.insert(identity, ExactEvidence::new(now));
        }
        let exact = self
            .exact
            .get_mut(&identity)
            .expect("exact policy evidence is present");
        exact.last_seen_sequence = sequence;
        exact.observe_request(
            RequestObservation {
                profile,
                sequence,
                now,
            },
            config,
            &mut events,
        );
        self.recency.insert((sequence, identity));
        events
    }

    fn observe_fill(
        &mut self,
        token: CohortToken,
        observation: FillObservation,
        family: Option<FamilyEstimate>,
        config: RelationCachePolicyConfig,
    ) -> FillEvents {
        let Some(exact) = self.exact.get_mut(&token.identity) else {
            return FillEvents::without_exact(observation, family, config);
        };
        exact.observe_fill(token.cohort, observation, family, config)
    }

    fn observe_success(
        &mut self,
        token: CohortToken,
        observation: SuccessObservation,
        family: Option<FamilyEstimate>,
        config: RelationCachePolicyConfig,
    ) -> SuccessEvents {
        let Some(exact) = self.exact.get_mut(&token.identity) else {
            return SuccessEvents::default();
        };
        exact.observe_success(token.cohort, observation, family, config)
    }

    fn observe_residency(&mut self, token: CohortToken) {
        let Some(exact) = self.exact.get_mut(&token.identity) else {
            return;
        };
        let Some(cohort) = exact
            .cohorts
            .iter_mut()
            .find(|cohort| cohort.profile.cohort == token.cohort)
        else {
            return;
        };
        cohort.actual_admissions = cohort.actual_admissions.saturating_add(1);
    }
}

struct FamilyShard {
    family_limit: usize,
    next_sequence: u64,
    families: BTreeMap<FamilyIdentity, FamilyEvidence>,
    recency: BTreeSet<(u64, FamilyIdentity)>,
}

impl FamilyShard {
    fn new(family_limit: usize) -> Self {
        Self {
            family_limit,
            next_sequence: 0,
            families: BTreeMap::new(),
            recency: BTreeSet::new(),
        }
    }

    fn estimate(
        &self,
        identity: FamilyIdentity,
        now: Duration,
        config: RelationCachePolicyConfig,
    ) -> Option<FamilyEstimate> {
        self.families.get(&identity)?.estimate(
            now,
            config.rate_half_life,
            config.family_minimum_observations,
        )
    }

    fn observe(
        &mut self,
        identity: FamilyIdentity,
        observation: FamilyObservation,
        now: Duration,
        half_life: Duration,
    ) -> bool {
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .expect("relation cache family sequence overflow");
        let sequence = self.next_sequence;
        let mut evicted = false;
        if let Some(family) = self.families.get(&identity) {
            self.recency.remove(&(family.last_seen_sequence, identity));
        } else {
            if self.families.len() == self.family_limit {
                let (_, oldest) = self
                    .recency
                    .pop_first()
                    .expect("family recency has every family entry");
                self.families
                    .remove(&oldest)
                    .expect("family recency points to a family entry");
                evicted = true;
            }
            self.families.insert(identity, FamilyEvidence::default());
        }
        let family = self
            .families
            .get_mut(&identity)
            .expect("family policy evidence is present");
        family.last_seen_sequence = sequence;
        family.observe(observation, now, half_life);
        self.recency.insert((sequence, identity));
        evicted
    }
}

#[derive(Clone, Copy)]
enum FamilyObservation {
    SecondTouch,
    ThirdTouch,
}

impl FamilyObservation {
    fn at_request_count(requests: u64) -> Option<Self> {
        match requests {
            2 => Some(Self::SecondTouch),
            3 => Some(Self::ThirdTouch),
            _ => None,
        }
    }
}

#[derive(Default)]
struct FamilyEvidence {
    last_seen_sequence: u64,
    second_touch: DecayedCount,
    third_touch: DecayedCount,
    second_touch_observations: u64,
    third_touch_conversions: u64,
}

impl FamilyEvidence {
    fn observe(&mut self, observation: FamilyObservation, now: Duration, half_life: Duration) {
        match observation {
            FamilyObservation::SecondTouch => {
                self.second_touch.observe(now, half_life);
                self.second_touch_observations = self.second_touch_observations.saturating_add(1);
            }
            FamilyObservation::ThirdTouch => {
                self.third_touch.observe(now, half_life);
                self.third_touch_conversions = self.third_touch_conversions.saturating_add(1);
            }
        }
    }

    fn estimate(
        &self,
        now: Duration,
        half_life: Duration,
        minimum_observations: usize,
    ) -> Option<FamilyEstimate> {
        if self.second_touch_observations < minimum_observations as u64 {
            return None;
        }
        let opportunities = self.second_touch.value_at(now, half_life);
        let conversions = self.third_touch.value_at(now, half_life).min(opportunities);
        (opportunities > 0).then_some(FamilyEstimate {
            conversions,
            opportunities,
        })
    }
}

#[derive(Clone, Copy)]
struct FamilyEstimate {
    conversions: u64,
    opportunities: u64,
}

struct ExactEvidence {
    last_seen_sequence: u64,
    cohorts: Vec<CohortEvidence>,
    completed_history: VecDeque<CompletedCohortEvidence>,
    completed_cohorts: u64,
    zero_reuse_cohorts: u64,
    completed_reuse_opportunities: u64,
    completed_oracle_value: i128,
    completed_actual_residency_value: i128,
    completed_coalescing_value: i128,
    completed_policy_regret: i128,
    completed_decision_outcomes: DecisionOutcomes,
    decisions: DecisionCounts,
    rates: RateEvidence,
}

impl ExactEvidence {
    fn new(now: Duration) -> Self {
        Self {
            last_seen_sequence: 0,
            cohorts: Vec::new(),
            completed_history: VecDeque::new(),
            completed_cohorts: 0,
            zero_reuse_cohorts: 0,
            completed_reuse_opportunities: 0,
            completed_oracle_value: 0,
            completed_actual_residency_value: 0,
            completed_coalescing_value: 0,
            completed_policy_regret: 0,
            completed_decision_outcomes: DecisionOutcomes::default(),
            decisions: DecisionCounts::default(),
            rates: RateEvidence::new(now),
        }
    }

    fn observe_fill(
        &mut self,
        cohort_id: [u8; 16],
        observation: FillObservation,
        family: Option<FamilyEstimate>,
        config: RelationCachePolicyConfig,
    ) -> FillEvents {
        let FillObservation {
            work_units,
            result_bytes,
            retained_bytes,
            result_byte_limit,
            now,
        } = observation;
        let history = self.history();
        let Some(cohort) = self
            .cohorts
            .iter_mut()
            .find(|cohort| cohort.profile.cohort == cohort_id)
        else {
            return FillEvents {
                decision: AdmissionDecision::for_candidate(CandidateEvidence {
                    non_coalesced_requests: 1,
                    current_reuse_opportunities: 0,
                    completed_cohorts: history.completed_cohorts,
                    zero_reuse_cohorts: history.zero_reuse_cohorts,
                    completed_reuse_opportunities: history.reuse_opportunities,
                    most_recent_reuse_opportunities: history.most_recent_reuse_opportunities,
                    work_units,
                    result_bytes,
                    retained_bytes,
                    result_byte_limit,
                    prior: self.rates.prior(now, config.rate_half_life),
                    family,
                    config,
                }),
                success: SuccessEvents::default(),
            };
        };
        let success = cohort.observe_success(
            SuccessObservation {
                source: SuccessSource::Fill,
                work_units,
                now,
                rate_half_life: config.rate_half_life,
            },
            &mut self.rates,
        );
        cohort.fill_work_units = work_units;
        cohort.result_bytes = result_bytes;
        cohort.retained_bytes = u64::try_from(retained_bytes).unwrap_or(u64::MAX);
        cohort.result_byte_limit = result_byte_limit;
        let prior = self.rates.prior(now, config.rate_half_life);
        let decision = AdmissionDecision::for_candidate(CandidateEvidence {
            non_coalesced_requests: cohort.non_coalesced_requests,
            current_reuse_opportunities: cohort.eligible_reuse_opportunities,
            completed_cohorts: history.completed_cohorts,
            zero_reuse_cohorts: history.zero_reuse_cohorts,
            completed_reuse_opportunities: history.reuse_opportunities,
            most_recent_reuse_opportunities: history.most_recent_reuse_opportunities,
            work_units,
            result_bytes,
            retained_bytes,
            result_byte_limit,
            prior,
            family,
            config,
        });
        cohort.pending_fill_decision =
            (decision.reason != AdmissionReason::TooLarge).then_some(PendingFillDecision {
                outcome: decision.outcome,
                work_units,
                retained_bytes: u64::try_from(retained_bytes).unwrap_or(u64::MAX),
            });
        cohort.last_decision = Some(decision);
        self.decisions.observe(decision);
        FillEvents { decision, success }
    }

    fn observe_success(
        &mut self,
        cohort_id: [u8; 16],
        observation: SuccessObservation,
        family: Option<FamilyEstimate>,
        config: RelationCachePolicyConfig,
    ) -> SuccessEvents {
        let history = self.history();
        let Some(index) = self
            .cohorts
            .iter()
            .position(|cohort| cohort.profile.cohort == cohort_id)
        else {
            return SuccessEvents::default();
        };
        let mut events = self.cohorts[index].observe_success(observation, &mut self.rates);
        let cohort = &self.cohorts[index];
        let reevaluate = matches!(observation.source, SuccessSource::CacheHit)
            && events.reuse_opportunity
            && cohort
                .last_decision
                .is_some_and(AdmissionDecision::is_promotable_rejection);
        if !reevaluate {
            return events;
        }
        let decision = AdmissionDecision::for_candidate(CandidateEvidence {
            non_coalesced_requests: cohort.non_coalesced_requests,
            current_reuse_opportunities: cohort.eligible_reuse_opportunities,
            completed_cohorts: history.completed_cohorts,
            zero_reuse_cohorts: history.zero_reuse_cohorts,
            completed_reuse_opportunities: history.reuse_opportunities,
            most_recent_reuse_opportunities: history.most_recent_reuse_opportunities,
            work_units: cohort.fill_work_units,
            result_bytes: cohort.result_bytes,
            retained_bytes: usize::try_from(cohort.retained_bytes).unwrap_or(usize::MAX),
            result_byte_limit: cohort.result_byte_limit,
            prior: self.rates.prior(observation.now, config.rate_half_life),
            family,
            config,
        });
        let cohort = &mut self.cohorts[index];
        cohort.last_decision = Some(decision);
        self.decisions.observe(decision);
        events.decision = Some(decision);
        events
    }

    fn observe_request(
        &mut self,
        observation: RequestObservation,
        config: RelationCachePolicyConfig,
        events: &mut RequestEvents,
    ) {
        let RequestObservation {
            profile,
            sequence,
            now,
        } = observation;
        if let Some(cohort) = self
            .cohorts
            .iter_mut()
            .find(|cohort| cohort.profile.cohort == profile.cohort)
        {
            cohort.attempts = cohort.attempts.saturating_add(1);
            cohort.last_seen_sequence = sequence;
            return;
        }

        self.observe_transition(&profile, now, config.rate_half_life, events);
        self.complete_superseded(&profile, now, config.cohorts_per_exact_relation, events);
        self.make_cohort_space(now, config.cohorts_per_exact_relation, events);
        self.cohorts.push(CohortEvidence::new(profile, sequence));
    }

    fn observe_transition(
        &mut self,
        profile: &DependencyProfile,
        now: Duration,
        half_life: Duration,
        events: &mut RequestEvents,
    ) {
        if let Some(previous) = self
            .cohorts
            .iter()
            .filter(|cohort| {
                !cohort.superseded && profile.order_from(&cohort.profile) == GenerationOrder::Newer
            })
            .max_by_key(|cohort| cohort.last_seen_sequence)
        {
            events.transition = Some(profile.transition_cause(&previous.profile));
            self.rates.observe_supersession(now, half_life);
            return;
        }
        let Some(previous) = self
            .cohorts
            .iter()
            .max_by_key(|cohort| cohort.last_seen_sequence)
        else {
            return;
        };
        events.transition = match profile.order_from(&previous.profile) {
            GenerationOrder::DifferentMembers => Some(TransitionCause::DependencySet),
            GenerationOrder::Mixed => Some(TransitionCause::Mixed),
            GenerationOrder::Newer | GenerationOrder::Older | GenerationOrder::Equal => None,
        };
    }

    fn complete_superseded(
        &mut self,
        profile: &DependencyProfile,
        now: Duration,
        history_limit: usize,
        events: &mut RequestEvents,
    ) {
        for index in 0..self.cohorts.len() {
            let completion = {
                let cohort = &self.cohorts[index];
                (!cohort.superseded
                    && profile.order_from(&cohort.profile) == GenerationOrder::Newer)
                    .then(|| {
                        (cohort.successful_requests > 0)
                            .then_some(cohort.eligible_reuse_opportunities)
                    })
            };
            if let Some(reuse_opportunities) = completion {
                self.complete_cohort(
                    index,
                    reuse_opportunities,
                    profile,
                    now,
                    history_limit,
                    events,
                );
            }
        }
    }

    fn complete_cohort(
        &mut self,
        index: usize,
        reuse_opportunities: Option<u64>,
        profile: &DependencyProfile,
        now: Duration,
        history_limit: usize,
        events: &mut RequestEvents,
    ) {
        let cohort = &mut self.cohorts[index];
        cohort.superseded = true;
        cohort.observed_supersession_at = Some(now);
        let Some(reuse_opportunities) = reuse_opportunities else {
            return;
        };
        self.completed_cohorts = self.completed_cohorts.saturating_add(1);
        if reuse_opportunities == 0 {
            self.zero_reuse_cohorts = self.zero_reuse_cohorts.saturating_add(1);
        }
        self.completed_reuse_opportunities = self
            .completed_reuse_opportunities
            .saturating_add(reuse_opportunities);
        let value = cohort.value();
        self.completed_oracle_value = self
            .completed_oracle_value
            .saturating_add(value.oracle_value);
        self.completed_actual_residency_value = self
            .completed_actual_residency_value
            .saturating_add(value.actual_residency_value);
        self.completed_coalescing_value = self
            .completed_coalescing_value
            .saturating_add(value.coalescing_value);
        self.completed_policy_regret = self
            .completed_policy_regret
            .saturating_add(value.policy_regret);
        self.completed_decision_outcomes = self
            .completed_decision_outcomes
            .saturating_add(cohort.decision_outcomes());
        if self.completed_history.len() == history_limit {
            self.completed_history.pop_front();
        }
        self.completed_history.push_back(CompletedCohortEvidence {
            reuse_opportunities,
            successful_requests: cohort.successful_requests,
            observed_lifetime_micros: duration_micros(cohort.observed_lifetime(now)),
        });
        events.cohort_summaries.push(cohort.summary(
            now,
            CohortCompletion::Superseded,
            Some(profile.transition_cause(&cohort.profile)),
        ));
    }

    fn make_cohort_space(
        &mut self,
        now: Duration,
        cohort_limit: usize,
        events: &mut RequestEvents,
    ) {
        if self.cohorts.len() == cohort_limit {
            let evicted = self
                .cohorts
                .iter()
                .enumerate()
                .min_by_key(|(_, cohort)| (cohort.last_seen_sequence, cohort.profile.cohort))
                .map(|(index, _)| index)
                .expect("a full cohort set is not empty");
            let evicted = self.cohorts.swap_remove(evicted);
            if !evicted.superseded {
                events.cohort_summaries.push(evicted.summary(
                    now,
                    CohortCompletion::CensoredCohortCapacity,
                    None,
                ));
            }
            events
                .evidence_evictions
                .push(EvidenceEviction::CohortCapacity);
        }
    }

    fn history(&self) -> CompletedHistory {
        let mut history = CompletedHistory::default();
        for cohort in &self.completed_history {
            history.completed_cohorts = history.completed_cohorts.saturating_add(1);
            history.reuse_opportunities = history
                .reuse_opportunities
                .saturating_add(cohort.reuse_opportunities);
            if cohort.reuse_opportunities == 0 {
                history.zero_reuse_cohorts = history.zero_reuse_cohorts.saturating_add(1);
            }
            history.most_recent_reuse_opportunities = Some(cohort.reuse_opportunities);
        }
        history
    }
}

struct CompletedCohortEvidence {
    reuse_opportunities: u64,
    successful_requests: u64,
    observed_lifetime_micros: u64,
}

#[derive(Clone, Copy, Default)]
struct CompletedHistory {
    completed_cohorts: u64,
    zero_reuse_cohorts: u64,
    reuse_opportunities: u64,
    most_recent_reuse_opportunities: Option<u64>,
}

struct CohortEvidence {
    profile: DependencyProfile,
    attempts: u64,
    // Failed and cancelled requests do not enter reuse evidence. They cannot
    // reuse a successful materialization and must not trigger second-touch
    // admission.
    successful_requests: u64,
    non_coalesced_requests: u64,
    eligible_reuse_opportunities: u64,
    cache_hits: u64,
    coalesced_reuses: u64,
    fills: u64,
    first_success_at: Option<Duration>,
    last_success_at: Option<Duration>,
    observed_supersession_at: Option<Duration>,
    fill_work_units: u64,
    result_bytes: usize,
    retained_bytes: u64,
    result_byte_limit: usize,
    resident_avoided_work: u64,
    coalesced_avoided_work: u64,
    actual_admissions: u64,
    resolved_decision_outcomes: DecisionOutcomes,
    pending_fill_decision: Option<PendingFillDecision>,
    last_seen_sequence: u64,
    superseded: bool,
    last_decision: Option<AdmissionDecision>,
}

impl CohortEvidence {
    fn new(profile: DependencyProfile, sequence: u64) -> Self {
        Self {
            profile,
            attempts: 1,
            successful_requests: 0,
            non_coalesced_requests: 0,
            eligible_reuse_opportunities: 0,
            cache_hits: 0,
            coalesced_reuses: 0,
            fills: 0,
            first_success_at: None,
            last_success_at: None,
            observed_supersession_at: None,
            fill_work_units: 0,
            result_bytes: 0,
            retained_bytes: 0,
            result_byte_limit: 0,
            resident_avoided_work: 0,
            coalesced_avoided_work: 0,
            actual_admissions: 0,
            resolved_decision_outcomes: DecisionOutcomes::default(),
            pending_fill_decision: None,
            last_seen_sequence: sequence,
            superseded: false,
            last_decision: None,
        }
    }

    fn observe_success(
        &mut self,
        observation: SuccessObservation,
        rates: &mut RateEvidence,
    ) -> SuccessEvents {
        let SuccessObservation {
            source,
            work_units,
            now,
            rate_half_life,
        } = observation;
        let mut events = SuccessEvents {
            reuse_opportunity: !matches!(source, SuccessSource::Coalesced)
                && self.non_coalesced_requests > 0,
            ..Default::default()
        };
        if !matches!(source, SuccessSource::Coalesced)
            && let Some(pending) = self.pending_fill_decision.take()
        {
            self.resolved_decision_outcomes.observe_reuse(pending);
        }
        if events.reuse_opportunity {
            self.eligible_reuse_opportunities = self.eligible_reuse_opportunities.saturating_add(1);
            events.rejected_reuse = self
                .last_decision
                .is_some_and(AdmissionDecision::is_rejection);
        }
        self.successful_requests = self.successful_requests.saturating_add(1);
        self.first_success_at.get_or_insert(now);
        self.last_success_at = Some(now);
        match source {
            SuccessSource::Fill => {
                self.fills = self.fills.saturating_add(1);
                self.non_coalesced_requests = self.non_coalesced_requests.saturating_add(1);
                rates.observe_request(now, rate_half_life);
                events.family_observation =
                    FamilyObservation::at_request_count(self.non_coalesced_requests);
            }
            SuccessSource::CacheHit => {
                self.cache_hits = self.cache_hits.saturating_add(1);
                self.non_coalesced_requests = self.non_coalesced_requests.saturating_add(1);
                self.resident_avoided_work = self.resident_avoided_work.saturating_add(work_units);
                rates.observe_request(now, rate_half_life);
                events.family_observation =
                    FamilyObservation::at_request_count(self.non_coalesced_requests);
            }
            SuccessSource::Coalesced => {
                self.coalesced_reuses = self.coalesced_reuses.saturating_add(1);
                self.coalesced_avoided_work =
                    self.coalesced_avoided_work.saturating_add(work_units);
            }
        }
        events
    }

    fn summary(
        &self,
        now: Duration,
        completion: CohortCompletion,
        transition: Option<TransitionCause>,
    ) -> CohortSummary {
        CohortSummary {
            completion,
            transition,
            successful_requests: self.successful_requests,
            eligible_reuse_opportunities: self.eligible_reuse_opportunities,
            cache_hits: self.cache_hits,
            coalesced_reuses: self.coalesced_reuses,
            fills: self.fills,
            observed_cohort_lifetime: self.observed_lifetime(now),
            fill_work_units: self.fill_work_units,
            retained_bytes: self.retained_bytes,
            resident_avoided_work: self.resident_avoided_work,
            coalesced_avoided_work: self.coalesced_avoided_work,
            last_decision: self.last_decision,
        }
    }

    fn observed_lifetime(&self, now: Duration) -> Duration {
        self.first_success_at
            .map(|first| {
                self.observed_supersession_at
                    .unwrap_or(now)
                    .saturating_sub(first)
            })
            .unwrap_or_default()
    }

    fn value(&self) -> CohortValue {
        let net_work = self.fill_work_units.saturating_sub(self.retained_bytes);
        let possible_benefit =
            u128::from(self.eligible_reuse_opportunities).saturating_mul(u128::from(net_work));
        let one_admission_cost = u128::from(self.retained_bytes);
        let possible_value = to_i128(possible_benefit).saturating_sub(to_i128(one_admission_cost));
        let oracle_value = possible_value.max(0);
        let resident_restore_cost =
            u128::from(self.cache_hits).saturating_mul(u128::from(self.retained_bytes));
        let resident_benefit =
            u128::from(self.resident_avoided_work).saturating_sub(resident_restore_cost);
        let actual_admission_cost =
            u128::from(self.actual_admissions).saturating_mul(u128::from(self.retained_bytes));
        let actual_residency_value =
            to_i128(resident_benefit).saturating_sub(to_i128(actual_admission_cost));
        let coalesced_restore_cost =
            u128::from(self.coalesced_reuses).saturating_mul(u128::from(self.retained_bytes));
        let coalescing_value =
            to_i128(u128::from(self.coalesced_avoided_work).saturating_sub(coalesced_restore_cost));
        CohortValue {
            oracle_value,
            actual_residency_value,
            coalescing_value,
            policy_regret: oracle_value.saturating_sub(actual_residency_value),
        }
    }

    fn decision_outcomes(&self) -> DecisionOutcomes {
        let mut outcomes = self.resolved_decision_outcomes;
        if let Some(pending) = self.pending_fill_decision
            && pending.outcome == AdmissionOutcome::Admit
        {
            outcomes.admissions_without_future_reuse =
                outcomes.admissions_without_future_reuse.saturating_add(1);
            outcomes.retained_bytes_without_future_reuse = outcomes
                .retained_bytes_without_future_reuse
                .saturating_add(pending.retained_bytes);
        }
        outcomes
    }
}

#[derive(Clone, Copy)]
struct PendingFillDecision {
    outcome: AdmissionOutcome,
    work_units: u64,
    retained_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct DecisionOutcomes {
    admissions_followed_by_reuse: u64,
    admissions_without_future_reuse: u64,
    rejections_followed_by_reuse: u64,
    avoidable_work_after_rejection: u64,
    retained_bytes_without_future_reuse: u64,
}

impl DecisionOutcomes {
    fn observe_reuse(&mut self, pending: PendingFillDecision) {
        match pending.outcome {
            AdmissionOutcome::Admit => {
                self.admissions_followed_by_reuse =
                    self.admissions_followed_by_reuse.saturating_add(1);
            }
            AdmissionOutcome::Reject => {
                self.rejections_followed_by_reuse =
                    self.rejections_followed_by_reuse.saturating_add(1);
                self.avoidable_work_after_rejection = self
                    .avoidable_work_after_rejection
                    .saturating_add(pending.work_units.saturating_sub(pending.retained_bytes));
            }
        }
    }

    fn saturating_add(self, other: Self) -> Self {
        Self {
            admissions_followed_by_reuse: self
                .admissions_followed_by_reuse
                .saturating_add(other.admissions_followed_by_reuse),
            admissions_without_future_reuse: self
                .admissions_without_future_reuse
                .saturating_add(other.admissions_without_future_reuse),
            rejections_followed_by_reuse: self
                .rejections_followed_by_reuse
                .saturating_add(other.rejections_followed_by_reuse),
            avoidable_work_after_rejection: self
                .avoidable_work_after_rejection
                .saturating_add(other.avoidable_work_after_rejection),
            retained_bytes_without_future_reuse: self
                .retained_bytes_without_future_reuse
                .saturating_add(other.retained_bytes_without_future_reuse),
        }
    }
}

fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn to_i128(value: u128) -> i128 {
    value.min(i128::MAX as u128) as i128
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionOutcome {
    Admit,
    Reject,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionReason {
    SecondTouch,
    ThirdTouch,
    ValueDensity,
    FamilyConversion,
    LearnedValue,
    GenerationRate,
    RecoveryProbe,
    ExpensiveProbation,
    RecentNoReuse,
    NoReuseHistory,
    InsufficientValue,
    TooLarge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmissionDecision {
    pub outcome: AdmissionOutcome,
    pub reason: AdmissionReason,
    pub evidence: EvidenceSource,
}

struct CandidateEvidence {
    non_coalesced_requests: u64,
    current_reuse_opportunities: u64,
    completed_cohorts: u64,
    zero_reuse_cohorts: u64,
    completed_reuse_opportunities: u64,
    most_recent_reuse_opportunities: Option<u64>,
    work_units: u64,
    result_bytes: usize,
    retained_bytes: usize,
    result_byte_limit: usize,
    prior: Option<PriorEstimate>,
    family: Option<FamilyEstimate>,
    config: RelationCachePolicyConfig,
}

impl AdmissionDecision {
    const fn admit(reason: AdmissionReason, evidence: EvidenceSource) -> Self {
        Self {
            outcome: AdmissionOutcome::Admit,
            reason,
            evidence,
        }
    }

    const fn reject(reason: AdmissionReason, evidence: EvidenceSource) -> Self {
        Self {
            outcome: AdmissionOutcome::Reject,
            reason,
            evidence,
        }
    }

    fn for_candidate(candidate: CandidateEvidence) -> Self {
        if candidate.result_bytes > candidate.result_byte_limit {
            return Self::reject(AdmissionReason::TooLarge, EvidenceSource::HardLimit);
        }
        let retained_bytes = u64::try_from(candidate.retained_bytes)
            .unwrap_or(u64::MAX)
            .max(1);
        if candidate.completed_cohorts >= candidate.config.minimum_completed_cohorts as u64 {
            return Self::from_completed_history(&candidate, retained_bytes);
        }
        if let Some(decision) = Self::from_family_conversion(&candidate, retained_bytes) {
            return decision;
        }
        if candidate.config.prior == RelationCachePrior::GenerationRate
            && let Some(prior) = candidate.prior
        {
            let benefit = u128::from(prior.request_rate).saturating_mul(u128::from(
                candidate.work_units.saturating_sub(retained_bytes),
            ));
            let cost =
                u128::from(prior.supersession_rate).saturating_mul(u128::from(retained_bytes));
            return if benefit >= cost {
                Self::admit(
                    AdmissionReason::GenerationRate,
                    EvidenceSource::GenerationRate,
                )
            } else {
                Self::reject(
                    AdmissionReason::InsufficientValue,
                    EvidenceSource::GenerationRate,
                )
            };
        }
        if let Some(decision) = Self::from_current_reuse(&candidate, retained_bytes) {
            return decision;
        }
        let expensive = candidate.work_units >= candidate.config.probation_minimum_work_units
            && candidate.work_units
                >= retained_bytes.saturating_mul(candidate.config.probation_minimum_work_per_byte);
        if expensive {
            Self::admit(AdmissionReason::ExpensiveProbation, EvidenceSource::Default)
        } else {
            Self::reject(AdmissionReason::InsufficientValue, EvidenceSource::Default)
        }
    }

    fn from_current_reuse(candidate: &CandidateEvidence, retained_bytes: u64) -> Option<Self> {
        match candidate.config.reuse_admission {
            RelationCacheReuseAdmission::SecondTouch if candidate.non_coalesced_requests >= 2 => {
                Some(Self::admit(
                    AdmissionReason::SecondTouch,
                    EvidenceSource::ExactCohort,
                ))
            }
            RelationCacheReuseAdmission::ThirdTouch if candidate.non_coalesced_requests >= 3 => {
                Some(Self::admit(
                    AdmissionReason::ThirdTouch,
                    EvidenceSource::ExactCohort,
                ))
            }
            RelationCacheReuseAdmission::ValueDensity
                if candidate.non_coalesced_requests >= 2
                    && candidate
                        .current_reuse_opportunities
                        .saturating_mul(candidate.work_units.saturating_sub(retained_bytes))
                        >= retained_bytes =>
            {
                Some(Self::admit(
                    AdmissionReason::ValueDensity,
                    EvidenceSource::ExactCohort,
                ))
            }
            RelationCacheReuseAdmission::SecondTouch
            | RelationCacheReuseAdmission::ThirdTouch
            | RelationCacheReuseAdmission::ValueDensity
            | RelationCacheReuseAdmission::FamilyConversion => None,
        }
    }

    fn from_family_conversion(candidate: &CandidateEvidence, retained_bytes: u64) -> Option<Self> {
        if candidate.config.reuse_admission != RelationCacheReuseAdmission::FamilyConversion {
            return None;
        }
        if candidate.non_coalesced_requests >= 3 {
            return Some(Self::admit(
                AdmissionReason::ThirdTouch,
                EvidenceSource::ExactCohort,
            ));
        }
        if candidate.non_coalesced_requests < 2 {
            return None;
        }
        let Some(family) = candidate.family else {
            return Some(Self::admit(
                AdmissionReason::SecondTouch,
                EvidenceSource::ExactCohort,
            ));
        };
        let net_work = candidate.work_units.saturating_sub(retained_bytes);
        // LHD ranks objects with conditional reuse probability and retained
        // size. This decision uses the same relation at the second touch.
        // See Beckmann, Chen, and Cidon, "LHD: Improving Cache Hit Rate by
        // Maximizing Hit Density," NSDI 2018, Section 3.1.
        let expected_benefit = u128::from(family.conversions).saturating_mul(u128::from(net_work));
        let expected_cost =
            u128::from(family.opportunities).saturating_mul(u128::from(retained_bytes));
        if expected_benefit >= expected_cost {
            Some(Self::admit(
                AdmissionReason::FamilyConversion,
                EvidenceSource::FamilyHistory,
            ))
        } else {
            Some(Self::reject(
                AdmissionReason::InsufficientValue,
                EvidenceSource::FamilyHistory,
            ))
        }
    }

    fn from_completed_history(candidate: &CandidateEvidence, retained_bytes: u64) -> Self {
        // One retained byte is one restore-work unit and one
        // admission-copy unit. Cross multiplication preserves the exact
        // ratio and avoids floating-point policy decisions.
        let net_work_per_hit = candidate.work_units.saturating_sub(retained_bytes);
        let current_benefit = candidate
            .current_reuse_opportunities
            .saturating_mul(net_work_per_hit);
        if candidate.most_recent_reuse_opportunities == Some(0) {
            if current_benefit >= retained_bytes {
                return Self::admit(AdmissionReason::RecoveryProbe, EvidenceSource::ExactCohort);
            }
            return Self::reject(AdmissionReason::RecentNoReuse, EvidenceSource::ExactHistory);
        }
        let learned_benefit = candidate
            .completed_reuse_opportunities
            .saturating_mul(net_work_per_hit);
        let learned_admission_cost = candidate.completed_cohorts.saturating_mul(retained_bytes);
        if learned_benefit >= learned_admission_cost {
            return Self::admit(AdmissionReason::LearnedValue, EvidenceSource::ExactHistory);
        }
        if current_benefit >= retained_bytes {
            return Self::admit(AdmissionReason::RecoveryProbe, EvidenceSource::ExactCohort);
        }
        let stable_no_reuse = candidate.zero_reuse_cohorts.saturating_mul(100)
            >= candidate
                .completed_cohorts
                .saturating_mul(u64::from(candidate.config.zero_reuse_percent));
        if stable_no_reuse {
            return Self::reject(
                AdmissionReason::NoReuseHistory,
                EvidenceSource::ExactHistory,
            );
        }
        Self::reject(
            AdmissionReason::InsufficientValue,
            EvidenceSource::ExactHistory,
        )
    }

    fn outcome(self) -> &'static str {
        match self.outcome {
            AdmissionOutcome::Admit => "admit",
            AdmissionOutcome::Reject => "reject",
        }
    }

    fn reason(self) -> &'static str {
        match self.reason {
            AdmissionReason::SecondTouch => "second_touch",
            AdmissionReason::ThirdTouch => "third_touch",
            AdmissionReason::ValueDensity => "value_density",
            AdmissionReason::FamilyConversion => "family_conversion",
            AdmissionReason::LearnedValue => "learned_value",
            AdmissionReason::GenerationRate => "generation_rate",
            AdmissionReason::RecoveryProbe => "recovery_probe",
            AdmissionReason::ExpensiveProbation => "expensive_probation",
            AdmissionReason::RecentNoReuse => "recent_no_reuse",
            AdmissionReason::NoReuseHistory => "no_reuse_history",
            AdmissionReason::InsufficientValue => "insufficient_value",
            AdmissionReason::TooLarge => "too_large",
        }
    }

    fn evidence_source(self) -> &'static str {
        self.evidence.as_str()
    }

    fn is_rejection(self) -> bool {
        self.outcome == AdmissionOutcome::Reject
    }

    fn is_promotable_rejection(self) -> bool {
        self.is_rejection() && self.reason != AdmissionReason::TooLarge
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvidenceSource {
    Default,
    ExactCohort,
    ExactHistory,
    FamilyHistory,
    GenerationRate,
    HardLimit,
}

impl EvidenceSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::ExactCohort => "exact_cohort",
            Self::ExactHistory => "exact_history",
            Self::FamilyHistory => "family_history",
            Self::GenerationRate => "generation_rate",
            Self::HardLimit => "hard_limit",
        }
    }
}

#[derive(Clone, Copy)]
struct PriorEstimate {
    request_rate: u64,
    supersession_rate: u64,
}

struct RateEvidence {
    started_at: Duration,
    request: DecayedRate,
    supersession: DecayedRate,
    request_intervals: u64,
    supersessions: u64,
}

impl RateEvidence {
    fn new(now: Duration) -> Self {
        Self {
            started_at: now,
            request: DecayedRate::default(),
            supersession: DecayedRate::default(),
            request_intervals: 0,
            supersessions: 0,
        }
    }

    fn observe_request(&mut self, now: Duration, half_life: Duration) {
        if self.request.observe(now, None, half_life) {
            self.request_intervals = self.request_intervals.saturating_add(1);
        }
    }

    fn observe_supersession(&mut self, now: Duration, half_life: Duration) {
        self.supersession
            .observe(now, Some(self.started_at), half_life);
        self.supersessions = self.supersessions.saturating_add(1);
    }

    fn prior(&self, now: Duration, half_life: Duration) -> Option<PriorEstimate> {
        if self.request_intervals < 2 || self.supersessions < 1 {
            return None;
        }
        let request_rate = self.request.value_at(now, half_life);
        let supersession_rate = self.supersession.value_at(now, half_life);
        (request_rate > 0 && supersession_rate > 0).then_some(PriorEstimate {
            request_rate,
            supersession_rate,
        })
    }
}

#[derive(Default)]
struct DecayedRate {
    value: u64,
    last_event_at: Option<Duration>,
}

impl DecayedRate {
    fn observe(&mut self, now: Duration, initial: Option<Duration>, half_life: Duration) -> bool {
        let Some(previous) = self.last_event_at.or(initial) else {
            self.last_event_at = Some(now);
            return false;
        };
        let elapsed = now.saturating_sub(previous).as_micros().max(1);
        let instantaneous = instantaneous_rate(elapsed);
        self.value = if self.value == 0 {
            instantaneous
        } else {
            let retained_weight = decay_weight(elapsed, half_life.as_micros().max(1));
            let new_weight = RATE_SCALE.saturating_sub(retained_weight);
            u128::from(self.value)
                .saturating_mul(u128::from(retained_weight))
                .saturating_add(u128::from(instantaneous).saturating_mul(u128::from(new_weight)))
                .checked_div(u128::from(RATE_SCALE))
                .unwrap_or(u128::from(u64::MAX))
                .min(u128::from(u64::MAX)) as u64
        };
        self.last_event_at = Some(now);
        true
    }

    fn value_at(&self, now: Duration, half_life: Duration) -> u64 {
        let Some(previous) = self.last_event_at else {
            return 0;
        };
        let elapsed = now.saturating_sub(previous).as_micros();
        let weight = decay_weight(elapsed, half_life.as_micros().max(1));
        u128::from(self.value)
            .saturating_mul(u128::from(weight))
            .checked_div(u128::from(RATE_SCALE))
            .unwrap_or_default()
            .min(u128::from(u64::MAX)) as u64
    }
}

#[derive(Default)]
struct DecayedCount {
    value: u64,
    updated_at: Option<Duration>,
}

impl DecayedCount {
    fn observe(&mut self, now: Duration, half_life: Duration) {
        self.value = self.value_at(now, half_life).saturating_add(RATE_SCALE);
        self.updated_at = Some(now);
    }

    fn value_at(&self, now: Duration, half_life: Duration) -> u64 {
        let Some(updated_at) = self.updated_at else {
            return 0;
        };
        let weight = decay_weight(
            now.saturating_sub(updated_at).as_micros(),
            half_life.as_micros().max(1),
        );
        u128::from(self.value)
            .saturating_mul(u128::from(weight))
            .checked_div(u128::from(RATE_SCALE))
            .unwrap_or_default()
            .min(u128::from(u64::MAX)) as u64
    }
}

fn instantaneous_rate(elapsed_micros: u128) -> u64 {
    let instantaneous = u128::from(RATE_SCALE)
        .saturating_mul(1_000_000)
        .checked_div(elapsed_micros)
        .unwrap_or(u128::MAX)
        .min(u128::from(u64::MAX));
    instantaneous as u64
}

fn decay_weight(elapsed_micros: u128, half_life_micros: u128) -> u64 {
    let complete_half_lives = elapsed_micros / half_life_micros;
    if complete_half_lives >= u64::BITS.into() {
        return 0;
    }
    let base = RATE_SCALE >> complete_half_lives as u32;
    let remainder = elapsed_micros % half_life_micros;
    let fractional_reduction = u128::from(base)
        .saturating_mul(remainder)
        .checked_div(half_life_micros.saturating_mul(2))
        .unwrap_or_default()
        .min(u128::from(u64::MAX)) as u64;
    base.saturating_sub(fractional_reduction)
}

#[derive(Clone, Copy)]
enum CohortCompletion {
    Superseded,
    CensoredExactCapacity,
    CensoredCohortCapacity,
}

impl CohortCompletion {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Superseded => "superseded",
            Self::CensoredExactCapacity => "censored_exact_capacity",
            Self::CensoredCohortCapacity => "censored_cohort_capacity",
        }
    }
}

struct CohortSummary {
    completion: CohortCompletion,
    transition: Option<TransitionCause>,
    successful_requests: u64,
    eligible_reuse_opportunities: u64,
    cache_hits: u64,
    coalesced_reuses: u64,
    fills: u64,
    observed_cohort_lifetime: Duration,
    fill_work_units: u64,
    retained_bytes: u64,
    resident_avoided_work: u64,
    coalesced_avoided_work: u64,
    last_decision: Option<AdmissionDecision>,
}

impl CohortSummary {
    fn emit(&self, domain: MaterializationDomain, mode: RelationCachePolicyMode) {
        let decision = self.last_decision;
        crate::telemetry::relation_cache_cohort_summary(
            domain.as_str(),
            mode.as_str(),
            self.completion.as_str(),
            self.transition
                .map_or("not_observed", TransitionCause::as_str),
            decision.map_or("none", |decision| decision.outcome()),
            decision.map_or("none", |decision| decision.reason()),
            decision.map_or("none", |decision| decision.evidence_source()),
            self.successful_requests,
            self.eligible_reuse_opportunities,
            self.cache_hits,
            self.coalesced_reuses,
            self.fills,
            self.observed_cohort_lifetime,
            self.fill_work_units,
            self.retained_bytes,
            self.resident_avoided_work,
            self.coalesced_avoided_work,
        );
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CohortValue {
    oracle_value: i128,
    actual_residency_value: i128,
    coalescing_value: i128,
    policy_regret: i128,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DependencyProfile {
    cohort: [u8; 16],
    members: [u8; 16],
    semantic: [u8; 16],
    storage: [u8; 16],
    data: [u8; 16],
    access: [u8; 16],
    write_protocol: [u8; 16],
    generations: SmallVec<[u128; 12]>,
    complete_generation_vector: bool,
}

impl DependencyProfile {
    fn new(key: &RelationCacheKey) -> Self {
        // Each digest has a separate domain. The member digest omits
        // generations and proves that numeric generation vectors have the
        // same canonical layout before the policy compares them. Category
        // digests identify a transition cause without retaining table names,
        // index names, or literal values in the statistics structure.
        let mut cohort = domain_hasher(b"rad relation cache cohort v1");
        let mut members = domain_hasher(b"rad relation cache members v1");
        let mut semantic = domain_hasher(b"rad relation cache semantic v1");
        let mut storage = domain_hasher(b"rad relation cache storage v1");
        let mut data = domain_hasher(b"rad relation cache data v1");
        let mut access = domain_hasher(b"rad relation cache access v1");
        let mut write_protocol = domain_hasher(b"rad relation cache write protocol v1");
        let mut generations = SmallVec::new();
        let mut complete_generation_vector = true;

        for dependency in key.dependencies.iter() {
            match dependency {
                DependencyGeneration::Table {
                    table_id,
                    existence_generation,
                    storage_generation,
                    data_generation,
                } => {
                    let identity = [table_id.as_str()];
                    update_identity(&mut cohort, TABLE_TAG, &identity);
                    update_identity(&mut members, TABLE_TAG, &identity);
                    update_identity(&mut semantic, TABLE_TAG, &identity);
                    update_identity(&mut storage, TABLE_TAG, &identity);
                    update_identity(&mut data, TABLE_TAG, &identity);
                    update_generation(&mut cohort, existence_generation.get());
                    update_generation(&mut cohort, storage_generation.get());
                    for (_, generation) in data_generation.entries() {
                        update_generation(&mut cohort, generation.get());
                    }
                    update_generation(&mut semantic, existence_generation.get());
                    update_generation(&mut storage, storage_generation.get());
                    for (_, generation) in data_generation.entries() {
                        update_generation(&mut data, generation.get());
                    }
                    for generation in [existence_generation.get(), storage_generation.get()] {
                        record_generation(
                            &mut generations,
                            &mut complete_generation_vector,
                            generation,
                        );
                    }
                    // Stripe generations only increase within one storage generation, so
                    // their sum preserves snapshot order without retaining every stripe.
                    let data_generation: u128 = data_generation
                        .entries()
                        .map(|(_, generation)| u128::from(generation.get()))
                        .sum();
                    record_generation(
                        &mut generations,
                        &mut complete_generation_vector,
                        data_generation,
                    );
                }
                DependencyGeneration::Column {
                    table_id,
                    column_id,
                    generation,
                } => {
                    let identity = [table_id.as_str(), column_id.as_str()];
                    update_identity(&mut cohort, COLUMN_TAG, &identity);
                    update_identity(&mut members, COLUMN_TAG, &identity);
                    update_identity(&mut semantic, COLUMN_TAG, &identity);
                    update_generation(&mut cohort, generation.get());
                    update_generation(&mut semantic, generation.get());
                    record_generation(
                        &mut generations,
                        &mut complete_generation_vector,
                        generation.get(),
                    );
                }
                DependencyGeneration::Index {
                    table_id,
                    index_id,
                    generation,
                } => {
                    let identity = [table_id.as_str(), index_id.as_str()];
                    update_identity(&mut cohort, INDEX_TAG, &identity);
                    update_identity(&mut members, INDEX_TAG, &identity);
                    update_identity(&mut access, INDEX_TAG, &identity);
                    update_generation(&mut cohort, generation.get());
                    update_generation(&mut access, generation.get());
                    record_generation(
                        &mut generations,
                        &mut complete_generation_vector,
                        generation.get(),
                    );
                }
                DependencyGeneration::WriteProtocol {
                    table_id,
                    generation,
                } => {
                    let identity = [table_id.as_str()];
                    update_identity(&mut cohort, WRITE_PROTOCOL_TAG, &identity);
                    update_identity(&mut members, WRITE_PROTOCOL_TAG, &identity);
                    update_identity(&mut write_protocol, WRITE_PROTOCOL_TAG, &identity);
                    update_generation(&mut cohort, generation.get());
                    update_generation(&mut write_protocol, generation.get());
                    record_generation(
                        &mut generations,
                        &mut complete_generation_vector,
                        generation.get(),
                    );
                }
            }
        }

        Self {
            cohort: finish_digest(cohort),
            members: finish_digest(members),
            semantic: finish_digest(semantic),
            storage: finish_digest(storage),
            data: finish_digest(data),
            access: finish_digest(access),
            write_protocol: finish_digest(write_protocol),
            generations,
            complete_generation_vector,
        }
    }

    fn order_from(&self, previous: &Self) -> GenerationOrder {
        if self.members != previous.members || self.generations.len() != previous.generations.len()
        {
            return GenerationOrder::DifferentMembers;
        }
        // The digests still identify an oversized dependency cohort. The
        // policy does not infer generation order from a truncated numeric
        // vector because that can classify an old snapshot as a new snapshot.
        if !self.complete_generation_vector || !previous.complete_generation_vector {
            return GenerationOrder::Mixed;
        }
        let mut has_less = false;
        let mut has_greater = false;
        // Component-wise order preserves MVCC interleaving. A lower vector is
        // an old pinned view, not a new epoch. A mixed vector is not treated
        // as supersession because no direction is safe to infer.
        for (current, previous) in self.generations.iter().zip(&previous.generations) {
            match current.cmp(previous) {
                Ordering::Less => has_less = true,
                Ordering::Greater => has_greater = true,
                Ordering::Equal => {}
            }
        }
        match (has_less, has_greater) {
            (false, false) => GenerationOrder::Equal,
            (false, true) => GenerationOrder::Newer,
            (true, false) => GenerationOrder::Older,
            (true, true) => GenerationOrder::Mixed,
        }
    }

    fn transition_cause(&self, previous: &Self) -> TransitionCause {
        if self.members != previous.members {
            return TransitionCause::DependencySet;
        }
        let changes = [
            (
                self.semantic != previous.semantic,
                TransitionCause::Semantic,
            ),
            (self.storage != previous.storage, TransitionCause::Storage),
            (self.data != previous.data, TransitionCause::Data),
            (self.access != previous.access, TransitionCause::Access),
            (
                self.write_protocol != previous.write_protocol,
                TransitionCause::WriteProtocol,
            ),
        ];
        let mut changed = changes
            .into_iter()
            .filter_map(|(changed, cause)| changed.then_some(cause));
        let first = changed.next().unwrap_or(TransitionCause::Mixed);
        if changed.next().is_some() {
            TransitionCause::Multiple
        } else {
            first
        }
    }
}

fn domain_hasher(domain: &[u8]) -> Sha256 {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher
}

fn update_identity(hasher: &mut Sha256, tag: u8, identities: &[&str]) {
    hasher.update([tag]);
    hasher.update((identities.len() as u64).to_be_bytes());
    for identity in identities {
        hasher.update((identity.len() as u64).to_be_bytes());
        hasher.update(identity.as_bytes());
    }
}

fn update_generation(hasher: &mut Sha256, generation: u64) {
    hasher.update(generation.to_be_bytes());
}

fn record_generation(
    generations: &mut SmallVec<[u128; 12]>,
    complete: &mut bool,
    generation: impl Into<u128>,
) {
    if generations.len() < GENERATION_VALUE_LIMIT {
        generations.push(generation.into());
    } else {
        *complete = false;
    }
}

fn finish_digest(hasher: Sha256) -> [u8; 16] {
    let digest = hasher.finalize();
    let mut result = [0; 16];
    result.copy_from_slice(&digest[..16]);
    result
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GenerationOrder {
    Newer,
    Older,
    Equal,
    Mixed,
    DifferentMembers,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransitionCause {
    Semantic,
    Storage,
    Data,
    Access,
    WriteProtocol,
    DependencySet,
    Multiple,
    Mixed,
}

impl TransitionCause {
    fn as_str(self) -> &'static str {
        match self {
            Self::Semantic => "semantic",
            Self::Storage => "storage",
            Self::Data => "data",
            Self::Access => "access",
            Self::WriteProtocol => "write_protocol",
            Self::DependencySet => "dependency_set",
            Self::Multiple => "multiple",
            Self::Mixed => "mixed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EvidenceEviction {
    ExactCapacity,
    CohortCapacity,
}

impl EvidenceEviction {
    fn as_str(self) -> &'static str {
        match self {
            Self::ExactCapacity => "exact_capacity",
            Self::CohortCapacity => "cohort_capacity",
        }
    }
}

#[derive(Default)]
struct RequestEvents {
    transition: Option<TransitionCause>,
    cohort_summaries: Vec<CohortSummary>,
    evidence_evictions: SmallVec<[EvidenceEviction; 2]>,
}

struct RequestObservation {
    profile: DependencyProfile,
    sequence: u64,
    now: Duration,
}

#[derive(Clone, Copy)]
enum SuccessSource {
    Fill,
    CacheHit,
    Coalesced,
}

#[derive(Clone, Copy)]
struct SuccessObservation {
    source: SuccessSource,
    work_units: u64,
    now: Duration,
    rate_half_life: Duration,
}

#[derive(Clone, Copy, Default)]
struct SuccessEvents {
    reuse_opportunity: bool,
    rejected_reuse: bool,
    decision: Option<AdmissionDecision>,
    family_observation: Option<FamilyObservation>,
}

struct FillEvents {
    decision: AdmissionDecision,
    success: SuccessEvents,
}

impl FillEvents {
    fn without_exact(
        observation: FillObservation,
        family: Option<FamilyEstimate>,
        config: RelationCachePolicyConfig,
    ) -> Self {
        Self {
            decision: AdmissionDecision::for_candidate(CandidateEvidence {
                non_coalesced_requests: 1,
                current_reuse_opportunities: 0,
                completed_cohorts: 0,
                zero_reuse_cohorts: 0,
                completed_reuse_opportunities: 0,
                most_recent_reuse_opportunities: None,
                work_units: observation.work_units,
                result_bytes: observation.result_bytes,
                retained_bytes: observation.retained_bytes,
                result_byte_limit: observation.result_byte_limit,
                prior: None,
                family,
                config,
            }),
            success: SuccessEvents::default(),
        }
    }
}

#[derive(Clone, Copy)]
struct FillObservation {
    work_units: u64,
    result_bytes: usize,
    retained_bytes: usize,
    result_byte_limit: usize,
    now: Duration,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RelationCachePolicyCounters {
    pub exact_entries: usize,
    pub family_entries: usize,
    pub family_second_touch_observations: u64,
    pub family_third_touch_conversions: u64,
    pub cohorts: usize,
    pub learned_cohorts: usize,
    pub requests: u64,
    pub reuse_opportunities: u64,
    pub cache_hits: u64,
    pub coalesced_reuses: u64,
    pub fills: u64,
    pub completed_cohorts: u64,
    pub zero_reuse_cohorts: u64,
    pub completed_reuse_opportunities: u64,
    pub oracle_value: i128,
    pub actual_residency_value: i128,
    pub coalescing_value: i128,
    pub policy_regret: i128,
    pub false_admissions: u64,
    pub false_rejections: u64,
    pub admissions_followed_by_reuse: u64,
    pub admissions_without_future_reuse: u64,
    pub rejections_followed_by_reuse: u64,
    pub avoidable_work_after_rejection: u64,
    pub retained_bytes_without_future_reuse: u64,
    pub admit_second_touch: u64,
    pub admit_third_touch: u64,
    pub admit_value_density: u64,
    pub admit_family_conversion: u64,
    pub admit_expensive_probation: u64,
    pub admit_learned_value: u64,
    pub admit_generation_rate: u64,
    pub admit_recovery_probe: u64,
    pub reject_too_large: u64,
    pub reject_recent_no_reuse: u64,
    pub reject_no_reuse_history: u64,
    pub reject_insufficient_value: u64,
}

impl RelationCachePolicyCounters {
    fn observe_family(&mut self, family: &FamilyEvidence) {
        self.family_entries = self.family_entries.saturating_add(1);
        self.family_second_touch_observations = self
            .family_second_touch_observations
            .saturating_add(family.second_touch_observations);
        self.family_third_touch_conversions = self
            .family_third_touch_conversions
            .saturating_add(family.third_touch_conversions);
    }

    fn observe_exact(&mut self, exact: &ExactEvidence) {
        self.exact_entries = self.exact_entries.saturating_add(1);
        self.cohorts = self.cohorts.saturating_add(exact.cohorts.len());
        self.learned_cohorts = self
            .learned_cohorts
            .saturating_add(exact.completed_history.len());
        self.completed_cohorts = self
            .completed_cohorts
            .saturating_add(exact.completed_cohorts);
        self.zero_reuse_cohorts = self
            .zero_reuse_cohorts
            .saturating_add(exact.zero_reuse_cohorts);
        self.completed_reuse_opportunities = self
            .completed_reuse_opportunities
            .saturating_add(exact.completed_reuse_opportunities);
        self.oracle_value = self
            .oracle_value
            .saturating_add(exact.completed_oracle_value);
        self.actual_residency_value = self
            .actual_residency_value
            .saturating_add(exact.completed_actual_residency_value);
        self.coalescing_value = self
            .coalescing_value
            .saturating_add(exact.completed_coalescing_value);
        self.policy_regret = self
            .policy_regret
            .saturating_add(exact.completed_policy_regret);
        self.observe_decision_outcomes(exact.completed_decision_outcomes);
        self.observe_decisions(exact.decisions);
        for cohort in &exact.cohorts {
            self.observe_cohort(cohort);
        }
    }

    fn observe_cohort(&mut self, cohort: &CohortEvidence) {
        self.requests = self.requests.saturating_add(cohort.successful_requests);
        self.reuse_opportunities = self
            .reuse_opportunities
            .saturating_add(cohort.eligible_reuse_opportunities);
        self.cache_hits = self.cache_hits.saturating_add(cohort.cache_hits);
        self.coalesced_reuses = self
            .coalesced_reuses
            .saturating_add(cohort.coalesced_reuses);
        self.fills = self.fills.saturating_add(cohort.fills);
        if !cohort.superseded {
            let value = cohort.value();
            self.oracle_value = self.oracle_value.saturating_add(value.oracle_value);
            self.actual_residency_value = self
                .actual_residency_value
                .saturating_add(value.actual_residency_value);
            self.coalescing_value = self.coalescing_value.saturating_add(value.coalescing_value);
            self.policy_regret = self.policy_regret.saturating_add(value.policy_regret);
            self.observe_decision_outcomes(cohort.decision_outcomes());
        }
    }

    fn observe_decision_outcomes(&mut self, outcomes: DecisionOutcomes) {
        self.admissions_followed_by_reuse = self
            .admissions_followed_by_reuse
            .saturating_add(outcomes.admissions_followed_by_reuse);
        self.admissions_without_future_reuse = self
            .admissions_without_future_reuse
            .saturating_add(outcomes.admissions_without_future_reuse);
        self.rejections_followed_by_reuse = self
            .rejections_followed_by_reuse
            .saturating_add(outcomes.rejections_followed_by_reuse);
        self.avoidable_work_after_rejection = self
            .avoidable_work_after_rejection
            .saturating_add(outcomes.avoidable_work_after_rejection);
        self.retained_bytes_without_future_reuse = self
            .retained_bytes_without_future_reuse
            .saturating_add(outcomes.retained_bytes_without_future_reuse);
        self.false_admissions = self.admissions_without_future_reuse;
        self.false_rejections = self.rejections_followed_by_reuse;
    }

    fn observe_decisions(&mut self, decisions: DecisionCounts) {
        self.admit_second_touch = self
            .admit_second_touch
            .saturating_add(decisions.admit_second_touch);
        self.admit_third_touch = self
            .admit_third_touch
            .saturating_add(decisions.admit_third_touch);
        self.admit_value_density = self
            .admit_value_density
            .saturating_add(decisions.admit_value_density);
        self.admit_family_conversion = self
            .admit_family_conversion
            .saturating_add(decisions.admit_family_conversion);
        self.admit_expensive_probation = self
            .admit_expensive_probation
            .saturating_add(decisions.admit_expensive_probation);
        self.admit_learned_value = self
            .admit_learned_value
            .saturating_add(decisions.admit_learned_value);
        self.admit_generation_rate = self
            .admit_generation_rate
            .saturating_add(decisions.admit_generation_rate);
        self.admit_recovery_probe = self
            .admit_recovery_probe
            .saturating_add(decisions.admit_recovery_probe);
        self.reject_too_large = self
            .reject_too_large
            .saturating_add(decisions.reject_too_large);
        self.reject_recent_no_reuse = self
            .reject_recent_no_reuse
            .saturating_add(decisions.reject_recent_no_reuse);
        self.reject_no_reuse_history = self
            .reject_no_reuse_history
            .saturating_add(decisions.reject_no_reuse_history);
        self.reject_insufficient_value = self
            .reject_insufficient_value
            .saturating_add(decisions.reject_insufficient_value);
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RelationCachePolicyStatistics {
    pub total: RelationCachePolicyCounters,
    pub query: RelationCachePolicyCounters,
    pub hash_build: RelationCachePolicyCounters,
    pub grouped_dimension: RelationCachePolicyCounters,
    pub total_cohorts: RelationCacheCohortStatistics,
    pub query_cohorts: RelationCacheCohortStatistics,
    pub hash_build_cohorts: RelationCacheCohortStatistics,
    pub grouped_dimension_cohorts: RelationCacheCohortStatistics,
}

impl RelationCachePolicyStatistics {
    fn for_domain_mut(
        &mut self,
        domain: MaterializationDomain,
    ) -> &mut RelationCachePolicyCounters {
        match domain {
            MaterializationDomain::QueryResult => &mut self.query,
            MaterializationDomain::HashJoinBuild | MaterializationDomain::SubrelationRowsV1 => {
                &mut self.hash_build
            }
            MaterializationDomain::GroupedHashJoinDimension => &mut self.grouped_dimension,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RelationCacheCohortStatistics {
    pub samples: u64,
    pub observed_lifetime_micros: RelationCacheQuantiles,
    pub successful_requests: RelationCacheQuantiles,
    pub reuse_opportunities: RelationCacheQuantiles,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RelationCacheQuantiles {
    pub p50: u64,
    pub p95: u64,
}

#[derive(Default)]
struct CohortSamples {
    observed_lifetime_micros: Vec<u64>,
    successful_requests: Vec<u64>,
    reuse_opportunities: Vec<u64>,
}

impl CohortSamples {
    fn observe_exact(&mut self, exact: &ExactEvidence) {
        for cohort in &exact.completed_history {
            self.observed_lifetime_micros
                .push(cohort.observed_lifetime_micros);
            self.successful_requests.push(cohort.successful_requests);
            self.reuse_opportunities.push(cohort.reuse_opportunities);
        }
    }

    fn finish(self) -> RelationCacheCohortStatistics {
        RelationCacheCohortStatistics {
            samples: self.successful_requests.len() as u64,
            observed_lifetime_micros: quantiles(self.observed_lifetime_micros),
            successful_requests: quantiles(self.successful_requests),
            reuse_opportunities: quantiles(self.reuse_opportunities),
        }
    }
}

fn quantiles(mut values: Vec<u64>) -> RelationCacheQuantiles {
    if values.is_empty() {
        return RelationCacheQuantiles::default();
    }
    values.sort_unstable();
    RelationCacheQuantiles {
        p50: nearest_rank(&values, 50),
        p95: nearest_rank(&values, 95),
    }
}

fn nearest_rank(values: &[u64], percent: usize) -> u64 {
    let index = values
        .len()
        .saturating_mul(percent)
        .saturating_add(99)
        .saturating_div(100)
        .saturating_sub(1);
    values[index.min(values.len() - 1)]
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct DecisionCounts {
    admit_second_touch: u64,
    admit_third_touch: u64,
    admit_value_density: u64,
    admit_family_conversion: u64,
    admit_expensive_probation: u64,
    admit_learned_value: u64,
    admit_generation_rate: u64,
    admit_recovery_probe: u64,
    reject_too_large: u64,
    reject_recent_no_reuse: u64,
    reject_no_reuse_history: u64,
    reject_insufficient_value: u64,
}

impl DecisionCounts {
    fn observe(&mut self, decision: AdmissionDecision) {
        match decision.reason {
            AdmissionReason::SecondTouch => {
                self.admit_second_touch = self.admit_second_touch.saturating_add(1);
            }
            AdmissionReason::ThirdTouch => {
                self.admit_third_touch = self.admit_third_touch.saturating_add(1);
            }
            AdmissionReason::ValueDensity => {
                self.admit_value_density = self.admit_value_density.saturating_add(1);
            }
            AdmissionReason::FamilyConversion => {
                self.admit_family_conversion = self.admit_family_conversion.saturating_add(1);
            }
            AdmissionReason::ExpensiveProbation => {
                self.admit_expensive_probation = self.admit_expensive_probation.saturating_add(1);
            }
            AdmissionReason::LearnedValue => {
                self.admit_learned_value = self.admit_learned_value.saturating_add(1);
            }
            AdmissionReason::GenerationRate => {
                self.admit_generation_rate = self.admit_generation_rate.saturating_add(1);
            }
            AdmissionReason::RecoveryProbe => {
                self.admit_recovery_probe = self.admit_recovery_probe.saturating_add(1);
            }
            AdmissionReason::TooLarge => {
                self.reject_too_large = self.reject_too_large.saturating_add(1);
            }
            AdmissionReason::RecentNoReuse => {
                self.reject_recent_no_reuse = self.reject_recent_no_reuse.saturating_add(1);
            }
            AdmissionReason::NoReuseHistory => {
                self.reject_no_reuse_history = self.reject_no_reuse_history.saturating_add(1);
            }
            AdmissionReason::InsufficientValue => {
                self.reject_insufficient_value = self.reject_insufficient_value.saturating_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use crate::engine::catalog::model::{CatalogDependencies, TableExistenceDependency};
    use crate::engine::catalog::store::TableDataGeneration;
    use crate::engine::lir::fingerprint::Fingerprint;

    use super::*;

    #[test]
    fn cohort_quantiles_use_nearest_ranks() {
        assert_eq!(
            quantiles(vec![1, 2, 3, 4, 5]),
            RelationCacheQuantiles { p50: 3, p95: 5 }
        );
    }

    fn policy(exact_limit: usize) -> RelationCachePolicy {
        RelationCachePolicy::new(exact_limit, RelationCachePolicyConfig::default())
    }

    fn reuse_policy(reuse_admission: RelationCacheReuseAdmission) -> RelationCachePolicy {
        RelationCachePolicy::new(
            16,
            RelationCachePolicyConfig {
                reuse_admission,
                probation_minimum_work_units: u64::MAX,
                ..RelationCachePolicyConfig::default()
            },
        )
    }

    fn fingerprint(seed: u8) -> Fingerprint {
        Fingerprint {
            canonicalization_version: 1,
            hash_algorithm: 1,
            digest: [seed; 16],
        }
    }

    fn key(seed: u8, data_generation: u64) -> RelationCacheKey {
        key_with_family(seed, seed, data_generation)
    }

    fn key_with_family(exact_seed: u8, family_seed: u8, data_generation: u64) -> RelationCacheKey {
        let mut dependencies = CatalogDependencies::default();
        dependencies.table_existence.push(TableExistenceDependency {
            table_id: "t1".into(),
            table_name: "items".into(),
            generation: 2.into(),
            storage_generation: 3.into(),
        });
        RelationCacheKey::from_generations(
            fingerprint(exact_seed),
            fingerprint(family_seed),
            &dependencies,
            &HashMap::from([(
                "t1".into(),
                TableDataGeneration::test_value(data_generation),
            )]),
        )
    }

    fn cheap_work() -> CachedWork {
        CachedWork::default()
    }

    fn expensive_work() -> CachedWork {
        CachedWork {
            kv: crate::engine::exec::observe::KvWork {
                bytes_read: 8 * 1024 * 1024,
                iterated: 10_000,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn prior_policy() -> RelationCachePolicy {
        RelationCachePolicy::new(
            16,
            RelationCachePolicyConfig {
                minimum_completed_cohorts: 4,
                cohorts_per_exact_relation: 8,
                probation_minimum_work_units: u64::MAX,
                prior: RelationCachePrior::GenerationRate,
                rate_half_life: Duration::from_secs(30),
                ..RelationCachePolicyConfig::default()
            },
        )
    }

    #[test]
    fn second_touch_overrides_a_low_first_fill_score() {
        let policy = reuse_policy(RelationCacheReuseAdmission::SecondTouch);
        let cache_key = key(1, 1);
        let first = policy.observe_request(&cache_key, Duration::from_secs(1));
        policy.observe_fill(
            first,
            cheap_work(),
            1024,
            2048,
            usize::MAX,
            Duration::from_secs(1),
        );
        let second = policy.observe_request(&cache_key, Duration::from_secs(2));
        policy.observe_cache_hit(second, cheap_work(), Duration::from_secs(2));

        let stats = policy.stats();
        assert_eq!(stats.total.requests, 2);
        assert_eq!(stats.total.reuse_opportunities, 1);
        assert_eq!(stats.total.fills, 1);
        assert_eq!(stats.total.admit_second_touch, 1);
    }

    #[test]
    fn third_touch_requires_three_non_coalesced_requests() {
        let policy = reuse_policy(RelationCacheReuseAdmission::ThirdTouch);
        let cache_key = key(1, 1);
        for second in 1..=2 {
            let token = policy.observe_request(&cache_key, Duration::from_secs(second));
            let decision = policy.observe_fill(
                token,
                cheap_work(),
                1024,
                2048,
                usize::MAX,
                Duration::from_secs(second),
            );
            assert_eq!(decision.outcome, AdmissionOutcome::Reject);
        }
        let third = policy.observe_request(&cache_key, Duration::from_secs(3));
        let decision = policy.observe_fill(
            third,
            cheap_work(),
            1024,
            2048,
            usize::MAX,
            Duration::from_secs(3),
        );

        assert_eq!(decision.outcome, AdmissionOutcome::Admit);
        assert_eq!(decision.reason, AdmissionReason::ThirdTouch);
        assert_eq!(policy.stats().total.admit_third_touch, 1);
    }

    #[test]
    fn value_density_qualifies_the_second_touch() {
        let policy = reuse_policy(RelationCacheReuseAdmission::ValueDensity);
        let cache_key = key(1, 1);
        let work = CachedWork {
            kv: crate::engine::exec::observe::KvWork {
                bytes_read: 4096,
                ..Default::default()
            },
            ..Default::default()
        };
        let first = policy.observe_request(&cache_key, Duration::from_secs(1));
        let first_decision =
            policy.observe_fill(first, work, 1024, 1024, usize::MAX, Duration::from_secs(1));
        let second = policy.observe_request(&cache_key, Duration::from_secs(2));
        let second_decision =
            policy.observe_fill(second, work, 1024, 1024, usize::MAX, Duration::from_secs(2));

        assert_eq!(first_decision.outcome, AdmissionOutcome::Reject);
        assert_eq!(second_decision.outcome, AdmissionOutcome::Admit);
        assert_eq!(second_decision.reason, AdmissionReason::ValueDensity);
        assert_eq!(policy.stats().total.admit_value_density, 1);
    }

    #[test]
    fn value_density_rejects_low_value_reuse() {
        let policy = reuse_policy(RelationCacheReuseAdmission::ValueDensity);
        let cache_key = key(1, 1);
        let work = CachedWork {
            kv: crate::engine::exec::observe::KvWork {
                bytes_read: 1500,
                ..Default::default()
            },
            ..Default::default()
        };
        for second in 1..=2 {
            let token = policy.observe_request(&cache_key, Duration::from_secs(second));
            let decision = policy.observe_fill(
                token,
                work,
                1024,
                1024,
                usize::MAX,
                Duration::from_secs(second),
            );
            assert_eq!(decision.outcome, AdmissionOutcome::Reject);
            assert_eq!(decision.reason, AdmissionReason::InsufficientValue);
        }

        assert_eq!(policy.stats().total.admit_value_density, 0);
    }

    fn family_conversion_policy() -> RelationCachePolicy {
        RelationCachePolicy::new(
            16,
            RelationCachePolicyConfig {
                reuse_admission: RelationCacheReuseAdmission::FamilyConversion,
                family_minimum_observations: 3,
                probation_minimum_work_units: u64::MAX,
                ..RelationCachePolicyConfig::default()
            },
        )
    }

    fn fill_request(
        policy: &RelationCachePolicy,
        key: &RelationCacheKey,
        second: u64,
    ) -> AdmissionDecision {
        let work = CachedWork {
            kv: crate::engine::exec::observe::KvWork {
                bytes_read: 5_000,
                ..Default::default()
            },
            ..Default::default()
        };
        let token = policy.observe_request(key, Duration::from_secs(second));
        policy.observe_fill(
            token,
            work,
            512,
            1_000,
            usize::MAX,
            Duration::from_secs(second),
        )
    }

    #[test]
    fn family_conversion_admits_a_second_touch_after_conversions() {
        let policy = family_conversion_policy();
        let mut second = 1;
        for exact_seed in 1..=3 {
            let cache_key = key_with_family(exact_seed, 100, 1);
            for _ in 0..3 {
                fill_request(&policy, &cache_key, second);
                second += 1;
            }
        }
        let candidate = key_with_family(4, 100, 1);
        fill_request(&policy, &candidate, second);
        let decision = fill_request(&policy, &candidate, second + 1);

        assert_eq!(
            decision,
            AdmissionDecision::admit(
                AdmissionReason::FamilyConversion,
                EvidenceSource::FamilyHistory,
            )
        );
        let counters = policy.stats().total;
        assert_eq!(counters.family_second_touch_observations, 4);
        assert_eq!(counters.family_third_touch_conversions, 3);
        assert_eq!(counters.admit_family_conversion, 1);
    }

    #[test]
    fn family_conversion_uses_second_touch_before_family_evidence_is_ready() {
        let policy = family_conversion_policy();
        let cache_key = key_with_family(1, 100, 1);
        fill_request(&policy, &cache_key, 1);
        let decision = fill_request(&policy, &cache_key, 2);

        assert_eq!(
            decision,
            AdmissionDecision::admit(AdmissionReason::SecondTouch, EvidenceSource::ExactCohort,)
        );
    }

    #[test]
    fn family_conversion_rejects_false_second_touches() {
        let policy = family_conversion_policy();
        let mut second = 1;
        for exact_seed in 1..=3 {
            let cache_key = key_with_family(exact_seed, 100, 1);
            for _ in 0..2 {
                fill_request(&policy, &cache_key, second);
                second += 1;
            }
        }
        let candidate = key_with_family(4, 100, 1);
        fill_request(&policy, &candidate, second);
        let second_decision = fill_request(&policy, &candidate, second + 1);
        let third_decision = fill_request(&policy, &candidate, second + 2);

        assert_eq!(
            second_decision,
            AdmissionDecision::reject(
                AdmissionReason::InsufficientValue,
                EvidenceSource::FamilyHistory,
            )
        );
        assert_eq!(third_decision.reason, AdmissionReason::ThirdTouch);
    }

    #[test]
    fn cohort_value_separates_oracle_residency_and_policy_decisions() {
        let profile = DependencyProfile::new(&key(1, 1));
        let mut cohort = CohortEvidence::new(profile, 1);
        cohort.eligible_reuse_opportunities = 2;
        cohort.fill_work_units = 5_000;
        cohort.retained_bytes = 1_000;
        cohort.actual_admissions = 1;

        assert_eq!(
            cohort.value(),
            CohortValue {
                oracle_value: 7_000,
                actual_residency_value: -1_000,
                coalescing_value: 0,
                policy_regret: 8_000,
            }
        );
    }

    #[test]
    fn decision_outcomes_follow_the_next_non_coalesced_request() {
        let policy = reuse_policy(RelationCacheReuseAdmission::SecondTouch);
        let cache_key = key(1, 1);
        let work = CachedWork {
            kv: crate::engine::exec::observe::KvWork {
                bytes_read: 5_000,
                ..Default::default()
            },
            ..Default::default()
        };

        for second in 1..=2 {
            let token = policy.observe_request(&cache_key, Duration::from_secs(second));
            policy.observe_fill(
                token,
                work,
                512,
                1_000,
                usize::MAX,
                Duration::from_secs(second),
            );
        }

        let counters = policy.stats().total;
        assert_eq!(counters.rejections_followed_by_reuse, 1);
        assert_eq!(counters.avoidable_work_after_rejection, 4_000);
        assert_eq!(counters.admissions_without_future_reuse, 1);
        assert_eq!(counters.retained_bytes_without_future_reuse, 1_000);
        assert_eq!(counters.false_rejections, 1);
        assert_eq!(counters.false_admissions, 1);
    }

    #[test]
    fn cache_hit_resolves_a_pending_admission() {
        let policy = reuse_policy(RelationCacheReuseAdmission::SecondTouch);
        let cache_key = key(1, 1);
        let work = CachedWork {
            kv: crate::engine::exec::observe::KvWork {
                bytes_read: 5_000,
                ..Default::default()
            },
            ..Default::default()
        };
        for second in 1..=2 {
            let token = policy.observe_request(&cache_key, Duration::from_secs(second));
            policy.observe_fill(
                token,
                work,
                512,
                1_000,
                usize::MAX,
                Duration::from_secs(second),
            );
        }
        let hit = policy.observe_request(&cache_key, Duration::from_secs(3));
        policy.observe_cache_hit(hit, work, Duration::from_secs(3));

        let counters = policy.stats().total;
        assert_eq!(counters.admissions_followed_by_reuse, 1);
        assert_eq!(counters.admissions_without_future_reuse, 0);
        assert_eq!(counters.rejections_followed_by_reuse, 1);
    }

    #[test]
    fn coalesced_reuse_does_not_resolve_a_pending_admission() {
        let policy = reuse_policy(RelationCacheReuseAdmission::SecondTouch);
        let cache_key = key(1, 1);
        let work = CachedWork {
            kv: crate::engine::exec::observe::KvWork {
                bytes_read: 5_000,
                ..Default::default()
            },
            ..Default::default()
        };
        for second in 1..=2 {
            let token = policy.observe_request(&cache_key, Duration::from_secs(second));
            policy.observe_fill(
                token,
                work,
                512,
                1_000,
                usize::MAX,
                Duration::from_secs(second),
            );
        }
        let waiter = policy.observe_request(&cache_key, Duration::from_secs(3));
        policy.observe_coalesced_reuse(waiter, work, Duration::from_secs(3));

        let counters = policy.stats().total;
        assert_eq!(counters.admissions_followed_by_reuse, 0);
        assert_eq!(counters.admissions_without_future_reuse, 1);
    }

    #[test]
    fn policy_decisions_replay_for_the_same_runtime_sequence() {
        fn run() -> (Vec<AdmissionDecision>, RelationCachePolicyStatistics) {
            let policy = prior_policy();
            let mut decisions = Vec::new();
            for generation in 1..=5 {
                let now = Duration::from_secs(generation * 10);
                let cache_key = key(1, generation);
                let token = policy.observe_request(&cache_key, now);
                decisions.push(policy.observe_fill(
                    token,
                    expensive_work(),
                    64 * 1024,
                    65 * 1024,
                    usize::MAX,
                    now,
                ));
                if generation % 2 == 1 {
                    let hit = policy.observe_request(&cache_key, now + Duration::from_secs(1));
                    policy.observe_cache_hit(hit, expensive_work(), now + Duration::from_secs(1));
                }
            }
            (decisions, policy.stats())
        }

        assert_eq!(run(), run());
    }

    #[test]
    fn family_decisions_replay_for_the_same_runtime_sequence() {
        fn run() -> (Vec<AdmissionDecision>, RelationCachePolicyStatistics) {
            let policy = family_conversion_policy();
            let mut decisions = Vec::new();
            let mut second = 1;
            for (exact_seed, requests) in [(1, 3), (2, 2), (3, 4), (4, 2), (5, 3)] {
                let cache_key = key_with_family(exact_seed, 100, 1);
                for _ in 0..requests {
                    decisions.push(fill_request(&policy, &cache_key, second));
                    second += 1;
                }
            }
            (decisions, policy.stats())
        }

        assert_eq!(run(), run());
    }

    #[test]
    fn expensive_dense_first_fill_receives_probation() {
        let policy = policy(16);
        let cache_key = key(1, 1);
        let token = policy.observe_request(&cache_key, Duration::from_secs(1));
        policy.observe_fill(
            token,
            expensive_work(),
            64 * 1024,
            65 * 1024,
            usize::MAX,
            Duration::from_secs(1),
        );

        assert_eq!(policy.stats().total.admit_expensive_probation, 1);
    }

    #[test]
    fn coalesced_reuse_does_not_trigger_second_touch() {
        let policy = policy(16);
        let cache_key = key(1, 1);
        let owner = policy.observe_request(&cache_key, Duration::from_secs(1));
        let waiter = policy.observe_request(&cache_key, Duration::from_secs(1));
        policy.observe_fill(
            owner,
            cheap_work(),
            1024,
            2048,
            usize::MAX,
            Duration::from_secs(2),
        );
        policy.observe_coalesced_reuse(waiter, cheap_work(), Duration::from_secs(2));

        let stats = policy.stats();
        assert_eq!(stats.total.requests, 2);
        assert_eq!(stats.total.reuse_opportunities, 0);
        assert_eq!(stats.total.coalesced_reuses, 1);
        assert_eq!(stats.total.admit_second_touch, 0);
    }

    #[test]
    fn generation_rate_prior_admits_reuse_before_enough_history() {
        let policy = prior_policy();
        let first_key = key(1, 1);
        let first = policy.observe_request(&first_key, Duration::from_secs(1));
        policy.observe_fill(
            first,
            cheap_work(),
            1024,
            2048,
            usize::MAX,
            Duration::from_secs(1),
        );
        for second in 2..=3 {
            let token = policy.observe_request(&first_key, Duration::from_secs(second));
            policy.observe_cache_hit(token, cheap_work(), Duration::from_secs(second));
        }

        let next_key = key(1, 2);
        let next = policy.observe_request(&next_key, Duration::from_secs(4));
        let decision = policy.observe_fill(
            next,
            CachedWork {
                kv: crate::engine::exec::observe::KvWork {
                    bytes_read: 64 * 1024,
                    ..Default::default()
                },
                ..Default::default()
            },
            1024,
            2048,
            usize::MAX,
            Duration::from_secs(4),
        );

        assert_eq!(decision.outcome, AdmissionOutcome::Admit);
        assert_eq!(decision.reason, AdmissionReason::GenerationRate);
        assert_eq!(decision.evidence, EvidenceSource::GenerationRate);
    }

    #[test]
    fn generation_rate_prior_rejects_insufficient_value() {
        let policy = prior_policy();
        let first_key = key(1, 1);
        let first = policy.observe_request(&first_key, Duration::from_secs(1));
        policy.observe_fill(
            first,
            cheap_work(),
            1024,
            2048,
            usize::MAX,
            Duration::from_secs(1),
        );
        for second in [101, 201] {
            let token = policy.observe_request(&first_key, Duration::from_secs(second));
            policy.observe_cache_hit(token, cheap_work(), Duration::from_secs(second));
        }

        let next_key = key(1, 2);
        let next = policy.observe_request(&next_key, Duration::from_secs(202));
        let decision = policy.observe_fill(
            next,
            CachedWork {
                kv: crate::engine::exec::observe::KvWork {
                    bytes_read: 2200,
                    ..Default::default()
                },
                ..Default::default()
            },
            1024,
            2048,
            usize::MAX,
            Duration::from_secs(202),
        );

        assert_eq!(decision.outcome, AdmissionOutcome::Reject);
        assert_eq!(decision.reason, AdmissionReason::InsufficientValue);
        assert_eq!(decision.evidence, EvidenceSource::GenerationRate);
    }

    #[test]
    fn repeated_zero_reuse_cohorts_suppress_first_touch_probation() {
        let policy = policy(16);
        for generation in 1..=4 {
            let cache_key = key(1, generation);
            let now = Duration::from_secs(generation);
            let token = policy.observe_request(&cache_key, now);
            policy.observe_fill(
                token,
                expensive_work(),
                64 * 1024,
                65 * 1024,
                usize::MAX,
                now,
            );
        }

        let stats = policy.stats();
        assert_eq!(stats.total.completed_cohorts, 3);
        assert_eq!(stats.total.zero_reuse_cohorts, 3);
        assert_eq!(stats.total.reject_recent_no_reuse, 1);
    }

    #[test]
    fn current_cohort_value_recovers_after_zero_reuse_history() {
        let policy = policy(16);
        for generation in 1..=3 {
            let cache_key = key(1, generation);
            let now = Duration::from_secs(generation);
            let token = policy.observe_request(&cache_key, now);
            policy.observe_fill(
                token,
                expensive_work(),
                64 * 1024,
                65 * 1024,
                usize::MAX,
                now,
            );
        }

        let current = key(1, 4);
        let first = policy.observe_request(&current, Duration::from_secs(4));
        let first_decision = policy.observe_fill(
            first,
            expensive_work(),
            64 * 1024,
            65 * 1024,
            usize::MAX,
            Duration::from_secs(4),
        );
        let second = policy.observe_request(&current, Duration::from_secs(5));
        let second_decision = policy.observe_fill(
            second,
            expensive_work(),
            64 * 1024,
            65 * 1024,
            usize::MAX,
            Duration::from_secs(5),
        );

        assert_eq!(first_decision.reason, AdmissionReason::RecentNoReuse);
        assert_eq!(second_decision.reason, AdmissionReason::RecoveryProbe);
        assert_eq!(second_decision.outcome, AdmissionOutcome::Admit);
    }

    #[test]
    fn recovery_waits_until_current_value_covers_residency() {
        let policy = policy(16);
        let work = CachedWork {
            kv: crate::engine::exec::observe::KvWork {
                bytes_read: 1_500,
                ..Default::default()
            },
            ..Default::default()
        };
        for generation in 1..=3 {
            let cache_key = key(1, generation);
            let now = Duration::from_secs(generation);
            let token = policy.observe_request(&cache_key, now);
            policy.observe_fill(token, work, 512, 1_000, usize::MAX, now);
        }

        let current = key(1, 4);
        let first = policy.observe_request(&current, Duration::from_secs(4));
        policy.observe_fill(first, work, 512, 1_000, usize::MAX, Duration::from_secs(4));
        let second = policy.observe_request(&current, Duration::from_secs(5));
        let second_decision =
            policy.observe_fill(second, work, 512, 1_000, usize::MAX, Duration::from_secs(5));
        let third = policy.observe_request(&current, Duration::from_secs(6));
        let third_decision =
            policy.observe_fill(third, work, 512, 1_000, usize::MAX, Duration::from_secs(6));

        assert_eq!(second_decision.reason, AdmissionReason::RecentNoReuse);
        assert_eq!(third_decision.reason, AdmissionReason::RecoveryProbe);
    }

    #[test]
    fn recent_zero_reuse_cools_positive_history() {
        let policy = RelationCachePolicy::new(
            16,
            RelationCachePolicyConfig {
                minimum_completed_cohorts: 3,
                cohorts_per_exact_relation: 4,
                probation_minimum_work_units: u64::MAX,
                ..RelationCachePolicyConfig::default()
            },
        );
        let work = CachedWork {
            kv: crate::engine::exec::observe::KvWork {
                bytes_read: 3_000,
                ..Default::default()
            },
            ..Default::default()
        };
        for generation in 1..=3 {
            let now = Duration::from_secs(generation * 10);
            let cache_key = key(1, generation);
            let fill = policy.observe_request(&cache_key, now);
            policy.observe_fill(fill, work, 512, 1_000, usize::MAX, now);
            for offset in 1..=2 {
                let hit = policy.observe_request(&cache_key, now + Duration::from_secs(offset));
                policy.observe_cache_hit(hit, work, now + Duration::from_secs(offset));
            }
        }

        let zero_reuse = key(1, 4);
        let zero_reuse_fill = policy.observe_request(&zero_reuse, Duration::from_secs(40));
        policy.observe_fill(
            zero_reuse_fill,
            work,
            512,
            1_000,
            usize::MAX,
            Duration::from_secs(40),
        );
        let cooled = key(1, 5);
        let cooled_fill = policy.observe_request(&cooled, Duration::from_secs(50));
        let decision = policy.observe_fill(
            cooled_fill,
            work,
            512,
            1_000,
            usize::MAX,
            Duration::from_secs(50),
        );

        assert_eq!(
            decision,
            AdmissionDecision::reject(AdmissionReason::RecentNoReuse, EvidenceSource::ExactHistory,)
        );
    }

    #[test]
    fn current_value_ends_recent_zero_reuse_cooldown() {
        let policy = RelationCachePolicy::new(
            16,
            RelationCachePolicyConfig {
                minimum_completed_cohorts: 3,
                cohorts_per_exact_relation: 4,
                probation_minimum_work_units: u64::MAX,
                ..RelationCachePolicyConfig::default()
            },
        );
        let work = CachedWork {
            kv: crate::engine::exec::observe::KvWork {
                bytes_read: 3_000,
                ..Default::default()
            },
            ..Default::default()
        };
        for generation in 1..=3 {
            let now = Duration::from_secs(generation * 10);
            let cache_key = key(1, generation);
            let fill = policy.observe_request(&cache_key, now);
            policy.observe_fill(fill, work, 512, 1_000, usize::MAX, now);
            for offset in 1..=2 {
                let hit = policy.observe_request(&cache_key, now + Duration::from_secs(offset));
                policy.observe_cache_hit(hit, work, now + Duration::from_secs(offset));
            }
        }

        let zero_reuse = key(1, 4);
        let zero_reuse_fill = policy.observe_request(&zero_reuse, Duration::from_secs(40));
        policy.observe_fill(
            zero_reuse_fill,
            work,
            512,
            1_000,
            usize::MAX,
            Duration::from_secs(40),
        );
        let recovered = key(1, 5);
        let first = policy.observe_request(&recovered, Duration::from_secs(50));
        policy.observe_fill(first, work, 512, 1_000, usize::MAX, Duration::from_secs(50));
        let second = policy.observe_request(&recovered, Duration::from_secs(51));
        let decision = policy.observe_fill(
            second,
            work,
            512,
            1_000,
            usize::MAX,
            Duration::from_secs(51),
        );

        assert_eq!(
            decision,
            AdmissionDecision::admit(AdmissionReason::RecoveryProbe, EvidenceSource::ExactCohort,)
        );
    }

    #[test]
    fn completed_history_window_adapts_in_both_directions() {
        let policy = RelationCachePolicy::new(
            16,
            RelationCachePolicyConfig {
                minimum_completed_cohorts: 3,
                cohorts_per_exact_relation: 4,
                probation_minimum_work_units: u64::MAX,
                ..RelationCachePolicyConfig::default()
            },
        );
        let work = CachedWork {
            kv: crate::engine::exec::observe::KvWork {
                bytes_read: 3_000,
                ..Default::default()
            },
            ..Default::default()
        };

        for generation in 1..=5 {
            let now = Duration::from_secs(generation * 10);
            let cache_key = key(1, generation);
            let fill = policy.observe_request(&cache_key, now);
            policy.observe_fill(fill, work, 512, 1_000, usize::MAX, now);
            for offset in 1..=2 {
                let hit = policy.observe_request(&cache_key, now + Duration::from_secs(offset));
                policy.observe_cache_hit(hit, work, now + Duration::from_secs(offset));
            }
        }
        let useful = key(1, 6);
        let useful_fill = policy.observe_request(&useful, Duration::from_secs(60));
        let useful_decision = policy.observe_fill(
            useful_fill,
            work,
            512,
            1_000,
            usize::MAX,
            Duration::from_secs(60),
        );
        assert_eq!(useful_decision.reason, AdmissionReason::LearnedValue);

        for generation in 7..=10 {
            let now = Duration::from_secs(generation * 10);
            let cache_key = key(1, generation);
            let fill = policy.observe_request(&cache_key, now);
            policy.observe_fill(fill, work, 512, 1_000, usize::MAX, now);
        }
        let wasteful = key(1, 11);
        let wasteful_fill = policy.observe_request(&wasteful, Duration::from_secs(110));
        let wasteful_decision = policy.observe_fill(
            wasteful_fill,
            work,
            512,
            1_000,
            usize::MAX,
            Duration::from_secs(110),
        );
        assert_eq!(wasteful_decision.reason, AdmissionReason::RecentNoReuse);

        for generation in 11..=14 {
            let now = Duration::from_secs(generation * 10);
            let cache_key = key(1, generation);
            for offset in 1..=2 {
                let hit = policy.observe_request(&cache_key, now + Duration::from_secs(offset));
                policy.observe_cache_hit(hit, work, now + Duration::from_secs(offset));
            }
            let next = key(1, generation + 1);
            let next_fill = policy.observe_request(&next, now + Duration::from_secs(9));
            policy.observe_fill(
                next_fill,
                work,
                512,
                1_000,
                usize::MAX,
                now + Duration::from_secs(9),
            );
        }
        let recovered = key(1, 15);
        let recovered_fill = policy.observe_request(&recovered, Duration::from_secs(150));
        let recovered_decision = policy.observe_fill(
            recovered_fill,
            work,
            512,
            1_000,
            usize::MAX,
            Duration::from_secs(150),
        );
        assert_eq!(recovered_decision.reason, AdmissionReason::LearnedValue);
        assert!(policy.stats().total.learned_cohorts <= 4);
    }

    #[test]
    fn completed_cohort_reuse_can_admit_a_cheaper_first_touch() {
        let policy = policy(16);
        for generation in 1..=3 {
            let cache_key = key(1, generation);
            let now = Duration::from_secs(generation * 10);
            let token = policy.observe_request(&cache_key, now);
            policy.observe_fill(token, cheap_work(), 64 * 1024, 65 * 1024, usize::MAX, now);
            let second = policy.observe_request(&cache_key, now + Duration::from_secs(1));
            policy.observe_cache_hit(second, cheap_work(), now + Duration::from_secs(1));
            let third = policy.observe_request(&cache_key, now + Duration::from_secs(2));
            policy.observe_cache_hit(third, cheap_work(), now + Duration::from_secs(2));
        }
        let next = key(1, 4);
        let token = policy.observe_request(&next, Duration::from_secs(40));
        let learned_work = CachedWork {
            kv: crate::engine::exec::observe::KvWork {
                bytes_read: 128 * 1024,
                ..Default::default()
            },
            ..Default::default()
        };
        policy.observe_fill(
            token,
            learned_work,
            64 * 1024,
            65 * 1024,
            usize::MAX,
            Duration::from_secs(40),
        );

        let stats = policy.stats();
        assert_eq!(stats.total.completed_cohorts, 3);
        assert_eq!(stats.total.completed_reuse_opportunities, 6);
        assert_eq!(stats.total.admit_learned_value, 1);
    }

    #[test]
    fn an_old_pinned_cohort_does_not_create_a_new_transition() {
        let policy = policy(16);
        let old = key(1, 100);
        let current = key(1, 101);
        let old_fill = policy.observe_request(&old, Duration::from_secs(1));
        policy.observe_fill(
            old_fill,
            cheap_work(),
            1024,
            2048,
            usize::MAX,
            Duration::from_secs(1),
        );
        let current_fill = policy.observe_request(&current, Duration::from_secs(2));
        policy.observe_fill(
            current_fill,
            cheap_work(),
            1024,
            2048,
            usize::MAX,
            Duration::from_secs(2),
        );
        let old_hit = policy.observe_request(&old, Duration::from_secs(3));
        policy.observe_cache_hit(old_hit, cheap_work(), Duration::from_secs(3));

        let stats = policy.stats();
        assert_eq!(stats.total.cohorts, 2);
        assert_eq!(stats.total.completed_cohorts, 1);
        assert_eq!(stats.total.requests, 3);
        assert_eq!(stats.total.reuse_opportunities, 1);
    }

    #[test]
    fn evidence_has_exact_and_cohort_bounds() {
        let exact_bounded_policy = policy(2);
        for seed in 1..=8 {
            exact_bounded_policy.observe_request(&key(seed, 1), Duration::from_secs(seed.into()));
        }
        assert!(exact_bounded_policy.stats().total.exact_entries <= 2);

        let cohort_bounded_policy = policy(2);
        for generation in 1..=8 {
            cohort_bounded_policy
                .observe_request(&key(1, generation), Duration::from_secs(generation));
        }
        assert!(cohort_bounded_policy.stats().total.cohorts <= 4);

        let family_bounded_policy = RelationCachePolicy::new(
            2,
            RelationCachePolicyConfig {
                reuse_admission: RelationCacheReuseAdmission::FamilyConversion,
                probation_minimum_work_units: u64::MAX,
                ..RelationCachePolicyConfig::default()
            },
        );
        for seed in 1..=8 {
            let cache_key = key_with_family(seed, seed, 1);
            fill_request(&family_bounded_policy, &cache_key, u64::from(seed) * 2);
            fill_request(&family_bounded_policy, &cache_key, u64::from(seed) * 2 + 1);
        }
        assert!(family_bounded_policy.stats().total.family_entries <= 2);
    }

    #[test]
    fn dependency_profiles_bound_the_numeric_generation_vector() {
        let dependencies = (0..GENERATION_VALUE_LIMIT + 1)
            .map(|index| DependencyGeneration::Column {
                table_id: "t1".into(),
                column_id: format!("c{index}").into(),
                generation: 1.into(),
            })
            .collect();
        let cache_key =
            RelationCacheKey::from_key_parts(fingerprint(1), fingerprint(1), dependencies);
        let profile = DependencyProfile::new(&cache_key);

        assert_eq!(profile.generations.len(), GENERATION_VALUE_LIMIT);
        assert!(!profile.complete_generation_vector);
    }

    #[test]
    fn dependency_profiles_classify_transition_causes() {
        fn profile(dependency: DependencyGeneration) -> DependencyProfile {
            DependencyProfile::new(&RelationCacheKey::from_key_parts(
                fingerprint(1),
                fingerprint(1),
                vec![dependency],
            ))
        }

        let table = |existence: u64, storage: u64, data: u64| {
            profile(DependencyGeneration::Table {
                table_id: "t1".into(),
                existence_generation: existence.into(),
                storage_generation: storage.into(),
                data_generation: Arc::new(data.into()),
            })
        };
        let base = table(1, 1, 1);
        assert_eq!(
            table(1, 1, 2).transition_cause(&base),
            TransitionCause::Data
        );
        assert_eq!(
            table(2, 1, 1).transition_cause(&base),
            TransitionCause::Semantic
        );
        assert_eq!(
            table(1, 2, 1).transition_cause(&base),
            TransitionCause::Storage
        );
        assert_eq!(
            table(2, 1, 2).transition_cause(&base),
            TransitionCause::Multiple
        );

        let access = |generation: u64| {
            profile(DependencyGeneration::Index {
                table_id: "t1".into(),
                index_id: "i1".into(),
                generation: generation.into(),
            })
        };
        assert_eq!(
            access(2).transition_cause(&access(1)),
            TransitionCause::Access
        );

        let write = |generation: u64| {
            profile(DependencyGeneration::WriteProtocol {
                table_id: "t1".into(),
                generation: generation.into(),
            })
        };
        assert_eq!(
            write(2).transition_cause(&write(1)),
            TransitionCause::WriteProtocol
        );
    }

    #[test]
    fn mixed_generation_vectors_do_not_supersede_each_other() {
        fn mixed_key(first: u64, second: u64) -> RelationCacheKey {
            RelationCacheKey::from_key_parts(
                fingerprint(1),
                fingerprint(1),
                vec![
                    DependencyGeneration::Table {
                        table_id: "t1".into(),
                        existence_generation: 1.into(),
                        storage_generation: 1.into(),
                        data_generation: Arc::new(first.into()),
                    },
                    DependencyGeneration::Table {
                        table_id: "t2".into(),
                        existence_generation: 1.into(),
                        storage_generation: 1.into(),
                        data_generation: Arc::new(second.into()),
                    },
                ],
            )
        }

        let policy = policy(16);
        let first = mixed_key(1, 2);
        let token = policy.observe_request(&first, Duration::from_secs(1));
        policy.observe_fill(
            token,
            cheap_work(),
            1024,
            2048,
            usize::MAX,
            Duration::from_secs(1),
        );
        let second = mixed_key(2, 1);
        let token = policy.observe_request(&second, Duration::from_secs(2));
        policy.observe_fill(
            token,
            cheap_work(),
            1024,
            2048,
            usize::MAX,
            Duration::from_secs(2),
        );

        let stats = policy.stats();
        assert_eq!(stats.total.cohorts, 2);
        assert_eq!(stats.total.completed_cohorts, 0);
    }

    #[test]
    fn different_dependency_members_do_not_complete_a_cohort() {
        let policy = policy(16);
        let first = key(1, 1);
        let first_token = policy.observe_request(&first, Duration::from_secs(1));
        policy.observe_fill(
            first_token,
            cheap_work(),
            1024,
            2048,
            usize::MAX,
            Duration::from_secs(1),
        );
        let second = RelationCacheKey::from_key_parts(
            fingerprint(1),
            fingerprint(1),
            vec![DependencyGeneration::Column {
                table_id: "t1".into(),
                column_id: "c1".into(),
                generation: 2.into(),
            }],
        );
        let second_token = policy.observe_request(&second, Duration::from_secs(2));
        policy.observe_fill(
            second_token,
            cheap_work(),
            1024,
            2048,
            usize::MAX,
            Duration::from_secs(2),
        );

        assert_eq!(policy.stats().total.completed_cohorts, 0);
    }
}
