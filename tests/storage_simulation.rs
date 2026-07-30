use std::future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use rad::engine::kv::fault::{FaultController, FaultingKv, RedactedTraceEvent};
use rad::engine::kv::slatedb::Store;
use rad::engine::kv::{IsolationLevel, TransactionalKv};
use slatedb::object_store::ObjectStore;
use slatedb::object_store::memory::InMemory;

const BASELINE_KEY: &[u8] = b"simulation/baseline";
const FIRST_KEY: &[u8] = b"simulation/first";
const SECOND_KEY: &[u8] = b"simulation/second";
const SCENARIO: &str = "slate-crash-recovery-v1";
const SEED_DERIVATION: &str = "splitmix64-domain-v1";
const TURMOIL_DOMAIN: u64 = 0x7475_726d_6f69_6c02;

#[derive(Clone, Copy, Debug)]
enum CrashPoint {
    ActiveTransaction,
    CommitAcknowledged,
}

impl CrashPoint {
    const ALL: [Self; 2] = [Self::ActiveTransaction, Self::CommitAcknowledged];

    fn name(self) -> &'static str {
        match self {
            Self::ActiveTransaction => "active_transaction",
            Self::CommitAcknowledged => "commit_acknowledged",
        }
    }

    fn parse(value: &str) -> Result<Self, Box<dyn std::error::Error>> {
        Self::ALL
            .into_iter()
            .find(|point| point.name() == value)
            .ok_or_else(|| format!("unknown storage crash point {value:?}").into())
    }
}

#[test]
fn turmoil_replays_slate_host_crash_and_recovery() -> Result<(), Box<dyn std::error::Error>> {
    for seed in 0..8 {
        for point in CrashPoint::ALL {
            run_case(seed, point)?;
        }
    }
    Ok(())
}

#[test]
#[ignore = "single-case replay; configure RAD_STORAGE_DST_SEED and RAD_STORAGE_DST_POINT"]
fn turmoil_replays_one_storage_case() -> Result<(), Box<dyn std::error::Error>> {
    let seed = std::env::var("RAD_STORAGE_DST_SEED")?.parse()?;
    let point = CrashPoint::parse(&std::env::var("RAD_STORAGE_DST_POINT")?)?;
    run_case(seed, point)
}

fn run_case(seed: u64, point: CrashPoint) -> Result<(), Box<dyn std::error::Error>> {
    let controller = FaultController::default();
    match simulate(seed, point, controller.clone()) {
        Ok(()) => Ok(()),
        Err(error) => {
            write_failure(seed, point, &controller.redacted_trace(), error.as_ref())?;
            Err(error)
        }
    }
}

fn simulate(
    seed: u64,
    point: CrashPoint,
    controller: FaultController,
) -> Result<(), Box<dyn std::error::Error>> {
    let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let generation = Arc::new(AtomicUsize::new(0));
    let checkpoint = Arc::new(AtomicUsize::new(0));
    let path = match point {
        CrashPoint::ActiveTransaction => format!("turmoil-active-{seed}"),
        CrashPoint::CommitAcknowledged => format!("turmoil-acknowledged-{seed}"),
    };

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
        move || {
            let objects = objects.clone();
            let generation = generation.clone();
            let checkpoint = checkpoint.clone();
            let controller = controller.clone();
            let path = path.clone();
            async move {
                let current = generation.fetch_add(1, Ordering::SeqCst);
                let store = Arc::new(Store::open(path, objects).await?);
                let traced: Arc<dyn TransactionalKv> = Arc::new(FaultingKv::new(store, controller));
                if current == 0 {
                    traced
                        .put(
                            Bytes::from_static(BASELINE_KEY),
                            Bytes::from_static(b"durable"),
                        )
                        .await?;
                    let transaction = traced.begin(IsolationLevel::SerializableSnapshot).await?;
                    transaction.put(Bytes::from_static(FIRST_KEY), Bytes::from_static(b"first"))?;
                    transaction.put(
                        Bytes::from_static(SECOND_KEY),
                        Bytes::from_static(b"second"),
                    )?;
                    if matches!(point, CrashPoint::CommitAcknowledged) {
                        transaction.commit().await?;
                    }
                    checkpoint.store(1, Ordering::SeqCst);
                    future::pending::<()>().await;
                } else {
                    assert_eq!(
                        traced.get(BASELINE_KEY).await?,
                        Some(Bytes::from_static(b"durable"))
                    );
                    let first = traced.get(FIRST_KEY).await?;
                    let second = traced.get(SECOND_KEY).await?;
                    match point {
                        CrashPoint::ActiveTransaction => {
                            assert_eq!(first, None);
                            assert_eq!(second, None);
                        }
                        CrashPoint::CommitAcknowledged => {
                            assert_eq!(first, Some(Bytes::from_static(b"first")));
                            assert_eq!(second, Some(Bytes::from_static(b"second")));
                        }
                    }
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

fn derive_seed(master: u64, domain: u64) -> u64 {
    let mut value = master ^ domain;
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn write_failure(
    seed: u64,
    point: CrashPoint,
    trace: &[RedactedTraceEvent],
    error: &dyn std::error::Error,
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = std::env::var_os("RAD_TEST_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/rad-test-artifacts/storage-dst"));
    std::fs::create_dir_all(&directory)?;
    let replay = format!(
        "RAD_STORAGE_DST_SEED={seed} RAD_STORAGE_DST_POINT={} cargo test --locked --test storage_simulation turmoil_replays_one_storage_case -- --exact --ignored --nocapture",
        point.name()
    );
    let body = serde_json::to_vec_pretty(&serde_json::json!({
        "format": "rad-storage-dst-failure-v1",
        "scenario": SCENARIO,
        "backend": "slatedb-memory-object-store",
        "revision": std::env::var("GITHUB_SHA").ok(),
        "package_version": env!("CARGO_PKG_VERSION"),
        "master_seed": seed,
        "derived_seeds": {
            "turmoil": derive_seed(seed, TURMOIL_DOMAIN),
        },
        "seed_derivation": SEED_DERIVATION,
        "crash_point": point.name(),
        "error": error.to_string(),
        "replay": replay,
        "kv_events": trace,
    }))?;
    std::fs::write(
        directory.join(format!("failure-{}-{seed}.json", point.name())),
        body,
    )?;
    Ok(())
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
