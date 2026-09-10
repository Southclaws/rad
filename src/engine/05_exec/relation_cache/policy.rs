//! Bounded admission evidence for the relation cache.
//!
//! This module does not select returned data and does not change Foyer
//! admission. The complete `RelationCacheKey` remains the only lookup
//! identity. The policy uses compact dependency profiles only for performance
//! evidence. A profile collision can change a shadow decision, but it cannot
//! cause a cache hit or expose a result from another dependency vector.
//!
//! One exact fingerprint can have several live dependency cohorts. An old
//! transaction can request an older cohort after a newer cohort is visible.
//! The evidence therefore retains cohorts by identity. It does not use one
//! mutable "current generation" record.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use sha2::{Digest as _, Sha256};
use smallvec::SmallVec;

use super::{CachedWork, DependencyGeneration, RelationCacheKey};
use crate::engine::lir::fingerprint::Fingerprint;

const COHORT_LIMIT_PER_EXACT: usize = 4;
const GENERATION_VALUE_LIMIT: usize = 64;
const PROBATION_MIN_WORK_UNITS: u64 = 1024 * 1024;
const PROBATION_MIN_WORK_PER_BYTE: u64 = 4;
const NO_REUSE_SAMPLE_MIN: u64 = 3;
const NO_REUSE_NUMERATOR: u64 = 3;
const NO_REUSE_DENOMINATOR: u64 = 4;

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
    exact: Fingerprint,
    cohort: [u8; 16],
}

pub(super) struct RelationCachePolicy {
    shards: Box<[Mutex<PolicyShard>]>,
}

impl RelationCachePolicy {
    pub(super) fn new(exact_limit: usize) -> Self {
        let exact_limit = exact_limit.max(1);
        let shard_count = super::CACHE_SHARDS.min(exact_limit);
        let base_limit = exact_limit / shard_count;
        let extra = exact_limit % shard_count;
        // The per-shard limits add up to the exact limit. A skewed fingerprint
        // distribution can evict one shard early, but it cannot make the
        // evidence structure exceed the process limit.
        let shards = (0..shard_count)
            .map(|index| Mutex::new(PolicyShard::new(base_limit + usize::from(index < extra))))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self { shards }
    }

    pub(super) fn observe_request(&self, key: &RelationCacheKey) -> CohortToken {
        let profile = DependencyProfile::new(key);
        let token = CohortToken {
            exact: key.exact,
            cohort: profile.cohort,
        };
        let events = self
            .shard(key.exact)
            .lock()
            .expect("relation cache policy lock poisoned")
            .observe_request(key.exact, profile);
        if let Some(cause) = events.transition {
            crate::telemetry::relation_cache_cohort_transition(cause.as_str());
        }
        for reuse_opportunities in events.superseded_reuse {
            crate::telemetry::relation_cache_cohort_reuse(reuse_opportunities, "superseded");
        }
        for cause in events.evidence_evictions {
            crate::telemetry::relation_cache_evidence_eviction(cause.as_str());
        }
        token
    }

    pub(super) fn observe_cache_hit(&self, token: CohortToken) {
        self.observe_success(token, SuccessSource::CacheHit);
    }

    pub(super) fn observe_coalesced_reuse(&self, token: CohortToken) {
        self.observe_success(token, SuccessSource::Coalesced);
    }

    pub(super) fn observe_fill(
        &self,
        token: CohortToken,
        work: CachedWork,
        result_bytes: usize,
        retained_bytes: usize,
        result_byte_limit: usize,
    ) {
        let work_units = deterministic_work(work);
        let fill = {
            let mut shard = self
                .shard(token.exact)
                .lock()
                .expect("relation cache policy lock poisoned");
            shard.observe_fill(
                token,
                work_units,
                result_bytes,
                retained_bytes,
                result_byte_limit,
            )
        };
        let retained_bytes_u64 = u64::try_from(retained_bytes).unwrap_or(u64::MAX).max(1);
        let density = work_units as f64 / retained_bytes_u64 as f64;
        Self::emit_success(fill.success);
        crate::telemetry::relation_cache_shadow_candidate(work_units, density);
        crate::telemetry::relation_cache_shadow_decision(
            fill.decision.outcome(),
            fill.decision.reason(),
            fill.decision.evidence_source(),
        );
    }

    fn shard(&self, exact: Fingerprint) -> &Mutex<PolicyShard> {
        &self.shards[usize::from(exact.digest[0]) % self.shards.len()]
    }

    fn observe_success(&self, token: CohortToken, source: SuccessSource) {
        let events = self
            .shard(token.exact)
            .lock()
            .expect("relation cache policy lock poisoned")
            .observe_success(token, source);
        Self::emit_success(events);
    }

    fn emit_success(events: SuccessEvents) {
        if events.reuse_opportunity {
            crate::telemetry::relation_cache_reuse_opportunity();
        }
        if events.rejected_reuse {
            crate::telemetry::relation_cache_shadow_rejected_reuse();
        }
        if events.promoted_on_reuse {
            crate::telemetry::relation_cache_shadow_decision(
                "admit",
                "second_touch",
                "exact_cohort",
            );
        }
    }

    #[cfg(test)]
    pub(super) fn stats(&self) -> PolicyStats {
        let mut result = PolicyStats::default();
        for shard in &self.shards {
            let shard = shard.lock().expect("relation cache policy lock poisoned");
            result.exact_entries += shard.exact.len();
            for exact in shard.exact.values() {
                result.cohorts += exact.cohorts.len();
                result.completed_cohorts = result
                    .completed_cohorts
                    .saturating_add(exact.completed_cohorts);
                result.zero_reuse_cohorts = result
                    .zero_reuse_cohorts
                    .saturating_add(exact.zero_reuse_cohorts);
                result.completed_reuse_opportunities = result
                    .completed_reuse_opportunities
                    .saturating_add(exact.completed_reuse_opportunities);
                for cohort in &exact.cohorts {
                    result.requests = result.requests.saturating_add(cohort.successful_requests);
                    result.reuse_opportunities = result
                        .reuse_opportunities
                        .saturating_add(cohort.successful_requests.saturating_sub(1));
                    result.cache_hits = result.cache_hits.saturating_add(cohort.cache_hits);
                    result.coalesced_reuses = result
                        .coalesced_reuses
                        .saturating_add(cohort.coalesced_reuses);
                    result.fills = result.fills.saturating_add(cohort.fills);
                    match cohort.last_decision {
                        Some(ShadowDecision::AdmitSecondTouch) => {
                            result.admit_second_touch += 1;
                        }
                        Some(ShadowDecision::AdmitExpensiveProbation { .. }) => {
                            result.admit_expensive_probation += 1;
                        }
                        Some(ShadowDecision::AdmitLearnedValue) => {
                            result.admit_learned_value += 1;
                        }
                        Some(ShadowDecision::RejectTooLarge) => result.reject_too_large += 1,
                        Some(ShadowDecision::RejectNoReuseHistory) => {
                            result.reject_no_reuse_history += 1;
                        }
                        Some(ShadowDecision::RejectInsufficientValue { .. }) => {
                            result.reject_insufficient_value += 1;
                        }
                        None => {}
                    }
                }
            }
        }
        result
    }
}

fn deterministic_work(work: CachedWork) -> u64 {
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
    exact: BTreeMap<Fingerprint, ExactEvidence>,
    recency: BTreeSet<(u64, Fingerprint)>,
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
        exact_fingerprint: Fingerprint,
        profile: DependencyProfile,
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
        if let Some(exact) = self.exact.get(&exact_fingerprint) {
            self.recency
                .remove(&(exact.last_seen_sequence, exact_fingerprint));
        } else {
            if self.exact.len() == self.exact_limit {
                let (_, evicted) = self
                    .recency
                    .pop_first()
                    .expect("policy recency has every exact entry");
                self.exact
                    .remove(&evicted)
                    .expect("policy recency points to an exact entry");
                events
                    .evidence_evictions
                    .push(EvidenceEviction::ExactCapacity);
            }
            self.exact.insert(exact_fingerprint, ExactEvidence::new());
        }
        let exact = self
            .exact
            .get_mut(&exact_fingerprint)
            .expect("exact policy evidence is present");
        exact.last_seen_sequence = sequence;
        exact.observe_request(profile, sequence, &mut events);
        self.recency.insert((sequence, exact_fingerprint));
        events
    }

    fn observe_fill(
        &mut self,
        token: CohortToken,
        work_units: u64,
        result_bytes: usize,
        retained_bytes: usize,
        result_byte_limit: usize,
    ) -> FillEvents {
        let Some(exact) = self.exact.get_mut(&token.exact) else {
            return FillEvents {
                decision: ShadowDecision::for_candidate(CandidateEvidence {
                    successful_requests: 1,
                    completed_cohorts: 0,
                    zero_reuse_cohorts: 0,
                    completed_reuse_opportunities: 0,
                    work_units,
                    result_bytes,
                    retained_bytes,
                    result_byte_limit,
                }),
                success: SuccessEvents::default(),
            };
        };
        let completed_cohorts = exact.completed_cohorts;
        let zero_reuse_cohorts = exact.zero_reuse_cohorts;
        let completed_reuse_opportunities = exact.completed_reuse_opportunities;
        let Some(cohort) = exact
            .cohorts
            .iter_mut()
            .find(|cohort| cohort.profile.cohort == token.cohort)
        else {
            return FillEvents {
                decision: ShadowDecision::for_candidate(CandidateEvidence {
                    successful_requests: 1,
                    completed_cohorts,
                    zero_reuse_cohorts,
                    completed_reuse_opportunities,
                    work_units,
                    result_bytes,
                    retained_bytes,
                    result_byte_limit,
                }),
                success: SuccessEvents::default(),
            };
        };
        cohort.fills = cohort.fills.saturating_add(1);
        let success = cohort.observe_success(SuccessSource::Fill);
        let decision = ShadowDecision::for_candidate(CandidateEvidence {
            successful_requests: cohort.successful_requests,
            completed_cohorts,
            zero_reuse_cohorts,
            completed_reuse_opportunities,
            work_units,
            result_bytes,
            retained_bytes,
            result_byte_limit,
        });
        cohort.last_decision = Some(decision);
        FillEvents { decision, success }
    }

    fn observe_success(&mut self, token: CohortToken, source: SuccessSource) -> SuccessEvents {
        let Some(exact) = self.exact.get_mut(&token.exact) else {
            return SuccessEvents::default();
        };
        let Some(cohort) = exact
            .cohorts
            .iter_mut()
            .find(|cohort| cohort.profile.cohort == token.cohort)
        else {
            return SuccessEvents::default();
        };
        cohort.observe_success(source)
    }
}

struct ExactEvidence {
    last_seen_sequence: u64,
    cohorts: Vec<CohortEvidence>,
    completed_cohorts: u64,
    zero_reuse_cohorts: u64,
    completed_reuse_opportunities: u64,
}

impl ExactEvidence {
    fn new() -> Self {
        Self {
            last_seen_sequence: 0,
            cohorts: Vec::new(),
            completed_cohorts: 0,
            zero_reuse_cohorts: 0,
            completed_reuse_opportunities: 0,
        }
    }

    fn observe_request(
        &mut self,
        profile: DependencyProfile,
        sequence: u64,
        events: &mut RequestEvents,
    ) {
        if let Some(cohort) = self
            .cohorts
            .iter_mut()
            .find(|cohort| cohort.profile.cohort == profile.cohort)
        {
            cohort.attempts = cohort.attempts.saturating_add(1);
            cohort.last_seen_sequence = sequence;
            return;
        }

        let latest_index = self
            .cohorts
            .iter()
            .enumerate()
            .max_by_key(|(_, cohort)| cohort.last_seen_sequence)
            .map(|(index, _)| index);
        if let Some(previous) = latest_index.map(|index| &self.cohorts[index]) {
            match profile.order_from(&previous.profile) {
                GenerationOrder::Newer => {
                    events.transition = Some(profile.transition_cause(&previous.profile));
                }
                GenerationOrder::DifferentMembers => {
                    events.transition = Some(TransitionCause::DependencySet);
                }
                GenerationOrder::Mixed => {
                    events.transition = Some(TransitionCause::Mixed);
                }
                GenerationOrder::Older | GenerationOrder::Equal => {}
            }
        }

        let mut completed = SmallVec::<[(usize, Option<u64>); COHORT_LIMIT_PER_EXACT]>::new();
        for (index, cohort) in self.cohorts.iter().enumerate() {
            let superseded = match profile.order_from(&cohort.profile) {
                GenerationOrder::Newer => true,
                GenerationOrder::DifferentMembers => {
                    Some(index) == latest_index && !cohort.superseded
                }
                GenerationOrder::Older | GenerationOrder::Equal | GenerationOrder::Mixed => false,
            };
            if superseded && !cohort.superseded {
                completed.push((
                    index,
                    (cohort.successful_requests > 0)
                        .then(|| cohort.successful_requests.saturating_sub(1)),
                ));
            }
        }
        for (index, reuse_opportunities) in completed {
            self.cohorts[index].superseded = true;
            let Some(reuse_opportunities) = reuse_opportunities else {
                continue;
            };
            self.completed_cohorts = self.completed_cohorts.saturating_add(1);
            if reuse_opportunities == 0 {
                self.zero_reuse_cohorts = self.zero_reuse_cohorts.saturating_add(1);
            }
            self.completed_reuse_opportunities = self
                .completed_reuse_opportunities
                .saturating_add(reuse_opportunities);
            events.superseded_reuse.push(reuse_opportunities);
        }

        if self.cohorts.len() == COHORT_LIMIT_PER_EXACT {
            let evicted = self
                .cohorts
                .iter()
                .enumerate()
                .min_by_key(|(_, cohort)| (cohort.last_seen_sequence, cohort.profile.cohort))
                .map(|(index, _)| index)
                .expect("a full cohort set is not empty");
            self.cohorts.swap_remove(evicted);
            events
                .evidence_evictions
                .push(EvidenceEviction::CohortCapacity);
        }
        self.cohorts.push(CohortEvidence::new(profile, sequence));
    }
}

struct CohortEvidence {
    profile: DependencyProfile,
    attempts: u64,
    // Failed and cancelled requests do not enter reuse evidence. They cannot
    // reuse a successful materialization and must not trigger second-touch
    // admission.
    successful_requests: u64,
    cache_hits: u64,
    coalesced_reuses: u64,
    fills: u64,
    last_seen_sequence: u64,
    superseded: bool,
    last_decision: Option<ShadowDecision>,
}

impl CohortEvidence {
    fn new(profile: DependencyProfile, sequence: u64) -> Self {
        Self {
            profile,
            attempts: 1,
            successful_requests: 0,
            cache_hits: 0,
            coalesced_reuses: 0,
            fills: 0,
            last_seen_sequence: sequence,
            superseded: false,
            last_decision: None,
        }
    }

    fn observe_success(&mut self, source: SuccessSource) -> SuccessEvents {
        let mut events = SuccessEvents {
            reuse_opportunity: self.successful_requests > 0,
            ..Default::default()
        };
        if events.reuse_opportunity {
            if self
                .last_decision
                .is_some_and(ShadowDecision::is_promotable_rejection)
            {
                // The active cache can return this request even when the
                // shadow gate rejects the first fill. Treat this as the second
                // successful request that causes admission. This prevents
                // each additional active-cache hit from becoming another shadow
                // miss.
                events.rejected_reuse = true;
                events.promoted_on_reuse = true;
                self.last_decision = Some(ShadowDecision::AdmitSecondTouch);
            } else {
                events.rejected_reuse =
                    self.last_decision.is_some_and(ShadowDecision::is_rejection);
            }
        }
        self.successful_requests = self.successful_requests.saturating_add(1);
        match source {
            SuccessSource::Fill => {}
            SuccessSource::CacheHit => self.cache_hits = self.cache_hits.saturating_add(1),
            SuccessSource::Coalesced => {
                self.coalesced_reuses = self.coalesced_reuses.saturating_add(1);
            }
        }
        events
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShadowDecision {
    AdmitSecondTouch,
    AdmitExpensiveProbation { evidence: EvidenceSource },
    AdmitLearnedValue,
    RejectTooLarge,
    RejectNoReuseHistory,
    RejectInsufficientValue { evidence: EvidenceSource },
}

struct CandidateEvidence {
    successful_requests: u64,
    completed_cohorts: u64,
    zero_reuse_cohorts: u64,
    completed_reuse_opportunities: u64,
    work_units: u64,
    result_bytes: usize,
    retained_bytes: usize,
    result_byte_limit: usize,
}

impl ShadowDecision {
    fn for_candidate(candidate: CandidateEvidence) -> Self {
        if candidate.result_bytes > candidate.result_byte_limit {
            return Self::RejectTooLarge;
        }
        if candidate.successful_requests > 1 {
            return Self::AdmitSecondTouch;
        }
        let retained_bytes = u64::try_from(candidate.retained_bytes)
            .unwrap_or(u64::MAX)
            .max(1);
        let stable_no_reuse = candidate.completed_cohorts >= NO_REUSE_SAMPLE_MIN
            && candidate
                .zero_reuse_cohorts
                .saturating_mul(NO_REUSE_DENOMINATOR)
                >= candidate
                    .completed_cohorts
                    .saturating_mul(NO_REUSE_NUMERATOR);
        if candidate.completed_cohorts >= NO_REUSE_SAMPLE_MIN {
            // One retained byte is one restore-work unit and one
            // admission-copy unit. Cross multiplication preserves the exact
            // ratio and avoids floating-point policy decisions.
            let net_work_per_hit = candidate.work_units.saturating_sub(retained_bytes);
            let learned_benefit = candidate
                .completed_reuse_opportunities
                .saturating_mul(net_work_per_hit);
            let learned_admission_cost = candidate.completed_cohorts.saturating_mul(retained_bytes);
            if learned_benefit >= learned_admission_cost {
                return Self::AdmitLearnedValue;
            }
            if stable_no_reuse {
                return Self::RejectNoReuseHistory;
            }
            return Self::RejectInsufficientValue {
                evidence: EvidenceSource::ExactHistory,
            };
        }
        let expensive = candidate.work_units >= PROBATION_MIN_WORK_UNITS
            && candidate.work_units >= retained_bytes.saturating_mul(PROBATION_MIN_WORK_PER_BYTE);
        if expensive {
            Self::AdmitExpensiveProbation {
                evidence: EvidenceSource::Default,
            }
        } else {
            Self::RejectInsufficientValue {
                evidence: EvidenceSource::Default,
            }
        }
    }

    fn outcome(self) -> &'static str {
        match self {
            Self::AdmitSecondTouch
            | Self::AdmitExpensiveProbation { .. }
            | Self::AdmitLearnedValue => "admit",
            Self::RejectTooLarge
            | Self::RejectNoReuseHistory
            | Self::RejectInsufficientValue { .. } => "reject",
        }
    }

    fn reason(self) -> &'static str {
        match self {
            Self::AdmitSecondTouch => "second_touch",
            Self::AdmitExpensiveProbation { .. } => "expensive_probation",
            Self::AdmitLearnedValue => "learned_value",
            Self::RejectTooLarge => "too_large",
            Self::RejectNoReuseHistory => "no_reuse_history",
            Self::RejectInsufficientValue { .. } => "insufficient_value",
        }
    }

    fn evidence_source(self) -> &'static str {
        match self {
            Self::AdmitSecondTouch => EvidenceSource::ExactCohort.as_str(),
            Self::AdmitLearnedValue => EvidenceSource::ExactHistory.as_str(),
            Self::AdmitExpensiveProbation { evidence }
            | Self::RejectInsufficientValue { evidence } => evidence.as_str(),
            Self::RejectNoReuseHistory => EvidenceSource::ExactHistory.as_str(),
            Self::RejectTooLarge => EvidenceSource::HardLimit.as_str(),
        }
    }

    fn is_rejection(self) -> bool {
        matches!(
            self,
            Self::RejectTooLarge
                | Self::RejectNoReuseHistory
                | Self::RejectInsufficientValue { .. }
        )
    }

    fn is_promotable_rejection(self) -> bool {
        matches!(
            self,
            Self::RejectNoReuseHistory | Self::RejectInsufficientValue { .. }
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EvidenceSource {
    Default,
    ExactCohort,
    ExactHistory,
    HardLimit,
}

impl EvidenceSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::ExactCohort => "exact_cohort",
            Self::ExactHistory => "exact_history",
            Self::HardLimit => "hard_limit",
        }
    }
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
    generations: SmallVec<[u64; 12]>,
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

        for dependency in &key.dependencies {
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
                    for generation in data_generation.stripes() {
                        update_generation(&mut cohort, generation.get());
                    }
                    update_generation(&mut semantic, existence_generation.get());
                    update_generation(&mut storage, storage_generation.get());
                    for generation in data_generation.stripes() {
                        update_generation(&mut data, generation.get());
                    }
                    for generation in [existence_generation.get(), storage_generation.get()] {
                        record_generation(
                            &mut generations,
                            &mut complete_generation_vector,
                            generation,
                        );
                    }
                    for generation in data_generation.stripes() {
                        record_generation(
                            &mut generations,
                            &mut complete_generation_vector,
                            generation.get(),
                        );
                    }
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

fn record_generation(generations: &mut SmallVec<[u64; 12]>, complete: &mut bool, generation: u64) {
    if generations.len() < GENERATION_VALUE_LIMIT {
        generations.push(generation);
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
    superseded_reuse: SmallVec<[u64; COHORT_LIMIT_PER_EXACT]>,
    evidence_evictions: SmallVec<[EvidenceEviction; 2]>,
}

#[derive(Clone, Copy)]
enum SuccessSource {
    Fill,
    CacheHit,
    Coalesced,
}

#[derive(Clone, Copy, Default)]
struct SuccessEvents {
    reuse_opportunity: bool,
    rejected_reuse: bool,
    promoted_on_reuse: bool,
}

struct FillEvents {
    decision: ShadowDecision,
    success: SuccessEvents,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct PolicyStats {
    pub exact_entries: usize,
    pub cohorts: usize,
    pub requests: u64,
    pub reuse_opportunities: u64,
    pub cache_hits: u64,
    pub coalesced_reuses: u64,
    pub fills: u64,
    pub completed_cohorts: u64,
    pub zero_reuse_cohorts: u64,
    pub completed_reuse_opportunities: u64,
    pub admit_second_touch: u64,
    pub admit_expensive_probation: u64,
    pub admit_learned_value: u64,
    pub reject_too_large: u64,
    pub reject_no_reuse_history: u64,
    pub reject_insufficient_value: u64,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::engine::catalog::model::{CatalogDependencies, TableExistenceDependency};
    use crate::engine::catalog::store::TableDataGeneration;
    use crate::engine::lir::fingerprint::Fingerprint;

    use super::*;

    fn fingerprint(seed: u8) -> Fingerprint {
        Fingerprint {
            canonicalization_version: 1,
            hash_algorithm: 1,
            digest: [seed; 16],
        }
    }

    fn key(seed: u8, data_generation: u64) -> RelationCacheKey {
        let mut dependencies = CatalogDependencies::default();
        dependencies.table_existence.push(TableExistenceDependency {
            table_id: "t1".into(),
            table_name: "items".into(),
            generation: 2.into(),
            storage_generation: 3.into(),
        });
        RelationCacheKey::from_generations(
            fingerprint(seed),
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

    #[test]
    fn second_touch_overrides_a_low_first_fill_score() {
        let policy = RelationCachePolicy::new(16);
        let cache_key = key(1, 1);
        let first = policy.observe_request(&cache_key);
        policy.observe_fill(first, cheap_work(), 1024, 2048, usize::MAX);
        let second = policy.observe_request(&cache_key);
        policy.observe_cache_hit(second);

        let stats = policy.stats();
        assert_eq!(stats.requests, 2);
        assert_eq!(stats.reuse_opportunities, 1);
        assert_eq!(stats.fills, 1);
        assert_eq!(stats.admit_second_touch, 1);
    }

    #[test]
    fn expensive_dense_first_fill_receives_probation() {
        let policy = RelationCachePolicy::new(16);
        let cache_key = key(1, 1);
        let token = policy.observe_request(&cache_key);
        policy.observe_fill(token, expensive_work(), 64 * 1024, 65 * 1024, usize::MAX);

        assert_eq!(policy.stats().admit_expensive_probation, 1);
    }

    #[test]
    fn repeated_zero_reuse_cohorts_suppress_first_touch_probation() {
        let policy = RelationCachePolicy::new(16);
        for generation in 1..=4 {
            let cache_key = key(1, generation);
            let token = policy.observe_request(&cache_key);
            policy.observe_fill(token, expensive_work(), 64 * 1024, 65 * 1024, usize::MAX);
        }

        let stats = policy.stats();
        assert_eq!(stats.completed_cohorts, 3);
        assert_eq!(stats.zero_reuse_cohorts, 3);
        assert_eq!(stats.reject_no_reuse_history, 1);
    }

    #[test]
    fn completed_cohort_reuse_can_admit_a_cheaper_first_touch() {
        let policy = RelationCachePolicy::new(16);
        for generation in 1..=3 {
            let cache_key = key(1, generation);
            let token = policy.observe_request(&cache_key);
            policy.observe_fill(token, cheap_work(), 64 * 1024, 65 * 1024, usize::MAX);
            let second = policy.observe_request(&cache_key);
            policy.observe_cache_hit(second);
            let third = policy.observe_request(&cache_key);
            policy.observe_cache_hit(third);
        }
        let next = key(1, 4);
        let token = policy.observe_request(&next);
        let learned_work = CachedWork {
            kv: crate::engine::exec::observe::KvWork {
                bytes_read: 128 * 1024,
                ..Default::default()
            },
            ..Default::default()
        };
        policy.observe_fill(token, learned_work, 64 * 1024, 65 * 1024, usize::MAX);

        let stats = policy.stats();
        assert_eq!(stats.completed_cohorts, 3);
        assert_eq!(stats.completed_reuse_opportunities, 6);
        assert_eq!(stats.admit_learned_value, 1);
    }

    #[test]
    fn an_old_pinned_cohort_does_not_create_a_new_transition() {
        let policy = RelationCachePolicy::new(16);
        let old = key(1, 100);
        let current = key(1, 101);
        let old_fill = policy.observe_request(&old);
        policy.observe_fill(old_fill, cheap_work(), 1024, 2048, usize::MAX);
        let current_fill = policy.observe_request(&current);
        policy.observe_fill(current_fill, cheap_work(), 1024, 2048, usize::MAX);
        let old_hit = policy.observe_request(&old);
        policy.observe_cache_hit(old_hit);

        let stats = policy.stats();
        assert_eq!(stats.cohorts, 2);
        assert_eq!(stats.completed_cohorts, 1);
        assert_eq!(stats.requests, 3);
        assert_eq!(stats.reuse_opportunities, 1);
    }

    #[test]
    fn evidence_has_exact_and_cohort_bounds() {
        let policy = RelationCachePolicy::new(2);
        for seed in 1..=8 {
            policy.observe_request(&key(seed, 1));
        }
        assert!(policy.stats().exact_entries <= 2);

        let policy = RelationCachePolicy::new(2);
        for generation in 1..=8 {
            policy.observe_request(&key(1, generation));
        }
        assert!(policy.stats().cohorts <= COHORT_LIMIT_PER_EXACT);
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
        let cache_key = RelationCacheKey::from_key_parts(fingerprint(1), dependencies);
        let profile = DependencyProfile::new(&cache_key);

        assert_eq!(profile.generations.len(), GENERATION_VALUE_LIMIT);
        assert!(!profile.complete_generation_vector);
    }

    #[test]
    fn dependency_profiles_classify_transition_causes() {
        fn profile(dependency: DependencyGeneration) -> DependencyProfile {
            DependencyProfile::new(&RelationCacheKey::from_key_parts(
                fingerprint(1),
                vec![dependency],
            ))
        }

        let table = |existence: u64, storage: u64, data: u64| {
            profile(DependencyGeneration::Table {
                table_id: "t1".into(),
                existence_generation: existence.into(),
                storage_generation: storage.into(),
                data_generation: data.into(),
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
                vec![
                    DependencyGeneration::Table {
                        table_id: "t1".into(),
                        existence_generation: 1.into(),
                        storage_generation: 1.into(),
                        data_generation: first.into(),
                    },
                    DependencyGeneration::Table {
                        table_id: "t2".into(),
                        existence_generation: 1.into(),
                        storage_generation: 1.into(),
                        data_generation: second.into(),
                    },
                ],
            )
        }

        let policy = RelationCachePolicy::new(16);
        let first = mixed_key(1, 2);
        let token = policy.observe_request(&first);
        policy.observe_fill(token, cheap_work(), 1024, 2048, usize::MAX);
        let second = mixed_key(2, 1);
        let token = policy.observe_request(&second);
        policy.observe_fill(token, cheap_work(), 1024, 2048, usize::MAX);

        let stats = policy.stats();
        assert_eq!(stats.cohorts, 2);
        assert_eq!(stats.completed_cohorts, 0);
    }
}
