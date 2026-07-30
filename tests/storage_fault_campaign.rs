use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use rad::engine::kv::fault::{
    FaultAction, FaultController, FaultRule, FaultingKv, Operation, RedactedTraceEvent,
    TraceOutcome, TracePhase,
};
use rad::engine::kv::slatedb::Store;
use rad::engine::kv::{ErrorKind, IsolationLevel, Kv, TransactionalKv};
use serde::Serialize;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FIRST: &[u8] = b"fault-campaign/first";
const SECOND: &[u8] = b"fault-campaign/second";
const THIRD: &[u8] = b"fault-campaign/third";
static STORE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct Failure {
    message: String,
    trace: Vec<RedactedTraceEvent>,
}

#[derive(Serialize)]
struct FailureArtifact<'a> {
    format: &'static str,
    generator: &'static str,
    revision: Option<String>,
    package_version: &'static str,
    master_seed: u64,
    error: &'a str,
    rules: &'a [FaultRule],
    trace: &'a [RedactedTraceEvent],
    replay: String,
}

#[tokio::test]
async fn generated_fault_plans_preserve_transaction_atomicity() -> TestResult {
    let replay = std::env::var("RAD_STORAGE_FAULT_SEED")
        .ok()
        .map(|value| value.parse::<u64>())
        .transpose()?;
    let cases = if replay.is_some() {
        1
    } else {
        std::env::var("RAD_STORAGE_FAULT_CASES")
            .ok()
            .map(|value| value.parse::<u64>())
            .transpose()?
            .unwrap_or(64)
    };
    let first_seed = replay.unwrap_or(0x7374_6f72_6167_652d);
    for seed in first_seed..first_seed.saturating_add(cases) {
        let rules = rules(seed);
        if run(&rules).await.is_err() {
            let (rules, failure) = minimize(rules).await;
            write_failure(seed, &rules, &failure)?;
            return Err(format!(
                "storage fault campaign failed at seed {seed}: {}; replay with RAD_STORAGE_FAULT_SEED={seed} cargo test --test storage_fault_campaign -- --nocapture",
                failure.message
            )
            .into());
        }
    }
    Ok(())
}

fn rules(seed: u64) -> Vec<FaultRule> {
    let mut random = SplitMix64(seed);
    let count = 1 + random.index(4);
    (0..count)
        .map(|_| {
            let operation = match random.index(5) {
                0 => Operation::Begin,
                1 => Operation::TransactionPut,
                2 => Operation::TransactionDelete,
                3 => Operation::Commit,
                _ => Operation::TransactionGet,
            };
            let occurrence = match operation {
                Operation::TransactionPut => 1 + random.index(2) as u64,
                _ => 1,
            };
            let action = if operation == Operation::Commit && random.index(2) == 1 {
                FaultAction::ErrorAfter(ErrorKind::CommitOutcomeUnknown)
            } else if random.index(2) == 0 {
                FaultAction::ErrorBefore(ErrorKind::Unavailable)
            } else {
                FaultAction::ErrorAfter(ErrorKind::Unavailable)
            };
            FaultRule {
                operation,
                occurrence,
                action,
            }
        })
        .collect()
}

async fn run(rules: &[FaultRule]) -> Result<(), Failure> {
    let sequence = STORE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let store = Arc::new(
        Store::memory(&format!("storage-fault-campaign-{sequence}"))
            .await
            .map_err(|error| failure(error.to_string(), Vec::new()))?,
    );
    for (key, value) in [
        (FIRST, b"before-1".as_slice()),
        (SECOND, b"before-2"),
        (THIRD, b"before-3"),
    ] {
        store
            .put(Bytes::copy_from_slice(key), Bytes::copy_from_slice(value))
            .await
            .map_err(|error| failure(error.to_string(), Vec::new()))?;
    }
    let controller = FaultController::new(rules.to_vec());
    let faulting = FaultingKv::new(store.clone(), controller.clone());
    let result = async {
        let transaction = faulting.begin(IsolationLevel::SerializableSnapshot).await?;
        let _ = transaction.get(FIRST).await?;
        transaction.put(Bytes::from_static(FIRST), Bytes::from_static(b"after-1"))?;
        transaction.put(Bytes::from_static(SECOND), Bytes::from_static(b"after-2"))?;
        transaction.delete(THIRD)?;
        transaction.commit().await
    }
    .await;

    let observed = [
        store.get(FIRST).await,
        store.get(SECOND).await,
        store.get(THIRD).await,
    ];
    let trace = controller.redacted_trace();
    validate_trace(&trace).map_err(|message| failure(message, trace.clone()))?;
    let encoded_trace =
        serde_json::to_string(&trace).map_err(|error| failure(error.to_string(), trace.clone()))?;
    if encoded_trace.contains("fault-campaign") {
        return Err(failure(
            "redacted storage trace leaked key material".into(),
            trace,
        ));
    }
    let values = observed
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| failure(error.to_string(), trace.clone()))?;
    let before = [
        Some(Bytes::from_static(b"before-1")),
        Some(Bytes::from_static(b"before-2")),
        Some(Bytes::from_static(b"before-3")),
    ];
    let after = [
        Some(Bytes::from_static(b"after-1")),
        Some(Bytes::from_static(b"after-2")),
        None,
    ];
    if values.as_slice() != before && values.as_slice() != after {
        return Err(failure(
            format!("partial transaction became visible: {values:?}; outcome={result:?}"),
            trace,
        ));
    }
    if result.is_ok() && values.as_slice() != after {
        return Err(failure(
            format!("acknowledged commit was not durable: {values:?}"),
            trace,
        ));
    }
    store
        .close()
        .await
        .map_err(|error| failure(error.to_string(), trace))?;
    Ok(())
}

fn validate_trace(trace: &[RedactedTraceEvent]) -> Result<(), String> {
    if trace
        .windows(2)
        .any(|pair| pair[1].sequence != pair[0].sequence + 1)
    {
        return Err("storage trace sequence is not contiguous".into());
    }
    for pair in trace.chunks(2) {
        let [started, finished] = pair else {
            return Err("storage trace ended with an unfinished operation".into());
        };
        if started.phase != TracePhase::Started
            || started.outcome != TraceOutcome::Pending
            || finished.phase != TracePhase::Finished
            || started.operation != finished.operation
            || started.occurrence != finished.occurrence
            || started.transaction != finished.transaction
            || started.target != finished.target
        {
            return Err(format!("storage trace pair is inconsistent: {pair:?}"));
        }
    }
    Ok(())
}

async fn minimize(mut rules: Vec<FaultRule>) -> (Vec<FaultRule>, Failure) {
    let mut failure = run(&rules)
        .await
        .expect_err("caller supplied a failing plan");
    let mut index = 0;
    while index < rules.len() {
        let mut candidate = rules.clone();
        candidate.remove(index);
        match run(&candidate).await {
            Err(candidate_failure) => {
                rules = candidate;
                failure = candidate_failure;
            }
            Ok(()) => index += 1,
        }
    }
    (rules, failure)
}

fn failure(message: String, trace: Vec<RedactedTraceEvent>) -> Failure {
    Failure { message, trace }
}

fn write_failure(seed: u64, rules: &[FaultRule], failure: &Failure) -> TestResult {
    let directory = std::env::var_os("RAD_TEST_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/rad-test-artifacts/storage"));
    std::fs::create_dir_all(&directory)?;
    let replay = format!(
        "RAD_STORAGE_FAULT_SEED={seed} cargo test --locked --test storage_fault_campaign -- --nocapture"
    );
    let body = serde_json::to_vec_pretty(&FailureArtifact {
        format: "rad-storage-fault-failure-v2",
        generator: "splitmix64-storage-fault-plan-v1",
        revision: std::env::var("GITHUB_SHA").ok(),
        package_version: env!("CARGO_PKG_VERSION"),
        master_seed: seed,
        error: &failure.message,
        rules,
        trace: &failure.trace,
        replay,
    })?;
    std::fs::write(directory.join(format!("fault-{seed}.json")), body)?;
    Ok(())
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn index(&mut self, length: usize) -> usize {
        (self.next() % length as u64) as usize
    }
}
