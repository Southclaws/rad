//! Bounded materialized relation cache.
//!
//! Cache identity is a correctness proof. It combines the exact bound LIR
//! fingerprint with all catalog, storage, access, and data generations that
//! can affect the relation value. The key does not use a database-wide data
//! position. A commit to an unrelated table therefore does not change the
//! key. Entries for different visible generations can remain resident at the
//! same time. Eviction removes them by cache policy; mutation does not remove
//! them directly.
//!
//! Every persistent read added to LIR must also add its complete catalog
//! dependency set. An omitted dependency can permit a stale cache hit. Every
//! logical row mutation must advance its table data generation in the same
//! transaction as the row writes. These two rules are required for cache
//! correctness.

mod policy;
mod prepared_read;
mod snapshot_catalog;

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use std::{fmt, str::FromStr};

use foyer::{Cache, CacheProperties, Event, EventListener, LfuConfig};
use sha2::{Digest as _, Sha256};
use smallvec::SmallVec;
use tokio::sync::watch;

use crate::engine::catalog::identity::{
    AccessGeneration, ColumnId, ExistenceGeneration, IndexId, StorageGeneration, TableId,
    ValueGeneration, WriteProtocolGeneration,
};
use crate::engine::catalog::model::CatalogDependencies;
use crate::engine::catalog::store;
use crate::engine::catalog::store::TableDataGeneration;
use crate::engine::kv::{DataPosition, KvView};
use crate::engine::lir::eval::Env;
use crate::engine::lir::fingerprint::Fingerprint;
use crate::engine::lir::{Datum, ObjectField, RootCardinality, RowType, Value};
use crate::engine::planner::physical::{RelationDataDependencies, TableDataDependencyScope};
use crate::runtime::{RuntimeEffects, SystemRuntime};

use super::observe::KvWork;
use super::observe::StatementSource;
use super::{
    EngineEvent, Error, ErrorKind, ErrorReason, RelationCacheAdmissionResult,
    RelationCacheEvictionCause, RelationCacheMaterialization, Result,
};
pub use policy::{
    AdmissionDecision, AdmissionOutcome, AdmissionReason, EvidenceSource,
    RelationCacheCohortStatistics, RelationCachePolicyCounters, RelationCachePolicyStatistics,
    RelationCacheQuantiles,
};
use policy::{CohortToken, RelationCachePolicy};
use prepared_read::PreparedReadCache;
pub(in crate::engine::exec) use prepared_read::{PreparedReadRequest, PreparedReadResult};
use snapshot_catalog::SnapshotCatalogCache;

const DEFAULT_BYTE_LIMIT: usize = 128 * 1024 * 1024;
const DEFAULT_ENTRY_LIMIT: usize = 4_096;
const DEFAULT_RESULT_BYTE_LIMIT: usize = 8 * 1024 * 1024;
const DEFAULT_PLAN_MATERIALIZATION_BUDGET_BYTES: u64 = 8 * 1024 * 1024;
const CACHE_SHARDS: usize = 8;
const ACCOUNTING_PENDING: u8 = 0;
const ACCOUNTING_RETAINED: u8 = 1;
const ACCOUNTING_REMOVED: u8 = 2;
const QUERY_VALIDATOR_DOMAIN: &[u8] = b"rad-query-validator";
const QUERY_VALIDATOR_DEPENDENCY_DOMAIN: &[u8] = b"rad-query-validator-dependencies";
const QUERY_RESULT_REPRESENTATION: &[u8] = b"application/json;rad-datum-v1";

pub(super) type SemanticCacheEventBatch = Vec<EngineEvent>;

pub(super) async fn reach_semantic_events(
    hook: Option<&dyn super::EngineEventHook>,
    events: SemanticCacheEventBatch,
) {
    let Some(hook) = hook else {
        return;
    };
    for event in events {
        hook.reach(event).await;
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct QueryValidator([u8; 32]);

impl QueryValidator {
    fn dependency_digest(dependencies: &[DependencyGeneration]) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash_bytes(&mut hash, 1, QUERY_VALIDATOR_DEPENDENCY_DOMAIN);
        hash_u64(&mut hash, 2, dependencies.len() as u64);
        for dependency in dependencies {
            dependency.hash_query_validator(&mut hash);
        }
        hash.finalize().into()
    }

    fn from_key_parts(exact: Fingerprint, dependency_digest: [u8; 32]) -> QueryValidator {
        // The encoding has explicit tags, lengths, and big-endian integers.
        // Rust layout and Hash implementations are not stable protocol inputs.
        let mut hash = Sha256::new();
        hash_bytes(&mut hash, 1, QUERY_VALIDATOR_DOMAIN);
        hash_bytes(&mut hash, 2, &exact.to_bytes());
        hash_bytes(&mut hash, 3, &dependency_digest);
        hash_bytes(&mut hash, 4, QUERY_RESULT_REPRESENTATION);
        QueryValidator(hash.finalize().into())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RelationCacheLimits {
    pub byte_limit: usize,
    pub entry_limit: usize,
    pub result_byte_limit: usize,
}

impl Default for RelationCacheLimits {
    fn default() -> Self {
        Self {
            byte_limit: DEFAULT_BYTE_LIMIT,
            entry_limit: DEFAULT_ENTRY_LIMIT,
            result_byte_limit: DEFAULT_RESULT_BYTE_LIMIT,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RelationCachePolicyMode {
    Foyer,
    Shadow,
    #[default]
    Enforced,
}

impl RelationCachePolicyMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Foyer => "foyer",
            Self::Shadow => "shadow",
            Self::Enforced => "enforced",
        }
    }
}

impl FromStr for RelationCachePolicyMode {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "foyer" => Ok(Self::Foyer),
            "shadow" => Ok(Self::Shadow),
            "enforced" => Ok(Self::Enforced),
            _ => Err(format!(
                "unknown relation cache policy {value:?} (foyer, shadow, or enforced)"
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RelationCacheReuseAdmission {
    SecondTouch,
    ThirdTouch,
    ValueDensity,
    #[default]
    FamilyConversion,
}

impl RelationCacheReuseAdmission {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SecondTouch => "second_touch",
            Self::ThirdTouch => "third_touch",
            Self::ValueDensity => "value_density",
            Self::FamilyConversion => "family_conversion",
        }
    }
}

impl FromStr for RelationCacheReuseAdmission {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "second-touch" => Ok(Self::SecondTouch),
            "third-touch" => Ok(Self::ThirdTouch),
            "value-density" => Ok(Self::ValueDensity),
            "family-conversion" => Ok(Self::FamilyConversion),
            _ => Err(format!(
                "unknown relation cache reuse admission {value:?} (second-touch, third-touch, value-density, or family-conversion)"
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RelationCachePrior {
    #[default]
    None,
    GenerationRate,
}

impl RelationCachePrior {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::GenerationRate => "generation_rate",
        }
    }
}

impl FromStr for RelationCachePrior {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "none" => Ok(Self::None),
            "generation-rate" => Ok(Self::GenerationRate),
            _ => Err(format!(
                "unknown relation cache prior {value:?} (none or generation-rate)"
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RelationCacheDomains(u8);

impl RelationCacheDomains {
    const QUERY: u8 = 1;
    const HASH_BUILD: u8 = 2;
    const GROUPED_DIMENSION: u8 = 4;

    pub const fn none() -> Self {
        Self(0)
    }

    pub const fn all() -> Self {
        Self(Self::QUERY | Self::HASH_BUILD | Self::GROUPED_DIMENSION)
    }

    pub const fn query_enabled(self) -> bool {
        self.0 & Self::QUERY != 0
    }

    pub const fn hash_build_enabled(self) -> bool {
        self.0 & Self::HASH_BUILD != 0
    }

    pub const fn grouped_dimension_enabled(self) -> bool {
        self.0 & Self::GROUPED_DIMENSION != 0
    }

    const fn contains(self, domain: MaterializationDomain) -> bool {
        match domain {
            MaterializationDomain::QueryResult => self.query_enabled(),
            MaterializationDomain::HashJoinBuild => self.hash_build_enabled(),
            MaterializationDomain::GroupedHashJoinDimension => self.grouped_dimension_enabled(),
            MaterializationDomain::SubrelationRowsV1 => false,
        }
    }
}

impl Default for RelationCacheDomains {
    fn default() -> Self {
        Self::all()
    }
}

impl fmt::Display for RelationCacheDomains {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if *self == Self::none() {
            return formatter.write_str("none");
        }
        let mut separator = "";
        for (enabled, name) in [
            (self.query_enabled(), "query"),
            (self.hash_build_enabled(), "hash-build"),
            (self.grouped_dimension_enabled(), "grouped-dimension"),
        ] {
            if enabled {
                formatter.write_str(separator)?;
                formatter.write_str(name)?;
                separator = ",";
            }
        }
        Ok(())
    }
}

impl FromStr for RelationCacheDomains {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if value == "none" {
            return Ok(Self::none());
        }
        let mut domains = Self::none();
        for domain in value.split(',') {
            match domain {
                "query" => domains.0 |= Self::QUERY,
                "hash-build" => domains.0 |= Self::HASH_BUILD,
                "grouped-dimension" => domains.0 |= Self::GROUPED_DIMENSION,
                "" => return Err("relation cache domains must not contain an empty value".into()),
                _ => {
                    return Err(format!(
                        "unknown relation cache domain {domain:?} (query, hash-build, grouped-dimension, or none)"
                    ));
                }
            }
        }
        Ok(domains)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RelationCachePolicyConfig {
    pub mode: RelationCachePolicyMode,
    pub reuse_admission: RelationCacheReuseAdmission,
    pub family_minimum_observations: usize,
    pub minimum_completed_cohorts: usize,
    pub zero_reuse_percent: u8,
    pub probation_minimum_work_units: u64,
    pub probation_minimum_work_per_byte: u64,
    pub cohorts_per_exact_relation: usize,
    pub prior: RelationCachePrior,
    pub rate_half_life: Duration,
}

impl Default for RelationCachePolicyConfig {
    fn default() -> Self {
        Self {
            mode: RelationCachePolicyMode::Enforced,
            reuse_admission: RelationCacheReuseAdmission::FamilyConversion,
            family_minimum_observations: 1,
            minimum_completed_cohorts: 3,
            zero_reuse_percent: 75,
            probation_minimum_work_units: 4 * 1024 * 1024,
            probation_minimum_work_per_byte: 4,
            cohorts_per_exact_relation: 4,
            prior: RelationCachePrior::None,
            rate_half_life: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RelationCacheConfig {
    pub limits: RelationCacheLimits,
    pub policy: RelationCachePolicyConfig,
    pub domains: RelationCacheDomains,
    pub plan_materialization_budget_bytes: u64,
}

impl Default for RelationCacheConfig {
    fn default() -> Self {
        Self {
            limits: RelationCacheLimits::default(),
            policy: RelationCachePolicyConfig::default(),
            domains: RelationCacheDomains::default(),
            plan_materialization_budget_bytes: DEFAULT_PLAN_MATERIALIZATION_BUDGET_BYTES,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum DependencyValidation {
    /// The caller cannot write through this transaction. A stable storage
    /// position can identify a completed dependency validation.
    Snapshot,
    /// The caller can add writes after this read. Each dependency fence must
    /// enter the transaction read set even when the relation result is cached.
    Transaction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CatalogDependencyAdmission {
    _private: (),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AdmittedRelationCacheKey {
    key: RelationCacheKey,
    admission: CatalogDependencyAdmission,
}

impl AdmittedRelationCacheKey {
    pub(super) fn key(&self) -> &RelationCacheKey {
        &self.key
    }

    pub(super) fn into_parts(self) -> (RelationCacheKey, CatalogDependencyAdmission) {
        (self.key, self.admission)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum CatalogDependencyGeneration {
    Table {
        table_id: TableId,
        existence_generation: ExistenceGeneration,
        storage_generation: StorageGeneration,
    },
    Column {
        table_id: TableId,
        column_id: ColumnId,
        generation: ValueGeneration,
    },
    Index {
        table_id: TableId,
        index_id: IndexId,
        generation: AccessGeneration,
    },
    WriteProtocol {
        table_id: TableId,
        generation: WriteProtocolGeneration,
    },
}

impl CatalogDependencyGeneration {
    fn collect(dependencies: &CatalogDependencies) -> Vec<Self> {
        let mut generations = Vec::with_capacity(
            dependencies.table_existence.len()
                + dependencies.column_values.len()
                + dependencies.index_access.len()
                + dependencies.write_protocols.len(),
        );
        generations.extend(
            dependencies
                .table_existence
                .iter()
                .map(|dependency| Self::Table {
                    table_id: dependency.table_id.clone(),
                    existence_generation: dependency.generation,
                    storage_generation: dependency.storage_generation,
                }),
        );
        generations.extend(
            dependencies
                .column_values
                .iter()
                .map(|dependency| Self::Column {
                    table_id: dependency.table_id.clone(),
                    column_id: dependency.column_id.clone(),
                    generation: dependency.generation,
                }),
        );
        generations.extend(
            dependencies
                .index_access
                .iter()
                .map(|dependency| Self::Index {
                    table_id: dependency.table_id.clone(),
                    index_id: dependency.index_id.clone(),
                    generation: dependency.generation,
                }),
        );
        generations.extend(dependencies.write_protocols.iter().map(|dependency| {
            Self::WriteProtocol {
                table_id: dependency.table_id.clone(),
                generation: dependency.generation,
            }
        }));
        generations.sort_unstable();
        generations.dedup();
        generations
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SnapshotDependencyKey {
    // The position is only a scope for dependency validation. It is not part
    // of RelationCacheKey, so unrelated commits do not change result identity.
    position: DataPosition,
    // Include the complete physical dependency set so one plan cannot use
    // validation completed for another plan. Exact relations with this same
    // set can share the validation because their result identity is added
    // after the dependency generations are read.
    dependencies: Vec<CatalogDependencyGeneration>,
    data_dependencies: ResolvedDataDependencies,
}

impl SnapshotDependencyKey {
    fn new(
        position: DataPosition,
        dependencies: &CatalogDependencies,
        data_dependencies: ResolvedDataDependencies,
    ) -> Self {
        Self {
            position,
            dependencies: CatalogDependencyGeneration::collect(dependencies),
            data_dependencies,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ResolvedDataDependencies(SmallVec<[ResolvedTableDataDependency; 4]>);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct ResolvedTableDataDependency {
    table_id: TableId,
    scope: ResolvedTableDataDependencyScope,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum ResolvedTableDataDependencyScope {
    All,
    Stripes(SmallVec<[u8; 4]>),
}

impl ResolvedDataDependencies {
    fn resolve(dependencies: &RelationDataDependencies) -> Result<Self> {
        let mut resolved =
            SmallVec::<[ResolvedTableDataDependency; 4]>::with_capacity(dependencies.tables.len());
        for dependency in &dependencies.tables {
            let scope = match &dependency.scope {
                TableDataDependencyScope::All => ResolvedTableDataDependencyScope::All,
                TableDataDependencyScope::PrimaryKeys(keys) => {
                    if keys.is_empty() {
                        return Err(Error::message(
                            ErrorKind::Internal,
                            "exec: primary-key data dependency is empty",
                        ));
                    }
                    let mut stripes = SmallVec::<[u8; 4]>::with_capacity(keys.len());
                    for key in keys {
                        let encoded = super::codec::encode_tuple(key)?;
                        let stripe = store::data_generation_stripe(&encoded);
                        stripes.push(
                            u8::try_from(stripe).expect("data generation stripe fits in one byte"),
                        );
                    }
                    stripes.sort_unstable();
                    stripes.dedup();
                    ResolvedTableDataDependencyScope::Stripes(stripes)
                }
            };
            resolved.push(ResolvedTableDataDependency {
                table_id: dependency.table_id.clone(),
                scope,
            });
        }
        resolved.sort_unstable();
        if resolved
            .windows(2)
            .any(|pair| pair[0].table_id == pair[1].table_id)
        {
            return Err(Error::message(
                ErrorKind::Internal,
                "exec: physical plan has duplicate table data dependencies",
            ));
        }
        Ok(Self(resolved))
    }

    fn for_table(&self, table_id: &TableId) -> Option<&ResolvedTableDataDependencyScope> {
        self.0
            .iter()
            .find(|dependency| &dependency.table_id == table_id)
            .map(|dependency| &dependency.scope)
    }
}

#[derive(Default)]
pub(super) struct TransactionDependencyCache {
    entries: Mutex<HashMap<SnapshotDependencyKey, Arc<ValidatedDependencies>>>,
}

impl TransactionDependencyCache {
    pub(super) fn clear(&self) {
        self.entries
            .lock()
            .expect("transaction dependency cache lock poisoned")
            .clear();
    }

    fn get(&self, key: &SnapshotDependencyKey) -> Option<Arc<ValidatedDependencies>> {
        self.entries
            .lock()
            .expect("transaction dependency cache lock poisoned")
            .get(key)
            .cloned()
    }

    fn insert(&self, key: SnapshotDependencyKey, value: Arc<ValidatedDependencies>) {
        self.entries
            .lock()
            .expect("transaction dependency cache lock poisoned")
            .insert(key, value);
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum DependencyGeneration {
    Table {
        table_id: TableId,
        existence_generation: ExistenceGeneration,
        storage_generation: StorageGeneration,
        data_generation: Arc<TableDataGeneration>,
    },
    Column {
        table_id: TableId,
        column_id: ColumnId,
        generation: ValueGeneration,
    },
    Index {
        table_id: TableId,
        index_id: IndexId,
        generation: AccessGeneration,
    },
    WriteProtocol {
        table_id: TableId,
        generation: WriteProtocolGeneration,
    },
}

#[derive(Debug)]
struct ValidatedDependencies {
    generations: Arc<[DependencyGeneration]>,
    query_validator_digest: [u8; 32],
}

impl ValidatedDependencies {
    fn new(generations: Arc<[DependencyGeneration]>) -> Self {
        let query_validator_digest = QueryValidator::dependency_digest(&generations);
        Self {
            generations,
            query_validator_digest,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct RelationCacheKey {
    domain: MaterializationDomain,
    exact: Fingerprint,
    family: Fingerprint,
    representation: SmallVec<[u8; 64]>,
    dependencies: Arc<[DependencyGeneration]>,
    // Query result keys store the derived validator so a conditional hit does
    // not repeat the canonical SHA-256 calculation.
    query_validator: Option<QueryValidator>,
}

pub(super) struct SubrelationCacheContext<'a> {
    pub cache: &'a RelationCache,
    pub root_key: RelationCacheKey,
    pub counters: Option<&'a super::observe::KvCounters>,
    events: Option<&'a dyn super::EngineEventHook>,
}

impl<'a> SubrelationCacheContext<'a> {
    pub(super) fn new(
        cache: &'a RelationCache,
        root_key: RelationCacheKey,
        counters: Option<&'a super::observe::KvCounters>,
        events: Option<&'a dyn super::EngineEventHook>,
    ) -> Self {
        Self {
            cache,
            root_key,
            counters,
            events,
        }
    }

    pub(super) async fn reach(&self, event: EngineEvent) {
        if let Some(events) = self.events {
            events.reach(event).await;
        }
    }

    pub(super) async fn reach_all(&self, events: SemanticCacheEventBatch) {
        reach_semantic_events(self.events, events).await;
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) enum MaterializationDomain {
    QueryResult,
    SubrelationRowsV1,
    HashJoinBuild,
    GroupedHashJoinDimension,
}

impl MaterializationDomain {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::QueryResult => "query_result",
            Self::SubrelationRowsV1 => "subrelation_rows",
            Self::HashJoinBuild => "hash_join_build",
            Self::GroupedHashJoinDimension => "grouped_hash_join_dimension",
        }
    }

    const fn is_query_result(self) -> bool {
        matches!(self, Self::QueryResult)
    }

    const fn event_materialization(self) -> RelationCacheMaterialization {
        match self {
            Self::QueryResult => RelationCacheMaterialization::QueryResult,
            Self::SubrelationRowsV1 => RelationCacheMaterialization::Rows,
            Self::HashJoinBuild => RelationCacheMaterialization::HashJoinBuild,
            Self::GroupedHashJoinDimension => {
                RelationCacheMaterialization::GroupedHashJoinDimension
            }
        }
    }

    fn lookup(self, result: &'static str) {
        crate::telemetry::relation_cache_materialization_lookup(self.as_str(), result);
    }

    fn admission(self, result: &'static str) {
        crate::telemetry::relation_cache_materialization_admission(self.as_str(), result);
    }

    fn eviction(self, cause: &'static str) {
        crate::telemetry::relation_cache_materialization_eviction(self.as_str(), cause);
    }

    fn avoided(self, work: CachedWork) {
        let reads = work.kv.gets.saturating_add(work.kv.iterated);
        crate::telemetry::relation_cache_materialization_avoided(
            self.as_str(),
            reads,
            work.kv.bytes_read,
            work.execution,
        );
    }
}

impl RelationCacheKey {
    /// Read data generations through the relation's pinned view. Reading a
    /// latest value outside this view can give an old transaction a key for
    /// data that it cannot observe.
    async fn for_view(
        exact: Fingerprint,
        family: Fingerprint,
        view: &dyn KvView,
        dependencies: &CatalogDependencies,
        data_dependencies: &ResolvedDataDependencies,
    ) -> Result<Self> {
        let dependencies =
            Self::read_dependencies_for_view(view, dependencies, data_dependencies).await?;
        Ok(Self::from_validated_dependencies(
            exact,
            family,
            dependencies,
        ))
    }

    async fn read_dependencies_for_view(
        view: &dyn KvView,
        dependencies: &CatalogDependencies,
        data_dependencies: &ResolvedDataDependencies,
    ) -> Result<Arc<ValidatedDependencies>> {
        // A cache hit skips the executor and its dependency admission. Read
        // all catalog fences here so the hit has the same conflict protection
        // as execution. This is important when an explicit transaction reads
        // from the cache and then writes before commit.
        store::admit_catalog_dependencies(view, dependencies).await?;
        let mut generations = Vec::with_capacity(Self::dependency_count(dependencies));
        for dependency in &dependencies.table_existence {
            let scope = data_dependencies
                .for_table(&dependency.table_id)
                .ok_or_else(missing_table_data_dependency)?;
            let data_generation = match scope {
                ResolvedTableDataDependencyScope::All => {
                    store::read_table_data_generation(view, &dependency.table_id).await?
                }
                ResolvedTableDataDependencyScope::Stripes(stripes) => {
                    store::read_table_data_generation_stripes(view, &dependency.table_id, stripes)
                        .await?
                }
            };
            generations.push(DependencyGeneration::Table {
                table_id: dependency.table_id.clone(),
                existence_generation: dependency.generation,
                storage_generation: dependency.storage_generation,
                data_generation: Arc::new(data_generation),
            });
        }
        Self::extend_non_table_dependencies(&mut generations, dependencies);
        Ok(Arc::new(ValidatedDependencies::new(
            Self::canonical_dependencies(generations),
        )))
    }

    #[cfg(test)]
    fn from_generations(
        exact: Fingerprint,
        family: Fingerprint,
        dependencies: &CatalogDependencies,
        data_generations: &HashMap<TableId, TableDataGeneration>,
    ) -> Self {
        let mut generations = Vec::with_capacity(Self::dependency_count(dependencies));
        generations.extend(dependencies.table_existence.iter().map(|dependency| {
            DependencyGeneration::Table {
                table_id: dependency.table_id.clone(),
                existence_generation: dependency.generation,
                storage_generation: dependency.storage_generation,
                data_generation: Arc::new(
                    data_generations
                        .get(&dependency.table_id)
                        .expect("table data generation is present")
                        .clone(),
                ),
            }
        }));
        Self::extend_non_table_dependencies(&mut generations, dependencies);
        let dependencies = Arc::new(ValidatedDependencies::new(Self::canonical_dependencies(
            generations,
        )));
        Self::from_validated_dependencies(exact, family, dependencies)
    }

    fn dependency_count(dependencies: &CatalogDependencies) -> usize {
        dependencies
            .table_existence
            .len()
            .saturating_add(dependencies.column_values.len())
            .saturating_add(dependencies.index_access.len())
            .saturating_add(dependencies.write_protocols.len())
    }

    fn extend_non_table_dependencies(
        generations: &mut Vec<DependencyGeneration>,
        dependencies: &CatalogDependencies,
    ) {
        generations.extend(dependencies.column_values.iter().map(|dependency| {
            DependencyGeneration::Column {
                table_id: dependency.table_id.clone(),
                column_id: dependency.column_id.clone(),
                generation: dependency.generation,
            }
        }));
        generations.extend(dependencies.index_access.iter().map(|dependency| {
            DependencyGeneration::Index {
                table_id: dependency.table_id.clone(),
                index_id: dependency.index_id.clone(),
                generation: dependency.generation,
            }
        }));
        generations.extend(dependencies.write_protocols.iter().map(|dependency| {
            DependencyGeneration::WriteProtocol {
                table_id: dependency.table_id.clone(),
                generation: dependency.generation,
            }
        }));
    }

    #[cfg(test)]
    fn from_key_parts(
        exact: Fingerprint,
        family: Fingerprint,
        dependencies: Vec<DependencyGeneration>,
    ) -> Self {
        let dependencies = Arc::new(ValidatedDependencies::new(Self::canonical_dependencies(
            dependencies,
        )));
        Self::from_validated_dependencies(exact, family, dependencies)
    }

    fn from_domain_key_parts(
        domain: MaterializationDomain,
        exact: Fingerprint,
        family: Fingerprint,
        dependencies: Vec<DependencyGeneration>,
    ) -> Self {
        let dependencies = Self::canonical_dependencies(dependencies);
        let query_validator = (domain == MaterializationDomain::QueryResult).then(|| {
            QueryValidator::from_key_parts(exact, QueryValidator::dependency_digest(&dependencies))
        });
        Self {
            domain,
            exact,
            family,
            representation: SmallVec::new(),
            dependencies,
            query_validator,
        }
    }

    fn canonical_dependencies(
        mut dependencies: Vec<DependencyGeneration>,
    ) -> Arc<[DependencyGeneration]> {
        // Catalog dependency collection order is not part of relation
        // identity. Canonical order also removes duplicate dependency records.
        dependencies.sort_unstable();
        dependencies.dedup();
        dependencies.into()
    }

    fn from_validated_dependencies(
        exact: Fingerprint,
        family: Fingerprint,
        dependencies: Arc<ValidatedDependencies>,
    ) -> Self {
        let query_validator =
            QueryValidator::from_key_parts(exact, dependencies.query_validator_digest);
        Self {
            domain: MaterializationDomain::QueryResult,
            exact,
            family,
            representation: SmallVec::new(),
            dependencies: dependencies.generations.clone(),
            query_validator: Some(query_validator),
        }
    }

    pub(super) fn for_subrelation(
        &self,
        exact: Fingerprint,
        family: Fingerprint,
        dependencies: &CatalogDependencies,
        data_dependencies: &RelationDataDependencies,
    ) -> Result<Self> {
        // Each selected record must match both physical identity and catalog
        // generation. Matching only the table, column, or index identity can
        // combine a prepared subtree with a root key from another catalog
        // state. A missing record is an invalid physical plan invariant.
        let mut selected = Vec::with_capacity(
            dependencies.table_existence.len()
                + dependencies.column_values.len()
                + dependencies.index_access.len()
                + dependencies.write_protocols.len(),
        );
        let resolved_data_dependencies = ResolvedDataDependencies::resolve(data_dependencies)?;
        for dependency in &dependencies.table_existence {
            let scope = resolved_data_dependencies
                .for_table(&dependency.table_id)
                .ok_or_else(missing_table_data_dependency)?;
            let generation = self
                .dependencies
                .iter()
                .find_map(|generation| match generation {
                    DependencyGeneration::Table {
                        table_id,
                        existence_generation,
                        storage_generation,
                        data_generation,
                    } if table_id == &dependency.table_id
                        && existence_generation == &dependency.generation
                        && storage_generation == &dependency.storage_generation =>
                    {
                        Some(data_generation)
                    }
                    _ => None,
                })
                .ok_or_else(missing_subrelation_dependency)?;
            let data_generation = match scope {
                ResolvedTableDataDependencyScope::All => generation.as_ref().clone(),
                ResolvedTableDataDependencyScope::Stripes(stripes) => generation
                    .project(stripes.iter().map(|stripe| usize::from(*stripe)))
                    .ok_or_else(missing_subrelation_dependency)?,
            };
            selected.push(DependencyGeneration::Table {
                table_id: dependency.table_id.clone(),
                existence_generation: dependency.generation,
                storage_generation: dependency.storage_generation,
                data_generation: Arc::new(data_generation),
            });
        }
        for dependency in &dependencies.column_values {
            selected.push(
                self.dependencies
                    .iter()
                    .find(|generation| {
                        matches!(
                            generation,
                            DependencyGeneration::Column {
                                table_id,
                                column_id,
                                generation,
                            } if table_id == &dependency.table_id
                                && column_id == &dependency.column_id
                                && generation == &dependency.generation
                        )
                    })
                    .cloned()
                    .ok_or_else(missing_subrelation_dependency)?,
            );
        }
        for dependency in &dependencies.index_access {
            selected.push(
                self.dependencies
                    .iter()
                    .find(|generation| {
                        matches!(
                            generation,
                            DependencyGeneration::Index {
                                table_id,
                                index_id,
                                generation,
                            } if table_id == &dependency.table_id
                                && index_id == &dependency.index_id
                                && generation == &dependency.generation
                        )
                    })
                    .cloned()
                    .ok_or_else(missing_subrelation_dependency)?,
            );
        }
        for dependency in &dependencies.write_protocols {
            selected.push(
                self.dependencies
                    .iter()
                    .find(|generation| {
                        matches!(
                            generation,
                            DependencyGeneration::WriteProtocol {
                                table_id,
                                generation,
                            } if table_id == &dependency.table_id
                                && generation == &dependency.generation
                        )
                    })
                    .cloned()
                    .ok_or_else(missing_subrelation_dependency)?,
            );
        }
        Ok(Self::from_domain_key_parts(
            MaterializationDomain::SubrelationRowsV1,
            exact,
            family,
            selected,
        ))
    }

    pub(super) fn for_physical_subrelation(
        &self,
        domain: MaterializationDomain,
        exact: Fingerprint,
        family: Fingerprint,
        dependencies: &CatalogDependencies,
        data_dependencies: &RelationDataDependencies,
        representation: impl Into<SmallVec<[u8; 64]>>,
    ) -> Result<Self> {
        if domain.is_query_result() || domain == MaterializationDomain::SubrelationRowsV1 {
            return Err(Error::message(
                ErrorKind::Internal,
                "exec: physical materialization requires a physical cache domain",
            ));
        }
        let mut key = self.for_subrelation(exact, family, dependencies, data_dependencies)?;
        key.domain = domain;
        key.representation = representation.into();
        Ok(key)
    }

    fn retained_bytes(&self) -> usize {
        let representation_bytes = if self.representation.spilled() {
            self.representation.capacity()
        } else {
            0
        };
        size_of::<Self>()
            .saturating_add(representation_bytes)
            .saturating_add(
                self.dependencies
                    .len()
                    .saturating_mul(size_of::<DependencyGeneration>()),
            )
            .saturating_add(
                self.dependencies
                    .iter()
                    .map(DependencyGeneration::dynamic_bytes)
                    .fold(0usize, usize::saturating_add),
            )
    }

    pub(super) fn query_validator(&self) -> QueryValidator {
        self.query_validator
            .expect("query result key has an HTTP validator")
    }
}

fn missing_subrelation_dependency() -> Error {
    Error::message(
        ErrorKind::Internal,
        "exec: subrelation dependency is absent from the root dependency vector",
    )
}

fn missing_table_data_dependency() -> Error {
    Error::message(
        ErrorKind::Internal,
        "exec: table data dependency is absent from the physical plan",
    )
}

impl DependencyGeneration {
    fn dynamic_bytes(&self) -> usize {
        match self {
            Self::Table {
                table_id,
                data_generation,
                ..
            } => table_id
                .as_str()
                .len()
                .saturating_add(size_of::<TableDataGeneration>())
                .saturating_add(data_generation.retained_bytes()),
            Self::WriteProtocol { table_id, .. } => table_id.as_str().len(),
            Self::Column {
                table_id,
                column_id,
                ..
            } => table_id
                .as_str()
                .len()
                .saturating_add(column_id.as_str().len()),
            Self::Index {
                table_id, index_id, ..
            } => table_id
                .as_str()
                .len()
                .saturating_add(index_id.as_str().len()),
        }
    }

    fn hash_query_validator(&self, hash: &mut Sha256) {
        match self {
            Self::Table {
                table_id,
                existence_generation,
                storage_generation,
                data_generation,
            } => {
                hash_u8(hash, 10, 1);
                hash_bytes(hash, 11, table_id.as_str().as_bytes());
                hash_u64(hash, 12, existence_generation.get());
                hash_u64(hash, 13, storage_generation.get());
                hash_u64(hash, 14, data_generation.entries().count() as u64);
                for (stripe, generation) in data_generation.entries() {
                    hash_u64(hash, 21, stripe as u64);
                    hash_u64(hash, 20, generation.get());
                }
            }
            Self::Column {
                table_id,
                column_id,
                generation,
            } => {
                hash_u8(hash, 10, 2);
                hash_bytes(hash, 11, table_id.as_str().as_bytes());
                hash_bytes(hash, 15, column_id.as_str().as_bytes());
                hash_u64(hash, 16, generation.get());
            }
            Self::Index {
                table_id,
                index_id,
                generation,
            } => {
                hash_u8(hash, 10, 3);
                hash_bytes(hash, 11, table_id.as_str().as_bytes());
                hash_bytes(hash, 17, index_id.as_str().as_bytes());
                hash_u64(hash, 18, generation.get());
            }
            Self::WriteProtocol {
                table_id,
                generation,
            } => {
                hash_u8(hash, 10, 4);
                hash_bytes(hash, 11, table_id.as_str().as_bytes());
                hash_u64(hash, 19, generation.get());
            }
        }
    }
}

fn hash_u8(hash: &mut Sha256, tag: u8, value: u8) {
    hash.update([tag]);
    hash.update(1u64.to_be_bytes());
    hash.update([value]);
}

fn hash_u64(hash: &mut Sha256, tag: u8, value: u64) {
    hash.update([tag]);
    hash.update(8u64.to_be_bytes());
    hash.update(value.to_be_bytes());
}

fn hash_bytes(hash: &mut Sha256, tag: u8, value: &[u8]) {
    hash.update([tag]);
    hash.update((value.len() as u64).to_be_bytes());
    hash.update(value);
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct CachedWork {
    pub kv: KvWork,
    pub execution: Duration,
}

#[derive(Debug)]
struct CachedRelation {
    rows: Vec<Box<[Datum]>>,
}

impl CachedRelation {
    fn from_frames(output: &RowType, frames: &[Env]) -> Self {
        // Execution slots are local planner identities. The same exact LIR can
        // receive different slots in another execution. Store values in output
        // field order, then map them to the current output slots on a hit.
        let mut rows = Vec::with_capacity(frames.len());
        for frame in frames {
            rows.push(positional_row(output, frame));
        }
        Self { rows }
    }

    fn restore(&self, output: &RowType) -> Vec<Env> {
        let slots = output
            .fields
            .iter()
            .map(|field| field.slot)
            .collect::<Vec<_>>();
        self.rows
            .iter()
            .map(|row| {
                let mut frame = Env::new();
                frame.set_datums(&slots, row.iter().cloned());
                frame
            })
            .collect()
    }

    fn shape(&self, cardinality: RootCardinality, output: &RowType) -> Result<Datum> {
        super::frames::validate_frame_cardinality(cardinality, self.rows.len())?;
        match cardinality {
            RootCardinality::Many => Ok(Datum::Array(
                self.rows
                    .iter()
                    .map(|row| cached_row_object(output, row))
                    .collect(),
            )),
            RootCardinality::First => Ok(self
                .rows
                .first()
                .map(|row| cached_row_object(output, row))
                .unwrap_or(Datum::Null)),
            RootCardinality::ExactlyOne => Ok(cached_row_object(
                output,
                self.rows.first().expect("cardinality is valid"),
            )),
            RootCardinality::Scalar => Ok(self
                .rows
                .first()
                .and_then(|row| row.first())
                .cloned()
                .unwrap_or(Datum::Null)),
        }
    }

    fn retained_bytes(&self) -> usize {
        size_of::<Self>().saturating_add(retained_row_bytes(&self.rows, self.rows.capacity()))
    }
}

pub(super) fn positional_row(output: &RowType, frame: &Env) -> Box<[Datum]> {
    output
        .fields
        .iter()
        .map(|field| frame.get(field.slot).cloned().unwrap_or(Datum::Null))
        .collect::<Vec<_>>()
        .into_boxed_slice()
}

#[derive(Debug)]
pub(super) struct CachedHashEntry {
    pub key: SmallVec<[Value; 4]>,
    pub rows: Vec<usize>,
}

#[derive(Debug)]
pub(super) struct CachedHashBuild {
    // Hash values are valid only with the RandomState that creates them. Keep
    // the state and buckets in one immutable value so every probe uses the
    // correct hash function.
    pub hash_builder: ahash::RandomState,
    pub entries: HashMap<u64, Vec<CachedHashEntry>>,
    // Planner slots belong to one prepared plan. Positional rows let this
    // value remain valid when another plan uses different slot numbers.
    pub rows: Vec<Box<[Datum]>>,
    pub input_row_count: usize,
    pub execution_retained_bytes: u64,
}

impl CachedHashBuild {
    fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(retained_row_bytes(&self.rows, self.rows.capacity()))
            .saturating_add(hash_map_retained_bytes::<u64, Vec<CachedHashEntry>>(
                self.entries.capacity(),
            ))
            .saturating_add(
                self.entries
                    .values()
                    .map(|bucket| {
                        bucket
                            .capacity()
                            .saturating_mul(size_of::<CachedHashEntry>())
                            .saturating_add(
                                bucket
                                    .iter()
                                    .map(|entry| {
                                        small_value_vec_retained_bytes(&entry.key).saturating_add(
                                            entry
                                                .rows
                                                .capacity()
                                                .saturating_mul(size_of::<usize>()),
                                        )
                                    })
                                    .fold(0usize, usize::saturating_add),
                            )
                    })
                    .fold(0usize, usize::saturating_add),
            )
    }
}

#[derive(Debug)]
pub(super) struct CachedGroupedDimensionEntry {
    pub key: SmallVec<[Value; 4]>,
    pub group_values: Vec<Value>,
    pub group_hash: u64,
    // This position selects request-local aggregate state. The cached entry
    // stays immutable while concurrent queries use different state arrays.
    pub position: usize,
}

#[derive(Debug)]
pub(super) struct CachedGroupedDimensionBuild {
    pub hash_builder: ahash::RandomState,
    pub entries: HashMap<u64, Vec<CachedGroupedDimensionEntry>>,
    pub row_count: usize,
    pub input_row_count: usize,
    pub execution_retained_bytes: u64,
}

impl CachedGroupedDimensionBuild {
    fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(hash_map_retained_bytes::<
                u64,
                Vec<CachedGroupedDimensionEntry>,
            >(self.entries.capacity()))
            .saturating_add(
                self.entries
                    .values()
                    .map(|bucket| {
                        bucket
                            .capacity()
                            .saturating_mul(size_of::<CachedGroupedDimensionEntry>())
                            .saturating_add(
                                bucket
                                    .iter()
                                    .map(|entry| {
                                        small_value_vec_retained_bytes(&entry.key)
                                            .saturating_add(
                                                entry
                                                    .group_values
                                                    .capacity()
                                                    .saturating_mul(size_of::<Value>()),
                                            )
                                            .saturating_add(
                                                entry
                                                    .group_values
                                                    .iter()
                                                    .map(value_dynamic_bytes)
                                                    .fold(0usize, usize::saturating_add),
                                            )
                                    })
                                    .fold(0usize, usize::saturating_add),
                            )
                    })
                    .fold(0usize, usize::saturating_add),
            )
    }
}

fn hash_map_retained_bytes<K, V>(capacity: usize) -> usize {
    // HashMap does not expose its allocation size. The bucket estimate rounds
    // above the load-factor capacity and includes control bytes. This weight
    // can reject an entry early, but it must not omit bucket storage.
    let buckets = capacity
        .saturating_mul(8)
        .div_ceil(7)
        .checked_next_power_of_two()
        .unwrap_or(usize::MAX);
    buckets
        .saturating_mul(size_of::<(K, V)>())
        .saturating_add(buckets.saturating_add(16))
}

fn small_value_vec_retained_bytes(values: &SmallVec<[Value; 4]>) -> usize {
    let allocation = if values.spilled() {
        values.capacity().saturating_mul(size_of::<Value>())
    } else {
        0
    };
    allocation.saturating_add(
        values
            .iter()
            .map(value_dynamic_bytes)
            .fold(0usize, usize::saturating_add),
    )
}

fn value_dynamic_bytes(value: &Value) -> usize {
    match value {
        Value::Text(value) => value.capacity(),
        Value::Bytes(value) => value.as_slice().len(),
        Value::Int64(_) | Value::Float64(_) | Value::Bool(_) | Value::Null(_) => 0,
    }
}

#[derive(Debug)]
enum CachedValue {
    Relation(CachedRelation),
    HashJoinBuild(CachedHashBuild),
    GroupedHashJoinDimension(CachedGroupedDimensionBuild),
}

#[derive(Debug)]
struct CachedMaterialization {
    value: CachedValue,
    result_bytes: usize,
    work: CachedWork,
    accounting_state: AtomicU8,
}

impl CachedMaterialization {
    fn relation(output: &RowType, frames: &[Env], work: CachedWork) -> Self {
        let relation = CachedRelation::from_frames(output, frames);
        let result_bytes = relation.retained_bytes();
        Self {
            value: CachedValue::Relation(relation),
            result_bytes,
            work,
            accounting_state: AtomicU8::new(ACCOUNTING_PENDING),
        }
    }

    fn hash_join_build(value: CachedHashBuild, work: CachedWork) -> Self {
        let result_bytes = value.retained_bytes();
        Self {
            value: CachedValue::HashJoinBuild(value),
            result_bytes,
            work,
            accounting_state: AtomicU8::new(ACCOUNTING_PENDING),
        }
    }

    fn grouped_hash_join_dimension(value: CachedGroupedDimensionBuild, work: CachedWork) -> Self {
        let result_bytes = value.retained_bytes();
        Self {
            value: CachedValue::GroupedHashJoinDimension(value),
            result_bytes,
            work,
            accounting_state: AtomicU8::new(ACCOUNTING_PENDING),
        }
    }

    fn retained_bytes(&self) -> usize {
        size_of::<Self>().saturating_add(self.result_bytes)
    }

    fn observed_rows_and_keys(&self) -> (usize, usize) {
        match &self.value {
            CachedValue::Relation(value) => (value.rows.len(), 0),
            CachedValue::HashJoinBuild(value) => (
                value.input_row_count,
                value
                    .entries
                    .values()
                    .map(Vec::len)
                    .fold(0usize, usize::saturating_add),
            ),
            CachedValue::GroupedHashJoinDimension(value) => (
                value.input_row_count,
                value
                    .entries
                    .values()
                    .map(Vec::len)
                    .fold(0usize, usize::saturating_add),
            ),
        }
    }

    fn relation_ref(&self) -> &CachedRelation {
        let CachedValue::Relation(value) = &self.value else {
            unreachable!("relation cache domain has a non-relation value")
        };
        value
    }

    fn hash_join_build_ref(&self) -> &CachedHashBuild {
        let CachedValue::HashJoinBuild(value) = &self.value else {
            unreachable!("hash-build cache domain has another value")
        };
        value
    }

    fn grouped_hash_join_dimension_ref(&self) -> &CachedGroupedDimensionBuild {
        let CachedValue::GroupedHashJoinDimension(value) = &self.value else {
            unreachable!("grouped-dimension cache domain has another value")
        };
        value
    }
}

#[derive(Clone, Debug)]
pub(super) struct CachedHashBuildHandle(Arc<CachedMaterialization>);

impl CachedHashBuildHandle {
    pub(super) fn uncached(value: CachedHashBuild) -> Self {
        Self(Arc::new(CachedMaterialization::hash_join_build(
            value,
            CachedWork::default(),
        )))
    }

    pub(super) fn value(&self) -> &CachedHashBuild {
        self.0.hash_join_build_ref()
    }
}

#[derive(Clone, Debug)]
pub(super) struct CachedGroupedDimensionBuildHandle(Arc<CachedMaterialization>);

impl CachedGroupedDimensionBuildHandle {
    pub(super) fn uncached(value: CachedGroupedDimensionBuild) -> Self {
        Self(Arc::new(
            CachedMaterialization::grouped_hash_join_dimension(value, CachedWork::default()),
        ))
    }

    pub(super) fn value(&self) -> &CachedGroupedDimensionBuild {
        self.0.grouped_hash_join_dimension_ref()
    }
}

pub(super) struct CachedHashBuildResult {
    pub value: CachedHashBuildHandle,
    pub source: StatementSource,
    events: SemanticCacheEventBatch,
}

impl CachedHashBuildResult {
    pub(super) fn take_events(&mut self) -> SemanticCacheEventBatch {
        std::mem::take(&mut self.events)
    }
}

pub(super) struct CachedGroupedDimensionBuildResult {
    pub value: CachedGroupedDimensionBuildHandle,
    pub source: StatementSource,
    events: SemanticCacheEventBatch,
}

impl CachedGroupedDimensionBuildResult {
    pub(super) fn take_events(&mut self) -> SemanticCacheEventBatch {
        std::mem::take(&mut self.events)
    }
}

fn cached_row_object(output: &RowType, row: &[Datum]) -> Datum {
    Datum::Object(
        output
            .fields
            .iter()
            .enumerate()
            .map(|(index, field)| ObjectField {
                name: field.name.clone(),
                datum: row.get(index).cloned().unwrap_or(Datum::Null),
            })
            .collect(),
    )
}

/// Estimate the owned row copy before allocation. The estimate uses logical
/// lengths because the copy does not preserve spare source capacity. The
/// cache checks the allocated value again before admission. An allocator can
/// therefore cause a conservative late rejection, but it cannot let the
/// allocated value exceed the result limit.
fn estimated_result_bytes(output: &RowType, frames: &[Env]) -> usize {
    frames
        .len()
        .saturating_mul(size_of::<Box<[Datum]>>())
        .saturating_add(
            frames
                .iter()
                .map(|frame| {
                    output
                        .fields
                        .len()
                        .saturating_mul(size_of::<Datum>())
                        .saturating_add(
                            output
                                .fields
                                .iter()
                                .filter_map(|field| frame.get(field.slot))
                                .map(cloned_datum_dynamic_bytes)
                                .fold(0usize, usize::saturating_add),
                        )
                })
                .fold(0usize, usize::saturating_add),
        )
}

fn retained_row_bytes(rows: &[Box<[Datum]>], row_capacity: usize) -> usize {
    row_capacity
        .saturating_mul(size_of::<Box<[Datum]>>())
        .saturating_add(
            rows.iter()
                .map(|row| {
                    row.len().saturating_mul(size_of::<Datum>()).saturating_add(
                        row.iter()
                            .map(datum_dynamic_bytes)
                            .fold(0usize, usize::saturating_add),
                    )
                })
                .fold(0usize, usize::saturating_add),
        )
}

fn datum_dynamic_bytes(datum: &Datum) -> usize {
    match datum {
        Datum::Scalar(Value::Text(value)) => value.capacity(),
        Datum::Null | Datum::Scalar(_) => 0,
        Datum::Object(fields) => fields
            .capacity()
            .saturating_mul(size_of::<ObjectField>())
            .saturating_add(
                fields
                    .iter()
                    .map(|field| {
                        field
                            .name
                            .capacity()
                            .saturating_add(datum_dynamic_bytes(&field.datum))
                    })
                    .fold(0usize, usize::saturating_add),
            ),
        Datum::Array(elements) => elements
            .capacity()
            .saturating_mul(size_of::<Datum>())
            .saturating_add(
                elements
                    .iter()
                    .map(datum_dynamic_bytes)
                    .fold(0usize, usize::saturating_add),
            ),
    }
}

fn cloned_datum_dynamic_bytes(datum: &Datum) -> usize {
    match datum {
        Datum::Scalar(Value::Text(value)) => value.len(),
        Datum::Null | Datum::Scalar(_) => 0,
        Datum::Object(fields) => fields
            .len()
            .saturating_mul(size_of::<ObjectField>())
            .saturating_add(
                fields
                    .iter()
                    .map(|field| {
                        field
                            .name
                            .len()
                            .saturating_add(cloned_datum_dynamic_bytes(&field.datum))
                    })
                    .fold(0usize, usize::saturating_add),
            ),
        Datum::Array(elements) => elements
            .len()
            .saturating_mul(size_of::<Datum>())
            .saturating_add(
                elements
                    .iter()
                    .map(cloned_datum_dynamic_bytes)
                    .fold(0usize, usize::saturating_add),
            ),
    }
}

#[derive(Clone)]
enum FlightResult {
    Success(Arc<CachedMaterialization>),
    Failure(CachedError),
    Cancelled,
}

enum FlightRole<'a> {
    Fill(FlightOwner<'a>),
    Wait(watch::Receiver<Option<FlightResult>>),
}

#[derive(Clone)]
enum SnapshotFlightResult {
    Success(Arc<ValidatedDependencies>),
    Failure(CachedError),
    Cancelled,
}

struct SnapshotFlight {
    result: watch::Sender<Option<SnapshotFlightResult>>,
}

impl SnapshotFlight {
    fn new() -> Self {
        let (result, _) = watch::channel(None);
        Self { result }
    }

    async fn wait(
        mut receiver: watch::Receiver<Option<SnapshotFlightResult>>,
    ) -> SnapshotFlightResult {
        loop {
            let result = receiver.borrow_and_update().clone();
            if let Some(result) = result {
                return result;
            }
            if receiver.changed().await.is_err() {
                return SnapshotFlightResult::Cancelled;
            }
        }
    }
}

#[derive(Default)]
struct SnapshotDependencyState {
    entries: HashMap<SnapshotDependencyKey, Arc<ValidatedDependencies>>,
    insertion_order: VecDeque<SnapshotDependencyKey>,
    flights: HashMap<SnapshotDependencyKey, Arc<SnapshotFlight>>,
}

struct SnapshotDependencyCache {
    state: Mutex<SnapshotDependencyState>,
    entry_limit: usize,
}

impl SnapshotDependencyCache {
    fn new(entry_limit: usize) -> Self {
        Self {
            state: Mutex::new(SnapshotDependencyState::default()),
            entry_limit: entry_limit.max(1),
        }
    }

    fn get(&self, key: &SnapshotDependencyKey) -> Option<Arc<ValidatedDependencies>> {
        self.state
            .lock()
            .expect("snapshot dependency cache lock poisoned")
            .entries
            .get(key)
            .cloned()
    }

    fn insert(&self, key: SnapshotDependencyKey, value: Arc<ValidatedDependencies>) -> usize {
        let mut state = self
            .state
            .lock()
            .expect("snapshot dependency cache lock poisoned");
        if state.entries.contains_key(&key) {
            return 0;
        }
        let mut evictions = 0usize;
        // Old storage positions enter the queue before new positions. FIFO
        // removal therefore gives current snapshots capacity without making
        // position age a correctness rule. An evicted old transaction reads
        // its fences again and can still use the relation result cache.
        while state.entries.len() >= self.entry_limit {
            let oldest = state
                .insertion_order
                .pop_front()
                .expect("snapshot dependency insertion order is complete");
            if state.entries.remove(&oldest).is_some() {
                evictions = evictions.saturating_add(1);
            }
        }
        state.insertion_order.push_back(key.clone());
        state.entries.insert(key, value);
        evictions
    }
}

struct Flight {
    result: watch::Sender<Option<FlightResult>>,
    waiters: AtomicUsize,
}

impl Flight {
    fn new() -> Self {
        let (result, _) = watch::channel(None);
        Self {
            result,
            waiters: AtomicUsize::new(0),
        }
    }

    fn subscribe(&self) -> watch::Receiver<Option<FlightResult>> {
        self.waiters.fetch_add(1, Ordering::Relaxed);
        self.result.subscribe()
    }

    async fn wait(mut receiver: watch::Receiver<Option<FlightResult>>) -> FlightResult {
        // A cancelled owner publishes Cancelled from Drop. A waiter retries
        // ownership instead of returning cancellation as a query failure.
        loop {
            let result = receiver.borrow_and_update().clone();
            if let Some(result) = result {
                return result;
            }
            if receiver.changed().await.is_err() {
                return FlightResult::Cancelled;
            }
        }
    }
}

#[derive(Clone)]
struct CachedError {
    kind: ErrorKind,
    reason: ErrorReason,
    message: String,
}

impl CachedError {
    fn capture(error: &Error) -> Self {
        Self {
            kind: error.kind(),
            reason: error.reason(),
            message: error.to_string(),
        }
    }

    fn restore(self) -> Error {
        Error::with_reason(self.kind, self.reason, self.message)
    }
}

#[derive(Default)]
struct RelationCacheMetrics {
    query: MaterializationMetrics,
    hash_build: MaterializationMetrics,
    grouped_dimension: MaterializationMetrics,
    retained_bytes: AtomicI64,
    dependency_hits: AtomicU64,
    dependency_misses: AtomicU64,
    dependency_coalesced: AtomicU64,
    dependency_evictions: AtomicU64,
}

impl RelationCacheMetrics {
    fn domain(&self, domain: MaterializationDomain) -> &MaterializationMetrics {
        match domain {
            MaterializationDomain::QueryResult => &self.query,
            MaterializationDomain::HashJoinBuild | MaterializationDomain::SubrelationRowsV1 => {
                &self.hash_build
            }
            MaterializationDomain::GroupedHashJoinDimension => &self.grouped_dimension,
        }
    }
}

#[derive(Default)]
struct MaterializationMetrics {
    hits: AtomicU64,
    misses: AtomicU64,
    fills: AtomicU64,
    fill_work_units: AtomicU64,
    avoided_work_units: AtomicU64,
    admissions: AtomicU64,
    rejected_too_large: AtomicU64,
    rad_policy_rejections: AtomicU64,
    foyer_rejections: AtomicU64,
    evictions: AtomicU64,
    coalesced: AtomicU64,
    entries: AtomicI64,
    retained_bytes: AtomicI64,
}

impl MaterializationMetrics {
    fn snapshot(&self) -> RelationCacheDomainStatistics {
        RelationCacheDomainStatistics {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            fills: self.fills.load(Ordering::Relaxed),
            fill_work_units: self.fill_work_units.load(Ordering::Relaxed),
            avoided_work_units: self.avoided_work_units.load(Ordering::Relaxed),
            admissions: self.admissions.load(Ordering::Relaxed),
            rejected_too_large: self.rejected_too_large.load(Ordering::Relaxed),
            rad_policy_rejections: self.rad_policy_rejections.load(Ordering::Relaxed),
            foyer_rejections: self.foyer_rejections.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            coalesced: self.coalesced.load(Ordering::Relaxed),
            entries: self.entries.load(Ordering::Relaxed).max(0) as u64,
            retained_bytes: self.retained_bytes.load(Ordering::Relaxed).max(0) as u64,
        }
    }
}

struct CacheEvents {
    metrics: Arc<RelationCacheMetrics>,
    semantic: Arc<SemanticCacheEvents>,
}

#[derive(Default)]
struct SemanticCacheEvents {
    enabled: AtomicBool,
    admission: Mutex<()>,
    active: Mutex<Option<SemanticCacheEventBatch>>,
}

impl SemanticCacheEvents {
    fn enable(&self) {
        self.enabled.store(true, Ordering::Release);
    }

    fn capture(&self) -> Option<SemanticCacheEventCapture<'_>> {
        if !self.enabled.load(Ordering::Acquire) {
            return None;
        }
        let admission = self
            .admission
            .lock()
            .expect("semantic cache admission lock poisoned");
        *self
            .active
            .lock()
            .expect("semantic cache event lock poisoned") = Some(SemanticCacheEventBatch::new());
        Some(SemanticCacheEventCapture {
            events: self,
            _admission: admission,
            finished: false,
        })
    }

    fn admission(
        &self,
        materialization: RelationCacheMaterialization,
        relation: Fingerprint,
        result: RelationCacheAdmissionResult,
    ) -> SemanticCacheEventBatch {
        if !self.enabled.load(Ordering::Acquire) {
            return SemanticCacheEventBatch::new();
        }
        vec![EngineEvent::RelationCacheAdmissionCompleted {
            materialization,
            relation,
            result,
        }]
    }

    fn record_eviction(
        &self,
        materialization: RelationCacheMaterialization,
        relation: Fingerprint,
        cause: RelationCacheEvictionCause,
    ) {
        if !self.enabled.load(Ordering::Acquire) {
            return;
        }
        if let Some(active) = self
            .active
            .lock()
            .expect("semantic cache event lock poisoned")
            .as_mut()
        {
            active.push(EngineEvent::RelationCacheEntryEvicted {
                materialization,
                relation,
                cause,
            });
        }
    }
}

struct SemanticCacheEventCapture<'a> {
    events: &'a SemanticCacheEvents,
    _admission: MutexGuard<'a, ()>,
    finished: bool,
}

impl SemanticCacheEventCapture<'_> {
    fn finish(
        mut self,
        materialization: RelationCacheMaterialization,
        relation: Fingerprint,
        result: RelationCacheAdmissionResult,
    ) -> SemanticCacheEventBatch {
        let mut events = self
            .events
            .active
            .lock()
            .expect("semantic cache event lock poisoned")
            .take()
            .expect("semantic cache event capture is active");
        events.push(EngineEvent::RelationCacheAdmissionCompleted {
            materialization,
            relation,
            result,
        });
        self.finished = true;
        events
    }
}

impl Drop for SemanticCacheEventCapture<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.events
                .active
                .lock()
                .expect("semantic cache event lock poisoned")
                .take();
        }
    }
}

impl EventListener for CacheEvents {
    type Key = RelationCacheKey;
    type Value = Arc<CachedMaterialization>;

    fn on_leave(&self, event: Event, key: &Self::Key, value: &Self::Value) {
        let bytes = key.retained_bytes().saturating_add(value.retained_bytes()) as u64;
        // Foyer can call on_leave before insert returns. The state exchange
        // prevents a late admission path from adding bytes for an entry that
        // is already absent. It also makes each removal subtract at most once.
        let was_retained = value
            .accounting_state
            .swap(ACCOUNTING_REMOVED, Ordering::AcqRel)
            == ACCOUNTING_RETAINED;
        if was_retained {
            self.metrics
                .retained_bytes
                .fetch_sub(bytes as i64, Ordering::Relaxed);
            let domain = self.metrics.domain(key.domain);
            domain.entries.fetch_sub(1, Ordering::Relaxed);
            domain
                .retained_bytes
                .fetch_sub(bytes as i64, Ordering::Relaxed);
        }
        let (cause, semantic_cause) = match event {
            Event::Evict => {
                self.metrics
                    .domain(key.domain)
                    .evictions
                    .fetch_add(1, Ordering::Relaxed);
                ("capacity", RelationCacheEvictionCause::Capacity)
            }
            Event::Replace => ("replace", RelationCacheEvictionCause::Replaced),
            Event::Remove => ("remove", RelationCacheEvictionCause::Removed),
            Event::Clear => ("clear", RelationCacheEvictionCause::Cleared),
        };
        key.domain.eviction(cause);
        if was_retained {
            self.semantic.record_eviction(
                key.domain.event_materialization(),
                key.exact,
                semantic_cause,
            );
        }
    }
}

pub(super) struct RelationCache {
    entries: Cache<RelationCacheKey, Arc<CachedMaterialization>>,
    flights: Mutex<HashMap<RelationCacheKey, Arc<Flight>>>,
    snapshot_dependencies: SnapshotDependencyCache,
    snapshot_catalog: SnapshotCatalogCache,
    prepared_reads: PreparedReadCache,
    policy: RelationCachePolicy,
    metrics: Arc<RelationCacheMetrics>,
    semantic_events: Arc<SemanticCacheEvents>,
    runtime: Arc<dyn RuntimeEffects>,
    config: RelationCacheConfig,
    limits: RelationCacheLimits,
    entry_weight: usize,
    result_byte_limit: usize,
}

impl Default for RelationCache {
    fn default() -> Self {
        Self::new(RelationCacheConfig::default())
    }
}

impl RelationCache {
    pub(super) fn new(config: RelationCacheConfig) -> Self {
        Self::new_with_runtime(config, Arc::new(SystemRuntime))
    }

    pub(super) fn new_with_runtime(
        config: RelationCacheConfig,
        runtime: Arc<dyn RuntimeEffects>,
    ) -> Self {
        let byte_limit = config.limits.byte_limit.clamp(1, i64::MAX as usize);
        let entry_limit = config.limits.entry_limit.max(1);
        let prepared_byte_limit = byte_limit
            .div_ceil(4)
            .max(config.limits.result_byte_limit)
            .min(byte_limit);
        // A minimum weight converts the byte capacity into an entry limit.
        // Larger entries still use their estimated retained byte size.
        let entry_weight = byte_limit.div_ceil(entry_limit);
        let shards = CACHE_SHARDS.min(entry_limit).min(byte_limit);
        let metrics = Arc::new(RelationCacheMetrics::default());
        let semantic_events = Arc::new(SemanticCacheEvents::default());
        let entries = Cache::builder(byte_limit)
            .with_shards(shards)
            .with_eviction_config(LfuConfig::default())
            .with_weighter(
                move |key: &RelationCacheKey, value: &Arc<CachedMaterialization>| {
                    key.retained_bytes()
                        .saturating_add(value.retained_bytes())
                        .max(entry_weight)
                },
            )
            .with_event_listener(Arc::new(CacheEvents {
                metrics: metrics.clone(),
                semantic: semantic_events.clone(),
            }))
            .build::<CacheProperties>();
        let limits = RelationCacheLimits {
            byte_limit,
            entry_limit,
            result_byte_limit: config.limits.result_byte_limit,
        };
        crate::telemetry::relation_cache_limits(
            limits.entry_limit,
            limits.byte_limit as u64,
            limits.result_byte_limit as u64,
        );
        crate::telemetry::relation_cache_residency(0, 0);
        Self {
            entries,
            flights: Mutex::new(HashMap::new()),
            snapshot_dependencies: SnapshotDependencyCache::new(entry_limit),
            snapshot_catalog: SnapshotCatalogCache::new(
                entry_limit,
                config.limits.result_byte_limit,
            ),
            prepared_reads: PreparedReadCache::new(entry_limit, prepared_byte_limit),
            policy: RelationCachePolicy::new(entry_limit, config.policy),
            metrics,
            semantic_events,
            runtime,
            config,
            limits,
            entry_weight,
            result_byte_limit: config.limits.result_byte_limit,
        }
    }

    pub(super) const fn config(&self) -> RelationCacheConfig {
        self.config
    }

    pub(super) const fn domain_enabled(&self, domain: MaterializationDomain) -> bool {
        self.config.domains.contains(domain)
    }

    pub(super) fn enable_semantic_events(&self) {
        self.semantic_events.enable();
    }

    /// Resolve table metadata through the caller's pinned snapshot.
    ///
    /// Only implicit read-only execution uses this path. An explicit
    /// serializable transaction must read the catalog keys through its own
    /// view so those keys enter its conflict set.
    pub(super) async fn catalog_table_for_snapshot(
        &self,
        view: &dyn KvView,
        name: &str,
    ) -> crate::engine::catalog::Result<Option<crate::engine::catalog::model::Table>> {
        self.snapshot_catalog.get_table(view, name).await
    }

    async fn catalog_table_matches(
        &self,
        view: &dyn KvView,
        name: &str,
        id: &TableId,
        definition_generation: crate::engine::catalog::identity::DefinitionGeneration,
        validation: DependencyValidation,
    ) -> crate::engine::catalog::Result<bool> {
        let table = match validation {
            DependencyValidation::Snapshot => self.snapshot_catalog.get_table(view, name).await?,
            DependencyValidation::Transaction => store::get_table(view, name).await?,
        };
        Ok(table.is_some_and(|table| {
            table.id == *id && table.definition_generation == definition_generation
        }))
    }

    /// Reuse binding and physical planning for one exact read request.
    ///
    /// Each candidate keeps its complete physical dependency set. Lookup
    /// validates that set through the caller's pinned view before it returns
    /// the plan. Data changes can reuse a plan and receive a new relation
    /// result key. Catalog changes can reuse a plan only when all dependencies
    /// still match. Transaction validation reads each dependency through the
    /// caller's view so the storage conflict set remains complete.
    pub(super) async fn get_or_prepare_read<F, Fut>(
        &self,
        view: &dyn KvView,
        request: PreparedReadRequest<'_>,
        transaction_dependencies: Option<&TransactionDependencyCache>,
        prepare: F,
    ) -> Result<PreparedReadResult>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<crate::engine::planner::bind::BoundStatement>>,
    {
        self.prepared_reads
            .get_or_prepare(self, view, request, transaction_dependencies, prepare)
            .await
    }

    /// Resolve the correctness key for one bound physical relation.
    ///
    /// Snapshot validation can be shared only when the view has the same
    /// storage position and physical dependency set. A memo hit skips storage
    /// reads but does not skip any value check: the stored generations were
    /// read through that same snapshot. Failed validation is sent to current
    /// waiters and is not stored.
    pub(super) async fn key_for_view(
        &self,
        fingerprints: (Fingerprint, Fingerprint),
        view: &dyn KvView,
        dependencies: &CatalogDependencies,
        data_dependencies: &RelationDataDependencies,
        validation: DependencyValidation,
        transaction_dependencies: Option<&TransactionDependencyCache>,
    ) -> Result<AdmittedRelationCacheKey> {
        let data_dependencies = ResolvedDataDependencies::resolve(data_dependencies)?;
        match validation {
            DependencyValidation::Transaction => {
                self.key_for_transaction(
                    fingerprints,
                    view,
                    dependencies,
                    &data_dependencies,
                    transaction_dependencies,
                )
                .await
            }
            DependencyValidation::Snapshot => {
                self.key_for_snapshot(fingerprints, view, dependencies, &data_dependencies)
                    .await
            }
        }
    }

    async fn key_for_transaction(
        &self,
        fingerprints: (Fingerprint, Fingerprint),
        view: &dyn KvView,
        dependencies: &CatalogDependencies,
        data_dependencies: &ResolvedDataDependencies,
        transaction_dependencies: Option<&TransactionDependencyCache>,
    ) -> Result<AdmittedRelationCacheKey> {
        let transaction_key = transaction_dependencies.and_then(|_| {
            view.begin_position().cloned().map(|position| {
                SnapshotDependencyKey::new(position, dependencies, data_dependencies.clone())
            })
        });
        if let Some((cache, key)) = transaction_dependencies.zip(transaction_key.as_ref())
            && let Some(validated) = cache.get(key)
        {
            crate::telemetry::relation_cache_dependency_lookup("transaction_hit");
            return Ok(Self::admitted_from_validated(fingerprints, validated));
        }
        crate::telemetry::relation_cache_dependency_lookup("tracked");
        let validated =
            RelationCacheKey::read_dependencies_for_view(view, dependencies, data_dependencies)
                .await?;
        if let Some((cache, key)) = transaction_dependencies.zip(transaction_key) {
            cache.insert(key, validated.clone());
        }
        Ok(Self::admitted_from_validated(fingerprints, validated))
    }

    async fn key_for_snapshot(
        &self,
        fingerprints: (Fingerprint, Fingerprint),
        view: &dyn KvView,
        dependencies: &CatalogDependencies,
        data_dependencies: &ResolvedDataDependencies,
    ) -> Result<AdmittedRelationCacheKey> {
        let Some(position) = view.begin_position().cloned() else {
            crate::telemetry::relation_cache_dependency_lookup("unpositioned");
            let (exact, family) = fingerprints;
            return RelationCacheKey::for_view(
                exact,
                family,
                view,
                dependencies,
                data_dependencies,
            )
            .await
            .map(Self::admitted_key);
        };
        let snapshot_key =
            SnapshotDependencyKey::new(position, dependencies, data_dependencies.clone());
        loop {
            if let Some(validated) = self.snapshot_dependencies.get(&snapshot_key) {
                self.metrics.dependency_hits.fetch_add(1, Ordering::Relaxed);
                crate::telemetry::relation_cache_dependency_lookup("hit");
                return Ok(Self::admitted_from_validated(fingerprints, validated));
            }
            self.metrics
                .dependency_misses
                .fetch_add(1, Ordering::Relaxed);
            crate::telemetry::relation_cache_dependency_lookup("miss");
            let (flight, owner, receiver) = {
                let mut state = self
                    .snapshot_dependencies
                    .state
                    .lock()
                    .expect("snapshot dependency cache lock poisoned");
                if let Some(flight) = state.flights.get(&snapshot_key) {
                    (flight.clone(), false, Some(flight.result.subscribe()))
                } else {
                    let flight = Arc::new(SnapshotFlight::new());
                    state.flights.insert(snapshot_key.clone(), flight.clone());
                    (flight, true, None)
                }
            };
            if !owner {
                self.metrics
                    .dependency_coalesced
                    .fetch_add(1, Ordering::Relaxed);
                crate::telemetry::relation_cache_dependency_lookup("coalesced");
                match SnapshotFlight::wait(receiver.expect("a waiter has a receiver")).await {
                    SnapshotFlightResult::Success(validated) => {
                        return Ok(Self::admitted_from_validated(fingerprints, validated));
                    }
                    SnapshotFlightResult::Failure(error) => return Err(error.restore()),
                    SnapshotFlightResult::Cancelled => continue,
                }
            }
            let mut owner =
                SnapshotFlightOwner::new(&self.snapshot_dependencies, snapshot_key.clone(), flight);
            match RelationCacheKey::read_dependencies_for_view(
                view,
                dependencies,
                data_dependencies,
            )
            .await
            {
                Ok(validated) => {
                    let evictions = self
                        .snapshot_dependencies
                        .insert(snapshot_key.clone(), validated.clone());
                    if evictions > 0 {
                        self.metrics
                            .dependency_evictions
                            .fetch_add(evictions as u64, Ordering::Relaxed);
                        crate::telemetry::relation_cache_dependency_eviction(evictions as u64);
                    }
                    owner.finish(SnapshotFlightResult::Success(validated.clone()));
                    return Ok(Self::admitted_from_validated(fingerprints, validated));
                }
                Err(error) => {
                    owner.finish(SnapshotFlightResult::Failure(CachedError::capture(&error)));
                    return Err(error);
                }
            }
        }
    }

    fn admitted_from_validated(
        (exact, family): (Fingerprint, Fingerprint),
        dependencies: Arc<ValidatedDependencies>,
    ) -> AdmittedRelationCacheKey {
        Self::admitted_key(RelationCacheKey::from_validated_dependencies(
            exact,
            family,
            dependencies,
        ))
    }

    fn admitted_key(key: RelationCacheKey) -> AdmittedRelationCacheKey {
        AdmittedRelationCacheKey {
            key,
            admission: CatalogDependencyAdmission { _private: () },
        }
    }

    pub(super) async fn get_or_fill<F, Fut>(
        &self,
        key: RelationCacheKey,
        output: &RowType,
        fill: F,
    ) -> Result<RelationCacheResult>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(Vec<Env>, CachedWork)>>,
    {
        if !self.domain_enabled(key.domain) {
            let (frames, _) = fill().await?;
            return Ok(RelationCacheResult {
                rows: RelationRows::Frames(frames),
                source: StatementSource::Executed,
                events: SemanticCacheEventBatch::new(),
            });
        }
        let cohort = self.observe_policy_request(&key);
        if let Some(relation) = self.get(&key, cohort) {
            return Ok(RelationCacheResult {
                rows: RelationRows::Cached(relation),
                source: StatementSource::RelationCache,
                events: SemanticCacheEventBatch::new(),
            });
        }
        self.metrics
            .domain(key.domain)
            .misses
            .fetch_add(1, Ordering::Relaxed);
        key.domain.lookup("miss");
        let mut fill = Some(fill);
        loop {
            // The flight map coalesces only equal correctness keys. Do not use
            // the query fingerprint alone here because visible generations can
            // differ between concurrent transactions.
            let mut owner = match self.acquire_flight(&key) {
                FlightRole::Wait(receiver) => {
                    self.metrics
                        .domain(key.domain)
                        .coalesced
                        .fetch_add(1, Ordering::Relaxed);
                    crate::telemetry::relation_cache_coalesced(key.domain.as_str());
                    match Flight::wait(receiver).await {
                        FlightResult::Success(relation) => {
                            if let Some(cohort) = cohort {
                                self.policy.observe_coalesced_reuse(
                                    cohort,
                                    relation.work,
                                    self.runtime.monotonic(),
                                );
                            }
                            key.domain.avoided(relation.work);
                            self.record_avoided(key.domain, relation.work);
                            return Ok(RelationCacheResult {
                                rows: RelationRows::Cached(relation),
                                source: StatementSource::RelationCache,
                                events: SemanticCacheEventBatch::new(),
                            });
                        }
                        FlightResult::Failure(error) => return Err(error.restore()),
                        FlightResult::Cancelled => continue,
                    }
                }
                FlightRole::Fill(owner) => owner,
            };
            // Another owner can admit the key between the first cache lookup
            // and flight ownership. Check again before the expensive read.
            if let Some(relation) = self.get_after_miss(&key, cohort) {
                owner.finish(FlightResult::Success(relation.clone()));
                return Ok(RelationCacheResult {
                    rows: RelationRows::Cached(relation),
                    source: StatementSource::RelationCache,
                    events: SemanticCacheEventBatch::new(),
                });
            }
            let result = fill.take().expect("relation cache fill runs once")().await;
            match result {
                Ok(fill) => return Ok(self.finish_query_fill(&key, output, cohort, fill, owner)),
                Err(error) => {
                    // A failed read is shared with current waiters but is never
                    // stored in the relation cache.
                    owner.finish(FlightResult::Failure(CachedError::capture(&error)));
                    return Err(error);
                }
            }
        }
    }

    fn finish_query_fill(
        &self,
        key: &RelationCacheKey,
        output: &RowType,
        cohort: Option<CohortToken>,
        fill: (Vec<Env>, CachedWork),
        mut owner: FlightOwner<'_>,
    ) -> RelationCacheResult {
        let (frames, work) = fill;
        self.record_fill(key.domain, work);
        crate::telemetry::relation_cache_materialization_fill(
            key.domain.as_str(),
            work.execution,
            frames.len(),
            0,
        );
        let estimated_bytes = estimated_result_bytes(output, &frames);
        crate::telemetry::relation_cache_materialization_result_size(
            key.domain.as_str(),
            estimated_bytes,
        );
        if estimated_bytes > self.result_byte_limit {
            if let Some(cohort) = cohort {
                self.policy.observe_fill(
                    cohort,
                    work,
                    estimated_bytes,
                    key.retained_bytes()
                        .saturating_add(size_of::<CachedMaterialization>())
                        .saturating_add(estimated_bytes),
                    self.result_byte_limit,
                    self.runtime.monotonic(),
                );
            }
            self.reject_too_large(key.domain);
            let events = self.semantic_events.admission(
                key.domain.event_materialization(),
                key.exact,
                RelationCacheAdmissionResult::TooLarge,
            );
            // Waiter registration and flight removal use the same lock. After
            // detach returns zero, no waiter can require this portable copy.
            if owner.detach() > 0 {
                let relation = Arc::new(CachedMaterialization::relation(output, &frames, work));
                owner.publish(FlightResult::Success(relation));
            }
            return RelationCacheResult {
                rows: RelationRows::Frames(frames),
                source: StatementSource::Executed,
                events,
            };
        }
        let relation = Arc::new(CachedMaterialization::relation(output, &frames, work));
        let decision = cohort.map(|cohort| {
            self.policy.observe_fill(
                cohort,
                work,
                relation.result_bytes,
                key.retained_bytes()
                    .saturating_add(relation.retained_bytes()),
                self.result_byte_limit,
                self.runtime.monotonic(),
            )
        });
        let (events, admitted) = self.admit_after_policy(key.clone(), relation.clone(), decision);
        if let Some(cohort) = cohort {
            self.policy.observe_residency(cohort, admitted);
        }
        // Admission policy can reject this value. Current waiters still share
        // the successful result. A new request fills the key again.
        owner.finish(FlightResult::Success(relation));
        RelationCacheResult {
            rows: RelationRows::Frames(frames),
            source: StatementSource::Executed,
            events,
        }
    }

    pub(super) async fn get_or_fill_hash_build<F, Fut>(
        &self,
        key: RelationCacheKey,
        fill: F,
    ) -> Result<CachedHashBuildResult>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(CachedHashBuild, CachedWork)>>,
    {
        if key.domain != MaterializationDomain::HashJoinBuild {
            return Err(Error::message(
                ErrorKind::Internal,
                "exec: hash-build lookup has an incorrect cache domain",
            ));
        }
        if !self.domain_enabled(key.domain) {
            let (value, work) = fill().await?;
            return Ok(CachedHashBuildResult {
                value: CachedHashBuildHandle(Arc::new(CachedMaterialization::hash_join_build(
                    value, work,
                ))),
                source: StatementSource::Executed,
                events: SemanticCacheEventBatch::new(),
            });
        }
        let result = self
            .get_or_fill_physical(key, || async {
                let (value, work) = fill().await?;
                Ok(CachedMaterialization::hash_join_build(value, work))
            })
            .await?;
        if !matches!(&result.0.value, CachedValue::HashJoinBuild(_)) {
            return Err(Error::message(
                ErrorKind::Internal,
                "exec: hash-build cache entry has an incorrect value",
            ));
        }
        Ok(CachedHashBuildResult {
            value: CachedHashBuildHandle(result.0),
            source: result.1,
            events: result.2,
        })
    }

    pub(super) async fn get_or_fill_grouped_dimension<F, Fut>(
        &self,
        key: RelationCacheKey,
        fill: F,
    ) -> Result<CachedGroupedDimensionBuildResult>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(CachedGroupedDimensionBuild, CachedWork)>>,
    {
        if key.domain != MaterializationDomain::GroupedHashJoinDimension {
            return Err(Error::message(
                ErrorKind::Internal,
                "exec: grouped-dimension lookup has an incorrect cache domain",
            ));
        }
        if !self.domain_enabled(key.domain) {
            let (value, work) = fill().await?;
            return Ok(CachedGroupedDimensionBuildResult {
                value: CachedGroupedDimensionBuildHandle(Arc::new(
                    CachedMaterialization::grouped_hash_join_dimension(value, work),
                )),
                source: StatementSource::Executed,
                events: SemanticCacheEventBatch::new(),
            });
        }
        let result = self
            .get_or_fill_physical(key, || async {
                let (value, work) = fill().await?;
                Ok(CachedMaterialization::grouped_hash_join_dimension(
                    value, work,
                ))
            })
            .await?;
        if !matches!(&result.0.value, CachedValue::GroupedHashJoinDimension(_)) {
            return Err(Error::message(
                ErrorKind::Internal,
                "exec: grouped-dimension cache entry has an incorrect value",
            ));
        }
        Ok(CachedGroupedDimensionBuildResult {
            value: CachedGroupedDimensionBuildHandle(result.0),
            source: result.1,
            events: result.2,
        })
    }

    async fn get_or_fill_physical<F, Fut>(
        &self,
        key: RelationCacheKey,
        fill: F,
    ) -> Result<(
        Arc<CachedMaterialization>,
        StatementSource,
        SemanticCacheEventBatch,
    )>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<CachedMaterialization>>,
    {
        let cohort = self.observe_policy_request(&key);
        if let Some(value) = self.get(&key, cohort) {
            return Ok((
                value,
                StatementSource::RelationCache,
                SemanticCacheEventBatch::new(),
            ));
        }
        self.metrics
            .domain(key.domain)
            .misses
            .fetch_add(1, Ordering::Relaxed);
        key.domain.lookup("miss");
        let mut fill = Some(fill);
        loop {
            let mut owner = match self.acquire_flight(&key) {
                FlightRole::Wait(receiver) => {
                    self.metrics
                        .domain(key.domain)
                        .coalesced
                        .fetch_add(1, Ordering::Relaxed);
                    crate::telemetry::relation_cache_coalesced(key.domain.as_str());
                    match Flight::wait(receiver).await {
                        FlightResult::Success(value) => {
                            if let Some(cohort) = cohort {
                                self.policy.observe_coalesced_reuse(
                                    cohort,
                                    value.work,
                                    self.runtime.monotonic(),
                                );
                            }
                            key.domain.avoided(value.work);
                            self.record_avoided(key.domain, value.work);
                            return Ok((
                                value,
                                StatementSource::RelationCache,
                                SemanticCacheEventBatch::new(),
                            ));
                        }
                        FlightResult::Failure(error) => return Err(error.restore()),
                        FlightResult::Cancelled => continue,
                    }
                }
                FlightRole::Fill(owner) => owner,
            };
            if let Some(value) = self.get_after_miss(&key, cohort) {
                owner.finish(FlightResult::Success(value.clone()));
                return Ok((
                    value,
                    StatementSource::RelationCache,
                    SemanticCacheEventBatch::new(),
                ));
            }
            match fill
                .take()
                .expect("physical materialization fill runs once")()
            .await
            {
                Ok(value) => return Ok(self.finish_physical_fill(&key, cohort, value, owner)),
                Err(error) => {
                    owner.finish(FlightResult::Failure(CachedError::capture(&error)));
                    return Err(error);
                }
            }
        }
    }

    fn finish_physical_fill(
        &self,
        key: &RelationCacheKey,
        cohort: Option<CohortToken>,
        value: CachedMaterialization,
        mut owner: FlightOwner<'_>,
    ) -> (
        Arc<CachedMaterialization>,
        StatementSource,
        SemanticCacheEventBatch,
    ) {
        self.record_fill(key.domain, value.work);
        let value = Arc::new(value);
        let (rows, keys) = value.observed_rows_and_keys();
        crate::telemetry::relation_cache_materialization_fill(
            key.domain.as_str(),
            value.work.execution,
            rows,
            keys,
        );
        crate::telemetry::relation_cache_materialization_result_size(
            key.domain.as_str(),
            value.result_bytes,
        );
        let decision = cohort.map(|cohort| {
            self.policy.observe_fill(
                cohort,
                value.work,
                value.result_bytes,
                key.retained_bytes().saturating_add(value.retained_bytes()),
                self.result_byte_limit,
                self.runtime.monotonic(),
            )
        });
        let (events, admitted) = if value.result_bytes > self.result_byte_limit {
            self.reject_too_large(key.domain);
            (
                self.semantic_events.admission(
                    key.domain.event_materialization(),
                    key.exact,
                    RelationCacheAdmissionResult::TooLarge,
                ),
                false,
            )
        } else {
            self.admit_after_policy(key.clone(), value.clone(), decision)
        };
        if let Some(cohort) = cohort {
            self.policy.observe_residency(cohort, admitted);
        }
        owner.finish(FlightResult::Success(value.clone()));
        (value, StatementSource::Executed, events)
    }

    fn get(
        &self,
        key: &RelationCacheKey,
        cohort: Option<CohortToken>,
    ) -> Option<Arc<CachedMaterialization>> {
        let relation = self.entries.get(key)?.value().clone();
        if let Some(cohort) = cohort {
            self.policy
                .observe_cache_hit(cohort, relation.work, self.runtime.monotonic());
        }
        self.metrics
            .domain(key.domain)
            .hits
            .fetch_add(1, Ordering::Relaxed);
        key.domain.lookup("hit");
        key.domain.avoided(relation.work);
        self.record_avoided(key.domain, relation.work);
        Some(relation)
    }

    fn get_after_miss(
        &self,
        key: &RelationCacheKey,
        cohort: Option<CohortToken>,
    ) -> Option<Arc<CachedMaterialization>> {
        let relation = self.entries.get(key)?.value().clone();
        if let Some(cohort) = cohort {
            self.policy
                .observe_cache_hit(cohort, relation.work, self.runtime.monotonic());
        }
        key.domain.avoided(relation.work);
        self.record_avoided(key.domain, relation.work);
        Some(relation)
    }

    fn observe_policy_request(&self, key: &RelationCacheKey) -> Option<CohortToken> {
        (self.config.policy.mode != RelationCachePolicyMode::Foyer)
            .then(|| self.policy.observe_request(key, self.runtime.monotonic()))
    }

    fn acquire_flight<'a>(&'a self, key: &RelationCacheKey) -> FlightRole<'a> {
        let (flight, receiver) = {
            let mut flights = self
                .flights
                .lock()
                .expect("relation cache flight lock poisoned");
            if let Some(flight) = flights.get(key) {
                (flight.clone(), Some(flight.subscribe()))
            } else {
                let flight = Arc::new(Flight::new());
                flights.insert(key.clone(), flight.clone());
                (flight, None)
            }
        };
        match receiver {
            Some(receiver) => FlightRole::Wait(receiver),
            None => FlightRole::Fill(FlightOwner::new(self, key.clone(), flight)),
        }
    }

    fn admit_after_policy(
        &self,
        key: RelationCacheKey,
        relation: Arc<CachedMaterialization>,
        decision: Option<AdmissionDecision>,
    ) -> (SemanticCacheEventBatch, bool) {
        if self.config.policy.mode == RelationCachePolicyMode::Enforced
            && decision.is_some_and(|decision| decision.outcome == AdmissionOutcome::Reject)
        {
            self.reject_by_rad_policy(key.domain);
            return (
                self.semantic_events.admission(
                    key.domain.event_materialization(),
                    key.exact,
                    RelationCacheAdmissionResult::PolicyRejected,
                ),
                false,
            );
        }
        self.admit(key, relation)
    }

    fn admit(
        &self,
        key: RelationCacheKey,
        relation: Arc<CachedMaterialization>,
    ) -> (SemanticCacheEventBatch, bool) {
        let domain = key.domain;
        let materialization = domain.event_materialization();
        let exact = key.exact;
        if relation.result_bytes > self.result_byte_limit {
            self.reject_too_large(domain);
            return (
                self.semantic_events.admission(
                    materialization,
                    exact,
                    RelationCacheAdmissionResult::TooLarge,
                ),
                false,
            );
        }
        let retained = key
            .retained_bytes()
            .saturating_add(relation.retained_bytes()) as u64;
        let weight = (retained as usize).max(self.entry_weight);
        let shards = self.entries.shards();
        let shard = self.entries.hash(&key) as usize % shards;
        let shard_capacity =
            self.limits.byte_limit / shards + usize::from(shard < self.limits.byte_limit % shards);
        // Foyer retains one item that is larger than its selected shard. That
        // behavior can make total retained bytes exceed the configured bound.
        // Reject the item before insertion so each shard stays within its
        // share of the total byte limit.
        if weight > shard_capacity {
            self.reject_by_capacity(domain);
            return (
                self.semantic_events.admission(
                    materialization,
                    exact,
                    RelationCacheAdmissionResult::PolicyRejected,
                ),
                false,
            );
        }
        // Foyer reports resident victims before insert returns. It reports a
        // rejected candidate when the returned entry drops. The capture lock
        // assigns both callback types to one admission.
        let capture = self.semantic_events.capture();
        let entry = self.entries.insert(key, relation);
        let admitted = !entry.is_outdated()
            && entry
                .value()
                .accounting_state
                .compare_exchange(
                    ACCOUNTING_PENDING,
                    ACCOUNTING_RETAINED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok();
        if !admitted {
            self.reject_by_capacity(domain);
        } else {
            self.metrics
                .retained_bytes
                .fetch_add(retained as i64, Ordering::Relaxed);
            let metrics = self.metrics.domain(domain);
            metrics.admissions.fetch_add(1, Ordering::Relaxed);
            metrics.entries.fetch_add(1, Ordering::Relaxed);
            metrics
                .retained_bytes
                .fetch_add(retained as i64, Ordering::Relaxed);
            domain.admission("admitted");
        }
        drop(entry);
        self.record_residency();
        let result = if admitted {
            RelationCacheAdmissionResult::Admitted
        } else {
            RelationCacheAdmissionResult::PolicyRejected
        };
        let events = capture.map_or_else(
            || {
                self.semantic_events
                    .admission(materialization, exact, result)
            },
            |capture| capture.finish(materialization, exact, result),
        );
        (events, admitted)
    }

    fn reject_too_large(&self, domain: MaterializationDomain) {
        self.metrics
            .domain(domain)
            .rejected_too_large
            .fetch_add(1, Ordering::Relaxed);
        domain.admission("too_large");
        crate::telemetry::relation_cache_admission_rejection_gate(
            domain.as_str(),
            self.config.policy.mode.as_str(),
            "hard_limit",
        );
    }

    fn record_fill(&self, domain: MaterializationDomain, work: CachedWork) {
        let work_units = policy::deterministic_work(work);
        let metrics = self.metrics.domain(domain);
        metrics.fills.fetch_add(1, Ordering::Relaxed);
        metrics
            .fill_work_units
            .fetch_add(work_units, Ordering::Relaxed);
    }

    fn record_avoided(&self, domain: MaterializationDomain, work: CachedWork) {
        let work_units = policy::deterministic_work(work);
        self.metrics
            .domain(domain)
            .avoided_work_units
            .fetch_add(work_units, Ordering::Relaxed);
    }

    fn reject_by_rad_policy(&self, domain: MaterializationDomain) {
        self.metrics
            .domain(domain)
            .rad_policy_rejections
            .fetch_add(1, Ordering::Relaxed);
        domain.admission("rad_policy_rejected");
        crate::telemetry::relation_cache_admission_rejection_gate(
            domain.as_str(),
            self.config.policy.mode.as_str(),
            "rad_policy",
        );
    }

    fn reject_by_capacity(&self, domain: MaterializationDomain) {
        self.metrics
            .domain(domain)
            .foyer_rejections
            .fetch_add(1, Ordering::Relaxed);
        domain.admission("capacity_rejected");
        crate::telemetry::relation_cache_admission_rejection_gate(
            domain.as_str(),
            self.config.policy.mode.as_str(),
            "capacity",
        );
    }

    fn record_residency(&self) {
        crate::telemetry::relation_cache_limits(
            self.limits.entry_limit,
            self.limits.byte_limit as u64,
            self.limits.result_byte_limit as u64,
        );
        crate::telemetry::relation_cache_residency(self.entries.entries(), self.retained_bytes());
    }

    fn retained_bytes(&self) -> u64 {
        self.metrics.retained_bytes.load(Ordering::Relaxed).max(0) as u64
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use async_trait::async_trait;
    use bytes::Bytes;
    use tokio::sync::Notify;

    use crate::engine::catalog::model::TableExistenceDependency;
    use crate::engine::exec::observe::{KvCounters, ObservedView};
    use crate::engine::kv::slatedb::Store;
    use crate::engine::kv::{
        IsolationLevel, KeyRange, KvIterator, KvView, TransactionView, TransactionalKv,
    };
    use crate::engine::lir::{Field, Kind, SlotId, Type};

    use super::*;

    fn fingerprint(seed: u8) -> Fingerprint {
        Fingerprint {
            canonicalization_version: 1,
            hash_algorithm: 1,
            digest: [seed; 16],
        }
    }

    fn dependencies(storage_generation: u64) -> CatalogDependencies {
        let mut dependencies = CatalogDependencies::default();
        dependencies.table_existence.push(TableExistenceDependency {
            table_id: "t1".into(),
            table_name: "items".into(),
            generation: 2.into(),
            storage_generation: storage_generation.into(),
        });
        dependencies
    }

    fn absent_table_dependencies() -> CatalogDependencies {
        let mut dependencies = dependencies(0);
        dependencies.table_existence[0].generation = 0.into();
        dependencies
    }

    fn complete_data_dependencies(dependencies: &CatalogDependencies) -> RelationDataDependencies {
        RelationDataDependencies {
            tables: dependencies
                .table_existence
                .iter()
                .map(
                    |dependency| crate::engine::planner::physical::TableDataDependency {
                        table_id: dependency.table_id.clone(),
                        scope: TableDataDependencyScope::All,
                    },
                )
                .collect(),
        }
    }

    #[test]
    fn one_primary_key_dependency_resolves_without_spilling() {
        let dependencies = RelationDataDependencies {
            tables: vec![crate::engine::planner::physical::TableDataDependency {
                table_id: "t1".into(),
                scope: TableDataDependencyScope::PrimaryKeys(vec![vec![Value::Text(
                    "item-0001".into(),
                )]]),
            }],
        };

        let resolved = ResolvedDataDependencies::resolve(&dependencies).unwrap();

        assert!(!resolved.0.spilled());
        let ResolvedTableDataDependencyScope::Stripes(stripes) = &resolved.0[0].scope else {
            panic!("expected selected generation stripes")
        };
        assert_eq!(stripes.len(), 1);
        assert!(!stripes.spilled());
    }

    #[test]
    fn empty_primary_key_dependency_is_rejected() {
        let dependencies = RelationDataDependencies {
            tables: vec![crate::engine::planner::physical::TableDataDependency {
                table_id: "t1".into(),
                scope: TableDataDependencyScope::PrimaryKeys(Vec::new()),
            }],
        };

        assert!(ResolvedDataDependencies::resolve(&dependencies).is_err());
    }

    #[test]
    fn duplicate_table_data_dependency_is_rejected() {
        let dependency = crate::engine::planner::physical::TableDataDependency {
            table_id: "t1".into(),
            scope: TableDataDependencyScope::PrimaryKeys(vec![vec![Value::Text(
                "item-0001".into(),
            )]]),
        };
        let dependencies = RelationDataDependencies {
            tables: vec![dependency.clone(), dependency],
        };

        assert!(ResolvedDataDependencies::resolve(&dependencies).is_err());
    }

    fn key(seed: u8, data_generation: u64) -> RelationCacheKey {
        RelationCacheKey::from_generations(
            fingerprint(seed),
            fingerprint(seed),
            &dependencies(3),
            &HashMap::from([("t1".into(), data_generation.into())]),
        )
    }

    fn validator_key() -> RelationCacheKey {
        RelationCacheKey::from_key_parts(
            fingerprint(1),
            fingerprint(1),
            vec![
                DependencyGeneration::Table {
                    table_id: "t1".into(),
                    existence_generation: 2.into(),
                    storage_generation: 3.into(),
                    data_generation: Arc::new(4.into()),
                },
                DependencyGeneration::Column {
                    table_id: "t1".into(),
                    column_id: "c1".into(),
                    generation: 5.into(),
                },
                DependencyGeneration::Index {
                    table_id: "t1".into(),
                    index_id: "i1".into(),
                    generation: 6.into(),
                },
                DependencyGeneration::WriteProtocol {
                    table_id: "t1".into(),
                    generation: 7.into(),
                },
            ],
        )
    }

    #[test]
    fn query_validator_covers_every_correctness_key_field() {
        let original = validator_key();
        let validator = original.query_validator();
        assert_eq!(validator, original.clone().query_validator());

        let changed = RelationCacheKey::from_key_parts(
            fingerprint(2),
            fingerprint(2),
            original.dependencies.to_vec(),
        );
        assert_ne!(validator, changed.query_validator());

        let replacements = [
            DependencyGeneration::Table {
                table_id: "t1".into(),
                existence_generation: 8.into(),
                storage_generation: 3.into(),
                data_generation: Arc::new(4.into()),
            },
            DependencyGeneration::Table {
                table_id: "t1".into(),
                existence_generation: 2.into(),
                storage_generation: 8.into(),
                data_generation: Arc::new(4.into()),
            },
            DependencyGeneration::Table {
                table_id: "t1".into(),
                existence_generation: 2.into(),
                storage_generation: 3.into(),
                data_generation: Arc::new(8.into()),
            },
        ];
        for replacement in replacements {
            let mut dependencies = original.dependencies.to_vec();
            dependencies[0] = replacement;
            let changed =
                RelationCacheKey::from_key_parts(original.exact, original.family, dependencies);
            assert_ne!(validator, changed.query_validator());
        }

        let mut dependencies = original.dependencies.to_vec();
        dependencies[1] = DependencyGeneration::Column {
            table_id: "t1".into(),
            column_id: "c1".into(),
            generation: 8.into(),
        };
        let changed =
            RelationCacheKey::from_key_parts(original.exact, original.family, dependencies);
        assert_ne!(validator, changed.query_validator());

        let mut dependencies = original.dependencies.to_vec();
        dependencies[2] = DependencyGeneration::Index {
            table_id: "t1".into(),
            index_id: "i1".into(),
            generation: 8.into(),
        };
        let changed =
            RelationCacheKey::from_key_parts(original.exact, original.family, dependencies);
        assert_ne!(validator, changed.query_validator());

        let mut dependencies = original.dependencies.to_vec();
        dependencies[3] = DependencyGeneration::WriteProtocol {
            table_id: "t1".into(),
            generation: 8.into(),
        };
        let changed =
            RelationCacheKey::from_key_parts(original.exact, original.family, dependencies);
        assert_ne!(validator, changed.query_validator());

        for index in 1..original.dependencies.len() {
            let mut dependencies = original.dependencies.to_vec();
            dependencies.remove(index);
            let changed =
                RelationCacheKey::from_key_parts(original.exact, original.family, dependencies);
            assert_ne!(validator, changed.query_validator());
        }
    }

    fn output(slot: usize) -> RowType {
        RowType {
            fields: vec![Field {
                name: "value".into(),
                slot: SlotId(slot),
                value_type: Type::scalar(Kind::Int64, false),
            }],
        }
    }

    fn frames(slot: usize, value: i64) -> Vec<Env> {
        let mut frame = Env::new();
        frame.insert(SlotId(slot), Datum::scalar(Value::Int64(value)));
        vec![frame]
    }

    fn text_frames(slot: usize, value: String) -> Vec<Env> {
        let mut frame = Env::new();
        frame.insert(SlotId(slot), Datum::scalar(Value::Text(value)));
        vec![frame]
    }

    fn empty_hash_build() -> CachedHashBuild {
        CachedHashBuild {
            hash_builder: ahash::RandomState::new(),
            entries: HashMap::new(),
            rows: Vec::new(),
            input_row_count: 0,
            execution_retained_bytes: 0,
        }
    }

    fn config() -> RelationCacheLimits {
        RelationCacheLimits {
            byte_limit: 64 * 1024,
            entry_limit: 16,
            result_byte_limit: 16 * 1024,
        }
    }

    fn cache_config() -> RelationCacheConfig {
        RelationCacheConfig {
            limits: config(),
            policy: RelationCachePolicyConfig {
                mode: RelationCachePolicyMode::Shadow,
                ..RelationCachePolicyConfig::default()
            },
            ..RelationCacheConfig::default()
        }
    }

    #[test]
    fn relation_cache_defaults_enforce_family_conversion_without_a_rate_prior() {
        let config = RelationCacheConfig::default();

        assert_eq!(config.policy.mode, RelationCachePolicyMode::Enforced);
        assert_eq!(
            config.policy.reuse_admission,
            RelationCacheReuseAdmission::FamilyConversion
        );
        assert_eq!(config.policy.prior, RelationCachePrior::None);
        assert_eq!(config.policy.probation_minimum_work_units, 4 * 1024 * 1024);
    }

    #[test]
    fn relation_cache_domains_parse_canonical_sets() {
        assert_eq!(
            RelationCacheDomains::default().to_string(),
            "query,hash-build,grouped-dimension"
        );
        assert_eq!("none".parse(), Ok(RelationCacheDomains::none()));
        assert_eq!(
            "query,grouped-dimension".parse::<RelationCacheDomains>(),
            Ok(RelationCacheDomains(
                RelationCacheDomains::QUERY | RelationCacheDomains::GROUPED_DIMENSION
            ))
        );
        assert!("query,unknown".parse::<RelationCacheDomains>().is_err());
        assert!("none,query".parse::<RelationCacheDomains>().is_err());
    }

    fn policy_config(mode: RelationCachePolicyMode) -> RelationCacheConfig {
        RelationCacheConfig {
            limits: config(),
            policy: RelationCachePolicyConfig {
                mode,
                reuse_admission: RelationCacheReuseAdmission::SecondTouch,
                probation_minimum_work_units: u64::MAX,
                ..RelationCachePolicyConfig::default()
            },
            ..RelationCacheConfig::default()
        }
    }

    struct SlowView<'a> {
        inner: &'a dyn KvView,
    }

    #[async_trait]
    impl KvView for SlowView<'_> {
        fn begin_position(&self) -> Option<&DataPosition> {
            self.inner.begin_position()
        }

        async fn get(&self, key: &[u8]) -> crate::engine::kv::Result<Option<Bytes>> {
            tokio::time::sleep(Duration::from_millis(10)).await;
            self.inner.get(key).await
        }

        async fn put(&self, key: Bytes, value: Bytes) -> crate::engine::kv::Result<()> {
            self.inner.put(key, value).await
        }

        async fn delete(&self, key: &[u8]) -> crate::engine::kv::Result<()> {
            self.inner.delete(key).await
        }

        async fn scan<'a>(
            &'a self,
            range: KeyRange,
        ) -> crate::engine::kv::Result<Box<dyn KvIterator + 'a>> {
            self.inner.scan(range).await
        }
    }

    #[tokio::test]
    async fn snapshot_dependency_keys_reuse_only_the_same_snapshot() {
        let store = Store::memory("relation-cache-snapshot-dependencies")
            .await
            .unwrap();
        let cache = RelationCache::new(cache_config());
        let dependencies = absent_table_dependencies();
        let data_dependencies = complete_data_dependencies(&dependencies);
        let pinned = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let pinned_view = TransactionView(&*pinned);
        let pinned_counters = KvCounters::new(false);
        let pinned_observed = ObservedView::new(&pinned_view, &pinned_counters);

        let first = cache
            .key_for_view(
                (fingerprint(1), fingerprint(1)),
                &pinned_observed,
                &dependencies,
                &data_dependencies,
                DependencyValidation::Snapshot,
                None,
            )
            .await
            .unwrap();
        let second = cache
            .key_for_view(
                (fingerprint(1), fingerprint(1)),
                &pinned_observed,
                &dependencies,
                &data_dependencies,
                DependencyValidation::Snapshot,
                None,
            )
            .await
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(pinned_counters.snapshot().gets, 1);
        assert_eq!(pinned_counters.snapshot().scans, 1);

        let writer = store
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        {
            let mut writer_view = TransactionView(&*writer);
            store::advance_table_data_generation(
                &mut writer_view,
                &TableId::from("t1"),
                [&b"row-1"[..]],
            )
            .await
            .unwrap();
        }
        writer.commit().await.unwrap();

        let current = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let current_view = TransactionView(&*current);
        let current_counters = KvCounters::new(false);
        let current_observed = ObservedView::new(&current_view, &current_counters);
        let third = cache
            .key_for_view(
                (fingerprint(1), fingerprint(1)),
                &current_observed,
                &dependencies,
                &data_dependencies,
                DependencyValidation::Snapshot,
                None,
            )
            .await
            .unwrap();
        assert_ne!(first, third);
        let old_again = cache
            .key_for_view(
                (fingerprint(1), fingerprint(1)),
                &pinned_observed,
                &dependencies,
                &data_dependencies,
                DependencyValidation::Snapshot,
                None,
            )
            .await
            .unwrap();
        assert_eq!(first, old_again);

        let stats = cache.stats();
        assert_eq!(stats.dependency_hits, 2);
        assert_eq!(stats.dependency_misses, 2);
        assert_eq!(pinned_counters.snapshot().gets, 1);
        assert_eq!(pinned_counters.snapshot().scans, 1);
        assert_eq!(current_counters.snapshot().gets, 1);
        assert_eq!(current_counters.snapshot().scans, 1);
        current.rollback();
        pinned.rollback();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn snapshot_dependency_validation_is_shared_across_exact_relations() {
        let store = Store::memory("relation-cache-shared-snapshot-dependencies")
            .await
            .unwrap();
        let cache = RelationCache::new(cache_config());
        let transaction = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let transaction_view = TransactionView(&*transaction);
        let counters = KvCounters::new(false);
        let view = ObservedView::new(&transaction_view, &counters);
        let dependencies = absent_table_dependencies();
        let data_dependencies = complete_data_dependencies(&dependencies);

        let first = cache
            .key_for_view(
                (fingerprint(1), fingerprint(1)),
                &view,
                &dependencies,
                &data_dependencies,
                DependencyValidation::Snapshot,
                None,
            )
            .await
            .unwrap();
        let second = cache
            .key_for_view(
                (fingerprint(2), fingerprint(2)),
                &view,
                &dependencies,
                &data_dependencies,
                DependencyValidation::Snapshot,
                None,
            )
            .await
            .unwrap();

        assert_ne!(first, second);
        assert_eq!(counters.snapshot().gets, 1);
        assert_eq!(counters.snapshot().scans, 1);
        assert_eq!(cache.stats().dependency_hits, 1);
        assert_eq!(cache.stats().dependency_misses, 1);

        transaction.rollback();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn transaction_dependency_validation_always_reads_fences() {
        let store = Store::memory("relation-cache-transaction-dependencies")
            .await
            .unwrap();
        let cache = RelationCache::new(cache_config());
        let transaction = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let transaction_view = TransactionView(&*transaction);
        let counters = KvCounters::new(false);
        let view = ObservedView::new(&transaction_view, &counters);
        let dependencies = absent_table_dependencies();
        let data_dependencies = complete_data_dependencies(&dependencies);
        for _ in 0..2 {
            cache
                .key_for_view(
                    (fingerprint(1), fingerprint(1)),
                    &view,
                    &dependencies,
                    &data_dependencies,
                    DependencyValidation::Transaction,
                    None,
                )
                .await
                .unwrap();
        }

        let stats = cache.stats();
        assert_eq!(stats.dependency_hits, 0);
        assert_eq!(stats.dependency_misses, 0);
        assert_eq!(counters.snapshot().gets, 2);
        assert_eq!(counters.snapshot().scans, 2);
        transaction.rollback();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn transaction_dependency_cache_reuses_and_clears_validation() {
        let store = Store::memory("relation-cache-transaction-dependency-cache")
            .await
            .unwrap();
        let cache = RelationCache::new(cache_config());
        let transaction = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let transaction_view = TransactionView(&*transaction);
        let counters = KvCounters::new(false);
        let view = ObservedView::new(&transaction_view, &counters);
        let dependencies = absent_table_dependencies();
        let data_dependencies = complete_data_dependencies(&dependencies);
        let transaction_dependencies = TransactionDependencyCache::default();

        let first = cache
            .key_for_view(
                (fingerprint(1), fingerprint(1)),
                &view,
                &dependencies,
                &data_dependencies,
                DependencyValidation::Transaction,
                Some(&transaction_dependencies),
            )
            .await
            .unwrap();
        let second = cache
            .key_for_view(
                (fingerprint(2), fingerprint(2)),
                &view,
                &dependencies,
                &data_dependencies,
                DependencyValidation::Transaction,
                Some(&transaction_dependencies),
            )
            .await
            .unwrap();
        assert_ne!(first, second);
        assert_eq!(counters.snapshot().gets, 1);
        assert_eq!(counters.snapshot().scans, 1);

        transaction_dependencies.clear();
        cache
            .key_for_view(
                (fingerprint(1), fingerprint(1)),
                &view,
                &dependencies,
                &data_dependencies,
                DependencyValidation::Transaction,
                Some(&transaction_dependencies),
            )
            .await
            .unwrap();
        assert_eq!(counters.snapshot().gets, 2);
        assert_eq!(counters.snapshot().scans, 2);

        transaction.rollback();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_snapshot_dependency_reads_share_one_validation() {
        let store = Store::memory("relation-cache-concurrent-dependencies")
            .await
            .unwrap();
        let cache = RelationCache::new(cache_config());
        let transaction = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let transaction_view = TransactionView(&*transaction);
        let counters = KvCounters::new(false);
        let observed = ObservedView::new(&transaction_view, &counters);
        let slow = SlowView { inner: &observed };
        let dependencies = absent_table_dependencies();
        let data_dependencies = complete_data_dependencies(&dependencies);

        let (first, second) = tokio::join!(
            cache.key_for_view(
                (fingerprint(2), fingerprint(2)),
                &slow,
                &dependencies,
                &data_dependencies,
                DependencyValidation::Snapshot,
                None,
            ),
            cache.key_for_view(
                (fingerprint(1), fingerprint(1)),
                &slow,
                &dependencies,
                &data_dependencies,
                DependencyValidation::Snapshot,
                None,
            )
        );
        assert_ne!(first.unwrap(), second.unwrap());
        assert_eq!(counters.snapshot().gets, 1);
        assert_eq!(counters.snapshot().scans, 1);
        assert_eq!(cache.stats().dependency_coalesced, 1);

        transaction.rollback();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn failed_snapshot_dependency_reads_are_not_stored() {
        let store = Store::memory("relation-cache-failed-dependencies")
            .await
            .unwrap();
        let cache = RelationCache::new(cache_config());
        let transaction = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let view = TransactionView(&*transaction);
        let dependencies = dependencies(0);
        let data_dependencies = complete_data_dependencies(&dependencies);
        for _ in 0..2 {
            assert!(
                cache
                    .key_for_view(
                        (fingerprint(1), fingerprint(1)),
                        &view,
                        &dependencies,
                        &data_dependencies,
                        DependencyValidation::Snapshot,
                        None,
                    )
                    .await
                    .is_err()
            );
        }
        assert_eq!(cache.stats().dependency_misses, 2);
        assert_eq!(cache.stats().dependency_hits, 0);

        transaction.rollback();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn hit_restores_values_to_current_slots() {
        let cache = RelationCache::new(cache_config());
        let cache_key = key(1, 1);
        cache
            .get_or_fill(cache_key.clone(), &output(0), || async {
                Ok((frames(0, 42), CachedWork::default()))
            })
            .await
            .unwrap();

        let hit = cache
            .get_or_fill(cache_key, &output(7), || async {
                Err(Error::message(ErrorKind::Internal, "fill must not run"))
            })
            .await
            .unwrap();
        assert_eq!(
            hit.shape(RootCardinality::Many, &output(7)).unwrap(),
            Datum::Array(vec![Datum::Object(vec![ObjectField {
                name: "value".into(),
                datum: Datum::scalar(Value::Int64(42)),
            }])])
        );
        let hit = hit.into_frames(&output(7));

        assert_eq!(
            hit[0].get(SlotId(7)),
            Some(&Datum::scalar(Value::Int64(42)))
        );
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().misses, 1);
        let policy = cache.policy.stats();
        assert_eq!(policy.total.requests, 2);
        assert_eq!(policy.total.reuse_opportunities, 1);
        assert_eq!(policy.total.cache_hits, 1);
        assert_eq!(policy.total.fills, 1);
    }

    #[test]
    fn candidate_size_matches_the_owned_materialization() {
        let output = output(0);
        let frames = text_frames(0, "value".repeat(20));
        let estimated = estimated_result_bytes(&output, &frames);
        let relation = CachedRelation::from_frames(&output, &frames);

        assert_eq!(
            estimated,
            retained_row_bytes(&relation.rows, relation.rows.capacity())
        );
    }

    #[test]
    fn key_changes_with_identity_data_and_physical_generations() {
        let first_dependencies = dependencies(3);
        let second_dependencies = dependencies(4);
        let data_generations = HashMap::from([("t1".into(), TableDataGeneration::test_value(7))]);
        let first = RelationCacheKey::from_generations(
            fingerprint(1),
            fingerprint(1),
            &first_dependencies,
            &data_generations,
        );

        assert_ne!(first, key(2, 7));
        assert_ne!(first, key(1, 8));
        assert_ne!(
            first,
            RelationCacheKey::from_generations(
                fingerprint(1),
                fingerprint(1),
                &second_dependencies,
                &data_generations,
            )
        );
    }

    #[test]
    fn subrelation_key_uses_only_its_dependency_generations() {
        let stable_dependencies = dependencies(3);
        let mut changing_dependencies = dependencies(3);
        changing_dependencies.table_existence[0].table_id = "t2".into();
        changing_dependencies.table_existence[0].table_name = "orders".into();
        let mut root_dependencies = stable_dependencies.clone();
        root_dependencies.merge(&changing_dependencies);
        let root = RelationCacheKey::from_generations(
            fingerprint(1),
            fingerprint(1),
            &root_dependencies,
            &HashMap::from([
                ("t1".into(), TableDataGeneration::test_value(7)),
                ("t2".into(), TableDataGeneration::test_value(10)),
            ]),
        );
        let advanced_root = RelationCacheKey::from_generations(
            fingerprint(1),
            fingerprint(1),
            &root_dependencies,
            &HashMap::from([
                ("t1".into(), TableDataGeneration::test_value(7)),
                ("t2".into(), TableDataGeneration::test_value(11)),
            ]),
        );

        let first = root
            .for_subrelation(
                fingerprint(2),
                fingerprint(2),
                &stable_dependencies,
                &complete_data_dependencies(&stable_dependencies),
            )
            .unwrap();
        let after_unrelated_change = advanced_root
            .for_subrelation(
                fingerprint(2),
                fingerprint(2),
                &stable_dependencies,
                &complete_data_dependencies(&stable_dependencies),
            )
            .unwrap();

        assert_eq!(first, after_unrelated_change);
        assert_eq!(first.domain, MaterializationDomain::SubrelationRowsV1);
        assert_ne!(first, root);
        assert_ne!(
            first,
            advanced_root
                .for_subrelation(
                    fingerprint(3),
                    fingerprint(3),
                    &stable_dependencies,
                    &complete_data_dependencies(&stable_dependencies),
                )
                .unwrap()
        );
    }

    #[test]
    fn policy_separates_query_results_from_row_subrelations() {
        let cache = RelationCache::default();
        let root = key(1, 7);
        let subrelation = root
            .for_subrelation(
                fingerprint(1),
                fingerprint(1),
                &dependencies(3),
                &complete_data_dependencies(&dependencies(3)),
            )
            .unwrap();

        cache.policy.observe_request(&root, Duration::ZERO);
        cache.policy.observe_request(&subrelation, Duration::ZERO);

        assert_eq!(cache.policy.stats().total.exact_entries, 2);
    }

    #[test]
    fn physical_subrelation_key_includes_its_representation() {
        let root = key(1, 7);
        let first = root
            .for_physical_subrelation(
                MaterializationDomain::HashJoinBuild,
                fingerprint(2),
                fingerprint(2),
                &dependencies(3),
                &complete_data_dependencies(&dependencies(3)),
                vec![1, 2],
            )
            .unwrap();
        let second = root
            .for_physical_subrelation(
                MaterializationDomain::HashJoinBuild,
                fingerprint(2),
                fingerprint(2),
                &dependencies(3),
                &complete_data_dependencies(&dependencies(3)),
                vec![1, 3],
            )
            .unwrap();

        assert_ne!(first, second);
        let cache = RelationCache::default();
        cache.policy.observe_request(&first, Duration::ZERO);
        cache.policy.observe_request(&second, Duration::ZERO);
        assert_eq!(cache.policy.stats().total.exact_entries, 2);
    }

    #[tokio::test]
    async fn oversized_results_are_not_admitted() {
        for mode in [
            RelationCachePolicyMode::Foyer,
            RelationCachePolicyMode::Shadow,
            RelationCachePolicyMode::Enforced,
        ] {
            let cache = RelationCache::new(RelationCacheConfig {
                limits: RelationCacheLimits {
                    result_byte_limit: 1,
                    ..config()
                },
                policy: RelationCachePolicyConfig {
                    mode,
                    ..RelationCachePolicyConfig::default()
                },
                ..RelationCacheConfig::default()
            });
            let cache_key = key(1, 1);
            for value in [1, 2] {
                cache
                    .get_or_fill(cache_key.clone(), &output(0), || async move {
                        Ok((frames(0, value), CachedWork::default()))
                    })
                    .await
                    .unwrap();
            }

            let stats = cache.stats();
            assert_eq!(stats.misses, 2);
            assert_eq!(stats.rejected_too_large, 2);
            assert_eq!(stats.entries, 0);
        }
    }

    #[tokio::test]
    async fn failed_fills_are_not_admitted() {
        let cache = RelationCache::new(cache_config());
        let cache_key = key(1, 1);
        for _ in 0..2 {
            let error = cache
                .get_or_fill(cache_key.clone(), &output(0), || async {
                    Err(Error::message(ErrorKind::Runtime, "expected failure"))
                })
                .await
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Runtime);
        }

        let stats = cache.stats();
        assert_eq!(stats.misses, 2);
        assert_eq!(stats.admissions, 0);
        assert_eq!(stats.entries, 0);
        let policy = cache.policy.stats();
        assert_eq!(policy.total.requests, 0);
        assert_eq!(policy.total.reuse_opportunities, 0);
        assert_eq!(policy.total.fills, 0);
    }

    #[tokio::test]
    async fn entry_limit_evicts_or_rejects_overflow() {
        let cache = RelationCache::new(RelationCacheConfig {
            limits: RelationCacheLimits {
                entry_limit: 2,
                ..config()
            },
            policy: RelationCachePolicyConfig {
                mode: RelationCachePolicyMode::Shadow,
                ..RelationCachePolicyConfig::default()
            },
            ..RelationCacheConfig::default()
        });
        for seed in 1..=8 {
            cache
                .get_or_fill(key(seed, 1), &output(0), || async {
                    Ok((frames(0, 1), CachedWork::default()))
                })
                .await
                .unwrap();
        }

        let stats = cache.stats();
        assert!(stats.entries <= 2);
        assert!(stats.evictions + stats.foyer_rejections > 0);
        assert!(stats.retained_bytes <= config().byte_limit as u64);
    }

    #[tokio::test]
    async fn byte_limit_evicts_or_rejects_overflow() {
        let byte_limit = 4 * 1024;
        let cache = RelationCache::new(RelationCacheConfig {
            limits: RelationCacheLimits {
                byte_limit,
                entry_limit: 4_096,
                result_byte_limit: 1024,
            },
            policy: RelationCachePolicyConfig {
                mode: RelationCachePolicyMode::Shadow,
                ..RelationCachePolicyConfig::default()
            },
            ..RelationCacheConfig::default()
        });
        for seed in 1..=64 {
            cache
                .get_or_fill(key(seed, 1), &output(0), || async move {
                    Ok((text_frames(0, "x".repeat(200)), CachedWork::default()))
                })
                .await
                .unwrap();
        }

        let stats = cache.stats();
        assert!(stats.entries < 64);
        assert!(stats.evictions + stats.foyer_rejections > 0);
        assert!(
            stats.retained_bytes <= byte_limit as u64,
            "relation cache exceeds its byte limit: {stats:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_fills_preserve_entry_and_byte_limits() {
        let limits = RelationCacheLimits {
            byte_limit: 4 * 1024,
            entry_limit: 4,
            result_byte_limit: 1024,
        };
        let cache = Arc::new(RelationCache::new(RelationCacheConfig {
            limits,
            ..RelationCacheConfig::default()
        }));
        let barrier = Arc::new(tokio::sync::Barrier::new(33));
        let mut fills = Vec::new();
        for seed in 0..32 {
            let cache = cache.clone();
            let barrier = barrier.clone();
            fills.push(tokio::spawn(async move {
                cache
                    .get_or_fill(key(seed, 1), &output(0), || async move {
                        barrier.wait().await;
                        Ok((text_frames(0, "x".repeat(200)), CachedWork::default()))
                    })
                    .await
            }));
        }
        barrier.wait().await;
        for fill in fills {
            fill.await.unwrap().unwrap();
        }

        let stats = cache.stats();
        assert!(stats.entries <= limits.entry_limit as u64, "{stats:?}");
        assert!(
            stats.retained_bytes <= limits.byte_limit as u64,
            "{stats:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_misses_share_one_fill() {
        let cache = Arc::new(RelationCache::new(cache_config()));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let fills = Arc::new(AtomicUsize::new(0));

        let first = {
            let cache = cache.clone();
            let started = started.clone();
            let release = release.clone();
            let fills = fills.clone();
            tokio::spawn(async move {
                cache
                    .get_or_fill(key(1, 1), &output(0), || async move {
                        fills.fetch_add(1, Ordering::Relaxed);
                        started.notify_one();
                        release.notified().await;
                        Ok((frames(0, 9), CachedWork::default()))
                    })
                    .await
            })
        };
        started.notified().await;
        let second = {
            let cache = cache.clone();
            let fills = fills.clone();
            tokio::spawn(async move {
                cache
                    .get_or_fill(key(1, 1), &output(0), || async move {
                        fills.fetch_add(1, Ordering::Relaxed);
                        Ok((frames(0, 10), CachedWork::default()))
                    })
                    .await
            })
        };
        tokio::task::yield_now().await;
        release.notify_waiters();

        assert_eq!(first.await.unwrap().unwrap().len(), 1);
        assert_eq!(second.await.unwrap().unwrap().len(), 1);
        assert_eq!(fills.load(Ordering::Relaxed), 1);
        assert_eq!(cache.stats().coalesced, 1);
        let policy = cache.policy.stats();
        assert_eq!(policy.total.requests, 2);
        assert_eq!(policy.total.reuse_opportunities, 0);
        assert_eq!(policy.total.coalesced_reuses, 1);
        assert_eq!(policy.total.fills, 1);
    }

    #[tokio::test]
    async fn foyer_mode_skips_rad_policy_evidence() {
        let cache = RelationCache::new(policy_config(RelationCachePolicyMode::Foyer));
        for value in [1, 2] {
            cache
                .get_or_fill(key(1, 1), &output(0), || async move {
                    Ok((frames(0, value), CachedWork::default()))
                })
                .await
                .unwrap();
        }

        assert_eq!(cache.stats().admissions, 1);
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.policy.stats().total.exact_entries, 0);
    }

    #[tokio::test]
    async fn shadow_mode_records_rejection_and_still_uses_foyer() {
        let cache = RelationCache::new(policy_config(RelationCachePolicyMode::Shadow));
        cache
            .get_or_fill(key(1, 1), &output(0), || async {
                Ok((frames(0, 1), CachedWork::default()))
            })
            .await
            .unwrap();
        assert_eq!(cache.policy.stats().total.reject_insufficient_value, 1);
        cache
            .get_or_fill(key(1, 1), &output(0), || async {
                Ok((frames(0, 2), CachedWork::default()))
            })
            .await
            .unwrap();

        assert_eq!(cache.stats().admissions, 1);
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.policy.stats().total.admit_second_touch, 1);
    }

    #[tokio::test]
    async fn foyer_and_shadow_modes_have_the_same_residency_behavior() {
        async fn exercise(mode: RelationCachePolicyMode) -> RelationCacheStatistics {
            let cache = RelationCache::new(policy_config(mode));
            for seed in [1, 1, 2, 3, 2] {
                cache
                    .get_or_fill(key(seed, 1), &output(0), || async move {
                        Ok((frames(0, i64::from(seed)), CachedWork::default()))
                    })
                    .await
                    .unwrap();
            }
            cache.stats()
        }

        let foyer = exercise(RelationCachePolicyMode::Foyer).await;
        let shadow = exercise(RelationCachePolicyMode::Shadow).await;
        assert_eq!(foyer.hits, shadow.hits);
        assert_eq!(foyer.misses, shadow.misses);
        assert_eq!(foyer.fills, shadow.fills);
        assert_eq!(foyer.admissions, shadow.admissions);
        assert_eq!(foyer.entries, shadow.entries);
        assert_eq!(foyer.retained_bytes, shadow.retained_bytes);
        assert_eq!(foyer.foyer_rejections, shadow.foyer_rejections);
    }

    #[tokio::test]
    async fn enforced_rejection_shares_the_result_then_admits_second_touch() {
        let cache = Arc::new(RelationCache::new(policy_config(
            RelationCachePolicyMode::Enforced,
        )));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let fills = Arc::new(AtomicUsize::new(0));
        let owner = {
            let cache = cache.clone();
            let started = started.clone();
            let release = release.clone();
            let fills = fills.clone();
            tokio::spawn(async move {
                cache
                    .get_or_fill(key(1, 1), &output(0), || async move {
                        fills.fetch_add(1, Ordering::Relaxed);
                        started.notify_one();
                        release.notified().await;
                        Ok((frames(0, 7), CachedWork::default()))
                    })
                    .await
            })
        };
        started.notified().await;
        let waiter = {
            let cache = cache.clone();
            let fills = fills.clone();
            tokio::spawn(async move {
                cache
                    .get_or_fill(key(1, 1), &output(0), || async move {
                        fills.fetch_add(1, Ordering::Relaxed);
                        Ok((frames(0, 8), CachedWork::default()))
                    })
                    .await
            })
        };
        tokio::task::yield_now().await;
        release.notify_waiters();

        assert_eq!(owner.await.unwrap().unwrap().len(), 1);
        assert_eq!(waiter.await.unwrap().unwrap().len(), 1);
        assert_eq!(fills.load(Ordering::Relaxed), 1);
        assert_eq!(cache.stats().admissions, 0);
        assert_eq!(cache.stats().rad_policy_rejections, 1);

        cache
            .get_or_fill(key(1, 1), &output(0), || {
                let fills = fills.clone();
                async move {
                    fills.fetch_add(1, Ordering::Relaxed);
                    Ok((frames(0, 9), CachedWork::default()))
                }
            })
            .await
            .unwrap();
        cache
            .get_or_fill(key(1, 1), &output(0), || async {
                panic!("the admitted second fill must be resident")
            })
            .await
            .unwrap();

        assert_eq!(fills.load(Ordering::Relaxed), 2);
        assert_eq!(cache.stats().admissions, 1);
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.policy.stats().total.admit_second_touch, 1);
    }

    #[tokio::test]
    async fn disabled_query_domain_bypasses_all_cache_activity() {
        let cache = RelationCache::new(RelationCacheConfig {
            domains: RelationCacheDomains::none(),
            ..policy_config(RelationCachePolicyMode::Enforced)
        });
        let fills = AtomicUsize::new(0);
        for value in [1, 2] {
            cache
                .get_or_fill(key(1, 1), &output(0), || async {
                    fills.fetch_add(1, Ordering::Relaxed);
                    Ok((frames(0, value), CachedWork::default()))
                })
                .await
                .unwrap();
        }

        assert_eq!(fills.load(Ordering::Relaxed), 2);
        assert_eq!(cache.stats(), RelationCacheStatistics::default());
        assert_eq!(
            cache.policy.stats(),
            RelationCachePolicyStatistics::default()
        );
    }

    #[tokio::test]
    async fn disabled_hash_build_domain_bypasses_all_cache_activity() {
        let cache = RelationCache::new(RelationCacheConfig {
            domains: "query".parse().unwrap(),
            ..policy_config(RelationCachePolicyMode::Enforced)
        });
        let fills = AtomicUsize::new(0);
        for _ in 0..2 {
            cache
                .get_or_fill_hash_build(
                    key(1, 1)
                        .for_physical_subrelation(
                            MaterializationDomain::HashJoinBuild,
                            fingerprint(2),
                            fingerprint(2),
                            &dependencies(3),
                            &complete_data_dependencies(&dependencies(3)),
                            vec![0],
                        )
                        .unwrap(),
                    || async {
                        fills.fetch_add(1, Ordering::Relaxed);
                        Ok((empty_hash_build(), CachedWork::default()))
                    },
                )
                .await
                .unwrap();
        }

        assert_eq!(fills.load(Ordering::Relaxed), 2);
        let statistics = cache.stats();
        assert_eq!(statistics.subrelation_hits, 0);
        assert_eq!(statistics.subrelation_misses, 0);
        assert_eq!(statistics.subrelation_fills, 0);
        assert_eq!(statistics.subrelation_admissions, 0);
        assert_eq!(statistics.subrelation_rad_policy_rejections, 0);
        assert_eq!(statistics.policy, RelationCachePolicyStatistics::default());
    }

    #[tokio::test]
    async fn concurrent_hash_build_misses_share_one_fill() {
        let cache = Arc::new(RelationCache::new(cache_config()));
        let cache_key = key(1, 1)
            .for_physical_subrelation(
                MaterializationDomain::HashJoinBuild,
                fingerprint(2),
                fingerprint(2),
                &dependencies(3),
                &complete_data_dependencies(&dependencies(3)),
                vec![1],
            )
            .unwrap();
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let fills = Arc::new(AtomicUsize::new(0));
        let first = {
            let cache = cache.clone();
            let cache_key = cache_key.clone();
            let started = started.clone();
            let release = release.clone();
            let fills = fills.clone();
            tokio::spawn(async move {
                cache
                    .get_or_fill_hash_build(cache_key, || async move {
                        fills.fetch_add(1, Ordering::Relaxed);
                        started.notify_one();
                        release.notified().await;
                        Ok((empty_hash_build(), CachedWork::default()))
                    })
                    .await
            })
        };
        started.notified().await;
        let second = {
            let cache = cache.clone();
            let fills = fills.clone();
            tokio::spawn(async move {
                cache
                    .get_or_fill_hash_build(cache_key, || async move {
                        fills.fetch_add(1, Ordering::Relaxed);
                        Ok((empty_hash_build(), CachedWork::default()))
                    })
                    .await
            })
        };
        tokio::task::yield_now().await;
        release.notify_waiters();

        let first = first.await.unwrap().unwrap();
        let second = second.await.unwrap().unwrap();
        assert_eq!(first.value.value().rows.len(), 0);
        assert_eq!(second.value.value().rows.len(), 0);
        assert_eq!(fills.load(Ordering::Relaxed), 1);
        assert_eq!(cache.stats().subrelation_coalesced, 1);
    }

    #[tokio::test]
    async fn oversized_hash_builds_are_not_admitted() {
        let cache = RelationCache::new(RelationCacheConfig {
            limits: RelationCacheLimits {
                result_byte_limit: 1,
                ..config()
            },
            ..RelationCacheConfig::default()
        });
        let cache_key = key(1, 1)
            .for_physical_subrelation(
                MaterializationDomain::HashJoinBuild,
                fingerprint(2),
                fingerprint(2),
                &dependencies(3),
                &complete_data_dependencies(&dependencies(3)),
                vec![1],
            )
            .unwrap();
        let fills = AtomicUsize::new(0);
        for _ in 0..2 {
            cache
                .get_or_fill_hash_build(cache_key.clone(), || async {
                    fills.fetch_add(1, Ordering::Relaxed);
                    Ok((empty_hash_build(), CachedWork::default()))
                })
                .await
                .unwrap();
        }

        assert_eq!(fills.load(Ordering::Relaxed), 2);
        let stats = cache.stats();
        assert_eq!(stats.subrelation_misses, 2);
        assert_eq!(stats.subrelation_rejected_too_large, 2);
        assert_eq!(stats.entries, 0);
    }

    #[tokio::test]
    async fn failed_hash_builds_are_not_admitted() {
        let cache = RelationCache::new(cache_config());
        let cache_key = key(1, 1)
            .for_physical_subrelation(
                MaterializationDomain::HashJoinBuild,
                fingerprint(2),
                fingerprint(2),
                &dependencies(3),
                &complete_data_dependencies(&dependencies(3)),
                vec![1],
            )
            .unwrap();
        for _ in 0..2 {
            let result = cache
                .get_or_fill_hash_build(cache_key.clone(), || async {
                    Err(Error::message(ErrorKind::Runtime, "expected failure"))
                })
                .await;
            let Err(error) = result else {
                panic!("failed hash build must return an error");
            };
            assert_eq!(error.kind(), ErrorKind::Runtime);
        }

        let stats = cache.stats();
        assert_eq!(stats.subrelation_misses, 2);
        assert_eq!(stats.subrelation_admissions, 0);
        assert_eq!(stats.entries, 0);
    }

    #[tokio::test]
    async fn concurrent_hash_build_fills_preserve_limits() {
        let limits = RelationCacheLimits {
            byte_limit: 4 * 1024,
            entry_limit: 4,
            result_byte_limit: 1024,
        };
        let cache = Arc::new(RelationCache::new(RelationCacheConfig {
            limits,
            ..RelationCacheConfig::default()
        }));
        let barrier = Arc::new(tokio::sync::Barrier::new(33));
        let mut fills = Vec::new();
        for seed in 0..32 {
            let cache = cache.clone();
            let barrier = barrier.clone();
            fills.push(tokio::spawn(async move {
                let cache_key = key(seed, 1)
                    .for_physical_subrelation(
                        MaterializationDomain::HashJoinBuild,
                        fingerprint(seed.wrapping_add(64)),
                        fingerprint(seed.wrapping_add(64)),
                        &dependencies(3),
                        &complete_data_dependencies(&dependencies(3)),
                        vec![seed],
                    )
                    .unwrap();
                cache
                    .get_or_fill_hash_build(cache_key, || async move {
                        barrier.wait().await;
                        Ok((empty_hash_build(), CachedWork::default()))
                    })
                    .await
            }));
        }
        barrier.wait().await;
        for fill in fills {
            fill.await.unwrap().unwrap();
        }

        let stats = cache.stats();
        assert!(stats.entries <= limits.entry_limit as u64, "{stats:?}");
        assert!(
            stats.retained_bytes <= limits.byte_limit as u64,
            "{stats:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_oversized_results_share_one_transient_copy() {
        let cache = Arc::new(RelationCache::new(RelationCacheConfig {
            limits: RelationCacheLimits {
                result_byte_limit: 1,
                ..config()
            },
            ..RelationCacheConfig::default()
        }));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let fills = Arc::new(AtomicUsize::new(0));

        let first = {
            let cache = cache.clone();
            let started = started.clone();
            let release = release.clone();
            let fills = fills.clone();
            tokio::spawn(async move {
                cache
                    .get_or_fill(key(1, 1), &output(0), || async move {
                        fills.fetch_add(1, Ordering::Relaxed);
                        started.notify_one();
                        release.notified().await;
                        Ok((frames(0, 9), CachedWork::default()))
                    })
                    .await
            })
        };
        started.notified().await;
        let second = {
            let cache = cache.clone();
            let fills = fills.clone();
            tokio::spawn(async move {
                cache
                    .get_or_fill(key(1, 1), &output(0), || async move {
                        fills.fetch_add(1, Ordering::Relaxed);
                        Ok((frames(0, 10), CachedWork::default()))
                    })
                    .await
            })
        };
        tokio::task::yield_now().await;
        release.notify_waiters();

        let first = first.await.unwrap().unwrap();
        let second = second.await.unwrap().unwrap();
        assert_eq!(
            first.into_frames(&output(0)),
            second.into_frames(&output(0))
        );
        assert_eq!(fills.load(Ordering::Relaxed), 1);
        assert_eq!(cache.stats().entries, 0);
        assert_eq!(cache.stats().rejected_too_large, 1);
        assert_eq!(cache.stats().policy.total.false_rejections, 0);
    }

    #[tokio::test]
    async fn cancelled_fill_releases_the_key() {
        let cache = Arc::new(RelationCache::new(cache_config()));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let first = {
            let cache = cache.clone();
            let started = started.clone();
            let release = release.clone();
            tokio::spawn(async move {
                cache
                    .get_or_fill(key(1, 1), &output(0), || async move {
                        started.notify_one();
                        release.notified().await;
                        Ok((frames(0, 1), CachedWork::default()))
                    })
                    .await
            })
        };
        started.notified().await;
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());

        let result = cache
            .get_or_fill(key(1, 1), &output(0), || async {
                Ok((frames(0, 2), CachedWork::default()))
            })
            .await
            .unwrap();
        let result = result.into_frames(&output(0));
        assert_eq!(
            result[0].get(SlotId(0)),
            Some(&Datum::scalar(Value::Int64(2)))
        );
        assert_eq!(cache.stats().admissions, 1);
    }
}

struct FlightOwner<'a> {
    cache: &'a RelationCache,
    key: RelationCacheKey,
    flight: Arc<Flight>,
    finished: bool,
}

impl<'a> FlightOwner<'a> {
    fn new(cache: &'a RelationCache, key: RelationCacheKey, flight: Arc<Flight>) -> Self {
        Self {
            cache,
            key,
            flight,
            finished: false,
        }
    }

    fn finish(&mut self, result: FlightResult) {
        self.flight.result.send_replace(Some(result));
        self.remove();
        self.finished = true;
    }

    fn detach(&mut self) -> usize {
        self.remove();
        self.finished = true;
        self.flight.waiters.load(Ordering::Acquire)
    }

    fn publish(&self, result: FlightResult) {
        debug_assert!(self.finished);
        self.flight.result.send_replace(Some(result));
    }

    fn remove(&self) {
        let mut flights = self
            .cache
            .flights
            .lock()
            .expect("relation cache flight lock poisoned");
        if flights
            .get(&self.key)
            .is_some_and(|flight| Arc::ptr_eq(flight, &self.flight))
        {
            flights.remove(&self.key);
        }
    }
}

impl Drop for FlightOwner<'_> {
    fn drop(&mut self) {
        if !self.finished {
            // Cancellation includes task abort and panic unwind. Wake all
            // waiters so one of them can become the next fill owner.
            self.flight
                .result
                .send_replace(Some(FlightResult::Cancelled));
            self.remove();
        }
    }
}

struct SnapshotFlightOwner<'a> {
    cache: &'a SnapshotDependencyCache,
    key: SnapshotDependencyKey,
    flight: Arc<SnapshotFlight>,
    finished: bool,
}

impl<'a> SnapshotFlightOwner<'a> {
    fn new(
        cache: &'a SnapshotDependencyCache,
        key: SnapshotDependencyKey,
        flight: Arc<SnapshotFlight>,
    ) -> Self {
        Self {
            cache,
            key,
            flight,
            finished: false,
        }
    }

    fn finish(&mut self, result: SnapshotFlightResult) {
        self.flight.result.send_replace(Some(result));
        self.remove();
        self.finished = true;
    }

    fn remove(&self) {
        let mut state = self
            .cache
            .state
            .lock()
            .expect("snapshot dependency cache lock poisoned");
        if state
            .flights
            .get(&self.key)
            .is_some_and(|flight| Arc::ptr_eq(flight, &self.flight))
        {
            state.flights.remove(&self.key);
        }
    }
}

impl Drop for SnapshotFlightOwner<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.flight
                .result
                .send_replace(Some(SnapshotFlightResult::Cancelled));
            self.remove();
        }
    }
}

#[derive(Debug)]
pub(super) struct RelationCacheResult {
    rows: RelationRows,
    pub source: StatementSource,
    events: SemanticCacheEventBatch,
}

// Keep a cache hit in slot-independent field order until the caller knows how
// it consumes the result. Final result shaping needs field order and names,
// but a later statement needs request-local execution slots. Restoring an Env
// before final shaping copies every datum twice.
#[derive(Debug)]
enum RelationRows {
    Frames(Vec<Env>),
    Cached(Arc<CachedMaterialization>),
}

impl RelationCacheResult {
    pub(super) fn executed(frames: Vec<Env>) -> Self {
        Self {
            rows: RelationRows::Frames(frames),
            source: StatementSource::Executed,
            events: SemanticCacheEventBatch::new(),
        }
    }

    pub(super) fn take_events(&mut self) -> SemanticCacheEventBatch {
        std::mem::take(&mut self.events)
    }

    pub(super) fn len(&self) -> usize {
        match &self.rows {
            RelationRows::Frames(frames) => frames.len(),
            RelationRows::Cached(relation) => relation.relation_ref().rows.len(),
        }
    }

    pub(super) fn shape(&self, cardinality: RootCardinality, output: &RowType) -> Result<Datum> {
        match &self.rows {
            RelationRows::Frames(frames) => super::shape_frames(cardinality, output, frames),
            RelationRows::Cached(relation) => relation.relation_ref().shape(cardinality, output),
        }
    }

    pub(super) fn into_frames(self, output: &RowType) -> Vec<Env> {
        match self.rows {
            RelationRows::Frames(frames) => frames,
            RelationRows::Cached(relation) => relation.relation_ref().restore(output),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RelationCacheDomainStatistics {
    pub hits: u64,
    pub misses: u64,
    pub fills: u64,
    pub fill_work_units: u64,
    pub avoided_work_units: u64,
    pub admissions: u64,
    pub rejected_too_large: u64,
    pub rad_policy_rejections: u64,
    pub foyer_rejections: u64,
    pub evictions: u64,
    pub coalesced: u64,
    pub entries: u64,
    pub retained_bytes: u64,
}

impl RelationCacheDomainStatistics {
    fn saturating_add(self, other: Self) -> Self {
        Self {
            hits: self.hits.saturating_add(other.hits),
            misses: self.misses.saturating_add(other.misses),
            fills: self.fills.saturating_add(other.fills),
            fill_work_units: self.fill_work_units.saturating_add(other.fill_work_units),
            avoided_work_units: self
                .avoided_work_units
                .saturating_add(other.avoided_work_units),
            admissions: self.admissions.saturating_add(other.admissions),
            rejected_too_large: self
                .rejected_too_large
                .saturating_add(other.rejected_too_large),
            rad_policy_rejections: self
                .rad_policy_rejections
                .saturating_add(other.rad_policy_rejections),
            foyer_rejections: self.foyer_rejections.saturating_add(other.foyer_rejections),
            evictions: self.evictions.saturating_add(other.evictions),
            coalesced: self.coalesced.saturating_add(other.coalesced),
            entries: self.entries.saturating_add(other.entries),
            retained_bytes: self.retained_bytes.saturating_add(other.retained_bytes),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RelationCacheDomainStatisticsSet {
    pub query: RelationCacheDomainStatistics,
    pub hash_build: RelationCacheDomainStatistics,
    pub grouped_dimension: RelationCacheDomainStatistics,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RelationCacheStatistics {
    pub hits: u64,
    pub misses: u64,
    pub fills: u64,
    pub fill_work_units: u64,
    pub avoided_work_units: u64,
    pub admissions: u64,
    pub rejected_too_large: u64,
    pub rad_policy_rejections: u64,
    pub foyer_rejections: u64,
    pub evictions: u64,
    pub coalesced: u64,
    pub entries: u64,
    pub retained_bytes: u64,
    pub dependency_hits: u64,
    pub dependency_misses: u64,
    pub dependency_coalesced: u64,
    pub dependency_evictions: u64,
    pub subrelation_hits: u64,
    pub subrelation_misses: u64,
    pub subrelation_fills: u64,
    pub subrelation_fill_work_units: u64,
    pub subrelation_avoided_work_units: u64,
    pub subrelation_admissions: u64,
    pub subrelation_rejected_too_large: u64,
    pub subrelation_rad_policy_rejections: u64,
    pub subrelation_foyer_rejections: u64,
    pub subrelation_evictions: u64,
    pub subrelation_coalesced: u64,
    pub catalog_hits: u64,
    pub catalog_misses: u64,
    pub catalog_coalesced: u64,
    pub catalog_evictions: u64,
    pub catalog_entries: u64,
    pub catalog_retained_bytes: u64,
    pub prepared_hits: u64,
    pub prepared_misses: u64,
    pub prepared_coalesced: u64,
    pub prepared_admissions: u64,
    pub prepared_evictions: u64,
    pub prepared_rejected_too_large: u64,
    pub prepared_superseded: u64,
    pub prepared_entries: u64,
    pub prepared_retained_bytes: u64,
    pub domains: RelationCacheDomainStatisticsSet,
    pub policy: RelationCachePolicyStatistics,
}

impl RelationCache {
    pub(super) fn stats(&self) -> RelationCacheStatistics {
        let catalog = self.snapshot_catalog.stats();
        let prepared = self.prepared_reads.stats();
        let domains = RelationCacheDomainStatisticsSet {
            query: self.metrics.query.snapshot(),
            hash_build: self.metrics.hash_build.snapshot(),
            grouped_dimension: self.metrics.grouped_dimension.snapshot(),
        };
        let query = domains.query;
        let subrelation = domains.hash_build.saturating_add(domains.grouped_dimension);
        RelationCacheStatistics {
            hits: query.hits,
            misses: query.misses,
            fills: query.fills,
            fill_work_units: query.fill_work_units,
            avoided_work_units: query.avoided_work_units,
            admissions: query.admissions,
            rejected_too_large: query.rejected_too_large,
            rad_policy_rejections: query.rad_policy_rejections,
            foyer_rejections: query.foyer_rejections,
            evictions: query.evictions,
            coalesced: query.coalesced,
            entries: self.entries.entries() as u64,
            retained_bytes: self.retained_bytes(),
            dependency_hits: self.metrics.dependency_hits.load(Ordering::Relaxed),
            dependency_misses: self.metrics.dependency_misses.load(Ordering::Relaxed),
            dependency_coalesced: self.metrics.dependency_coalesced.load(Ordering::Relaxed),
            dependency_evictions: self.metrics.dependency_evictions.load(Ordering::Relaxed),
            subrelation_hits: subrelation.hits,
            subrelation_misses: subrelation.misses,
            subrelation_fills: subrelation.fills,
            subrelation_fill_work_units: subrelation.fill_work_units,
            subrelation_avoided_work_units: subrelation.avoided_work_units,
            subrelation_admissions: subrelation.admissions,
            subrelation_rejected_too_large: subrelation.rejected_too_large,
            subrelation_rad_policy_rejections: subrelation.rad_policy_rejections,
            subrelation_foyer_rejections: subrelation.foyer_rejections,
            subrelation_evictions: subrelation.evictions,
            subrelation_coalesced: subrelation.coalesced,
            catalog_hits: catalog.hits,
            catalog_misses: catalog.misses,
            catalog_coalesced: catalog.coalesced,
            catalog_evictions: catalog.evictions,
            catalog_entries: catalog.entries,
            catalog_retained_bytes: catalog.retained_bytes,
            prepared_hits: prepared.hits,
            prepared_misses: prepared.misses,
            prepared_coalesced: prepared.coalesced,
            prepared_admissions: prepared.admissions,
            prepared_evictions: prepared.evictions,
            prepared_rejected_too_large: prepared.rejected_too_large,
            prepared_superseded: prepared.superseded,
            prepared_entries: prepared.entries,
            prepared_retained_bytes: prepared.retained_bytes,
            domains,
            policy: self.policy.stats(),
        }
    }
}
