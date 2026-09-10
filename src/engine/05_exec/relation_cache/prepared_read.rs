use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use crate::engine::catalog::identity::{DefinitionGeneration, TableId};
use crate::engine::exec::{ErrorKind, Result};
use crate::engine::kv::{DataPosition, KvView};
use crate::engine::lir;
use crate::engine::lir::fingerprint::{Fingerprint, QueryFingerprints};
use crate::engine::planner::bind::BoundStatement;
use crate::engine::planner::memo::MemoLimits;
use crate::engine::planner::models::PlannerStats;
use crate::engine::planner::{PlanOptions, PlannerMode};

use super::{
    CachedError, CatalogDependencyGeneration, DependencyValidation, RelationCache, RelationCacheKey,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct PlanningKey {
    full_scan_only: bool,
    mode: u8,
    hash_join_memory_limit_bytes: u64,
    max_groups: u32,
    max_alternatives_per_group: u32,
    max_total_alternatives: u32,
    max_rule_applications: u32,
    max_planning_effort: u32,
}

impl From<PlanOptions> for PlanningKey {
    fn from(options: PlanOptions) -> Self {
        let MemoLimits {
            max_groups,
            max_alternatives_per_group,
            max_total_alternatives,
            max_rule_applications,
            max_planning_effort,
        } = options.memo_limits;
        Self {
            full_scan_only: options.full_scan_only,
            mode: match options.mode {
                PlannerMode::Structural => 1,
                PlannerMode::Cost => 2,
            },
            hash_join_memory_limit_bytes: options.hash_join_memory_limit_bytes,
            max_groups,
            max_alternatives_per_group,
            max_total_alternatives,
            max_rule_applications,
            max_planning_effort,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RequestKey {
    request: Fingerprint,
    // Statistics can change the selected plan without changing query meaning.
    // Exact identity makes publication a deterministic refresh boundary.
    statistics: Option<String>,
    planning: PlanningKey,
}

impl RequestKey {
    fn new(request: Fingerprint, statistics: Option<&PlannerStats>, options: PlanOptions) -> Self {
        Self {
            request,
            statistics: statistics.map(|statistics| statistics.snapshot_identity.clone()),
            planning: options.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct FlightKey {
    request: RequestKey,
    // Flight sharing includes the pinned position because the owner validates
    // dependencies through its view. Requests at different positions must not
    // share that validation result.
    position: DataPosition,
}

pub(in crate::engine::exec) struct PreparedRead {
    pub(in crate::engine::exec) statement: Arc<BoundStatement>,
    pub(in crate::engine::exec) fingerprints: Arc<QueryFingerprints>,
}

#[derive(Clone)]
pub(in crate::engine::exec) struct PreparedReadResult {
    pub(in crate::engine::exec) prepared: Arc<PreparedRead>,
    pub(in crate::engine::exec) relation_key: RelationCacheKey,
}

struct Entry {
    id: u64,
    // Physical dependencies do not contain the complete table definition.
    // Table definitions also protect binding rules such as the output of a
    // scan with no explicit projection.
    dependencies: Vec<CatalogDependencyGeneration>,
    table_definitions: Vec<TableDefinitionDependency>,
    prepared: Arc<PreparedRead>,
    retained_bytes: usize,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct TableDefinitionDependency {
    id: TableId,
    name: String,
    generation: DefinitionGeneration,
}

#[derive(Clone)]
enum FlightResult {
    Success(PreparedReadResult),
    Failure(CachedError),
    Cancelled,
}

struct Flight {
    result: watch::Sender<Option<FlightResult>>,
}

impl Flight {
    fn new() -> Self {
        let (result, _) = watch::channel(None);
        Self { result }
    }

    async fn wait(mut receiver: watch::Receiver<Option<FlightResult>>) -> FlightResult {
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

#[derive(Default)]
struct State {
    entries: HashMap<RequestKey, Vec<Arc<Entry>>>,
    insertion_order: VecDeque<(RequestKey, u64)>,
    flights: HashMap<FlightKey, Arc<Flight>>,
    entry_count: usize,
    retained_bytes: usize,
    next_id: u64,
}

#[derive(Default)]
struct Metrics {
    hits: AtomicU64,
    misses: AtomicU64,
    coalesced: AtomicU64,
    admissions: AtomicU64,
    evictions: AtomicU64,
    rejected_too_large: AtomicU64,
    superseded: AtomicU64,
}

pub(super) struct PreparedReadCache {
    state: Mutex<State>,
    metrics: Metrics,
    entry_limit: usize,
    byte_limit: usize,
    plan_byte_limit: usize,
}

impl PreparedReadCache {
    pub(super) fn new(entry_limit: usize, byte_limit: usize) -> Self {
        let entry_limit = entry_limit.max(1);
        let byte_limit = byte_limit.max(1);
        // One plan can use at most one eighth of this auxiliary cache. This
        // rule prevents one complex request from removing the complete plan
        // working set. The limit depends only on configured resource bounds.
        let plan_byte_limit = byte_limit.div_ceil(8).max(1);
        crate::telemetry::relation_cache_prepared_limits(
            entry_limit as u64,
            byte_limit as u64,
            plan_byte_limit as u64,
        );
        crate::telemetry::relation_cache_prepared_residency(0, 0);
        Self {
            state: Mutex::new(State::default()),
            metrics: Metrics::default(),
            entry_limit,
            byte_limit,
            plan_byte_limit,
        }
    }

    pub(super) async fn get_or_prepare<F, Fut>(
        &self,
        relation_cache: &RelationCache,
        view: &dyn KvView,
        query: &lir::Query,
        statistics: Option<&PlannerStats>,
        options: PlanOptions,
        prepare: F,
    ) -> Result<PreparedReadResult>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<BoundStatement>>,
    {
        let Some(position) = view.begin_position().cloned() else {
            crate::telemetry::relation_cache_prepared_lookup("unpositioned");
            return Self::prepare_uncached(relation_cache, view, prepare).await;
        };
        let (request, request_bytes) = lir::fingerprint::request_with_size(query);
        let request = RequestKey::new(request, statistics, options);
        if let Some(result) = self
            .find_valid(relation_cache, view, &request, true)
            .await?
        {
            return Ok(result);
        }
        self.metrics.misses.fetch_add(1, Ordering::Relaxed);
        crate::telemetry::relation_cache_prepared_lookup("miss");
        let flight_key = FlightKey {
            request: request.clone(),
            position,
        };
        let mut prepare = Some(prepare);
        loop {
            let (flight, owner, receiver) = {
                let mut state = self
                    .state
                    .lock()
                    .expect("prepared read cache lock poisoned");
                if let Some(flight) = state.flights.get(&flight_key) {
                    (flight.clone(), false, Some(flight.result.subscribe()))
                } else {
                    let flight = Arc::new(Flight::new());
                    state.flights.insert(flight_key.clone(), flight.clone());
                    (flight, true, None)
                }
            };
            if !owner {
                self.metrics.coalesced.fetch_add(1, Ordering::Relaxed);
                crate::telemetry::relation_cache_prepared_lookup("coalesced");
                match Flight::wait(receiver.expect("a prepared read waiter has a receiver")).await {
                    FlightResult::Success(result) => {
                        crate::telemetry::relation_cache_prepared_avoided();
                        return Ok(result);
                    }
                    FlightResult::Failure(error) => return Err(error.restore()),
                    FlightResult::Cancelled => continue,
                }
            }
            let mut owner = FlightOwner::new(self, flight_key.clone(), flight);
            // A different request can insert a valid catalog variant before
            // this request takes flight ownership. Validate again before bind.
            match self.find_valid(relation_cache, view, &request, false).await {
                Ok(Some(result)) => {
                    owner.finish(FlightResult::Success(result.clone()));
                    return Ok(result);
                }
                Ok(None) => {}
                Err(error) => {
                    owner.finish(FlightResult::Failure(CachedError::capture(&error)));
                    return Err(error);
                }
            }
            let result = prepare.take().expect("prepared read fill runs once")().await;
            match result {
                Ok(statement) => {
                    let plan = statement
                        .plan
                        .as_ref()
                        .expect("a production prepared read has a physical plan");
                    let fingerprints = Arc::new(lir::fingerprint::query(&statement.bound));
                    let relation_key = match relation_cache
                        .key_for_view(
                            fingerprints.exact,
                            view,
                            &plan.dependencies,
                            DependencyValidation::Snapshot,
                        )
                        .await
                    {
                        Ok(key) => key,
                        Err(error) => {
                            owner.finish(FlightResult::Failure(CachedError::capture(&error)));
                            return Err(error);
                        }
                    };
                    let prepared = Arc::new(PreparedRead {
                        statement: Arc::new(statement),
                        fingerprints,
                    });
                    self.insert(request.clone(), request_bytes, prepared.clone());
                    let result = PreparedReadResult {
                        prepared,
                        relation_key,
                    };
                    owner.finish(FlightResult::Success(result.clone()));
                    return Ok(result);
                }
                Err(error) => {
                    owner.finish(FlightResult::Failure(CachedError::capture(&error)));
                    return Err(error);
                }
            }
        }
    }

    async fn prepare_uncached<F, Fut>(
        relation_cache: &RelationCache,
        view: &dyn KvView,
        prepare: F,
    ) -> Result<PreparedReadResult>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<BoundStatement>>,
    {
        let statement = prepare().await?;
        let plan = statement
            .plan
            .as_ref()
            .expect("a production prepared read has a physical plan");
        let fingerprints = Arc::new(lir::fingerprint::query(&statement.bound));
        let relation_key = relation_cache
            .key_for_view(
                fingerprints.exact,
                view,
                &plan.dependencies,
                DependencyValidation::Snapshot,
            )
            .await?;
        Ok(PreparedReadResult {
            prepared: Arc::new(PreparedRead {
                statement: Arc::new(statement),
                fingerprints,
            }),
            relation_key,
        })
    }

    async fn find_valid(
        &self,
        relation_cache: &RelationCache,
        view: &dyn KvView,
        request: &RequestKey,
        record_hit: bool,
    ) -> Result<Option<PreparedReadResult>> {
        let candidates = self
            .state
            .lock()
            .expect("prepared read cache lock poisoned")
            .entries
            .get(request)
            .cloned()
            .unwrap_or_default();
        for entry in candidates.iter().rev() {
            // Definition validation must occur before the plan is used. A new
            // column can change scan output without changing any dependency
            // already present in the stored physical plan.
            let mut definitions_match = true;
            for dependency in &entry.table_definitions {
                if !relation_cache
                    .catalog_table_matches_snapshot(
                        view,
                        &dependency.name,
                        &dependency.id,
                        dependency.generation,
                    )
                    .await?
                {
                    definitions_match = false;
                    break;
                }
            }
            if !definitions_match {
                if record_hit {
                    self.metrics.superseded.fetch_add(1, Ordering::Relaxed);
                    crate::telemetry::relation_cache_prepared_lookup("superseded");
                }
                continue;
            }
            let plan = entry
                .prepared
                .statement
                .plan
                .as_ref()
                .expect("a cached prepared read has a physical plan");
            match relation_cache
                .key_for_view(
                    entry.prepared.fingerprints.exact,
                    view,
                    &plan.dependencies,
                    DependencyValidation::Snapshot,
                )
                .await
            {
                Ok(relation_key) => {
                    if record_hit {
                        self.metrics.hits.fetch_add(1, Ordering::Relaxed);
                        crate::telemetry::relation_cache_prepared_lookup("hit");
                        crate::telemetry::relation_cache_prepared_avoided();
                    }
                    return Ok(Some(PreparedReadResult {
                        prepared: entry.prepared.clone(),
                        relation_key,
                    }));
                }
                Err(error) if error.kind() == ErrorKind::Conflict => {
                    if record_hit {
                        self.metrics.superseded.fetch_add(1, Ordering::Relaxed);
                        crate::telemetry::relation_cache_prepared_lookup("superseded");
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }

    fn insert(&self, request: RequestKey, request_bytes: usize, prepared: Arc<PreparedRead>) {
        let plan = prepared
            .statement
            .plan
            .as_ref()
            .expect("a cached prepared read has a physical plan");
        let dependencies = CatalogDependencyGeneration::collect(&plan.dependencies);
        let table_definitions = table_definitions(&prepared.statement.bound);
        let retained_bytes = retained_bytes(&request, request_bytes, &prepared);
        crate::telemetry::relation_cache_prepared_candidate(retained_bytes as u64);
        if retained_bytes > self.plan_byte_limit {
            self.metrics
                .rejected_too_large
                .fetch_add(1, Ordering::Relaxed);
            crate::telemetry::relation_cache_prepared_admission("too_large");
            return;
        }
        let mut state = self
            .state
            .lock()
            .expect("prepared read cache lock poisoned");
        if state.entries.get(&request).is_some_and(|entries| {
            entries.iter().any(|entry| {
                entry.dependencies == dependencies && entry.table_definitions == table_definitions
            })
        }) {
            return;
        }
        let mut evictions = 0u64;
        // FIFO uses only request event order. It keeps deterministic behavior
        // and permits old and current catalog variants to overlap in memory.
        while state.entry_count >= self.entry_limit
            || state.retained_bytes.saturating_add(retained_bytes) > self.byte_limit
        {
            let (old_request, old_id) = state
                .insertion_order
                .pop_front()
                .expect("prepared read insertion order is complete");
            let mut removed_bytes = 0;
            let mut remove_request = false;
            if let Some(entries) = state.entries.get_mut(&old_request)
                && let Some(position) = entries.iter().position(|entry| entry.id == old_id)
            {
                removed_bytes = entries.remove(position).retained_bytes;
                remove_request = entries.is_empty();
            }
            if remove_request {
                state.entries.remove(&old_request);
            }
            if removed_bytes > 0 {
                state.entry_count = state.entry_count.saturating_sub(1);
                state.retained_bytes = state.retained_bytes.saturating_sub(removed_bytes);
                evictions = evictions.saturating_add(1);
            }
        }
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1);
        state
            .entries
            .entry(request.clone())
            .or_default()
            .push(Arc::new(Entry {
                id,
                dependencies,
                table_definitions,
                prepared,
                retained_bytes,
            }));
        state.insertion_order.push_back((request, id));
        state.entry_count = state.entry_count.saturating_add(1);
        state.retained_bytes = state.retained_bytes.saturating_add(retained_bytes);
        self.metrics.admissions.fetch_add(1, Ordering::Relaxed);
        crate::telemetry::relation_cache_prepared_admission("admitted");
        if evictions > 0 {
            self.metrics
                .evictions
                .fetch_add(evictions, Ordering::Relaxed);
            crate::telemetry::relation_cache_prepared_eviction(evictions);
        }
        crate::telemetry::relation_cache_prepared_residency(
            state.entry_count as u64,
            state.retained_bytes as u64,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::sync::atomic::AtomicUsize;

    use tokio::sync::Notify;

    use crate::engine::exec::Error;
    use crate::engine::exec::relation_cache::RelationCacheLimits;
    use crate::engine::kv::slatedb::Store;
    use crate::engine::kv::{IsolationLevel, TransactionView, TransactionalKv};
    use crate::engine::lir::{Kind, RawScalar, Relation, RootCardinality, RowsColumn};
    use crate::engine::planner::bind::{ProgramBinder, ProgramStatement};

    use super::*;

    fn query() -> lir::Query {
        query_with_value("value")
    }

    fn query_with_value(value: &str) -> lir::Query {
        lir::Query {
            root: Relation::Rows {
                scope: "row".into(),
                columns: vec![RowsColumn {
                    name: "value".into(),
                    kind: Kind::Text,
                    nullable: false,
                }],
                values: vec![vec![RawScalar::Text(value.into())]],
            },
            cardinality: RootCardinality::ExactlyOne,
            bindings: HashMap::new(),
        }
    }

    async fn prepared_statement(view: &dyn KvView, query: lir::Query) -> BoundStatement {
        let mut binder = ProgramBinder::new(vec!["read".into()]).unwrap();
        binder
            .bind(
                &crate::engine::exec::engine::ViewCatalog {
                    view,
                    relation_cache: None,
                },
                ProgramStatement {
                    name: "read".into(),
                    relation: query,
                    mutation: None,
                },
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_bind_and_plan() {
        let store = Arc::new(Store::memory("prepared-read-concurrent").await.unwrap());
        let transaction = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let view = TransactionView(&*transaction);
        let query = query();
        let prepared = Arc::new(prepared_statement(&view, query.clone()).await);
        let cache = RelationCache::new(RelationCacheLimits::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let requests = (0..16)
            .map(|_| {
                let prepared = prepared.clone();
                let calls = calls.clone();
                let cache = &cache;
                let view = &view;
                let query = &query;
                async move {
                    cache
                        .get_or_prepare_read(view, query, None, PlanOptions::default(), || async {
                            calls.fetch_add(1, Ordering::Relaxed);
                            for _ in 0..16 {
                                tokio::task::yield_now().await;
                            }
                            Ok((*prepared).clone())
                        })
                        .await
                        .unwrap()
                }
            })
            .collect::<Vec<_>>();

        let results = futures::future::join_all(requests).await;

        assert_eq!(results.len(), 16);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let stats = cache.stats();
        assert_eq!(stats.prepared_misses, 16);
        assert_eq!(stats.prepared_coalesced, 15);
        assert_eq!(stats.prepared_admissions, 1);
        transaction.rollback();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn changed_literals_use_distinct_prepared_reads() {
        let store = Arc::new(Store::memory("prepared-read-literals").await.unwrap());
        let transaction = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let view = TransactionView(&*transaction);
        let first_query = query_with_value("first");
        let second_query = query_with_value("second");
        let first = prepared_statement(&view, first_query.clone()).await;
        let second = prepared_statement(&view, second_query.clone()).await;
        let cache = RelationCache::new(RelationCacheLimits::default());
        let calls = AtomicUsize::new(0);

        for (query, prepared) in [(&first_query, first), (&second_query, second)] {
            cache
                .get_or_prepare_read(&view, query, None, PlanOptions::default(), || async {
                    calls.fetch_add(1, Ordering::Relaxed);
                    Ok(prepared)
                })
                .await
                .unwrap();
        }

        let stats = cache.stats();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(stats.prepared_misses, 2);
        assert_eq!(stats.prepared_admissions, 2);
        assert_eq!(stats.prepared_entries, 2);
        transaction.rollback();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn failed_preparations_are_not_retained() {
        let store = Arc::new(Store::memory("prepared-read-failure").await.unwrap());
        let transaction = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let view = TransactionView(&*transaction);
        let query = query();
        let cache = RelationCache::new(RelationCacheLimits::default());
        let calls = AtomicUsize::new(0);

        for _ in 0..2 {
            let error = cache
                .get_or_prepare_read(&view, &query, None, PlanOptions::default(), || async {
                    calls.fetch_add(1, Ordering::Relaxed);
                    Err(Error::message(ErrorKind::Runtime, "expected failure"))
                })
                .await
                .err()
                .expect("preparation fails");
            assert_eq!(error.kind(), ErrorKind::Runtime);
        }

        let stats = cache.stats();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(stats.prepared_misses, 2);
        assert_eq!(stats.prepared_admissions, 0);
        assert_eq!(stats.prepared_entries, 0);
        transaction.rollback();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn prepared_read_entry_limit_evicts_old_requests() {
        let store = Arc::new(Store::memory("prepared-read-eviction").await.unwrap());
        let transaction = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let view = TransactionView(&*transaction);
        let first_query = query_with_value("first");
        let second_query = query_with_value("second");
        let first = prepared_statement(&view, first_query.clone()).await;
        let second = prepared_statement(&view, second_query.clone()).await;
        let cache = RelationCache::new(RelationCacheLimits {
            entry_limit: 1,
            ..RelationCacheLimits::default()
        });
        let calls = AtomicUsize::new(0);

        for (query, prepared) in [
            (&first_query, first.clone()),
            (&second_query, second),
            (&first_query, first),
        ] {
            cache
                .get_or_prepare_read(&view, query, None, PlanOptions::default(), || async {
                    calls.fetch_add(1, Ordering::Relaxed);
                    Ok(prepared)
                })
                .await
                .unwrap();
        }

        let stats = cache.stats();
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        assert_eq!(stats.prepared_evictions, 2);
        assert_eq!(stats.prepared_entries, 1);
        transaction.rollback();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn oversized_prepared_reads_are_not_retained() {
        let store = Arc::new(Store::memory("prepared-read-oversized").await.unwrap());
        let transaction = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let view = TransactionView(&*transaction);
        let query = query();
        let prepared = prepared_statement(&view, query.clone()).await;
        let cache = RelationCache::new(RelationCacheLimits {
            result_byte_limit: 8,
            ..RelationCacheLimits::default()
        });
        let calls = AtomicUsize::new(0);

        for _ in 0..2 {
            cache
                .get_or_prepare_read(&view, &query, None, PlanOptions::default(), || async {
                    calls.fetch_add(1, Ordering::Relaxed);
                    Ok(prepared.clone())
                })
                .await
                .unwrap();
        }

        let stats = cache.stats();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(stats.prepared_rejected_too_large, 2);
        assert_eq!(stats.prepared_entries, 0);
        transaction.rollback();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_preparation_releases_the_request() {
        let store = Arc::new(Store::memory("prepared-read-cancelled").await.unwrap());
        let setup = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let setup_view = TransactionView(&*setup);
        let query = query();
        let prepared = prepared_statement(&setup_view, query.clone()).await;
        setup.rollback();
        let cache = Arc::new(RelationCache::new(RelationCacheLimits::default()));
        let started = Arc::new(Notify::new());
        let first = {
            let store = store.clone();
            let cache = cache.clone();
            let query = query.clone();
            let started = started.clone();
            tokio::spawn(async move {
                let transaction = store.begin(IsolationLevel::Snapshot).await.unwrap();
                let view = TransactionView(&*transaction);
                cache
                    .get_or_prepare_read(
                        &view,
                        &query,
                        None,
                        PlanOptions::default(),
                        || async move {
                            started.notify_one();
                            pending::<Result<BoundStatement>>().await
                        },
                    )
                    .await
            })
        };
        started.notified().await;
        first.abort();
        assert!(matches!(first.await, Err(error) if error.is_cancelled()));

        let transaction = store.begin(IsolationLevel::Snapshot).await.unwrap();
        let view = TransactionView(&*transaction);
        cache
            .get_or_prepare_read(&view, &query, None, PlanOptions::default(), || async {
                Ok(prepared)
            })
            .await
            .unwrap();

        assert_eq!(cache.stats().prepared_admissions, 1);
        transaction.rollback();
        store.close().await.unwrap();
    }
}

fn table_definitions(query: &lir::bound::Query) -> Vec<TableDefinitionDependency> {
    let mut definitions = Vec::new();
    let mut collect = |relation: &lir::bound::Relation| {
        if let lir::bound::RelationNode::Scan { table, .. } = &relation.node {
            definitions.push(TableDefinitionDependency {
                id: table.id.clone(),
                name: table.name.clone(),
                generation: table.definition_generation,
            });
        }
    };
    lir::inspect::walk_relation(&query.root, &mut collect, &mut |_| {});
    for binding in &query.bindings {
        lir::inspect::walk_relation(&binding.root, &mut collect, &mut |_| {});
        if let Some(step) = &binding.step {
            lir::inspect::walk_relation(step, &mut collect, &mut |_| {});
        }
    }
    definitions.sort_unstable();
    definitions.dedup();
    definitions
}

fn retained_bytes(request: &RequestKey, request_bytes: usize, prepared: &PreparedRead) -> usize {
    // The debug form includes all owned strings and collection elements in
    // the bound query and plan. The multiplier covers decoded containers and
    // spare capacity. This value is a deterministic admission weight. It is
    // not an allocator measurement.
    let decoded = format!("{:?}", prepared.statement).len();
    size_of::<Entry>()
        .saturating_add(size_of::<PreparedRead>())
        .saturating_add(request_bytes)
        .saturating_add(
            request
                .statistics
                .as_ref()
                .map_or(0, |identity| identity.capacity()),
        )
        .saturating_add(decoded.saturating_mul(2))
}

struct FlightOwner<'a> {
    cache: &'a PreparedReadCache,
    key: FlightKey,
    flight: Arc<Flight>,
    finished: bool,
}

impl<'a> FlightOwner<'a> {
    fn new(cache: &'a PreparedReadCache, key: FlightKey, flight: Arc<Flight>) -> Self {
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

    fn remove(&self) {
        let mut state = self
            .cache
            .state
            .lock()
            .expect("prepared read cache lock poisoned");
        if state
            .flights
            .get(&self.key)
            .is_some_and(|flight| Arc::ptr_eq(flight, &self.flight))
        {
            state.flights.remove(&self.key);
        }
    }
}

impl Drop for FlightOwner<'_> {
    fn drop(&mut self) {
        if !self.finished {
            // A failed or cancelled owner does not create retained state.
            // Waiters retry through their own pinned views.
            self.flight
                .result
                .send_replace(Some(FlightResult::Cancelled));
            self.remove();
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct PreparedReadStats {
    pub hits: u64,
    pub misses: u64,
    pub coalesced: u64,
    pub admissions: u64,
    pub evictions: u64,
    pub rejected_too_large: u64,
    pub superseded: u64,
    pub entries: u64,
    pub retained_bytes: u64,
}

#[cfg(test)]
impl PreparedReadCache {
    pub(super) fn stats(&self) -> PreparedReadStats {
        let state = self
            .state
            .lock()
            .expect("prepared read cache lock poisoned");
        PreparedReadStats {
            hits: self.metrics.hits.load(Ordering::Relaxed),
            misses: self.metrics.misses.load(Ordering::Relaxed),
            coalesced: self.metrics.coalesced.load(Ordering::Relaxed),
            admissions: self.metrics.admissions.load(Ordering::Relaxed),
            evictions: self.metrics.evictions.load(Ordering::Relaxed),
            rejected_too_large: self.metrics.rejected_too_large.load(Ordering::Relaxed),
            superseded: self.metrics.superseded.load(Ordering::Relaxed),
            entries: state.entry_count as u64,
            retained_bytes: state.retained_bytes as u64,
        }
    }
}
