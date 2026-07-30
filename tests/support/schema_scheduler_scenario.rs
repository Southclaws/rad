#![allow(dead_code)]

use std::collections::BTreeMap;
use std::future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use rad::engine::catalog;
use rad::engine::catalog::identity::{SchemaId, TransitionId};
use rad::engine::catalog::model::{
    Column, ColumnConversion, ColumnDef, ColumnReplacementDef, ConstraintDef, ConstraintKind,
    IndexDef, ScalarType, TableDef, TransitionState,
};
use rad::engine::exec::{
    CatalogPolicy, Engine, EngineEvent, EngineEventHook, EngineOperation, ErrorReason, Program,
    Statement,
};
use rad::engine::kv::fault::{FaultController, FaultingKv, RedactedTraceEvent};
use rad::engine::kv::key_encoding::prefix_end;
use rad::engine::kv::slatedb::Store;
use rad::engine::kv::{KeyRange, Kv, TransactionalKv};
use rad::engine::lir::{Field, Kind, Row, RowType, SlotId, Type, Value};
use rad::runtime::RuntimeEffects;
use rad::scheduler::schema_jobs::{SchemaJobConfig, SchemaJobRunner};
use serde::{Deserialize, Serialize};
use slatedb::object_store::ObjectStore;
use slatedb::object_store::memory::InMemory;
use uuid::Uuid;

const SEED_DERIVATION: &str = "splitmix64-domain-v1";
const TURMOIL_DOMAIN: u64 = 0x7475_726d_6f69_6c01;
const RUNTIME_DOMAIN: u64 = 0x7275_6e74_696d_6501;
const AMBIENT_DOMAIN: u64 = 0x616d_6269_656e_7401;

pub fn ambient_seed(master: u64) -> u64 {
    derive_seed(master, AMBIENT_DOMAIN)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scenario {
    IndexBuild,
    ColumnReplacement,
    ConstraintValidation,
    DependencyGraph,
    CancelledIndex,
    CancelledReplacement,
    CancelledConstraint,
}

impl Scenario {
    pub const ALL: [Self; 7] = [
        Self::IndexBuild,
        Self::ColumnReplacement,
        Self::ConstraintValidation,
        Self::DependencyGraph,
        Self::CancelledIndex,
        Self::CancelledReplacement,
        Self::CancelledConstraint,
    ];

    pub const NORMAL: [Self; 4] = [
        Self::IndexBuild,
        Self::ColumnReplacement,
        Self::ConstraintValidation,
        Self::DependencyGraph,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::IndexBuild => "index_build",
            Self::ColumnReplacement => "column_replacement",
            Self::ConstraintValidation => "constraint_validation",
            Self::DependencyGraph => "dependency_graph",
            Self::CancelledIndex => "cancelled_index",
            Self::CancelledReplacement => "cancelled_replacement",
            Self::CancelledConstraint => "cancelled_constraint",
        }
    }

    pub fn parse(value: &str) -> Result<Self, Box<dyn std::error::Error>> {
        Self::ALL
            .into_iter()
            .find(|scenario| scenario.name() == value)
            .ok_or_else(|| format!("unknown DST scenario {value:?}").into())
    }

    pub fn boundaries(self) -> &'static [CrashBoundary] {
        match self {
            Self::DependencyGraph => &CrashBoundary::DEPENDENCY,
            Self::CancelledIndex | Self::CancelledReplacement | Self::CancelledConstraint => {
                &CrashBoundary::CANCELLATION
            }
            _ => &CrashBoundary::TRANSITION,
        }
    }

    fn is_cancelled(self) -> bool {
        matches!(
            self,
            Self::CancelledIndex | Self::CancelledReplacement | Self::CancelledConstraint
        )
    }

    fn is_replacement(self) -> bool {
        matches!(self, Self::ColumnReplacement | Self::CancelledReplacement)
    }
}

pub fn campaign_cases() -> Vec<(Scenario, CrashBoundary)> {
    let maximum_boundaries = Scenario::ALL
        .iter()
        .map(|scenario| scenario.boundaries().len())
        .max()
        .unwrap_or(0);
    (0..maximum_boundaries)
        .flat_map(|boundary_index| {
            Scenario::ALL.into_iter().filter_map(move |scenario| {
                scenario
                    .boundaries()
                    .get(boundary_index)
                    .copied()
                    .map(|boundary| (scenario, boundary))
            })
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CrashBoundary {
    PhysicalBatch,
    Checkpoint,
    Gate,
    WriteProtocol,
    Publication,
    Compaction,
    CommitStarted,
    CommitSucceeded,
    ReclamationPhysicalBatch,
    ReclamationCheckpoint,
    ReclamationCompaction,
    ReclamationCommitStarted,
    ReclamationCommitSucceeded,
    ActivationCommitStarted,
    ActivationCommitSucceeded,
    CancellationCommitStarted,
    CancellationCommitSucceeded,
}

impl CrashBoundary {
    pub const TRANSITION: [Self; 12] = [
        Self::PhysicalBatch,
        Self::Checkpoint,
        Self::Gate,
        Self::WriteProtocol,
        Self::Publication,
        Self::CommitStarted,
        Self::CommitSucceeded,
        Self::ReclamationPhysicalBatch,
        Self::ReclamationCheckpoint,
        Self::ReclamationCompaction,
        Self::ReclamationCommitStarted,
        Self::ReclamationCommitSucceeded,
    ];

    pub const DEPENDENCY: [Self; 14] = [
        Self::PhysicalBatch,
        Self::Checkpoint,
        Self::Gate,
        Self::WriteProtocol,
        Self::Publication,
        Self::CommitStarted,
        Self::CommitSucceeded,
        Self::ReclamationPhysicalBatch,
        Self::ReclamationCheckpoint,
        Self::ReclamationCompaction,
        Self::ReclamationCommitStarted,
        Self::ReclamationCommitSucceeded,
        Self::ActivationCommitStarted,
        Self::ActivationCommitSucceeded,
    ];

    pub const CANCELLATION: [Self; 7] = [
        Self::CancellationCommitStarted,
        Self::CancellationCommitSucceeded,
        Self::ReclamationPhysicalBatch,
        Self::ReclamationCheckpoint,
        Self::ReclamationCompaction,
        Self::ReclamationCommitStarted,
        Self::ReclamationCommitSucceeded,
    ];

    pub const ALL: [Self; 16] = [
        Self::PhysicalBatch,
        Self::Checkpoint,
        Self::Gate,
        Self::WriteProtocol,
        Self::Publication,
        Self::CommitStarted,
        Self::CommitSucceeded,
        Self::ReclamationPhysicalBatch,
        Self::ReclamationCheckpoint,
        Self::ReclamationCompaction,
        Self::ReclamationCommitStarted,
        Self::ReclamationCommitSucceeded,
        Self::ActivationCommitStarted,
        Self::ActivationCommitSucceeded,
        Self::CancellationCommitStarted,
        Self::CancellationCommitSucceeded,
    ];

    const KNOWN: [Self; 17] = [
        Self::PhysicalBatch,
        Self::Checkpoint,
        Self::Gate,
        Self::WriteProtocol,
        Self::Publication,
        Self::Compaction,
        Self::CommitStarted,
        Self::CommitSucceeded,
        Self::ReclamationPhysicalBatch,
        Self::ReclamationCheckpoint,
        Self::ReclamationCompaction,
        Self::ReclamationCommitStarted,
        Self::ReclamationCommitSucceeded,
        Self::ActivationCommitStarted,
        Self::ActivationCommitSucceeded,
        Self::CancellationCommitStarted,
        Self::CancellationCommitSucceeded,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::PhysicalBatch => "physical_batch",
            Self::Checkpoint => "checkpoint",
            Self::Gate => "gate",
            Self::WriteProtocol => "write_protocol",
            Self::Publication => "publication",
            Self::Compaction => "compaction",
            Self::CommitStarted => "commit_started",
            Self::CommitSucceeded => "commit_succeeded",
            Self::ReclamationPhysicalBatch => "reclamation_physical_batch",
            Self::ReclamationCheckpoint => "reclamation_checkpoint",
            Self::ReclamationCompaction => "reclamation_compaction",
            Self::ReclamationCommitStarted => "reclamation_commit_started",
            Self::ReclamationCommitSucceeded => "reclamation_commit_succeeded",
            Self::ActivationCommitStarted => "activation_commit_started",
            Self::ActivationCommitSucceeded => "activation_commit_succeeded",
            Self::CancellationCommitStarted => "cancellation_commit_started",
            Self::CancellationCommitSucceeded => "cancellation_commit_succeeded",
        }
    }

    pub fn parse(value: &str) -> Result<Self, Box<dyn std::error::Error>> {
        Self::KNOWN
            .into_iter()
            .find(|boundary| boundary.name() == value)
            .ok_or_else(|| format!("unknown crash boundary {value:?}").into())
    }

    fn matches(self, event: &EngineEvent) -> bool {
        match (self, event) {
            (Self::PhysicalBatch, EngineEvent::PhysicalBatchStaged { operation, .. })
            | (Self::Checkpoint, EngineEvent::CheckpointStaged { operation, .. })
            | (Self::Gate, EngineEvent::FinalizationGateStaged { operation, .. })
            | (Self::WriteProtocol, EngineEvent::WriteProtocolStaged { operation, .. })
            | (Self::Publication, EngineEvent::CatalogPublicationStaged { operation, .. }) => {
                matches!(operation, EngineOperation::StepTransition { .. })
            }
            (
                Self::Compaction,
                EngineEvent::CompactionStaged {
                    operation: EngineOperation::CompactTransition { .. },
                    ..
                },
            ) => true,
            (Self::CommitStarted, EngineEvent::CommitStarted { operation })
            | (Self::CommitSucceeded, EngineEvent::CommitSucceeded { operation }) => {
                matches!(operation, EngineOperation::StepTransition { .. })
            }
            (
                Self::ReclamationPhysicalBatch,
                EngineEvent::PhysicalBatchStaged {
                    operation: EngineOperation::StepReclamation { .. },
                    ..
                },
            )
            | (
                Self::ReclamationCheckpoint,
                EngineEvent::ReclamationCheckpointStaged {
                    operation: EngineOperation::StepReclamation { .. },
                    ..
                },
            )
            | (
                Self::ReclamationCompaction,
                EngineEvent::CompactionStaged {
                    operation: EngineOperation::StepReclamation { .. },
                    ..
                },
            ) => true,
            (
                Self::ReclamationCommitStarted,
                EngineEvent::CommitStarted {
                    operation: EngineOperation::StepReclamation { .. },
                },
            )
            | (
                Self::ReclamationCommitSucceeded,
                EngineEvent::CommitSucceeded {
                    operation: EngineOperation::StepReclamation { .. },
                },
            ) => true,
            (
                Self::ActivationCommitStarted,
                EngineEvent::CommitStarted {
                    operation: EngineOperation::ActivateTransition { .. },
                },
            )
            | (
                Self::ActivationCommitSucceeded,
                EngineEvent::CommitSucceeded {
                    operation: EngineOperation::ActivateTransition { .. },
                },
            ) => true,
            (
                Self::CancellationCommitStarted,
                EngineEvent::CommitStarted {
                    operation: EngineOperation::CancelTransition { .. },
                },
            )
            | (
                Self::CancellationCommitSucceeded,
                EngineEvent::CommitSucceeded {
                    operation: EngineOperation::CancelTransition { .. },
                },
            ) => true,
            _ => false,
        }
    }
}

pub fn run_case(
    seed: u64,
    scenario: Scenario,
    boundary: CrashBoundary,
) -> Result<(), Box<dyn std::error::Error>> {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let kv = FaultController::default();
    match simulate(seed, scenario, boundary, trace.clone(), kv.clone()) {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Err(artifact_error) = write_failure_artifact(
                seed,
                scenario,
                boundary,
                &trace,
                &kv.redacted_trace(),
                error.as_ref(),
            ) {
                return Err(format!(
                    "{error}; also failed to persist DST evidence: {artifact_error}"
                )
                .into());
            }
            Err(error)
        }
    }
}

pub fn capture_case_trace(
    seed: u64,
    scenario: Scenario,
    boundary: CrashBoundary,
) -> Result<CapturedTrace, Box<dyn std::error::Error>> {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let kv = FaultController::default();
    simulate(seed, scenario, boundary, trace.clone(), kv.clone())?;
    let engine_events = trace
        .lock()
        .expect("engine event trace lock poisoned")
        .clone();
    Ok(CapturedTrace {
        engine_events,
        kv_events: kv.redacted_trace(),
    })
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CapturedTrace {
    engine_events: Vec<EngineEvent>,
    kv_events: Vec<RedactedTraceEvent>,
}

impl CapturedTrace {
    pub fn kv_event_count(&self) -> usize {
        self.kv_events.len()
    }
}

#[derive(Clone, Debug)]
struct ScenarioState {
    transition_ids: Vec<TransitionId>,
    replacement_target: Option<Column>,
}

pub fn assert_trace_eq(
    scenario: Scenario,
    boundary: CrashBoundary,
    first: &CapturedTrace,
    replay: &CapturedTrace,
) {
    if first.engine_events != replay.engine_events {
        let index = first_mismatch(&first.engine_events, &replay.engine_events);
        panic!(
            "same-seed replay diverged in {} at the {} crash boundary: engine event {index}; first={:?}; replay={:?}; lengths={} and {}",
            scenario.name(),
            boundary.name(),
            first.engine_events.get(index),
            replay.engine_events.get(index),
            first.engine_events.len(),
            replay.engine_events.len(),
        );
    }
    if first.kv_events != replay.kv_events {
        let index = first_mismatch(&first.kv_events, &replay.kv_events);
        panic!(
            "same-seed replay diverged in {} at the {} crash boundary: KV event {index}; first={:?}; replay={:?}; lengths={} and {}",
            scenario.name(),
            boundary.name(),
            first.kv_events.get(index),
            replay.kv_events.get(index),
            first.kv_events.len(),
            replay.kv_events.len(),
        );
    }
}

fn first_mismatch<T: PartialEq>(left: &[T], right: &[T]) -> usize {
    left.iter()
        .zip(right)
        .position(|(left, right)| left != right)
        .unwrap_or_else(|| left.len().min(right.len()))
}

fn simulate(
    seed: u64,
    scenario: Scenario,
    boundary: CrashBoundary,
    trace: Arc<Mutex<Vec<EngineEvent>>>,
    kv: FaultController,
) -> Result<(), Box<dyn std::error::Error>> {
    let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let generation = Arc::new(AtomicUsize::new(0));
    let checkpoint = Arc::new(AtomicUsize::new(0));
    let scenario_state = Arc::new(Mutex::new(None));
    let runtime: Arc<dyn RuntimeEffects> =
        Arc::new(SeededRuntime::new(derive_seed(seed, RUNTIME_DOMAIN)));
    let path = format!("schema-scheduler-turmoil-{scenario:?}-{boundary:?}-{seed}");

    let mut builder = turmoil::Builder::new();
    builder
        .rng_seed(derive_seed(seed, TURMOIL_DOMAIN))
        .enable_random_order()
        .tick_duration(Duration::from_millis(1))
        .simulation_duration(Duration::from_secs(30));
    let mut simulation = builder.build();
    simulation.host("database", {
        let objects = objects.clone();
        let checkpoint = checkpoint.clone();
        let runtime = runtime.clone();
        move || {
            let objects = objects.clone();
            let generation = generation.clone();
            let checkpoint = checkpoint.clone();
            let scenario_state = scenario_state.clone();
            let trace = trace.clone();
            let kv = kv.clone();
            let runtime = runtime.clone();
            let path = path.clone();
            async move {
                let current = generation.fetch_add(1, Ordering::SeqCst);
                let store = Arc::new(Store::open(path, objects).await?);
                let traced: Arc<dyn TransactionalKv> = Arc::new(FaultingKv::new(store.clone(), kv));
                if current == 0 {
                    let bootstrap = Engine::with_runtime(traced.clone(), runtime.clone());
                    let state =
                        seed_scenario(scenario, &bootstrap, traced.clone(), runtime.clone())
                            .await?;
                    *scenario_state.lock().expect("scenario lock poisoned") = Some(state.clone());
                    let hook = Arc::new(CrashAtEngineBoundary {
                        boundary,
                        checkpoint: checkpoint.clone(),
                        reached: AtomicBool::new(false),
                        trace: trace.clone(),
                    });
                    let engine = Arc::new(
                        Engine::with_runtime(traced.clone(), runtime.clone()).with_event_hook(hook),
                    );
                    if scenario.is_cancelled() {
                        prepare_partial_transition_and_cancel(&engine, &state).await?;
                    }
                    let _runner_a = SchemaJobRunner::start(engine.clone(), scheduler_config())?;
                    let _runner_b = SchemaJobRunner::start(engine.clone(), scheduler_config())?;
                    tokio::spawn(write_foreground(engine, scenario, current));
                    future::pending::<()>().await;
                } else {
                    let engine = Arc::new(
                        Engine::with_runtime(traced.clone(), runtime.clone()).with_event_hook(
                            Arc::new(RecordEngineEvents {
                                trace: trace.clone(),
                            }),
                        ),
                    );
                    let state = scenario_state
                        .lock()
                        .expect("scenario lock poisoned")
                        .clone()
                        .expect("first host recorded its scenario state");
                    let runner_a = SchemaJobRunner::start(engine.clone(), scheduler_config())?;
                    let runner_b = SchemaJobRunner::start(engine.clone(), scheduler_config())?;
                    let foreground =
                        tokio::spawn(write_foreground(engine.clone(), scenario, current));
                    loop {
                        let mut terminal = true;
                        for id in &state.transition_ids {
                            terminal &= engine
                                .inspect_schema_transition(id)
                                .await?
                                .state
                                .is_terminal();
                        }
                        if terminal && engine.discover_schema_jobs(8).await?.is_empty() {
                            break;
                        }
                        // This is an external completion observer, not a workload actor.
                        // Keep it frequent enough to notice recovery promptly without
                        // flooding the semantic trace with read-only catalog polling.
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    runner_a.shutdown().await?;
                    runner_b.shutdown().await?;
                    foreground
                        .await?
                        .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
                    let metrics = engine.schema_storage_metrics().await?;
                    assert_eq!(metrics.pending_reclamations, 0);
                    assert_eq!(metrics.uncompacted_terminal_transitions, 0);
                    audit_scenario(scenario, boundary, &engine, store.clone(), &state).await?;
                    traced.close().await?;
                    checkpoint.store(2, Ordering::SeqCst);
                }
                Ok(())
            }
        }
    });

    step_until(&mut simulation, &checkpoint, 1)?;
    simulation.crash("database");
    simulation.bounce("database");
    step_until(&mut simulation, &checkpoint, 2)?;
    Ok(())
}

async fn prepare_partial_transition_and_cancel(
    engine: &Engine,
    state: &ScenarioState,
) -> turmoil::Result<()> {
    let [transition_id] = state.transition_ids.as_slice() else {
        return Err("cancellation scenario must contain exactly one transition".into());
    };
    let owner = engine.claim_schema_transition(transition_id).await?;
    let mut processed = 0;
    let mut last_state = None;
    for _ in 0..8 {
        let step = engine
            .step_schema_transition(transition_id, owner, 1)
            .await?;
        processed += step.items;
        last_state = Some(step.transition.state);
        if step.transition.state.is_terminal() {
            return Err(format!(
                "cancellation scenario became terminal before cancellation: items={processed} state={:?}",
                step.transition.state
            )
            .into());
        }
        if processed > 0 {
            break;
        }
    }
    if processed == 0 {
        return Err(format!(
            "cancellation scenario failed to create partial work: state={last_state:?}"
        )
        .into());
    }
    engine.cancel_schema_transition(transition_id).await?;
    Ok(())
}

async fn write_foreground(
    engine: Arc<Engine>,
    scenario: Scenario,
    generation: usize,
) -> Result<(), String> {
    let tables: &[&str] = match scenario {
        Scenario::DependencyGraph => &["items", "related_items"],
        _ => &["items"],
    };
    for table in tables {
        if scenario.is_replacement() {
            write_replacement_foreground_table(&engine, table, generation).await?;
        } else {
            write_foreground_table(&engine, table, generation).await?;
        }
    }
    Ok(())
}

async fn write_replacement_foreground_table(
    engine: &Engine,
    table: &str,
    generation: usize,
) -> Result<(), String> {
    for ordinal in 0..8 {
        let id = format!("{table}-g{generation}-{ordinal}");
        let value = i64::try_from(generation * 1_000 + ordinal)
            .map_err(|error| format!("foreground replacement value overflowed: {error}"))?;
        retry_replacement_create(engine, table, &id, value).await?;
        if ordinal % 2 == 0 {
            retry_replacement_update(engine, table, &id, value + 100).await?;
        }
        if ordinal % 3 == 0 {
            retry_delete(engine, table, &id).await?;
        }
        tokio::task::yield_now().await;
    }
    Ok(())
}

async fn retry_replacement_create(
    engine: &Engine,
    table: &str,
    id: &str,
    value: i64,
) -> Result<(), String> {
    for _ in 0..10_000 {
        match engine
            .create(
                table,
                Row::from([
                    ("id".into(), Value::Text(id.into())),
                    ("status".into(), Value::Text(value.to_string())),
                ]),
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(error) if error.is_conflict() => tokio::time::sleep(Duration::from_millis(1)).await,
            Err(error) if error.reason() == ErrorReason::TypeMismatch => {
                match engine
                    .create(
                        table,
                        Row::from([
                            ("id".into(), Value::Text(id.into())),
                            ("status".into(), Value::Int64(value)),
                        ]),
                    )
                    .await
                {
                    Ok(_) => return Ok(()),
                    Err(error) if error.is_conflict() => {
                        tokio::time::sleep(Duration::from_millis(1)).await
                    }
                    Err(error) => return Err(error.to_string()),
                }
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Err(format!(
        "foreground replacement create for {id:?} exhausted its retry budget"
    ))
}

async fn retry_replacement_update(
    engine: &Engine,
    table: &str,
    id: &str,
    value: i64,
) -> Result<(), String> {
    for _ in 0..10_000 {
        match engine
            .update_many(
                table,
                text_input_type(&["id", "status"]),
                vec![Row::from([
                    ("id".into(), Value::Text(id.into())),
                    ("status".into(), Value::Text(value.to_string())),
                ])],
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(error) if error.is_conflict() => tokio::time::sleep(Duration::from_millis(1)).await,
            Err(error) if error.reason() == ErrorReason::TypeMismatch => {
                match engine
                    .update_many(
                        table,
                        replacement_input_type(),
                        vec![Row::from([
                            ("id".into(), Value::Text(id.into())),
                            ("status".into(), Value::Int64(value)),
                        ])],
                    )
                    .await
                {
                    Ok(_) => return Ok(()),
                    Err(error) if error.is_conflict() => {
                        tokio::time::sleep(Duration::from_millis(1)).await
                    }
                    Err(error) => return Err(error.to_string()),
                }
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Err(format!(
        "foreground replacement update for {id:?} exhausted its retry budget"
    ))
}

async fn write_foreground_table(
    engine: &Engine,
    table: &str,
    generation: usize,
) -> Result<(), String> {
    for ordinal in 0..8 {
        let id = format!("{table}-g{generation}-{ordinal}");
        let status = format!("status-{generation}-{ordinal}");
        retry_create(engine, table, &id, &status).await?;
        if ordinal % 2 == 0 {
            retry_update(
                engine,
                table,
                &id,
                &format!("updated-status-{generation}-{ordinal}"),
            )
            .await?;
        }
        if ordinal % 3 == 0 {
            retry_delete(engine, table, &id).await?;
        }
        tokio::task::yield_now().await;
    }
    Ok(())
}

async fn retry_create(engine: &Engine, table: &str, id: &str, status: &str) -> Result<(), String> {
    for _ in 0..10_000 {
        match engine
            .create(
                table,
                Row::from([
                    ("id".into(), Value::Text(id.into())),
                    ("status".into(), Value::Text(status.into())),
                ]),
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(error) if error.is_conflict() => tokio::time::sleep(Duration::from_millis(1)).await,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err(format!(
        "foreground create for {id:?} exhausted its retry budget"
    ))
}

async fn retry_update(engine: &Engine, table: &str, id: &str, status: &str) -> Result<(), String> {
    for _ in 0..10_000 {
        match engine
            .update_many(
                table,
                text_input_type(&["id", "status"]),
                vec![Row::from([
                    ("id".into(), Value::Text(id.into())),
                    ("status".into(), Value::Text(status.into())),
                ])],
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(error) if error.is_conflict() => tokio::time::sleep(Duration::from_millis(1)).await,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err(format!(
        "foreground update for {id:?} exhausted its retry budget"
    ))
}

async fn retry_delete(engine: &Engine, table: &str, id: &str) -> Result<(), String> {
    for _ in 0..10_000 {
        match engine
            .delete_many(
                table,
                text_input_type(&["id"]),
                vec![Row::from([("id".into(), Value::Text(id.into()))])],
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(error) if error.is_conflict() => tokio::time::sleep(Duration::from_millis(1)).await,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err(format!(
        "foreground delete for {id:?} exhausted its retry budget"
    ))
}

fn text_input_type(names: &[&str]) -> RowType {
    RowType {
        fields: names
            .iter()
            .enumerate()
            .map(|(index, name)| Field {
                name: (*name).into(),
                slot: SlotId(index),
                value_type: Type::scalar(Kind::Text, false),
            })
            .collect(),
    }
}

fn replacement_input_type() -> RowType {
    RowType {
        fields: vec![
            Field {
                name: "id".into(),
                slot: SlotId(0),
                value_type: Type::scalar(Kind::Text, false),
            },
            Field {
                name: "status".into(),
                slot: SlotId(1),
                value_type: Type::scalar(Kind::Int64, false),
            },
        ],
    }
}

async fn audit_scenario(
    scenario: Scenario,
    boundary: CrashBoundary,
    engine: &Engine,
    store: Arc<Store>,
    state: &ScenarioState,
) -> turmoil::Result<()> {
    let mut transition_states = Vec::with_capacity(state.transition_ids.len());
    for id in &state.transition_ids {
        let transition = engine.inspect_schema_transition(id).await?;
        let expected =
            if scenario.is_cancelled() && boundary != CrashBoundary::CancellationCommitStarted {
                TransitionState::Cancelled
            } else {
                TransitionState::Ready
            };
        if transition.state != expected {
            return Err(format!(
                "scenario {:?} at {:?} completed transition {:?} in {:?}, expected {:?}: {}",
                scenario,
                boundary,
                transition.id,
                transition.state,
                expected,
                transition.last_error
            )
            .into());
        }
        transition_states.push(transition.state);
    }
    match scenario {
        Scenario::IndexBuild => audit_ready_index(store, "items", "status_idx").await,
        Scenario::ColumnReplacement => audit_replaced_column(store).await,
        Scenario::ConstraintValidation => audit_not_null_constraint(store).await,
        Scenario::DependencyGraph => {
            audit_ready_index(store.clone(), "items", "first_status_idx").await?;
            audit_ready_index(store.clone(), "related_items", "related_status_idx").await?;
            audit_ready_index(store, "items", "final_status_idx").await
        }
        Scenario::CancelledIndex => match transition_states.as_slice() {
            [TransitionState::Ready] => audit_ready_index(store, "items", "status_idx").await,
            [TransitionState::Cancelled] => audit_cancelled_index(store, engine, state).await,
            states => Err(format!("unexpected cancelled-index states: {states:?}").into()),
        },
        Scenario::CancelledReplacement => match transition_states.as_slice() {
            [TransitionState::Ready] => audit_replaced_column(store).await,
            [TransitionState::Cancelled] => audit_cancelled_replacement(store, engine, state).await,
            states => Err(format!("unexpected cancelled-replacement states: {states:?}").into()),
        },
        Scenario::CancelledConstraint => match transition_states.as_slice() {
            [TransitionState::Ready] => audit_not_null_constraint(store).await,
            [TransitionState::Cancelled] => audit_cancelled_constraint(store, engine).await,
            states => Err(format!("unexpected cancelled-constraint states: {states:?}").into()),
        },
    }
}

async fn audit_cancelled_index(
    store: Arc<Store>,
    engine: &Engine,
    state: &ScenarioState,
) -> turmoil::Result<()> {
    let catalog = catalog::Catalog::new(store.clone());
    let table = catalog
        .get_table("items")
        .await?
        .ok_or("items table disappeared")?;
    if table.indexes.iter().any(|index| index.name == "status_idx") {
        return Err("cancelled index remained published in the catalog".into());
    }
    let transition = engine
        .inspect_schema_transition(&state.transition_ids[0])
        .await?;
    let remaining = scan_prefix(
        store.as_ref(),
        rad::engine::exec::codec::index_prefix(&table, &transition.index.id),
    )
    .await?;
    if !remaining.is_empty() {
        return Err(format!(
            "cancelled index retained {} physical entries after reclamation",
            remaining.len()
        )
        .into());
    }
    Ok(())
}

async fn audit_cancelled_replacement(
    store: Arc<Store>,
    engine: &Engine,
    state: &ScenarioState,
) -> turmoil::Result<()> {
    let catalog = catalog::Catalog::new(store.clone());
    let table = catalog
        .get_table("items")
        .await?
        .ok_or("items table disappeared")?;
    let status = table.column("status").ok_or("status column disappeared")?;
    if status.scalar_type != ScalarType::Text {
        return Err(format!(
            "cancelled replacement published {:?}, expected text",
            status.scalar_type
        )
        .into());
    }
    let target = state
        .replacement_target
        .as_ref()
        .ok_or("cancelled replacement scenario lost its physical target")?;
    for (_, raw) in scan_prefix(
        store.as_ref(),
        rad::engine::exec::codec::data_prefix(&table),
    )
    .await?
    {
        let row = rad::engine::exec::codec::unmarshal_row(&table, &raw)?;
        if !matches!(row["status"], Value::Text(_)) {
            return Err(format!("cancelled replacement retained non-text status: {row:?}").into());
        }
        let retired = rad::engine::exec::codec::read_column_value(&raw, target)?;
        if !retired.is_null() {
            return Err(format!(
                "cancelled replacement retained physical target value {retired:?}"
            )
            .into());
        }
    }
    engine
        .create(
            "items",
            Row::from([
                (
                    "id".into(),
                    Value::Text("audit-cancelled-replacement".into()),
                ),
                ("status".into(), Value::Text("not-an-int".into())),
            ]),
        )
        .await?;
    Ok(())
}

async fn audit_cancelled_constraint(store: Arc<Store>, engine: &Engine) -> turmoil::Result<()> {
    let catalog = catalog::Catalog::new(store);
    let table = catalog
        .get_table("items")
        .await?
        .ok_or("items table disappeared")?;
    let status = table.column("status").ok_or("status column disappeared")?;
    if !status.nullable {
        return Err("cancelled not-null constraint made status non-nullable".into());
    }
    engine
        .create(
            "items",
            Row::from([
                (
                    "id".into(),
                    Value::Text("audit-cancelled-constraint".into()),
                ),
                ("status".into(), Value::Null(ScalarType::Text)),
            ]),
        )
        .await?;
    Ok(())
}

async fn audit_ready_index(
    store: Arc<Store>,
    table_name: &str,
    index_name: &str,
) -> turmoil::Result<()> {
    let catalog = catalog::Catalog::new(store.clone());
    let table = catalog
        .get_table(table_name)
        .await?
        .ok_or_else(|| format!("{table_name} table disappeared"))?;
    let index = table
        .indexes
        .iter()
        .find(|index| index.name == index_name)
        .ok_or_else(|| format!("{index_name} was not published"))?;
    let data_prefix = rad::engine::exec::codec::data_prefix(&table);
    let data = scan_prefix(store.as_ref(), data_prefix.clone()).await?;
    let mut expected = BTreeMap::new();
    for (key, raw) in data {
        let primary_key = key
            .strip_prefix(data_prefix.as_slice())
            .ok_or("table scan escaped its prefix")?
            .to_vec();
        let row = rad::engine::exec::codec::unmarshal_row(&table, &raw)?;
        let encoded_primary_key =
            rad::engine::exec::codec::encode_row_tuple(&row, &table.primary_key)?;
        if primary_key != encoded_primary_key {
            return Err("row body's primary key disagrees with its storage key".into());
        }
        let tuple = rad::engine::exec::codec::encode_row_tuple(&row, &index.columns)?;
        expected.insert(
            rad::engine::exec::codec::index_key(&table, &index.id, &tuple, &primary_key),
            primary_key,
        );
    }
    let actual = scan_prefix(
        store.as_ref(),
        rad::engine::exec::codec::index_prefix(&table, &index.id),
    )
    .await?
    .into_iter()
    .collect::<BTreeMap<_, _>>();
    if actual != expected {
        return Err(format!(
            "ready index contents diverged: actual={} expected={}",
            actual.len(),
            expected.len()
        )
        .into());
    }
    Ok(())
}

async fn audit_replaced_column(store: Arc<Store>) -> turmoil::Result<()> {
    let catalog = catalog::Catalog::new(store.clone());
    let table = catalog
        .get_table("items")
        .await?
        .ok_or("items table disappeared")?;
    let status = table.column("status").ok_or("status column disappeared")?;
    if status.scalar_type != ScalarType::Int64 {
        return Err(format!(
            "replacement published {:?}, expected int64",
            status.scalar_type
        )
        .into());
    }
    for (_, raw) in scan_prefix(
        store.as_ref(),
        rad::engine::exec::codec::data_prefix(&table),
    )
    .await?
    {
        let row = rad::engine::exec::codec::unmarshal_row(&table, &raw)?;
        if !matches!(row["status"], Value::Int64(_)) {
            return Err(format!("replacement row retained non-int status: {row:?}").into());
        }
    }
    Ok(())
}

async fn audit_not_null_constraint(store: Arc<Store>) -> turmoil::Result<()> {
    let catalog = catalog::Catalog::new(store.clone());
    let table = catalog
        .get_table("items")
        .await?
        .ok_or("items table disappeared")?;
    let status = table.column("status").ok_or("status column disappeared")?;
    if status.nullable {
        return Err("not-null validation left status nullable".into());
    }
    for (_, raw) in scan_prefix(
        store.as_ref(),
        rad::engine::exec::codec::data_prefix(&table),
    )
    .await?
    {
        let row = rad::engine::exec::codec::unmarshal_row(&table, &raw)?;
        if row["status"].is_null() {
            return Err(format!("validated row retained NULL status: {row:?}").into());
        }
    }
    Ok(())
}

async fn scan_prefix(store: &Store, prefix: Vec<u8>) -> turmoil::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut iterator = Kv::scan(
        store,
        KeyRange {
            start: Some(Bytes::copy_from_slice(&prefix)),
            end: prefix_end(&prefix).map(Bytes::from),
        },
    )
    .await?;
    let mut entries = Vec::new();
    while let Some(entry) = iterator.next().await? {
        entries.push((entry.key.to_vec(), entry.value.to_vec()));
    }
    Ok(entries)
}

async fn seed_scenario(
    scenario: Scenario,
    engine: &Engine,
    store: Arc<dyn TransactionalKv>,
    runtime: Arc<dyn RuntimeEffects>,
) -> turmoil::Result<ScenarioState> {
    let catalog = catalog::Catalog::with_runtime(store, runtime);
    let table = catalog
        .create_table(TableDef {
            id: SchemaId::new(1)?,
            name: "items".into(),
            columns: vec![
                column(1, "id", ScalarType::Text, false)?,
                column(
                    2,
                    "status",
                    ScalarType::Text,
                    matches!(
                        scenario,
                        Scenario::ConstraintValidation | Scenario::CancelledConstraint
                    ),
                )?,
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await?;
    let seed_rows = match scenario {
        Scenario::ColumnReplacement | Scenario::CancelledReplacement => {
            [("a", "11"), ("b", "22"), ("c", "33")]
        }
        _ => [("a", "open"), ("b", "closed"), ("c", "pending")],
    };
    for (id, status) in seed_rows {
        engine
            .create(
                "items",
                Row::from([
                    ("id".into(), Value::Text(id.into())),
                    ("status".into(), Value::Text(status.into())),
                ]),
            )
            .await?;
    }
    let statements = match scenario {
        Scenario::IndexBuild | Scenario::CancelledIndex => vec![Statement::StartIndexBuild {
            name: "start_status_idx".into(),
            table_id: table.schema_id,
            index: IndexDef {
                name: "status_idx".into(),
                columns: vec!["status".into()],
                unique: true,
            },
            prerequisites: Vec::new(),
            after: Vec::new(),
        }],
        Scenario::ColumnReplacement | Scenario::CancelledReplacement => {
            vec![Statement::StartColumnReplacement {
                name: "replace_status".into(),
                table_id: table.schema_id,
                column_id: SchemaId::new(2)?,
                replacement: ColumnReplacementDef {
                    scalar_type: ScalarType::Int64,
                    nullable: false,
                    format: String::new(),
                    default: None,
                    conversion: ColumnConversion::StrictBuiltin,
                    prerequisites: Vec::new(),
                },
                after: Vec::new(),
            }]
        }
        Scenario::ConstraintValidation | Scenario::CancelledConstraint => {
            vec![Statement::StartConstraintValidation {
                name: "validate_status".into(),
                table_id: table.schema_id,
                constraint: ConstraintDef {
                    name: "status_not_null".into(),
                    kind: ConstraintKind::NotNull,
                    column_id: SchemaId::new(2)?,
                    prerequisites: Vec::new(),
                },
                after: Vec::new(),
            }]
        }
        Scenario::DependencyGraph => {
            let related = catalog
                .create_table(TableDef {
                    id: SchemaId::new(10)?,
                    name: "related_items".into(),
                    columns: vec![
                        column(10, "id", ScalarType::Text, false)?,
                        column(11, "status", ScalarType::Text, false)?,
                    ],
                    primary_key: vec!["id".into()],
                    indexes: Vec::new(),
                    foreign_keys: Vec::new(),
                })
                .await?;
            for (id, status) in [("r-a", "open"), ("r-b", "closed"), ("r-c", "pending")] {
                engine
                    .create(
                        "related_items",
                        Row::from([
                            ("id".into(), Value::Text(id.into())),
                            ("status".into(), Value::Text(status.into())),
                        ]),
                    )
                    .await?;
            }
            vec![
                Statement::StartIndexBuild {
                    name: "first".into(),
                    table_id: table.schema_id,
                    index: IndexDef {
                        name: "first_status_idx".into(),
                        columns: vec!["status".into()],
                        unique: true,
                    },
                    prerequisites: Vec::new(),
                    after: Vec::new(),
                },
                Statement::StartIndexBuild {
                    name: "related".into(),
                    table_id: related.schema_id,
                    index: IndexDef {
                        name: "related_status_idx".into(),
                        columns: vec!["status".into()],
                        unique: true,
                    },
                    prerequisites: Vec::new(),
                    after: vec!["first".into()],
                },
                Statement::StartIndexBuild {
                    name: "final".into(),
                    table_id: table.schema_id,
                    index: IndexDef {
                        name: "final_status_idx".into(),
                        columns: vec!["status".into()],
                        unique: true,
                    },
                    prerequisites: Vec::new(),
                    after: vec!["related".into()],
                },
            ]
        }
    };
    let result = engine
        .execute_program(
            Program {
                statements,
                result: None,
            },
            CatalogPolicy::RevisionPerProgram,
        )
        .await?;
    let transition_ids = result
        .statements
        .into_iter()
        .map(|statement| {
            statement
                .control
                .expect("schema start statement control")
                .transition_id
        })
        .collect::<Vec<_>>();
    let replacement_target = if scenario.is_replacement() {
        Some(
            engine
                .inspect_schema_transition(&transition_ids[0])
                .await?
                .column_replacement
                .ok_or("replacement transition lacks physical target")?
                .target,
        )
    } else {
        None
    };
    Ok(ScenarioState {
        transition_ids,
        replacement_target,
    })
}

fn column(
    id: u32,
    name: &str,
    scalar_type: ScalarType,
    nullable: bool,
) -> turmoil::Result<ColumnDef> {
    Ok(ColumnDef {
        id: SchemaId::new(id)?,
        name: name.into(),
        scalar_type,
        nullable,
        format: String::new(),
        default: None,
    })
}

fn scheduler_config() -> SchemaJobConfig {
    SchemaJobConfig {
        transition_batch_size: 1,
        reclamation_batch_size: 1,
        batches_per_round: 1,
        items_per_round: 1,
        yield_interval: Duration::from_millis(1),
        idle_poll_interval: Duration::from_secs(1),
        retry_backoff_min: Duration::from_millis(1),
        retry_backoff_max: Duration::from_millis(10),
        catalog_history_retain: 8,
        catalog_history_batch_size: 2,
        ..SchemaJobConfig::default()
    }
}

struct CrashAtEngineBoundary {
    boundary: CrashBoundary,
    checkpoint: Arc<AtomicUsize>,
    reached: AtomicBool,
    trace: Arc<Mutex<Vec<EngineEvent>>>,
}

#[async_trait]
impl EngineEventHook for CrashAtEngineBoundary {
    async fn reach(&self, event: EngineEvent) {
        self.trace
            .lock()
            .expect("engine event trace lock poisoned")
            .push(event.clone());
        if self.boundary.matches(&event) && !self.reached.swap(true, Ordering::AcqRel) {
            self.checkpoint.store(1, Ordering::SeqCst);
            future::pending::<()>().await;
        }
    }
}

struct RecordEngineEvents {
    trace: Arc<Mutex<Vec<EngineEvent>>>,
}

#[async_trait]
impl EngineEventHook for RecordEngineEvents {
    async fn reach(&self, event: EngineEvent) {
        self.trace
            .lock()
            .expect("engine event trace lock poisoned")
            .push(event);
    }
}

fn artifact_directory() -> PathBuf {
    std::env::var_os("RAD_TEST_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/rad-test-artifacts/dst"))
}

fn write_failure_artifact(
    seed: u64,
    scenario: Scenario,
    boundary: CrashBoundary,
    trace: &Mutex<Vec<EngineEvent>>,
    kv_trace: &[RedactedTraceEvent],
    error: &dyn std::error::Error,
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = artifact_directory();
    std::fs::create_dir_all(&directory)?;
    let replay = format!(
        "RAD_DST_SEED={seed} RAD_DST_SCENARIO={} RAD_DST_BOUNDARY={} cargo test --locked --test schema_scheduler_simulation turmoil_schema_work_replay -- --exact --ignored --nocapture",
        scenario.name(),
        boundary.name()
    );
    let body = serde_json::to_vec_pretty(&serde_json::json!({
        "format": "rad-dst-failure-v2",
        "scenario": scenario.name(),
        "backend": "slatedb-memory-object-store",
        "revision": std::env::var("GITHUB_SHA").ok(),
        "source_revision": std::env::var("RAD_SOURCE_REVISION").ok(),
        "base_revision": std::env::var("RAD_BASE_REVISION").ok().filter(|value| !value.is_empty()),
        "package_version": env!("CARGO_PKG_VERSION"),
        "master_seed": seed,
        "derived_seeds": {
            "turmoil": derive_seed(seed, TURMOIL_DOMAIN),
            "runtime": derive_seed(seed, RUNTIME_DOMAIN),
        },
        "algorithms": {
            "seed_derivation": SEED_DERIVATION,
            "runtime": SeededRuntime::ALGORITHM,
        },
        "crash_boundary": boundary.name(),
        "error": error.to_string(),
        "replay": replay,
        "events": &*trace.lock().expect("engine event trace lock poisoned"),
        "kv_events": kv_trace,
    }))?;
    std::fs::write(
        directory.join(format!(
            "failure-{}-{}-{seed}.json",
            scenario.name(),
            boundary.name()
        )),
        body,
    )?;
    Ok(())
}

pub fn write_campaign_summary(
    elapsed: Duration,
    completed: u64,
    seconds: Option<u64>,
    seeds: u64,
    seed_start: u64,
    scenario_counts: &BTreeMap<String, u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = artifact_directory();
    std::fs::create_dir_all(&directory)?;
    let body = serde_json::to_vec_pretty(&serde_json::json!({
        "format": "rad-dst-campaign-v2",
        "scenarios": scenario_counts,
        "backend": "slatedb-memory-object-store",
        "revision": std::env::var("GITHUB_SHA").ok(),
        "source_revision": std::env::var("RAD_SOURCE_REVISION").ok(),
        "base_revision": std::env::var("RAD_BASE_REVISION").ok().filter(|value| !value.is_empty()),
        "package_version": env!("CARGO_PKG_VERSION"),
        "seed_derivation": SEED_DERIVATION,
        "completed_cases": completed,
        "elapsed_milliseconds": elapsed.as_millis(),
        "requested_seconds": seconds,
        "requested_seeds": if seconds.is_none() { Some(seeds) } else { None },
        "seed_start": seed_start,
        "crash_boundaries": CrashBoundary::ALL.map(CrashBoundary::name),
    }))?;
    std::fs::write(Path::new(&directory).join("campaign.json"), body)?;
    Ok(())
}

fn derive_seed(master: u64, domain: u64) -> u64 {
    let mut value = master ^ domain;
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

struct SeededRuntime {
    seed: u64,
    clock: AtomicU64,
    identifiers: AtomicU64,
}

impl SeededRuntime {
    const ALGORITHM: &str = "logical-milliseconds-and-uuid-sequence-v1";

    fn new(seed: u64) -> Self {
        Self {
            seed,
            clock: AtomicU64::new(0),
            identifiers: AtomicU64::new(0),
        }
    }
}

impl RuntimeEffects for SeededRuntime {
    fn now(&self) -> DateTime<Utc> {
        let logical = self.clock.fetch_add(1, Ordering::SeqCst);
        let epoch_millis = 1_700_000_000_000_i64
            + i64::try_from(self.seed % 1_000_000).expect("bounded seed offset")
            + i64::try_from(logical).expect("logical DST clock exhausted i64");
        DateTime::from_timestamp_millis(epoch_millis).expect("DST timestamp is representable")
    }

    fn new_uuid(&self) -> Uuid {
        let sequence = self.identifiers.fetch_add(1, Ordering::SeqCst) + 1;
        Uuid::from_u128((u128::from(self.seed) << 64) | u128::from(sequence))
    }
}

#[test]
fn master_seed_derives_stable_independent_streams() {
    assert_eq!(derive_seed(42, TURMOIL_DOMAIN), 16_197_249_927_080_582_130);
    assert_eq!(derive_seed(42, RUNTIME_DOMAIN), 13_454_358_635_263_715_376);
    assert_ne!(
        derive_seed(42, TURMOIL_DOMAIN),
        derive_seed(42, RUNTIME_DOMAIN)
    );
}

#[test]
fn campaign_matrix_is_unique_and_front_loads_every_scenario() {
    let cases = campaign_cases();
    assert_eq!(cases.len(), 71);
    assert_eq!(
        cases
            .iter()
            .take(Scenario::ALL.len())
            .map(|(scenario, _)| *scenario)
            .collect::<Vec<_>>(),
        Scenario::ALL
    );
    for (index, case) in cases.iter().enumerate() {
        assert!(case.0.boundaries().contains(&case.1));
        assert_eq!(
            cases.iter().filter(|candidate| *candidate == case).count(),
            1,
            "duplicate campaign case at index {index}: scenario={} boundary={}",
            case.0.name(),
            case.1.name()
        );
    }
}

fn step_until(
    simulation: &mut turmoil::Sim<'_>,
    checkpoint: &AtomicUsize,
    expected: usize,
) -> turmoil::Result {
    for _ in 0..30_000 {
        simulation.step()?;
        if checkpoint.load(Ordering::SeqCst) == expected {
            return Ok(());
        }
    }
    Err(format!("simulation did not reach checkpoint {expected}").into())
}
