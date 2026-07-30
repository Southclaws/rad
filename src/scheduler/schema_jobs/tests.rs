use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Notify;
use tokio::time::{sleep, timeout};

use crate::engine::catalog;
use crate::engine::catalog::identity::{SchemaId, TransitionId};
use crate::engine::catalog::model::{ColumnDef, IndexDef, ScalarType, TableDef, TransitionState};
use crate::engine::exec::schema_jobs::SchemaJob;
use crate::engine::exec::{
    CatalogPolicy, Engine, EngineEvent, EngineEventHook, EngineOperation, GateAction, Program,
    Statement, codec,
};
use crate::engine::kv::ErrorKind as KvErrorKind;
use crate::engine::kv::fault::{FaultAction, FaultController, FaultRule, FaultingKv, Operation};
use crate::engine::kv::slatedb::Store;
use crate::engine::kv::{Kv, TransactionalKv};
use crate::engine::lir::{Row, Value};

use super::{SchemaJobConfig, SchemaJobEvent, SchemaJobHook, SchemaJobRunner};

fn column(id: u32, name: &str) -> ColumnDef {
    ColumnDef {
        id: SchemaId::new(id).unwrap(),
        name: name.into(),
        scalar_type: ScalarType::Text,
        nullable: false,
        format: String::new(),
        default: None,
    }
}

async fn seed_index_work(name: &str) -> (Arc<Store>, Arc<Engine>, catalog::Catalog, TransitionId) {
    let store = Arc::new(Store::memory(name).await.unwrap());
    let catalog = catalog::Catalog::new(store.clone());
    let table = catalog
        .create_table(TableDef {
            id: SchemaId::new(1).unwrap(),
            name: "items".into(),
            columns: vec![column(1, "id"), column(2, "status")],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    let engine = Arc::new(Engine::new(store.clone()));
    for (id, status) in [("a", "open"), ("b", "closed"), ("c", "open")] {
        engine
            .create(
                "items",
                Row::from([
                    ("id".into(), Value::Text(id.into())),
                    ("status".into(), Value::Text(status.into())),
                ]),
            )
            .await
            .unwrap();
    }
    let transition_id = start_index_work(&engine, table.schema_id).await;
    let transition = engine
        .inspect_schema_transition(&transition_id)
        .await
        .unwrap();
    assert_eq!(transition.table_id, table.id);
    (store, engine, catalog, transition.id)
}

async fn start_index_work(engine: &Engine, table_id: SchemaId) -> TransitionId {
    start_index_work_with_uniqueness(engine, table_id, false).await
}

async fn start_index_work_with_uniqueness(
    engine: &Engine,
    table_id: SchemaId,
    unique: bool,
) -> TransitionId {
    engine
        .execute_program(
            Program {
                statements: vec![Statement::StartIndexBuild {
                    name: "start_status_idx".into(),
                    table_id,
                    index: IndexDef {
                        name: "status_idx".into(),
                        columns: vec!["status".into()],
                        unique,
                    },
                    prerequisites: Vec::new(),
                    after: Vec::new(),
                }],
                result: None,
            },
            CatalogPolicy::RevisionPerProgram,
        )
        .await
        .unwrap()
        .statements
        .into_iter()
        .next()
        .unwrap()
        .control
        .unwrap()
        .transition_id
}

#[derive(Default)]
struct RecordingEngineHook {
    events: Mutex<Vec<EngineEvent>>,
}

#[async_trait]
impl EngineEventHook for RecordingEngineHook {
    async fn reach(&self, event: EngineEvent) {
        self.events
            .lock()
            .expect("engine event lock poisoned")
            .push(event);
    }
}

impl RecordingEngineHook {
    fn snapshot(&self) -> Vec<EngineEvent> {
        self.events
            .lock()
            .expect("engine event lock poisoned")
            .clone()
    }
}

async fn wait_terminal(engine: &Engine, id: &TransitionId) {
    timeout(Duration::from_secs(5), async {
        loop {
            if engine
                .inspect_schema_transition(id)
                .await
                .unwrap()
                .state
                .is_terminal()
            {
                return;
            }
            sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("schema transition did not converge");
}

fn fast_config() -> SchemaJobConfig {
    SchemaJobConfig {
        transition_batch_size: 1,
        reclamation_batch_size: 1,
        batches_per_round: 2,
        items_per_round: 2,
        yield_interval: Duration::from_millis(1),
        idle_poll_interval: Duration::from_secs(30),
        retry_backoff_min: Duration::from_millis(1),
        retry_backoff_max: Duration::from_millis(10),
        catalog_history_retain: 8,
        catalog_history_batch_size: 2,
        ..SchemaJobConfig::default()
    }
}

#[tokio::test]
async fn startup_discovery_runs_transition_and_cleanup_work() {
    let (store, engine, _, transition_id) = seed_index_work("scheduler-startup").await;
    let runner = SchemaJobRunner::start(engine.clone(), fast_config()).unwrap();

    wait_terminal(&engine, &transition_id).await;
    timeout(Duration::from_secs(5), async {
        loop {
            let metrics = engine.schema_storage_metrics().await.unwrap();
            if metrics.pending_reclamations == 0 && metrics.uncompacted_terminal_transitions == 0 {
                return;
            }
            sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("schema cleanup did not converge");

    let stats = runner.stats();
    assert!(stats.rounds > 0);
    assert!(stats.batches > 0);
    assert!(stats.items >= 3);
    assert_eq!(runner.last_error(), None);
    runner.shutdown().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn engine_events_cover_physical_checkpoint_gate_publication_and_compaction_boundaries() {
    let store = Arc::new(Store::memory("scheduler-engine-events").await.unwrap());
    let catalog = catalog::Catalog::new(store.clone());
    let table = catalog
        .create_table(TableDef {
            id: SchemaId::new(1).unwrap(),
            name: "items".into(),
            columns: vec![column(1, "id"), column(2, "status")],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    let events = Arc::new(RecordingEngineHook::default());
    let engine = Arc::new(Engine::new(store.clone()).with_event_hook(events.clone()));
    for (id, status) in [("a", "one"), ("b", "two"), ("c", "three")] {
        engine
            .create(
                "items",
                Row::from([
                    ("id".into(), Value::Text(id.into())),
                    ("status".into(), Value::Text(status.into())),
                ]),
            )
            .await
            .unwrap();
    }
    let transition_id = start_index_work_with_uniqueness(&engine, table.schema_id, true).await;
    let runner = SchemaJobRunner::start(engine.clone(), fast_config()).unwrap();

    wait_terminal(&engine, &transition_id).await;
    timeout(Duration::from_secs(5), async {
        loop {
            let metrics = engine.schema_storage_metrics().await.unwrap();
            if metrics.pending_reclamations == 0 && metrics.uncompacted_terminal_transitions == 0 {
                break;
            }
            sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("schema cleanup did not converge");
    runner.shutdown().await.unwrap();

    let recorded = events.snapshot();
    assert!(recorded.iter().any(|event| matches!(
        event,
        EngineEvent::CommitSucceeded {
            operation: EngineOperation::CatalogProgram { .. }
        }
    )));
    assert!(
        recorded
            .iter()
            .any(|event| matches!(event, EngineEvent::PhysicalBatchStaged { items: 1, .. }))
    );
    assert!(
        recorded
            .iter()
            .any(|event| matches!(event, EngineEvent::CheckpointStaged { .. }))
    );
    assert!(recorded.iter().any(|event| matches!(
        event,
        EngineEvent::FinalizationGateStaged {
            action: GateAction::Acquired,
            ..
        }
    )));
    assert!(recorded.iter().any(|event| matches!(
        event,
        EngineEvent::FinalizationGateStaged {
            action: GateAction::Released,
            ..
        }
    )));
    assert!(
        recorded
            .iter()
            .any(|event| matches!(event, EngineEvent::WriteProtocolStaged { .. }))
    );
    assert!(recorded.iter().any(|event| matches!(
        event,
        EngineEvent::CatalogPublicationStaged {
            state: crate::engine::catalog::model::TransitionState::Ready,
            ..
        }
    )));
    assert!(
        recorded
            .iter()
            .any(|event| matches!(event, EngineEvent::ReclamationCheckpointStaged { .. }))
    );
    assert!(
        recorded
            .iter()
            .any(|event| matches!(event, EngineEvent::CompactionStaged { .. }))
    );
    let encoded = serde_json::to_string(&recorded).unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<EngineEvent>>(&encoded).unwrap(),
        recorded
    );

    store.close().await.unwrap();
}

#[tokio::test]
async fn engine_catalog_program_wakes_an_idle_runner() {
    let store = Arc::new(Store::memory("scheduler-catalog-wake").await.unwrap());
    let catalog = catalog::Catalog::new(store.clone());
    let table = catalog
        .create_table(TableDef {
            id: SchemaId::new(1).unwrap(),
            name: "items".into(),
            columns: vec![column(1, "id"), column(2, "status")],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    let engine = Arc::new(Engine::new(store.clone()));
    let hook = Arc::new(IdleHook {
        reached: Notify::new(),
    });
    let runner =
        SchemaJobRunner::start_with_hook(engine.clone(), fast_config(), hook.clone()).unwrap();
    timeout(Duration::from_secs(5), hook.reached.notified())
        .await
        .expect("runner did not become idle");

    let transition_id = start_index_work(&engine, table.schema_id).await;
    wait_terminal(&engine, &transition_id).await;

    runner.shutdown().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn direct_catalog_observer_remains_a_wake_hint() {
    let store = Arc::new(
        Store::memory("scheduler-direct-catalog-wake")
            .await
            .unwrap(),
    );
    let catalog = catalog::Catalog::new(store.clone());
    let engine = Arc::new(Engine::new(store.clone()));
    let hook = Arc::new(IdleHook {
        reached: Notify::new(),
    });
    let runner = SchemaJobRunner::start_with_hook(engine, fast_config(), hook.clone()).unwrap();
    runner.observe_catalog(&catalog);
    timeout(Duration::from_secs(5), hook.reached.notified())
        .await
        .expect("runner did not become idle");

    catalog
        .create_table(TableDef {
            id: SchemaId::new(10).unwrap(),
            name: "wake_hint".into(),
            columns: vec![column(10, "id")],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();

    timeout(Duration::from_secs(5), hook.reached.notified())
        .await
        .expect("direct catalog mutation did not wake the runner");
    runner.shutdown().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn retryable_claim_conflict_is_backed_off_and_recovered() {
    let (store, _, _, transition_id) = seed_index_work("scheduler-retry").await;
    let controller = FaultController::new(vec![FaultRule {
        operation: Operation::Commit,
        occurrence: 1,
        action: FaultAction::ErrorBefore(KvErrorKind::Conflict),
    }]);
    let faulting = Arc::new(FaultingKv::new(store.clone(), controller));
    let engine = Arc::new(Engine::new(faulting));
    let runner = SchemaJobRunner::start(engine.clone(), fast_config()).unwrap();

    wait_terminal(&engine, &transition_id).await;
    assert!(runner.stats().retries >= 1);
    assert_eq!(runner.stats().quarantined, 0);

    runner.shutdown().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn corrupt_job_is_quarantined_without_starving_independent_reclamation() {
    let (store, engine, catalog, transition_id) = seed_index_work("scheduler-quarantine").await;
    let owner = engine
        .claim_schema_transition(&transition_id)
        .await
        .unwrap();
    loop {
        let step = engine
            .step_schema_transition(&transition_id, owner, 16)
            .await
            .unwrap();
        if step.transition.state == TransitionState::CatchingUp {
            break;
        }
        assert!(!step.transition.state.is_terminal());
    }
    engine
        .create(
            "items",
            Row::from([
                ("id".into(), Value::Text("delta".into())),
                ("status".into(), Value::Text("new".into())),
            ]),
        )
        .await
        .unwrap();
    Kv::put(
        &*store,
        Bytes::from(crate::engine::catalog::store::delta_key(&transition_id, 1)),
        Bytes::from_static(b"{"),
    )
    .await
    .unwrap();

    let retired = catalog
        .create_table(TableDef {
            id: SchemaId::new(20).unwrap(),
            name: "quarantine_reclaim".into(),
            columns: vec![column(20, "id")],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    engine
        .create(
            "quarantine_reclaim",
            Row::from([("id".into(), Value::Text("retired".into()))]),
        )
        .await
        .unwrap();
    let primary_key = codec::encode_row_tuple(
        &Row::from([("id".into(), Value::Text("retired".into()))]),
        &retired.primary_key,
    )
    .unwrap();
    let retired_key = codec::data_key(&retired, &primary_key);
    catalog.delete_table("quarantine_reclaim").await.unwrap();

    let runner = SchemaJobRunner::start(
        engine.clone(),
        SchemaJobConfig {
            max_failures: 1,
            ..fast_config()
        },
    )
    .unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            if runner.stats().quarantined >= 1
                && Kv::get(&*store, &retired_key).await.unwrap().is_none()
            {
                return;
            }
            sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("bad transition starved independent reclamation");
    let bad = engine
        .inspect_schema_transition(&transition_id)
        .await
        .unwrap();
    assert_eq!(bad.state, TransitionState::CatchingUp);
    assert!(!bad.last_error.is_empty());

    runner.shutdown().await.unwrap();
    store.close().await.unwrap();
}

#[derive(Clone, Copy)]
enum UnknownOutcomeBoundary {
    Gate,
    Publication,
}

struct UnknownCommitAtBoundary {
    boundary: UnknownOutcomeBoundary,
    controller: FaultController,
    armed: AtomicBool,
}

#[async_trait]
impl EngineEventHook for UnknownCommitAtBoundary {
    async fn reach(&self, event: EngineEvent) {
        let selected = match self.boundary {
            UnknownOutcomeBoundary::Gate => matches!(
                event,
                EngineEvent::FinalizationGateStaged {
                    action: GateAction::Acquired,
                    ..
                }
            ),
            UnknownOutcomeBoundary::Publication => matches!(
                event,
                EngineEvent::CatalogPublicationStaged {
                    state: crate::engine::catalog::model::TransitionState::Ready,
                    ..
                }
            ),
        };
        if selected && !self.armed.swap(true, Ordering::AcqRel) {
            self.controller.inject_next(
                Operation::Commit,
                FaultAction::ErrorAfter(KvErrorKind::CommitOutcomeUnknown),
            );
        }
    }
}

#[tokio::test]
async fn unknown_commit_outcomes_after_gate_and_publication_are_recovered() {
    for (name, boundary) in [
        ("gate", UnknownOutcomeBoundary::Gate),
        ("publication", UnknownOutcomeBoundary::Publication),
    ] {
        let path = format!("scheduler-unknown-{name}");
        let store = Arc::new(Store::memory(&path).await.unwrap());
        let catalog = catalog::Catalog::new(store.clone());
        let table = catalog
            .create_table(TableDef {
                id: SchemaId::new(1).unwrap(),
                name: "items".into(),
                columns: vec![column(1, "id"), column(2, "status")],
                primary_key: vec!["id".into()],
                indexes: Vec::new(),
                foreign_keys: Vec::new(),
            })
            .await
            .unwrap();
        let bootstrap = Engine::new(store.clone());
        for (id, status) in [("a", "one"), ("b", "two"), ("c", "three")] {
            bootstrap
                .create(
                    "items",
                    Row::from([
                        ("id".into(), Value::Text(id.into())),
                        ("status".into(), Value::Text(status.into())),
                    ]),
                )
                .await
                .unwrap();
        }
        let transition_id =
            start_index_work_with_uniqueness(&bootstrap, table.schema_id, true).await;
        let controller = FaultController::default();
        let faulting = Arc::new(FaultingKv::new(store.clone(), controller.clone()));
        let hook = Arc::new(UnknownCommitAtBoundary {
            boundary,
            controller,
            armed: AtomicBool::new(false),
        });
        let engine = Arc::new(Engine::new(faulting).with_event_hook(hook.clone()));
        let runner = SchemaJobRunner::start(engine.clone(), fast_config()).unwrap();

        wait_terminal(&engine, &transition_id).await;
        assert!(hook.armed.load(Ordering::Acquire));
        timeout(Duration::from_secs(5), async {
            while runner.stats().retries == 0 {
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("unknown commit outcome was not observed by the scheduler");
        runner.shutdown().await.unwrap();
        store.close().await.unwrap();
    }
}

struct IdleHook {
    reached: Notify,
}

#[async_trait]
impl SchemaJobHook for IdleHook {
    async fn reach(&self, event: SchemaJobEvent) {
        if matches!(event, SchemaJobEvent::Idle) {
            self.reached.notify_one();
        }
    }
}

struct BlockingHook {
    reached: Notify,
    release: Notify,
    held: AtomicBool,
}

#[async_trait]
impl SchemaJobHook for BlockingHook {
    async fn reach(&self, event: SchemaJobEvent) {
        if matches!(
            event,
            SchemaJobEvent::BeforeJob {
                job: SchemaJob::Transition(_)
            }
        ) && !self.held.swap(true, Ordering::AcqRel)
        {
            self.reached.notify_one();
            self.release.notified().await;
        }
    }
}

#[tokio::test]
async fn shutdown_waits_for_the_selected_bounded_step() {
    let (store, engine, _, transition_id) = seed_index_work("scheduler-shutdown").await;
    let hook = Arc::new(BlockingHook {
        reached: Notify::new(),
        release: Notify::new(),
        held: AtomicBool::new(false),
    });
    let runner =
        SchemaJobRunner::start_with_hook(engine.clone(), fast_config(), hook.clone()).unwrap();
    timeout(Duration::from_secs(5), hook.reached.notified())
        .await
        .expect("worker did not reach the transition boundary");

    let shutdown = runner.shutdown();
    tokio::pin!(shutdown);
    assert!(
        timeout(Duration::from_millis(20), &mut shutdown)
            .await
            .is_err()
    );
    hook.release.notify_one();
    timeout(Duration::from_secs(5), shutdown)
        .await
        .expect("graceful shutdown did not finish")
        .unwrap();

    let transition = engine
        .inspect_schema_transition(&transition_id)
        .await
        .unwrap();
    assert_eq!(transition.rows_scanned, 1);
    store.close().await.unwrap();
}

#[test]
fn configuration_rejects_zero_work_bounds_and_inverted_backoff() {
    let config = SchemaJobConfig {
        items_per_round: 0,
        ..SchemaJobConfig::default()
    };
    assert!(config.validate().is_err());
    let config = SchemaJobConfig {
        retry_backoff_min: Duration::from_secs(2),
        retry_backoff_max: Duration::from_secs(1),
        ..SchemaJobConfig::default()
    };
    assert!(config.validate().is_err());
    let config = SchemaJobConfig {
        idle_poll_interval: Duration::ZERO,
        ..SchemaJobConfig::default()
    };
    assert!(config.validate().is_err());
}
