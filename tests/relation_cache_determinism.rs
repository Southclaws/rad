use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rad::engine::catalog;
use rad::engine::catalog::identity::SchemaId;
use rad::engine::catalog::model::{ColumnDef, ScalarType, Table, TableDef};
use rad::engine::exec::{
    Engine, EngineEvent, EngineEventHook, RelationCacheAdmissionResult, RelationCacheEvictionCause,
    RelationCacheLimits, RelationCacheLookupResult, RelationCacheMaterialization,
};
use rad::engine::kv::TransactionalKv;
use rad::engine::kv::slatedb::Store;
use rad::engine::lir::{
    self, BinaryOp, Expr, Kind, Literal, RawScalar, Relation, RootCardinality, Row, Value,
};
use rad::engine::planner::estimator::StatisticsProvider;
use rad::engine::planner::models::{ColumnSynopsis, PlannerStats, SynopsisCoverage, SynopsisModel};
use tokio::sync::Notify;

static STORE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct FixedPlannerStats(Arc<PlannerStats>);

impl StatisticsProvider for FixedPlannerStats {
    fn planning_stats(&self) -> Arc<PlannerStats> {
        self.0.clone()
    }
}

struct FillGate {
    blocked: AtomicBool,
    reused_blocked: AtomicBool,
    block_reused: bool,
    release: Notify,
    reused_release: Notify,
    changed: Notify,
    events: Mutex<Vec<EngineEvent>>,
}

#[derive(Default)]
struct EventRecorder {
    events: Mutex<Vec<EngineEvent>>,
}

impl EventRecorder {
    fn events(&self) -> Vec<EngineEvent> {
        self.events
            .lock()
            .expect("cache event lock poisoned")
            .clone()
    }
}

#[async_trait]
impl EngineEventHook for EventRecorder {
    async fn reach(&self, event: EngineEvent) {
        self.events
            .lock()
            .expect("cache event lock poisoned")
            .push(event);
    }
}

impl FillGate {
    fn new(block_reused: bool) -> Self {
        Self {
            blocked: AtomicBool::new(false),
            reused_blocked: AtomicBool::new(false),
            block_reused,
            release: Notify::new(),
            reused_release: Notify::new(),
            changed: Notify::new(),
            events: Mutex::new(Vec::new()),
        }
    }

    async fn wait_for_first_fill(&self) {
        self.wait_for(|events| {
            events.iter().any(|event| {
                matches!(
                    event,
                    EngineEvent::SubrelationCacheFillReady {
                        materialization: RelationCacheMaterialization::HashJoinBuild,
                        ..
                    }
                )
            })
        })
        .await;
    }

    async fn wait_for_lookup_count(&self, count: usize) {
        self.wait_for(|events| {
            events
                .iter()
                .filter(|event| {
                    matches!(
                        event,
                        EngineEvent::SubrelationCacheLookupStarted {
                            materialization: RelationCacheMaterialization::HashJoinBuild,
                            ..
                        }
                    )
                })
                .count()
                >= count
        })
        .await;
    }

    async fn wait_for_reused_lookup(&self) {
        self.wait_for(|events| {
            events.iter().any(|event| {
                matches!(
                    event,
                    EngineEvent::SubrelationCacheLookupCompleted {
                        materialization: RelationCacheMaterialization::HashJoinBuild,
                        result: RelationCacheLookupResult::Reused,
                        ..
                    }
                )
            })
        })
        .await;
    }

    async fn wait_for(&self, condition: impl Fn(&[EngineEvent]) -> bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let changed = self.changed.notified();
                if condition(&self.events.lock().expect("cache event lock poisoned")) {
                    return;
                }
                changed.await;
            }
        })
        .await
        .expect("cache event boundary was not reached");
    }

    fn release(&self) {
        self.release.notify_one();
    }

    fn events(&self) -> Vec<EngineEvent> {
        self.events
            .lock()
            .expect("cache event lock poisoned")
            .clone()
    }
}

#[async_trait]
impl EngineEventHook for FillGate {
    async fn reach(&self, event: EngineEvent) {
        let block_fill = matches!(
            event,
            EngineEvent::SubrelationCacheFillReady {
                materialization: RelationCacheMaterialization::HashJoinBuild,
                ..
            }
        ) && !self.blocked.swap(true, Ordering::AcqRel);
        let block_reused = self.block_reused
            && matches!(
                event,
                EngineEvent::SubrelationCacheLookupCompleted {
                    materialization: RelationCacheMaterialization::HashJoinBuild,
                    result: RelationCacheLookupResult::Reused,
                    ..
                }
            )
            && !self.reused_blocked.swap(true, Ordering::AcqRel);
        self.events
            .lock()
            .expect("cache event lock poisoned")
            .push(event);
        self.changed.notify_waiters();
        if block_fill {
            self.release.notified().await;
        }
        if block_reused {
            self.reused_release.notified().await;
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct ScenarioTrace {
    events: Vec<EngineEvent>,
}

#[tokio::test(flavor = "current_thread")]
async fn physical_fill_is_shared_across_coherent_snapshots_and_replays() {
    let first = run_scenario(Scenario::SharedFill).await;
    let second = run_scenario(Scenario::SharedFill).await;
    assert_eq!(first, second);
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_physical_fill_releases_waiters_and_replays() {
    let first = run_scenario(Scenario::CancelOwner).await;
    let second = run_scenario(Scenario::CancelOwner).await;
    assert_eq!(first, second);
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_during_waiter_wake_preserves_the_published_fill() {
    let first = run_scenario(Scenario::CancelWokenWaiter).await;
    let second = run_scenario(Scenario::CancelWokenWaiter).await;
    assert_eq!(first, second);
}

#[tokio::test(flavor = "current_thread")]
async fn capacity_admission_and_eviction_decisions_replay() {
    let first = run_capacity_scenario().await;
    let second = run_capacity_scenario().await;
    assert_eq!(first, second);
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Scenario {
    SharedFill,
    CancelOwner,
    CancelWokenWaiter,
}

#[derive(Debug, Eq, PartialEq)]
enum CapacityDecision {
    Admitted,
    Evicted,
}

async fn run_scenario(scenario: Scenario) -> ScenarioTrace {
    let name = format!(
        "relation-cache-determinism-{}",
        STORE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let store = Arc::new(Store::memory(&name).await.expect("open memory store"));
    let catalog = catalog::Catalog::new(store.clone());
    let stable = catalog
        .create_table(join_table(10, "stable"))
        .await
        .expect("create stable table");
    let changing = catalog
        .create_table(join_table(20, "changing"))
        .await
        .expect("create changing table");
    let mut statistics = complete_stats(&stable, 2);
    statistics
        .synopsis_models
        .extend(complete_stats(&changing, 5_000).synopsis_models);
    let gate = Arc::new(FillGate::new(scenario == Scenario::CancelWokenWaiter));
    let engine = Arc::new(
        Engine::new(store.clone())
            .with_statistics_provider(Arc::new(FixedPlannerStats(Arc::new(statistics))))
            .with_event_hook(gate.clone()),
    );
    engine
        .create_many("stable", vec![join_row("s1", "a"), join_row("s2", "b")])
        .await
        .expect("seed stable table");
    engine
        .create("changing", join_row("f1", "a"))
        .await
        .expect("seed changing table");
    let query = join_query();
    let expected_before = engine
        .execute_uncached(query.clone())
        .await
        .expect("execute initial oracle query");

    let owner = {
        let engine = engine.clone();
        let query = query.clone();
        tokio::spawn(async move { engine.execute(query).await })
    };
    gate.wait_for_first_fill().await;

    engine
        .create("changing", join_row("f2", "b"))
        .await
        .expect("advance changing generation");
    let expected_after = engine
        .execute_uncached(query.clone())
        .await
        .expect("execute current oracle query");
    let waiter = {
        let engine = engine.clone();
        let waiter_query = query.clone();
        tokio::spawn(async move { engine.execute(waiter_query).await })
    };
    gate.wait_for_lookup_count(2).await;

    if scenario == Scenario::CancelOwner {
        owner.abort();
        assert!(
            owner
                .await
                .expect_err("cancelled owner must not complete")
                .is_cancelled()
        );
        let actual_after = waiter
            .await
            .expect("join waiter task")
            .expect("execute waiter query");
        assert_eq!(actual_after, expected_after);
    } else if scenario == Scenario::SharedFill {
        gate.release();
        let actual_before = owner
            .await
            .expect("join fill owner task")
            .expect("execute old-snapshot query");
        let actual_after = waiter
            .await
            .expect("join fill waiter task")
            .expect("execute current-snapshot query");
        assert_eq!(actual_before, expected_before);
        assert_eq!(actual_after, expected_after);
    } else {
        gate.release();
        gate.wait_for_reused_lookup().await;
        let actual_before = owner
            .await
            .expect("join fill owner task")
            .expect("execute old-snapshot query");
        assert_eq!(actual_before, expected_before);
        waiter.abort();
        assert!(
            waiter
                .await
                .expect_err("cancelled waiter must not complete")
                .is_cancelled()
        );
        let recovered = engine
            .execute(query)
            .await
            .expect("execute after waiter cancellation");
        assert_eq!(recovered, expected_after);
    }

    let events = gate.events();
    let fills = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                EngineEvent::SubrelationCacheFillStarted {
                    materialization: RelationCacheMaterialization::HashJoinBuild,
                    ..
                }
            )
        })
        .count();
    if scenario == Scenario::CancelOwner {
        assert_eq!(fills, 2);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                EngineEvent::SubrelationCacheLookupCompleted {
                    materialization: RelationCacheMaterialization::HashJoinBuild,
                    result: RelationCacheLookupResult::Filled,
                    ..
                }
            )
        }));
    } else {
        assert_eq!(fills, 1);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                EngineEvent::SubrelationCacheLookupCompleted {
                    materialization: RelationCacheMaterialization::HashJoinBuild,
                    result: RelationCacheLookupResult::Reused,
                    ..
                }
            )
        }));
    }
    store.close().await.expect("close memory store");
    ScenarioTrace { events }
}

async fn run_capacity_scenario() -> ScenarioTrace {
    let name = format!(
        "relation-cache-capacity-determinism-{}",
        STORE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let store = Arc::new(Store::memory(&name).await.expect("open memory store"));
    let catalog = catalog::Catalog::new(store.clone());
    let stable = catalog
        .create_table(join_table(10, "stable"))
        .await
        .expect("create stable table");
    let changing = catalog
        .create_table(join_table(20, "changing"))
        .await
        .expect("create changing table");
    let mut statistics = complete_stats(&stable, 2);
    statistics
        .synopsis_models
        .extend(complete_stats(&changing, 5_000).synopsis_models);
    let recorder = Arc::new(EventRecorder::default());
    let engine = Engine::new(store.clone())
        .with_statistics_provider(Arc::new(FixedPlannerStats(Arc::new(statistics))))
        .with_event_hook(recorder.clone())
        .with_relation_cache_limits(RelationCacheLimits {
            byte_limit: 64 * 1024,
            entry_limit: 1,
            result_byte_limit: 4 * 1024,
        });
    engine
        .create_many("stable", vec![join_row("s1", "a"), join_row("s2", "b")])
        .await
        .expect("seed stable table");
    engine
        .create("changing", join_row("f1", "a"))
        .await
        .expect("seed changing table");
    let query = join_query_with_tag("x".repeat(8 * 1024));

    engine
        .execute(query.clone())
        .await
        .expect("execute first cache generation");
    engine
        .create("stable", join_row("s3", "a"))
        .await
        .expect("advance stable generation");
    engine
        .execute(query)
        .await
        .expect("execute second cache generation");

    let events = recorder.events();
    let physical_decisions = events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::RelationCacheAdmissionCompleted {
                materialization: RelationCacheMaterialization::HashJoinBuild,
                result: RelationCacheAdmissionResult::Admitted,
                ..
            } => Some(CapacityDecision::Admitted),
            EngineEvent::RelationCacheEntryEvicted {
                materialization: RelationCacheMaterialization::HashJoinBuild,
                cause: RelationCacheEvictionCause::Capacity,
                ..
            } => Some(CapacityDecision::Evicted),
            _ => None,
        })
        .collect::<Vec<_>>();
    let root_rejections = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                EngineEvent::RelationCacheAdmissionCompleted {
                    materialization: RelationCacheMaterialization::QueryResult,
                    result: RelationCacheAdmissionResult::TooLarge,
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        (physical_decisions, root_rejections),
        (
            vec![
                CapacityDecision::Admitted,
                CapacityDecision::Evicted,
                CapacityDecision::Admitted,
            ],
            2,
        )
    );
    store.close().await.expect("close memory store");
    ScenarioTrace { events }
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
                name: "key".into(),
                scalar_type: ScalarType::Text,
                nullable: false,
                format: String::new(),
                default: None,
            },
        ],
        primary_key: vec!["id".into()],
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
    }
}

fn join_row(id: &str, key: &str) -> Row {
    Row::from([
        ("id".into(), Value::Text(id.into())),
        ("key".into(), Value::Text(key.into())),
    ])
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

fn join_query() -> lir::Query {
    let joined = Relation::Join {
        left: Box::new(Relation::Scan {
            table: "stable".into(),
            scope: "stable".into(),
        }),
        right: Box::new(Relation::Scan {
            table: "changing".into(),
            scope: "changing".into(),
        }),
        kind: lir::JoinKind::Inner,
        on: Expr::Binary {
            op: BinaryOp::Eq,
            left: Box::new(column("stable", "key")),
            right: Box::new(column("changing", "key")),
        },
    };
    lir::Query {
        root: Relation::Order {
            input: Box::new(Relation::Project {
                input: Box::new(joined),
                scope: Some("result".into()),
                spread: Vec::new(),
                fields: vec![
                    lir::ProjectField {
                        name: "stable_id".into(),
                        expression: column("stable", "id"),
                    },
                    lir::ProjectField {
                        name: "changing_id".into(),
                        expression: column("changing", "id"),
                    },
                ],
            }),
            terms: vec![lir::OrderTerm {
                expression: column("result", "changing_id"),
                descending: false,
            }],
        },
        cardinality: RootCardinality::Many,
        bindings: HashMap::new(),
    }
}

fn join_query_with_tag(tag: String) -> lir::Query {
    let mut query = join_query();
    let Relation::Order { input, .. } = &mut query.root else {
        unreachable!("join query root is ordered")
    };
    let Relation::Project { fields, .. } = input.as_mut() else {
        unreachable!("join query input is projected")
    };
    fields.push(lir::ProjectField {
        name: "tag".into(),
        expression: Expr::Literal(Literal {
            raw: RawScalar::Text(tag),
            kind: Some(Kind::Text),
        }),
    });
    query
}

fn column(scope: &str, name: &str) -> Expr {
    Expr::Column {
        scope: scope.into(),
        name: name.into(),
    }
}
