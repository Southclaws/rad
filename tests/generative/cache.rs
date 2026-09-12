use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rad::engine::catalog;
use rad::engine::catalog::identity::SchemaId;
use rad::engine::catalog::model::{ColumnDef, IndexDef, ScalarType, Table, TableDef};
use rad::engine::exec::{
    Engine, EngineEvent, EngineEventHook, RelationCacheLookupResult, RelationCacheMaterialization,
};
use rad::engine::kv::TransactionalKv;
use rad::engine::kv::slatedb::Store;
use rad::engine::lir::{self, BinaryOp, Datum, Expr, Relation, RootCardinality, Row, Value};
use rad::engine::lir::{Kind, Literal, RawScalar, RowsColumn};
use rad::engine::planner::estimator::StatisticsProvider;
use rad::engine::planner::models::{ColumnSynopsis, PlannerStats, SynopsisCoverage, SynopsisModel};
use slatedb::object_store::{ObjectStore, local::LocalFileSystem};
use tempfile::TempDir;

use super::{Choices, TestResult, decisions_from_seed};

static STORE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CacheCaseKind {
    HashBuild,
    GroupedDimension,
    RecursiveBuild,
    SiblingHashBuilds,
    NestedHashBuild,
    DerivedBindingHashBuild,
}

#[derive(Clone, Debug)]
pub struct CacheCase {
    pub decisions: Vec<u64>,
    kind: CacheCaseKind,
    stable_rows: usize,
    probe_rows: usize,
    include_nulls: bool,
    text_length: usize,
    second_cardinality: RootCardinality,
    reverse_order: bool,
}

impl CacheCase {
    pub fn from_seed(seed: u64) -> Self {
        Self::generate(decisions_from_seed(seed))
    }

    pub fn generate(decisions: Vec<u64>) -> Self {
        let mut choices = Choices::new(&decisions);
        let kind = match choices.index(6) {
            0 => CacheCaseKind::HashBuild,
            1 => CacheCaseKind::GroupedDimension,
            2 => CacheCaseKind::RecursiveBuild,
            3 => CacheCaseKind::SiblingHashBuilds,
            4 => CacheCaseKind::NestedHashBuild,
            _ => CacheCaseKind::DerivedBindingHashBuild,
        };
        let stable_rows = choices.range(2, 8);
        let probe_rows = choices.range(0, 8);
        let include_nulls = choices.coin();
        let text_length = [0, 1, 16, 256][choices.index(4)];
        let second_cardinality = [
            RootCardinality::Many,
            RootCardinality::First,
            RootCardinality::ExactlyOne,
        ][choices.index(3)];
        let reverse_order = choices.coin();
        Self {
            decisions,
            kind,
            stable_rows,
            probe_rows,
            include_nulls,
            text_length,
            second_cardinality,
            reverse_order,
        }
    }

    pub fn for_kind(kind: usize, seed: u64) -> Self {
        let mut decisions = decisions_from_seed(seed);
        decisions[0] = kind as u64;
        Self::generate(decisions)
    }
}

#[derive(Default)]
struct RecordingEvents {
    events: Mutex<Vec<EngineEvent>>,
}

impl RecordingEvents {
    fn take(&self) -> Vec<EngineEvent> {
        std::mem::take(&mut *self.events.lock().expect("cache event lock poisoned"))
    }
}

#[async_trait]
impl EngineEventHook for RecordingEvents {
    async fn reach(&self, event: EngineEvent) {
        self.events
            .lock()
            .expect("cache event lock poisoned")
            .push(event);
    }
}

struct FixedPlannerStats(Arc<PlannerStats>);

impl StatisticsProvider for FixedPlannerStats {
    fn planning_stats(&self) -> Arc<PlannerStats> {
        self.0.clone()
    }
}

pub async fn check_cache(case: &CacheCase) -> TestResult<()> {
    let name = format!(
        "generated-cache-{}",
        STORE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let store = Arc::new(
        Store::memory(&name)
            .await
            .map_err(|error| error.to_string())?,
    );
    check_cache_in_store(&store, case).await?;
    store.close().await.map_err(|error| error.to_string())
}

pub async fn check_cache_file(case: &CacheCase) -> TestResult<()> {
    let directory = TempDir::new().map_err(|error| error.to_string())?;
    let objects: Arc<dyn ObjectStore> = Arc::new(
        LocalFileSystem::new_with_prefix(directory.path()).map_err(|error| error.to_string())?,
    );
    let name = format!(
        "generated-cache-file-{}",
        STORE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let store = Arc::new(
        Store::open(name, objects)
            .await
            .map_err(|error| error.to_string())?,
    );
    check_cache_in_store(&store, case).await?;
    store.close().await.map_err(|error| error.to_string())
}

async fn check_cache_in_store(store: &Arc<Store>, case: &CacheCase) -> TestResult<()> {
    match case.kind {
        CacheCaseKind::HashBuild => check_hash_build(store, case).await,
        CacheCaseKind::GroupedDimension => check_grouped_dimension(store, case).await,
        CacheCaseKind::RecursiveBuild => check_recursive_build(store, case).await,
        CacheCaseKind::SiblingHashBuilds => check_sibling_hash_builds(store, case).await,
        CacheCaseKind::NestedHashBuild => check_nested_hash_build(store, case).await,
        CacheCaseKind::DerivedBindingHashBuild => {
            check_derived_binding_hash_build(store, case).await
        }
    }
}

async fn check_hash_build(store: &Arc<Store>, case: &CacheCase) -> TestResult<()> {
    let (stable, probe) = create_join_tables(store, "dimensions", "facts").await?;
    let mut statistics = complete_stats(&stable, case.stable_rows as u64);
    statistics
        .synopsis_models
        .extend(complete_stats(&probe, 5_000).synopsis_models);
    let events = Arc::new(RecordingEvents::default());
    let engine = Engine::new(store.clone())
        .with_statistics_provider(Arc::new(FixedPlannerStats(Arc::new(statistics))))
        .with_event_hook(events.clone());
    seed_join_rows(&engine, case, "dimensions", "facts").await?;
    let query = join_query("dimensions", "facts", case.reverse_order, Some("first"));

    assert_same_outcomes(
        "cold hash-build execution",
        engine.execute_uncached(query.clone()).await,
        engine.execute(query.clone()).await,
    )?;
    require_cache_result(
        &events.take(),
        RelationCacheMaterialization::HashJoinBuild,
        RelationCacheLookupResult::Filled,
    )?;

    let changed_literal = join_query("dimensions", "facts", case.reverse_order, Some("second"));
    assert_same_outcomes(
        "changed hash-build literal",
        engine.execute_uncached(changed_literal.clone()).await,
        engine.execute(changed_literal).await,
    )?;
    require_cache_result(
        &events.take(),
        RelationCacheMaterialization::HashJoinBuild,
        RelationCacheLookupResult::Reused,
    )
    .map_err(|error| format!("changed-literal cache result: {error}"))?;

    let scalar = scalar_join_query("dimensions", "facts", "scalar");
    assert_same_outcomes(
        "scalar hash-build result",
        engine.execute_uncached(scalar.clone()).await,
        engine.execute(scalar).await,
    )?;
    events.take();

    engine
        .create("facts", join_row("probe-new", "shared", case.text_length))
        .await
        .map_err(|error| error.to_string())?;
    let second_query = changed_root_shape(&query, case.second_cardinality);
    assert_same_outcomes(
        "hash-build reuse after probe change",
        engine.execute_uncached(second_query.clone()).await,
        engine.execute(second_query.clone()).await,
    )?;
    require_cache_result(
        &events.take(),
        RelationCacheMaterialization::HashJoinBuild,
        RelationCacheLookupResult::Reused,
    )?;

    engine
        .create(
            "dimensions",
            join_row("stable-new", "shared", case.text_length),
        )
        .await
        .map_err(|error| error.to_string())?;
    let uncached = engine.execute_uncached(second_query.clone()).await;
    let cached = engine.execute(second_query.clone()).await;
    assert_same_outcomes("hash-build refill after build change", uncached, cached)?;
    require_cache_result(
        &events.take(),
        RelationCacheMaterialization::HashJoinBuild,
        RelationCacheLookupResult::Filled,
    )?;

    let catalog = catalog::Catalog::new(store.clone());
    catalog
        .create_column(
            "dimensions",
            ColumnDef {
                id: SchemaId::new(103).expect("positive schema ID"),
                name: "note".into(),
                scalar_type: ScalarType::Text,
                nullable: true,
                format: String::new(),
                default: None,
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    assert_same_outcomes(
        "hash-build result after column creation",
        engine.execute_uncached(second_query.clone()).await,
        engine.execute(second_query.clone()).await,
    )?;
    require_cache_result(
        &events.take(),
        RelationCacheMaterialization::HashJoinBuild,
        RelationCacheLookupResult::Filled,
    )?;

    catalog
        .create_index(
            "dimensions",
            IndexDef {
                name: "dimensions_customer_id".into(),
                columns: vec!["customer_id".into()],
                unique: false,
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    assert_same_outcomes(
        "hash-build result after index creation",
        engine.execute_uncached(second_query.clone()).await,
        engine.execute(second_query.clone()).await,
    )?;
    events.take();
    catalog
        .delete_index("dimensions", "dimensions_customer_id")
        .await
        .map_err(|error| error.to_string())?;
    assert_same_outcomes(
        "hash-build result after index deletion",
        engine.execute_uncached(second_query.clone()).await,
        engine.execute(second_query.clone()).await,
    )?;
    events.take();
    assert_same_outcomes(
        "hash-build result and interpreter",
        engine.execute_uncached(second_query.clone()).await,
        engine.execute_reference(second_query).await,
    )
}

async fn check_grouped_dimension(store: &Arc<Store>, case: &CacheCase) -> TestResult<()> {
    let (dimension, fact) = create_join_tables(store, "customers", "orders").await?;
    let mut statistics = complete_stats(&dimension, case.stable_rows as u64);
    statistics
        .synopsis_models
        .extend(complete_stats(&fact, 5_000).synopsis_models);
    let events = Arc::new(RecordingEvents::default());
    let engine = Engine::new(store.clone())
        .with_statistics_provider(Arc::new(FixedPlannerStats(Arc::new(statistics))))
        .with_event_hook(events.clone());
    seed_grouped_rows(&engine, case).await?;
    let query = grouped_query(case.reverse_order);

    assert_same_outcomes(
        "cold grouped-dimension execution",
        engine.execute_uncached(query.clone()).await,
        engine.execute(query.clone()).await,
    )?;
    require_cache_result(
        &events.take(),
        RelationCacheMaterialization::GroupedHashJoinDimension,
        RelationCacheLookupResult::Filled,
    )?;

    engine
        .create(
            "orders",
            join_row(
                "probe-new",
                &stored_id("customer-0", case.text_length),
                case.text_length,
            ),
        )
        .await
        .map_err(|error| error.to_string())?;
    let second_query = changed_root_shape(&query, case.second_cardinality);
    assert_same_outcomes(
        "grouped-dimension reuse after fact change",
        engine.execute_uncached(second_query.clone()).await,
        engine.execute(second_query.clone()).await,
    )?;
    require_cache_result(
        &events.take(),
        RelationCacheMaterialization::GroupedHashJoinDimension,
        RelationCacheLookupResult::Reused,
    )?;

    engine
        .create(
            "customers",
            join_row("stable-new", "unused", case.text_length),
        )
        .await
        .map_err(|error| error.to_string())?;
    assert_same_outcomes(
        "grouped-dimension refill after dimension change",
        engine.execute_uncached(second_query.clone()).await,
        engine.execute(second_query).await,
    )?;
    require_cache_result(
        &events.take(),
        RelationCacheMaterialization::GroupedHashJoinDimension,
        RelationCacheLookupResult::Filled,
    )
}

async fn check_recursive_build(store: &Arc<Store>, case: &CacheCase) -> TestResult<()> {
    let catalog = catalog::Catalog::new(store.clone());
    catalog
        .create_table(join_table(10, "seeds"))
        .await
        .map_err(|error| error.to_string())?;
    let edges = catalog
        .create_table(join_table(20, "edges"))
        .await
        .map_err(|error| error.to_string())?;
    let events = Arc::new(RecordingEvents::default());
    let engine = Engine::new(store.clone())
        .with_statistics_provider(Arc::new(FixedPlannerStats(Arc::new(complete_stats(
            &edges,
            case.stable_rows as u64,
        )))))
        .with_event_hook(events.clone());
    engine
        .create("seeds", join_row("n0", "", 0))
        .await
        .map_err(|error| error.to_string())?;
    let mut edge_rows = Vec::with_capacity(case.stable_rows);
    for index in 0..case.stable_rows {
        edge_rows.push(join_row(
            &format!("n{}", index + 1),
            &format!("n{index}"),
            0,
        ));
    }
    engine
        .create_many("edges", edge_rows)
        .await
        .map_err(|error| error.to_string())?;
    let query = recursive_query(case.reverse_order);

    assert_same_outcomes(
        "cold recursive execution",
        engine.execute_uncached(query.clone()).await,
        engine.execute(query.clone()).await,
    )?;
    let first_events = events.take();
    require_cache_result(
        &first_events,
        RelationCacheMaterialization::HashJoinBuild,
        RelationCacheLookupResult::Filled,
    )?;
    require_cache_result(
        &first_events,
        RelationCacheMaterialization::HashJoinBuild,
        RelationCacheLookupResult::Reused,
    )?;

    engine
        .create("seeds", join_row("isolated", "", 0))
        .await
        .map_err(|error| error.to_string())?;
    let second_query = changed_root_shape(&query, case.second_cardinality);
    assert_same_outcomes(
        "recursive build reuse after anchor change",
        engine.execute_uncached(second_query.clone()).await,
        engine.execute(second_query.clone()).await,
    )?;
    require_cache_result(
        &events.take(),
        RelationCacheMaterialization::HashJoinBuild,
        RelationCacheLookupResult::Reused,
    )?;

    engine
        .create(
            "edges",
            join_row(
                &format!("n{}", case.stable_rows + 1),
                &format!("n{}", case.stable_rows),
                0,
            ),
        )
        .await
        .map_err(|error| error.to_string())?;
    assert_same_outcomes(
        "recursive build refill after edge change",
        engine.execute_uncached(second_query.clone()).await,
        engine.execute(second_query.clone()).await,
    )?;
    require_cache_result(
        &events.take(),
        RelationCacheMaterialization::HashJoinBuild,
        RelationCacheLookupResult::Filled,
    )?;
    assert_same_outcomes(
        "recursive result and interpreter",
        engine.execute_uncached(second_query.clone()).await,
        engine.execute_reference(second_query).await,
    )
}

async fn check_sibling_hash_builds(store: &Arc<Store>, case: &CacheCase) -> TestResult<()> {
    let catalog = catalog::Catalog::new(store.clone());
    let stable_a = catalog
        .create_table(join_table(10, "stable_a"))
        .await
        .map_err(|error| error.to_string())?;
    let probe_a = catalog
        .create_table(join_table(20, "probe_a"))
        .await
        .map_err(|error| error.to_string())?;
    let stable_b = catalog
        .create_table(join_table(30, "stable_b"))
        .await
        .map_err(|error| error.to_string())?;
    let probe_b = catalog
        .create_table(join_table(40, "probe_b"))
        .await
        .map_err(|error| error.to_string())?;
    let mut statistics = complete_stats(&stable_a, case.stable_rows as u64);
    statistics
        .synopsis_models
        .extend(complete_stats(&stable_b, case.stable_rows as u64).synopsis_models);
    statistics
        .synopsis_models
        .extend(complete_stats(&probe_a, 5_000).synopsis_models);
    statistics
        .synopsis_models
        .extend(complete_stats(&probe_b, 5_000).synopsis_models);
    let events = Arc::new(RecordingEvents::default());
    let engine = Engine::new(store.clone())
        .with_statistics_provider(Arc::new(FixedPlannerStats(Arc::new(statistics))))
        .with_event_hook(events.clone());
    seed_join_rows(&engine, case, "stable_a", "probe_a").await?;
    seed_join_rows(&engine, case, "stable_b", "probe_b").await?;
    let query = sibling_join_query(case.reverse_order);

    assert_same_outcomes(
        "cold sibling execution",
        engine.execute_uncached(query.clone()).await,
        engine.execute(query.clone()).await,
    )?;
    require_cache_result_count(
        &events.take(),
        RelationCacheMaterialization::HashJoinBuild,
        RelationCacheLookupResult::Filled,
        2,
    )?;

    for table in ["probe_a", "probe_b"] {
        engine
            .create(table, join_row("probe-new", "shared", case.text_length))
            .await
            .map_err(|error| error.to_string())?;
    }
    let second_query = changed_root_shape(&query, case.second_cardinality);
    assert_same_outcomes(
        "sibling reuse after both probes change",
        engine.execute_uncached(second_query.clone()).await,
        engine.execute(second_query.clone()).await,
    )?;
    require_cache_result_count(
        &events.take(),
        RelationCacheMaterialization::HashJoinBuild,
        RelationCacheLookupResult::Reused,
        2,
    )?;

    engine
        .create(
            "stable_a",
            join_row("stable-new", "shared", case.text_length),
        )
        .await
        .map_err(|error| error.to_string())?;
    assert_same_outcomes(
        "one sibling refill after one build changes",
        engine.execute_uncached(second_query.clone()).await,
        engine.execute(second_query.clone()).await,
    )?;
    let final_events = events.take();
    require_cache_result(
        &final_events,
        RelationCacheMaterialization::HashJoinBuild,
        RelationCacheLookupResult::Filled,
    )?;
    require_cache_result(
        &final_events,
        RelationCacheMaterialization::HashJoinBuild,
        RelationCacheLookupResult::Reused,
    )?;
    assert_same_outcomes(
        "sibling result and interpreter",
        engine.execute_uncached(second_query.clone()).await,
        engine.execute_reference(second_query).await,
    )
}

async fn check_nested_hash_build(store: &Arc<Store>, case: &CacheCase) -> TestResult<()> {
    let (stable, probe) = create_join_tables(store, "dimensions", "facts").await?;
    let mut statistics = complete_stats(&stable, case.stable_rows as u64);
    statistics
        .synopsis_models
        .extend(complete_stats(&probe, 5_000).synopsis_models);
    let events = Arc::new(RecordingEvents::default());
    let engine = Engine::new(store.clone())
        .with_statistics_provider(Arc::new(FixedPlannerStats(Arc::new(statistics))))
        .with_event_hook(events.clone());
    seed_join_rows(&engine, case, "dimensions", "facts").await?;
    let query = nested_join_query(case.reverse_order);

    assert_same_outcomes(
        "cold nested execution",
        engine.execute_uncached(query.clone()).await,
        engine.execute(query.clone()).await,
    )?;
    require_cache_result(
        &events.take(),
        RelationCacheMaterialization::HashJoinBuild,
        RelationCacheLookupResult::Filled,
    )?;

    engine
        .create("facts", join_row("probe-new", "shared", case.text_length))
        .await
        .map_err(|error| error.to_string())?;
    let second_query = changed_root_shape(&query, case.second_cardinality);
    assert_same_outcomes(
        "nested build reuse after probe change",
        engine.execute_uncached(second_query.clone()).await,
        engine.execute(second_query.clone()).await,
    )?;
    require_cache_result(
        &events.take(),
        RelationCacheMaterialization::HashJoinBuild,
        RelationCacheLookupResult::Reused,
    )?;

    engine
        .create(
            "dimensions",
            join_row("stable-new", "shared", case.text_length),
        )
        .await
        .map_err(|error| error.to_string())?;
    assert_same_outcomes(
        "nested build refill after dimension change",
        engine.execute_uncached(second_query.clone()).await,
        engine.execute(second_query.clone()).await,
    )?;
    require_cache_result(
        &events.take(),
        RelationCacheMaterialization::HashJoinBuild,
        RelationCacheLookupResult::Filled,
    )?;
    assert_same_outcomes(
        "nested result and interpreter",
        engine.execute_uncached(second_query.clone()).await,
        engine.execute_reference(second_query).await,
    )
}

async fn check_derived_binding_hash_build(store: &Arc<Store>, case: &CacheCase) -> TestResult<()> {
    let (stable, probe) = create_join_tables(store, "dimensions", "facts").await?;
    let mut statistics = complete_stats(&stable, case.stable_rows as u64);
    statistics
        .synopsis_models
        .extend(complete_stats(&probe, 5_000).synopsis_models);
    let events = Arc::new(RecordingEvents::default());
    let engine = Engine::new(store.clone())
        .with_statistics_provider(Arc::new(FixedPlannerStats(Arc::new(statistics))))
        .with_event_hook(events.clone());
    seed_join_rows(&engine, case, "dimensions", "facts").await?;
    let query = derived_join_query("joined", case.reverse_order);

    assert_same_outcomes(
        "cold derived-binding execution",
        engine.execute_uncached(query.clone()).await,
        engine.execute(query.clone()).await,
    )?;
    require_cache_result(
        &events.take(),
        RelationCacheMaterialization::HashJoinBuild,
        RelationCacheLookupResult::Filled,
    )?;

    engine
        .create("facts", join_row("probe-new", "shared", case.text_length))
        .await
        .map_err(|error| error.to_string())?;
    let mut second_query = derived_join_query("renamed", !case.reverse_order);
    second_query.cardinality = case.second_cardinality;
    assert_same_outcomes(
        "derived-binding reuse after probe change and binding rename",
        engine.execute_uncached(second_query.clone()).await,
        engine.execute(second_query.clone()).await,
    )?;
    require_cache_result(
        &events.take(),
        RelationCacheMaterialization::HashJoinBuild,
        RelationCacheLookupResult::Reused,
    )?;

    engine
        .create(
            "dimensions",
            join_row("stable-new", "shared", case.text_length),
        )
        .await
        .map_err(|error| error.to_string())?;
    assert_same_outcomes(
        "derived-binding refill after build change",
        engine.execute_uncached(second_query.clone()).await,
        engine.execute(second_query.clone()).await,
    )?;
    require_cache_result(
        &events.take(),
        RelationCacheMaterialization::HashJoinBuild,
        RelationCacheLookupResult::Filled,
    )?;
    assert_same_outcomes(
        "derived-binding result and interpreter",
        engine.execute_uncached(second_query.clone()).await,
        engine.execute_reference(second_query).await,
    )
}

fn assert_same_outcomes(
    label: &str,
    expected: rad::engine::exec::Result<Datum>,
    actual: rad::engine::exec::Result<Datum>,
) -> TestResult<()> {
    if crate::exact::outcomes_eq(&expected, &actual) {
        Ok(())
    } else {
        Err(format!(
            "{label} differs\nexpected: {expected:?}\n  actual: {actual:?}"
        ))
    }
}

fn require_cache_result(
    events: &[EngineEvent],
    materialization: RelationCacheMaterialization,
    result: RelationCacheLookupResult,
) -> TestResult<()> {
    if events.iter().any(|event| {
        matches!(
            event,
            EngineEvent::SubrelationCacheLookupCompleted {
                materialization: actual_materialization,
                result: actual_result,
                ..
            } if *actual_materialization == materialization && *actual_result == result
        )
    }) {
        return Ok(());
    }
    Err(format!(
        "cache events do not contain {materialization:?} {result:?}: {events:#?}"
    ))
}

fn require_cache_result_count(
    events: &[EngineEvent],
    materialization: RelationCacheMaterialization,
    result: RelationCacheLookupResult,
    minimum: usize,
) -> TestResult<()> {
    let actual = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                EngineEvent::SubrelationCacheLookupCompleted {
                    materialization: actual_materialization,
                    result: actual_result,
                    ..
                } if *actual_materialization == materialization && *actual_result == result
            )
        })
        .count();
    if actual >= minimum {
        Ok(())
    } else {
        Err(format!(
            "cache events contain {actual} {materialization:?} {result:?} results, want at least {minimum}: {events:#?}"
        ))
    }
}

async fn create_join_tables(
    store: &Arc<Store>,
    stable: &str,
    probe: &str,
) -> TestResult<(Table, Table)> {
    let catalog = catalog::Catalog::new(store.clone());
    let stable = catalog
        .create_table(join_table(10, stable))
        .await
        .map_err(|error| error.to_string())?;
    let probe = catalog
        .create_table(join_table(20, probe))
        .await
        .map_err(|error| error.to_string())?;
    catalog
        .create_table(join_table(30, "unrelated"))
        .await
        .map_err(|error| error.to_string())?;
    Ok((stable, probe))
}

async fn seed_join_rows(
    engine: &Engine,
    case: &CacheCase,
    stable: &str,
    probe: &str,
) -> TestResult<()> {
    let mut stable_rows = Vec::with_capacity(case.stable_rows);
    for index in 0..case.stable_rows {
        let key = if case.include_nulls && index + 1 == case.stable_rows {
            None
        } else if index < 2 {
            Some("shared".to_owned())
        } else {
            Some(format!("key-{index}"))
        };
        stable_rows.push(join_row_value(
            &format!("stable-{index}"),
            key,
            case.text_length,
        ));
    }
    engine
        .create_many(stable, stable_rows)
        .await
        .map_err(|error| error.to_string())?;

    let mut probe_rows = Vec::with_capacity(case.probe_rows);
    for index in 0..case.probe_rows {
        let key = if case.include_nulls && index + 1 == case.probe_rows {
            None
        } else if index % 2 == 0 {
            Some("shared".to_owned())
        } else {
            Some(format!(
                "key-{}",
                2 + index % case.stable_rows.saturating_sub(2).max(1)
            ))
        };
        probe_rows.push(join_row_value(
            &format!("probe-{index}"),
            key,
            case.text_length,
        ));
    }
    engine
        .create_many(probe, probe_rows)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

async fn seed_grouped_rows(engine: &Engine, case: &CacheCase) -> TestResult<()> {
    let customers = (0..case.stable_rows)
        .map(|index| {
            join_row_value(
                &format!("customer-{index}"),
                (index + 1 != case.stable_rows || !case.include_nulls)
                    .then(|| format!("group-{index}")),
                case.text_length,
            )
        })
        .collect();
    engine
        .create_many("customers", customers)
        .await
        .map_err(|error| error.to_string())?;

    let orders = (0..case.probe_rows)
        .map(|index| {
            let customer = index % case.stable_rows;
            join_row(
                &format!("order-{index}"),
                &stored_id(&format!("customer-{customer}"), case.text_length),
                case.text_length,
            )
        })
        .collect();
    engine
        .create_many("orders", orders)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn join_table(id: u32, name: &str) -> TableDef {
    TableDef {
        id: SchemaId::new(id).expect("positive schema ID"),
        name: name.into(),
        columns: vec![
            ColumnDef {
                id: SchemaId::new(id * 10 + 1).expect("positive schema ID"),
                name: "id".into(),
                scalar_type: ScalarType::Text,
                nullable: false,
                format: String::new(),
                default: None,
            },
            ColumnDef {
                id: SchemaId::new(id * 10 + 2).expect("positive schema ID"),
                name: "customer_id".into(),
                scalar_type: ScalarType::Text,
                nullable: true,
                format: String::new(),
                default: None,
            },
        ],
        primary_key: vec!["id".into()],
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
    }
}

fn join_row(id: &str, key: &str, text_length: usize) -> Row {
    join_row_value(id, Some(key.to_owned()), text_length)
}

fn join_row_value(id: &str, key: Option<String>, text_length: usize) -> Row {
    let id = stored_id(id, text_length);
    Row::from([
        ("id".into(), Value::Text(id)),
        (
            "customer_id".into(),
            key.map_or(Value::Null(ScalarType::Text), Value::Text),
        ),
    ])
}

fn stored_id(id: &str, text_length: usize) -> String {
    format!("{id}{}", "x".repeat(text_length))
}

fn complete_stats(table: &Table, rows: u64) -> PlannerStats {
    let mut statistics = PlannerStats::empty();
    statistics.synopsis_models.insert(
        table.schema_id,
        SynopsisModel {
            table: table.schema_id,
            observed_rows: rows,
            coverage: SynopsisCoverage::Complete,
            sample_size: rows,
            changes_since_collection: 0,
            table_existence_generation: table.existence_generation.get(),
            collected_at_unix_micros: 0,
            catalog_version: 1,
            columns: table
                .columns
                .iter()
                .map(|column| ColumnSynopsis {
                    column: column.schema_id,
                    value_generation: column.value_generation.get(),
                    null_fraction: 0.0,
                    null_count: 0,
                    distinct: rows.max(1),
                    distinct_is_exact: false,
                    average_width: 8,
                    maximum_width: Some(8),
                    minimum: None,
                    maximum: None,
                    most_common_values: Vec::new(),
                    range_distribution: None,
                    degree_sequence: None,
                })
                .collect(),
            column_groups: Vec::new(),
            predicate_conditioned_degrees: Vec::new(),
        },
    );
    statistics
}

fn join_query(stable: &str, probe: &str, descending: bool, result_tag: Option<&str>) -> lir::Query {
    let joined = Relation::Join {
        left: Box::new(Relation::Scan {
            table: stable.into(),
            scope: "stable".into(),
        }),
        right: Box::new(Relation::Scan {
            table: probe.into(),
            scope: "probe".into(),
        }),
        kind: lir::JoinKind::Inner,
        on: Expr::Binary {
            op: BinaryOp::Eq,
            left: Box::new(column("stable", "customer_id")),
            right: Box::new(column("probe", "customer_id")),
        },
    };
    let mut fields = vec![
        lir::ProjectField {
            name: "stable_id".into(),
            expression: column("stable", "id"),
        },
        lir::ProjectField {
            name: "probe_id".into(),
            expression: column("probe", "id"),
        },
    ];
    if let Some(tag) = result_tag {
        fields.push(lir::ProjectField {
            name: "tag".into(),
            expression: Expr::Literal(Literal {
                raw: RawScalar::Text(tag.into()),
                kind: Some(Kind::Text),
            }),
        });
    }
    lir::Query {
        root: Relation::Order {
            input: Box::new(Relation::Project {
                input: Box::new(joined),
                scope: Some("result".into()),
                spread: Vec::new(),
                fields,
            }),
            terms: vec![
                lir::OrderTerm {
                    expression: column("result", "probe_id"),
                    descending,
                },
                lir::OrderTerm {
                    expression: column("result", "stable_id"),
                    descending: false,
                },
            ],
        },
        cardinality: RootCardinality::Many,
        bindings: HashMap::new(),
    }
}

fn scalar_join_query(stable: &str, probe: &str, value: &str) -> lir::Query {
    lir::Query {
        root: Relation::Project {
            input: Box::new(Relation::Join {
                left: Box::new(Relation::Scan {
                    table: stable.into(),
                    scope: "stable".into(),
                }),
                right: Box::new(Relation::Scan {
                    table: probe.into(),
                    scope: "probe".into(),
                }),
                kind: lir::JoinKind::Inner,
                on: Expr::Binary {
                    op: BinaryOp::Eq,
                    left: Box::new(column("stable", "customer_id")),
                    right: Box::new(column("probe", "customer_id")),
                },
            }),
            scope: Some("scalar_result".into()),
            spread: Vec::new(),
            fields: vec![lir::ProjectField {
                name: "value".into(),
                expression: Expr::Literal(Literal {
                    raw: RawScalar::Text(value.into()),
                    kind: Some(Kind::Text),
                }),
            }],
        },
        cardinality: RootCardinality::Scalar,
        bindings: HashMap::new(),
    }
}

fn grouped_query(descending: bool) -> lir::Query {
    let joined = Relation::Join {
        left: Box::new(Relation::Scan {
            table: "customers".into(),
            scope: "customer".into(),
        }),
        right: Box::new(Relation::Scan {
            table: "orders".into(),
            scope: "order".into(),
        }),
        kind: lir::JoinKind::Inner,
        on: Expr::Binary {
            op: BinaryOp::Eq,
            left: Box::new(column("customer", "id")),
            right: Box::new(column("order", "customer_id")),
        },
    };
    lir::Query {
        root: Relation::Order {
            input: Box::new(Relation::Aggregate {
                input: Box::new(joined),
                scope: Some("summary".into()),
                groups: vec![lir::GroupTerm {
                    name: "customer_id".into(),
                    expression: column("customer", "id"),
                }],
                terms: vec![lir::AggregateTerm {
                    function: lir::AggregateFunction::Count,
                    argument: Some(column("order", "id")),
                    name: "order_count".into(),
                }],
            }),
            terms: vec![lir::OrderTerm {
                expression: column("summary", "customer_id"),
                descending,
            }],
        },
        cardinality: RootCardinality::Many,
        bindings: HashMap::new(),
    }
}

fn sibling_join_query(descending: bool) -> lir::Query {
    let branch = |stable_table: &str,
                  stable_scope: &str,
                  probe_table: &str,
                  probe_scope: &str,
                  result_scope: &str| {
        Relation::Project {
            input: Box::new(Relation::Join {
                left: Box::new(Relation::Scan {
                    table: stable_table.into(),
                    scope: stable_scope.into(),
                }),
                right: Box::new(Relation::Scan {
                    table: probe_table.into(),
                    scope: probe_scope.into(),
                }),
                kind: lir::JoinKind::Inner,
                on: Expr::Binary {
                    op: BinaryOp::Eq,
                    left: Box::new(column(stable_scope, "customer_id")),
                    right: Box::new(column(probe_scope, "customer_id")),
                },
            }),
            scope: Some(result_scope.into()),
            spread: Vec::new(),
            fields: vec![
                lir::ProjectField {
                    name: "stable_id".into(),
                    expression: column(stable_scope, "id"),
                },
                lir::ProjectField {
                    name: "probe_id".into(),
                    expression: column(probe_scope, "id"),
                },
            ],
        }
    };
    lir::Query {
        root: Relation::Order {
            input: Box::new(Relation::Concatenate {
                scope: "all".into(),
                inputs: vec![
                    branch("stable_a", "stable_a", "probe_a", "probe_a", "result_a"),
                    branch("stable_b", "stable_b", "probe_b", "probe_b", "result_b"),
                ],
            }),
            terms: vec![
                lir::OrderTerm {
                    expression: column("all", "probe_id"),
                    descending,
                },
                lir::OrderTerm {
                    expression: column("all", "stable_id"),
                    descending: false,
                },
            ],
        },
        cardinality: RootCardinality::Many,
        bindings: HashMap::new(),
    }
}

fn nested_join_query(descending: bool) -> lir::Query {
    let nested = join_query("dimensions", "facts", descending, None).root;
    lir::Query {
        root: Relation::Order {
            input: Box::new(Relation::Project {
                input: Box::new(Relation::Rows {
                    scope: "outer".into(),
                    columns: vec![RowsColumn {
                        name: "id".into(),
                        kind: Kind::Text,
                        nullable: false,
                    }],
                    values: vec![vec![RawScalar::Text("outer".into())]],
                }),
                scope: Some("nested".into()),
                spread: Vec::new(),
                fields: vec![
                    lir::ProjectField {
                        name: "id".into(),
                        expression: column("outer", "id"),
                    },
                    lir::ProjectField {
                        name: "matches".into(),
                        expression: Expr::Array(Box::new(nested)),
                    },
                ],
            }),
            terms: vec![lir::OrderTerm {
                expression: column("nested", "id"),
                descending,
            }],
        },
        cardinality: RootCardinality::Many,
        bindings: HashMap::new(),
    }
}

fn derived_join_query(binding: &str, descending: bool) -> lir::Query {
    lir::Query {
        root: Relation::Order {
            input: Box::new(Relation::Ref {
                binding: binding.into(),
                scope: "bound".into(),
            }),
            terms: vec![lir::OrderTerm {
                expression: column("bound", "probe_id"),
                descending,
            }],
        },
        cardinality: RootCardinality::Many,
        bindings: HashMap::from([(
            binding.into(),
            join_query("dimensions", "facts", descending, None).root,
        )]),
    }
}

fn recursive_query(descending: bool) -> lir::Query {
    let anchor = Relation::Project {
        input: Box::new(Relation::Scan {
            table: "seeds".into(),
            scope: "seed".into(),
        }),
        scope: Some("anchor".into()),
        spread: Vec::new(),
        fields: vec![lir::ProjectField {
            name: "id".into(),
            expression: column("seed", "id"),
        }],
    };
    let step = Relation::Project {
        input: Box::new(Relation::Join {
            left: Box::new(Relation::RecursiveRef {
                binding: "reachable".into(),
                scope: "frontier".into(),
            }),
            right: Box::new(Relation::Scan {
                table: "edges".into(),
                scope: "edge".into(),
            }),
            kind: lir::JoinKind::Inner,
            on: Expr::Binary {
                op: BinaryOp::Eq,
                left: Box::new(column("frontier", "id")),
                right: Box::new(column("edge", "customer_id")),
            },
        }),
        scope: Some("step".into()),
        spread: Vec::new(),
        fields: vec![lir::ProjectField {
            name: "id".into(),
            expression: column("edge", "id"),
        }],
    };
    lir::Query {
        root: Relation::Order {
            input: Box::new(Relation::Ref {
                binding: "reachable".into(),
                scope: "result".into(),
            }),
            terms: vec![lir::OrderTerm {
                expression: column("result", "id"),
                descending,
            }],
        },
        cardinality: RootCardinality::Many,
        bindings: HashMap::from([(
            "reachable".into(),
            Relation::Recursive {
                anchor: Box::new(anchor),
                step: Box::new(step),
                accumulation: lir::RecursiveAccumulation::New,
            },
        )]),
    }
}

fn column(scope: &str, name: &str) -> Expr {
    Expr::Column {
        scope: scope.into(),
        name: name.into(),
    }
}

fn changed_root_shape(query: &lir::Query, cardinality: RootCardinality) -> lir::Query {
    let mut query = query.clone();
    query.cardinality = cardinality;
    if let Relation::Order { terms, .. } = &mut query.root {
        for term in terms {
            term.descending = !term.descending;
        }
    }
    query
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_cases_cover_each_cache_shape_and_input_dimension() {
        let mut kinds = [false; 6];
        let mut cardinalities = [false; 3];
        let mut text_lengths = [false; 4];
        let mut includes_nulls = [false; 2];
        let mut orders = [false; 2];
        let mut has_empty_probe = false;

        for seed in 0..4_096 {
            let case = CacheCase::from_seed(seed);
            kinds[match case.kind {
                CacheCaseKind::HashBuild => 0,
                CacheCaseKind::GroupedDimension => 1,
                CacheCaseKind::RecursiveBuild => 2,
                CacheCaseKind::SiblingHashBuilds => 3,
                CacheCaseKind::NestedHashBuild => 4,
                CacheCaseKind::DerivedBindingHashBuild => 5,
            }] = true;
            cardinalities[match case.second_cardinality {
                RootCardinality::Many => 0,
                RootCardinality::First => 1,
                RootCardinality::ExactlyOne => 2,
                RootCardinality::Scalar => panic!("unexpected generated cardinality"),
            }] = true;
            text_lengths[match case.text_length {
                0 => 0,
                1 => 1,
                16 => 2,
                256 => 3,
                other => panic!("unexpected generated text length: {other}"),
            }] = true;
            includes_nulls[usize::from(case.include_nulls)] = true;
            orders[usize::from(case.reverse_order)] = true;
            has_empty_probe |= case.probe_rows == 0;
        }

        assert!(kinds.into_iter().all(|covered| covered));
        assert!(cardinalities.into_iter().all(|covered| covered));
        assert!(text_lengths.into_iter().all(|covered| covered));
        assert!(includes_nulls.into_iter().all(|covered| covered));
        assert!(orders.into_iter().all(|covered| covered));
        assert!(has_empty_probe);
    }
}
