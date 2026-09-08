use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rad::engine::exec::observe::{ExecutionObserver, KvWork, PhaseTimings, StatementObservation};
use rad::engine::lir::fingerprint::{
    CANONICALIZATION_VERSION, Fingerprint, HASH_SHA256_128, QueryFingerprints, RelationFingerprints,
};
use rad::engine::planner::models::{
    DependencyStamp, FingerprintCardinalitySketch, Log2Histogram, ObservationModelKind,
    PlannerStats, SynopsisModel,
};
use rad::runtime::RuntimeEffects;
use rad::scheduler::relay::{
    IngestOutcome, ObservationBatch, ObservationTransport, RELAY_FORMAT, RelaySink, TransportError,
};
use rad::scheduler::statistics::{
    QueryModel, StatisticsConfig, StatisticsRunner, StatisticsSink, StatisticsSource,
};

const WALL_TIME_MICROS: i64 = 1_800_000_000_000_000;
const TURMOIL_DOMAIN: u64 = 0x7374_6174_732d_6473;

#[derive(Clone, Debug, Eq, PartialEq)]
struct ModelTrace {
    family: String,
    executions: u64,
    rows_max: u64,
    semantic_stamp: u64,
}

#[derive(Debug, Eq, PartialEq)]
struct SnapshotTrace {
    absorbed: u64,
    dropped: u64,
    evicted: u64,
    shed: u64,
    published_at_micros: u64,
    hot_frequency: u32,
    models: Vec<ModelTrace>,
}

#[derive(Debug, Eq, PartialEq)]
struct TransportAttempt {
    sequence: u64,
    families: usize,
    outcome: &'static str,
}

#[derive(Debug, Eq, PartialEq)]
struct SimulationTrace {
    snapshots: Vec<SnapshotTrace>,
    ingest_outcomes: Vec<IngestOutcome>,
    transport_attempts: Vec<TransportAttempt>,
    collector_dropped: u64,
    relay_sent: u64,
    relay_abandoned: u64,
    relay_rejected: u64,
    ingest_accepted: u64,
    ingest_already_admitted: u64,
    ingest_format_mismatch: u64,
    ingest_saturated: u64,
    monotonic_micros: u64,
}

struct ReplayRuntime {
    seed: u64,
}

impl RuntimeEffects for ReplayRuntime {
    fn now(&self) -> DateTime<Utc> {
        DateTime::from_timestamp_micros(WALL_TIME_MICROS + self.seed as i64)
            .expect("simulation wall time")
    }

    fn new_uuid(&self) -> uuid::Uuid {
        uuid::Uuid::from_u128(u128::from(self.seed))
    }
}

struct SlowSource {
    model: QueryModel,
}

#[async_trait::async_trait]
impl StatisticsSource for SlowSource {
    async fn load_models(&self, _limit: usize) -> Vec<QueryModel> {
        tokio::time::sleep(Duration::from_millis(17)).await;
        vec![self.model.clone()]
    }

    async fn load_synopses(&self) -> Vec<SynopsisModel> {
        tokio::time::sleep(Duration::from_millis(11)).await;
        Vec::new()
    }
}

struct ScriptedTransport {
    outcomes: Mutex<VecDeque<Result<(), TransportError>>>,
    attempts: Arc<Mutex<Vec<TransportAttempt>>>,
}

#[async_trait::async_trait]
impl ObservationTransport for ScriptedTransport {
    async fn submit(&self, batch: &ObservationBatch) -> Result<(), TransportError> {
        tokio::time::sleep(Duration::from_millis(15)).await;
        let outcome = self
            .outcomes
            .lock()
            .expect("transport outcomes")
            .pop_front()
            .unwrap_or(Ok(()));
        self.attempts
            .lock()
            .expect("transport attempts")
            .push(TransportAttempt {
                sequence: batch.sequence,
                families: batch.families.len(),
                outcome: match outcome {
                    Ok(()) => "sent",
                    Err(TransportError::Unavailable) => "unavailable",
                    Err(TransportError::Backpressured) => "backpressured",
                    Err(TransportError::Rejected) => "rejected",
                },
            });
        outcome
    }
}

#[test]
fn statistics_collection_exercises_deterministic_simulation() {
    let mut expected_semantics = None;
    for seed in 0..4 {
        let trace = simulate(seed).expect("statistics simulation");
        assert_trace_exercises_the_failure_paths(&trace);
        let final_snapshot = trace.snapshots.last().expect("final statistics snapshot");
        let semantics = (
            final_snapshot.absorbed,
            final_snapshot.dropped,
            final_snapshot.evicted,
            final_snapshot.hot_frequency,
            final_snapshot.models.clone(),
        );
        if let Some(expected) = &expected_semantics {
            assert_eq!(
                &semantics, expected,
                "semantic result changed with the task schedule for seed {seed}"
            );
        } else {
            expected_semantics = Some(semantics);
        }
    }
}

#[test]
#[ignore = "requires RUSTFLAGS=--cfg tokio_unstable to seed Tokio's task scheduler"]
fn statistics_collection_replays_identically_for_the_same_seed() {
    for seed in 0..4 {
        let first = simulate(seed).expect("first statistics simulation");
        let replay = simulate(seed).expect("replayed statistics simulation");
        assert_replay_eq(seed, &first, &replay);
        assert_trace_exercises_the_failure_paths(&first);
    }
}

fn assert_replay_eq(seed: u64, first: &SimulationTrace, replay: &SimulationTrace) {
    if first == replay {
        return;
    }
    let snapshot = first
        .snapshots
        .iter()
        .zip(&replay.snapshots)
        .position(|(first, replay)| first != replay);
    let attempt = first
        .transport_attempts
        .iter()
        .zip(&replay.transport_attempts)
        .position(|(first, replay)| first != replay);
    panic!(
        "statistics trace diverged for seed {seed}: first snapshots={}, replay snapshots={}, first differing snapshot={snapshot:?}, first attempts={}, replay attempts={}, first differing attempt={attempt:?}, first relay=({},{},{}), replay relay=({},{},{})",
        first.snapshots.len(),
        replay.snapshots.len(),
        first.transport_attempts.len(),
        replay.transport_attempts.len(),
        first.relay_sent,
        first.relay_abandoned,
        first.relay_rejected,
        replay.relay_sent,
        replay.relay_abandoned,
        replay.relay_rejected,
    );
}

fn simulate(seed: u64) -> Result<SimulationTrace, Box<dyn std::error::Error>> {
    let completed = Arc::new(AtomicUsize::new(0));
    let trace = Arc::new(Mutex::new(None));
    let transport_attempts = Arc::new(Mutex::new(Vec::new()));
    let runtime: Arc<dyn RuntimeEffects> = Arc::new(ReplayRuntime { seed });

    let mut builder = turmoil::Builder::new();
    builder
        .rng_seed(derive_seed(seed, TURMOIL_DOMAIN))
        .enable_random_order()
        .tick_duration(Duration::from_millis(1))
        .simulation_duration(Duration::from_secs(5));
    let mut simulation = builder.build();
    simulation.host("statistics", {
        let completed = completed.clone();
        let trace = trace.clone();
        let transport_attempts = transport_attempts.clone();
        move || {
            let completed = completed.clone();
            let trace = trace.clone();
            let transport_attempts = transport_attempts.clone();
            let runtime = runtime.clone();
            async move {
                let transport = Arc::new(ScriptedTransport {
                    outcomes: Mutex::new(VecDeque::from([
                        Err(TransportError::Unavailable),
                        Err(TransportError::Unavailable),
                        Err(TransportError::Unavailable),
                        Err(TransportError::Unavailable),
                        Err(TransportError::Rejected),
                        Ok(()),
                    ])),
                    attempts: transport_attempts.clone(),
                });
                let relay = Arc::new(RelaySink::new(
                    transport,
                    runtime.clone(),
                    "reader-a".to_owned(),
                ));
                let source = Arc::new(SlowSource {
                    model: model(90, 4, 40, 1),
                });
                let runner = StatisticsRunner::start(
                    runtime.clone(),
                    StatisticsConfig {
                        channel_capacity: 2,
                        registry_capacity: 4,
                        publish_interval: Duration::from_millis(10),
                        decay_every: 2,
                        flush_every: 1,
                        survey_every: u32::MAX,
                        survey_change_threshold: 10_000,
                        survey_max_age: Duration::from_secs(24 * 60 * 60),
                        survey_row_budget: 50_000,
                        survey_byte_budget: 64 * 1024 * 1024,
                        survey_time_budget: Duration::from_secs(30),
                        refresh_every: 3,
                        capture_programs: false,
                    },
                    Some(source),
                    Some(relay.clone() as Arc<dyn StatisticsSink>),
                );
                let collector = runner.collector();
                for index in 0..12 {
                    collector.statement(observation(1, index, 10 + u64::from(index)));
                }

                let ingest = runner.ingest();
                let mut ingest_outcomes = vec![
                    ingest.submit(batch("ordered", 2, 2, model(2, 1, 20, 1))),
                    ingest.submit(batch("ordered", 1, 1, model(2, 1, 20, 1))),
                    ingest.submit(ObservationBatch {
                        format: RELAY_FORMAT + 1,
                        ..batch("wrong-format", 1, 1, model(3, 1, 30, 1))
                    }),
                    ingest.submit(batch("stale", 1, 1, model(4, 2, 40, 9))),
                    ingest.submit(batch("stale", 2, 2, model(4, 3, 400, 7))),
                ];
                ingest_outcomes.push(ingest.submit(ObservationBatch {
                    format: RELAY_FORMAT,
                    instance: "frequency".to_owned(),
                    boot: "boot".to_owned(),
                    sequence: 1,
                    sent_at_micros: WALL_TIME_MICROS as u64,
                    families: Vec::new(),
                    frequency: vec![(fingerprint(80), 100)],
                    corpus: Vec::new(),
                }));
                ingest_outcomes.push(ingest.submit(batch("frequency", 2, 2, model(80, 1, 800, 1))));
                for source in 0..59 {
                    ingest_outcomes.push(ingest.submit(batch(
                        &format!("source-{source}"),
                        1,
                        source as u64,
                        model(10 + source as u8, 1, source as u64 + 1, 1),
                    )));
                }
                ingest_outcomes.push(ingest.submit(batch("saturated", 1, 1, model(81, 1, 810, 1))));

                let producer = {
                    let collector = collector.clone();
                    tokio::spawn(async move {
                        for index in 0..12 {
                            tokio::time::sleep(Duration::from_millis(13)).await;
                            collector.statement(observation(1, 20 + index, 100 + u64::from(index)));
                        }
                    })
                };
                let mut snapshots = Vec::new();
                for _ in 0..44 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    snapshots.push(snapshot(&runner.stats()));
                }
                producer.await.expect("statistics producer");
                runner.shutdown().await;
                snapshots.push(snapshot(&runner.stats()));

                let relay_counters = relay.counters();
                let ingest_counters = ingest.counters();
                *trace.lock().expect("simulation trace") = Some(SimulationTrace {
                    snapshots,
                    ingest_outcomes,
                    transport_attempts: std::mem::take(
                        &mut *transport_attempts.lock().expect("transport attempts"),
                    ),
                    collector_dropped: collector.dropped(),
                    relay_sent: relay_counters.sent,
                    relay_abandoned: relay_counters.abandoned,
                    relay_rejected: relay_counters.rejected,
                    ingest_accepted: ingest_counters.accepted,
                    ingest_already_admitted: ingest_counters.already_admitted,
                    ingest_format_mismatch: ingest_counters.format_mismatch,
                    ingest_saturated: ingest_counters.saturated,
                    monotonic_micros: runtime.monotonic().as_micros() as u64,
                });
                completed.store(1, Ordering::SeqCst);
                Ok(())
            }
        }
    });

    for _ in 0..5_000 {
        simulation.step()?;
        if completed.load(Ordering::SeqCst) == 1 {
            return trace
                .lock()
                .expect("simulation trace")
                .take()
                .ok_or_else(|| "statistics simulation completed without a trace".into());
        }
    }
    Err("statistics simulation did not complete".into())
}

fn assert_trace_exercises_the_failure_paths(trace: &SimulationTrace) {
    assert!(trace.collector_dropped > 0, "collector pressure was absent");
    assert!(
        trace.snapshots.iter().any(|snapshot| snapshot.evicted > 0),
        "registry eviction was absent"
    );
    assert!(
        trace.snapshots.iter().any(|snapshot| snapshot.shed > 0),
        "registry shedding was absent"
    );
    assert!(trace.relay_abandoned > 0, "retry exhaustion was absent");
    assert!(trace.relay_rejected > 0, "terminal rejection was absent");
    assert!(trace.relay_sent > 0, "successful relay was absent");
    assert!(
        trace
            .ingest_outcomes
            .contains(&IngestOutcome::AlreadyAdmitted),
        "duplicate admission was absent"
    );
    assert!(
        trace
            .ingest_outcomes
            .contains(&IngestOutcome::FormatMismatch),
        "format rejection was absent"
    );
    assert!(
        trace.ingest_outcomes.contains(&IngestOutcome::Saturated),
        "ingest saturation was absent"
    );
    assert_eq!(trace.monotonic_micros, 0);
    assert!(
        trace
            .snapshots
            .iter()
            .all(|snapshot| snapshot.published_at_micros > 0),
        "wall time was not preserved independently of monotonic time"
    );
    assert!(
        trace.snapshots.windows(2).any(|pair| {
            pair[0].hot_frequency > pair[1].hot_frequency && pair[1].hot_frequency > 0
        }),
        "frequency decay was absent"
    );
    let final_snapshot = trace.snapshots.last().expect("final statistics snapshot");
    let current = final_snapshot
        .models
        .iter()
        .find(|model| model.family == fingerprint(4).to_string())
        .expect("current semantic model");
    assert_eq!(current.semantic_stamp, 9);
    assert_eq!(current.rows_max, 40);
    assert!(
        final_snapshot
            .models
            .iter()
            .any(|model| model.family == fingerprint(90).to_string()),
        "slow source refresh was absent"
    );
}

fn snapshot(stats: &PlannerStats) -> SnapshotTrace {
    let mut models = stats
        .statement_models
        .values()
        .map(|model| ModelTrace {
            family: model.family.to_string(),
            executions: model.retained_executions,
            rows_max: model.rows_max,
            semantic_stamp: model.stamp.semantic,
        })
        .collect::<Vec<_>>();
    models.sort_by(|left, right| left.family.cmp(&right.family));
    SnapshotTrace {
        absorbed: stats.absorbed,
        dropped: stats.dropped,
        evicted: stats.evicted,
        shed: stats.shed,
        published_at_micros: stats.published_at.as_micros() as u64,
        hot_frequency: stats.frequency(&fingerprint(80)),
        models,
    }
}

fn batch(instance: &str, sequence: u64, age_micros: u64, model: QueryModel) -> ObservationBatch {
    ObservationBatch {
        format: RELAY_FORMAT,
        instance: instance.to_owned(),
        boot: "boot".to_owned(),
        sequence,
        sent_at_micros: WALL_TIME_MICROS as u64 + age_micros,
        frequency: vec![(model.family, 1)],
        families: vec![model],
        corpus: Vec::new(),
    }
}

fn model(family_seed: u8, executions: u64, rows: u64, semantic_stamp: u64) -> QueryModel {
    let mut row_histogram = Log2Histogram::default();
    row_histogram.record(rows);
    QueryModel {
        family: fingerprint(family_seed),
        kind: ObservationModelKind::Statement,
        executions,
        last_seen: Duration::from_micros(WALL_TIME_MICROS as u64),
        rows: row_histogram,
        execute_micros: Log2Histogram::default(),
        bind_micros: Log2Histogram::default(),
        resources: Default::default(),
        duration_ewma_micros: 0.0,
        plans: Vec::new(),
        plan_profiles: Vec::new(),
        exact_variants: FingerprintCardinalitySketch::default(),
        estimated_executions: 0,
        q_errors: Log2Histogram::default(),
        q_error_max_x100: 0,
        stamp: DependencyStamp {
            semantic: semantic_stamp,
            access: 1,
        },
    }
}

fn observation(family_seed: u8, exact_seed: u8, rows: u64) -> StatementObservation {
    let family = fingerprint(family_seed);
    let exact = fingerprint(exact_seed);
    StatementObservation {
        source: rad::engine::exec::observe::StatementSource::Executed,
        query: QueryFingerprints {
            exact,
            family,
            root: RelationFingerprints { exact, family },
            subtrees: vec![RelationFingerprints { exact, family }],
            tables: Default::default(),
            bindings: Vec::new(),
        },
        plan: Some(fingerprint(family_seed.wrapping_add(100))),
        phase: PhaseTimings {
            bind: Duration::ZERO,
            execute: Duration::ZERO,
        },
        rows,
        estimate: None,
        stamp: DependencyStamp::default(),
        relations: Vec::new(),
        affected: rows,
        mutated: None,
        kv: KvWork::default(),
        operators: Vec::new(),
        physical_storage: None,
        join_operators: Vec::new(),
        failure: None,
    }
}

fn fingerprint(seed: u8) -> Fingerprint {
    let mut digest = [0; 16];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = seed.wrapping_mul(31).wrapping_add(index as u8);
    }
    Fingerprint {
        canonicalization_version: CANONICALIZATION_VERSION,
        hash_algorithm: HASH_SHA256_128,
        digest,
    }
}

fn derive_seed(master: u64, domain: u64) -> u64 {
    let mut value = master ^ domain;
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}
