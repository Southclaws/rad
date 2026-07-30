#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "support/schema_scheduler_scenario.rs"]
mod scenario;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::fs::File;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io::{BufWriter, Read, Write};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::process::Command;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use rand::{SeedableRng, rngs::StdRng};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use scenario::{CapturedTrace, CrashBoundary, Scenario, assert_trace_eq, capture_case_trace};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use sha2::{Digest, Sha256};

#[test]
#[ignore = "requires RUSTFLAGS=--cfg tokio_unstable to seed Tokio's task scheduler"]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn identical_seed_and_boundary_produce_identical_traces() -> Result<(), Box<dyn std::error::Error>>
{
    const REPLAY_SEED: u64 = 0x7261_642d_6473_7401;
    let mut evidence = Vec::new();
    write_evidence(REPLAY_SEED, "running", &evidence)?;

    for (index, boundary) in CrashBoundary::ALL.into_iter().enumerate() {
        let scenario = match boundary {
            CrashBoundary::ActivationCommitStarted | CrashBoundary::ActivationCommitSucceeded => {
                Scenario::DependencyGraph
            }
            CrashBoundary::CancellationCommitStarted => Scenario::CancelledReplacement,
            CrashBoundary::CancellationCommitSucceeded => Scenario::CancelledConstraint,
            _ => Scenario::NORMAL[index % Scenario::NORMAL.len()],
        };
        let directory = tempfile::tempdir()?;
        let first_path = directory.path().join("first.json");
        let replay_path = directory.path().join("replay.json");
        run_trace_process(REPLAY_SEED, scenario, boundary, &first_path)?;
        run_trace_process(REPLAY_SEED, scenario, boundary, &replay_path)?;
        let comparison = compare_trace_files(&first_path, &replay_path)?;
        if !comparison.identical {
            let first_bytes = std::fs::read(first_path)?;
            let replay_bytes = std::fs::read(replay_path)?;
            let first: CapturedTrace = serde_json::from_slice(&first_bytes)?;
            let replay: CapturedTrace = serde_json::from_slice(&replay_bytes)?;
            retain_divergence(REPLAY_SEED, scenario, boundary, &first_bytes, &replay_bytes)?;
            write_evidence(REPLAY_SEED, "diverged", &evidence)?;
            assert_trace_eq(scenario, boundary, &first, &replay);
            assert_eq!(
                first_bytes,
                replay_bytes,
                "semantic traces matched but encoded bytes diverged in {} at {}",
                scenario.name(),
                boundary.name()
            );
        }
        evidence.push(serde_json::json!({
            "scenario": scenario.name(),
            "boundary": boundary.name(),
            "trace_bytes": comparison.bytes,
            "trace_sha256": comparison.sha256,
        }));
        write_evidence(REPLAY_SEED, "running", &evidence)?;
    }
    write_evidence(REPLAY_SEED, "passed", &evidence)?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct TraceFileComparison {
    identical: bool,
    bytes: u64,
    sha256: String,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn compare_trace_files(
    first_path: &Path,
    replay_path: &Path,
) -> Result<TraceFileComparison, Box<dyn std::error::Error>> {
    const BUFFER_BYTES: usize = 64 * 1024;

    let first_bytes = first_path.metadata()?.len();
    let replay_bytes = replay_path.metadata()?.len();
    let mut first = File::open(first_path)?;
    let mut replay = File::open(replay_path)?;
    let mut first_buffer = [0_u8; BUFFER_BYTES];
    let mut replay_buffer = [0_u8; BUFFER_BYTES];
    let mut remaining = first_bytes;
    let mut identical = first_bytes == replay_bytes;
    let mut digest = Sha256::new();

    while remaining > 0 {
        let count = usize::try_from(remaining.min(BUFFER_BYTES as u64))?;
        first.read_exact(&mut first_buffer[..count])?;
        digest.update(&first_buffer[..count]);
        if identical {
            replay.read_exact(&mut replay_buffer[..count])?;
            identical &= first_buffer[..count] == replay_buffer[..count];
        }
        remaining -= count as u64;
    }

    Ok(TraceFileComparison {
        identical,
        bytes: first_bytes,
        sha256: format!("sha256:{:x}", digest.finalize()),
    })
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn trace_file_comparison_is_exact_and_streamed() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let first = directory.path().join("first.json");
    let replay = directory.path().join("replay.json");
    let first_bytes = vec![b'x'; 70_000];
    std::fs::write(&first, &first_bytes)?;
    std::fs::write(&replay, &first_bytes)?;

    let equal = compare_trace_files(&first, &replay)?;
    assert!(equal.identical);
    assert_eq!(equal.bytes, first_bytes.len() as u64);
    assert_eq!(
        equal.sha256,
        format!("sha256:{:x}", Sha256::digest(&first_bytes))
    );

    let mut changed = first_bytes.clone();
    changed[65_536] = b'y';
    std::fs::write(&replay, &changed)?;
    assert!(!compare_trace_files(&first, &replay)?.identical);
    let mut longer = first_bytes.clone();
    longer.push(b'z');
    std::fs::write(&replay, &longer)?;
    assert!(!compare_trace_files(&first, &replay)?.identical);
    Ok(())
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn recovery_monitor_does_not_dominate_the_semantic_trace() -> Result<(), Box<dyn std::error::Error>>
{
    const MAX_KV_EVENTS: usize = 100_000;
    const MAX_ENCODED_BYTES: usize = 20 * 1024 * 1024;
    let trace = capture_case_trace(
        0x7261_642d_6473_7401,
        Scenario::DependencyGraph,
        CrashBoundary::WriteProtocol,
    )?;
    let encoded = serde_json::to_vec(&trace)?;
    assert!(
        trace.kv_event_count() <= MAX_KV_EVENTS,
        "tiny dependency scenario emitted {} KV events; budget is {MAX_KV_EVENTS}",
        trace.kv_event_count()
    );
    assert!(
        encoded.len() <= MAX_ENCODED_BYTES,
        "tiny dependency scenario encoded {} bytes; budget is {MAX_ENCODED_BYTES}",
        encoded.len()
    );
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_evidence(
    seed: u64,
    status: &str,
    cases: &[serde_json::Value],
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(directory) = std::env::var_os("RAD_TEST_ARTIFACT_DIR") else {
        return Ok(());
    };
    let directory = PathBuf::from(directory);
    std::fs::create_dir_all(&directory)?;
    std::fs::write(
        directory.join("deterministic-replay.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "format": "rad-dst-determinism-v1",
            "status": status,
            "revision": std::env::var("GITHUB_SHA").ok(),
            "source_revision": std::env::var("RAD_SOURCE_REVISION").ok(),
            "base_revision": std::env::var("RAD_BASE_REVISION").ok().filter(|value| !value.is_empty()),
            "master_seed": seed,
            "controls": [
                "turmoil_seed",
                "tokio_scheduler_seed",
                "ambient_entropy",
                "simulated_clocks",
                "process_isolation",
            ],
            "cases": cases,
        }))?,
    )?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn retain_divergence(
    seed: u64,
    scenario: Scenario,
    boundary: CrashBoundary,
    first: &[u8],
    replay: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(directory) = std::env::var_os("RAD_TEST_ARTIFACT_DIR") else {
        return Ok(());
    };
    let directory = PathBuf::from(directory);
    std::fs::create_dir_all(&directory)?;
    std::fs::write(
        directory.join(format!(
            "divergence-{}-{}-first.json",
            scenario.name(),
            boundary.name()
        )),
        first,
    )?;
    std::fs::write(
        directory.join(format!(
            "divergence-{}-{}-replay.json",
            scenario.name(),
            boundary.name()
        )),
        replay,
    )?;
    std::fs::write(
        directory.join("replay.txt"),
        format!(
            "master_seed={seed}\nscenario={}\nboundary={}\ncommand=task test:dst:determinism\n",
            scenario.name(),
            boundary.name()
        ),
    )?;
    Ok(())
}

#[test]
#[ignore = "process-isolated deterministic trace replay helper"]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn turmoil_schema_work_trace_replay() -> Result<(), Box<dyn std::error::Error>> {
    let seed = std::env::var("RAD_DST_SEED")?.parse()?;
    let scenario = Scenario::parse(&std::env::var("RAD_DST_SCENARIO")?)?;
    let boundary = CrashBoundary::parse(&std::env::var("RAD_DST_BOUNDARY")?)?;
    let output = PathBuf::from(
        std::env::var_os("RAD_DST_TRACE_PATH")
            .ok_or("RAD_DST_TRACE_PATH must name the output file for deterministic trace replay")?,
    );
    let ambient_seed = scenario::ambient_seed(seed);
    mad_turmoil::rand::set_rng(StdRng::seed_from_u64(ambient_seed));
    fastrand::seed(ambient_seed);
    let _clocks = mad_turmoil::time::SimClocksGuard::init();
    let mut output = BufWriter::new(File::create(output)?);
    serde_json::to_writer(&mut output, &capture_case_trace(seed, scenario, boundary)?)?;
    output.flush()?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_trace_process(
    seed: u64,
    scenario: Scenario,
    boundary: CrashBoundary,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let result = Command::new(std::env::current_exe()?)
        .args(["turmoil_schema_work_trace_replay", "--exact", "--ignored"])
        .env("RAD_DST_SEED", seed.to_string())
        .env("RAD_DST_SCENARIO", scenario.name())
        .env("RAD_DST_BOUNDARY", boundary.name())
        .env("RAD_DST_TRACE_PATH", output)
        .output()?;
    if !result.status.success() {
        return Err(format!(
            "same-seed trace process failed for {} at {}: status={}; stdout={}; stderr={}",
            scenario.name(),
            boundary.name(),
            result.status,
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr),
        )
        .into());
    }
    Ok(())
}

#[test]
#[ignore = "mad-turmoil supports simulation binaries on Linux and macOS"]
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn deterministic_trace_replay_is_not_supported_on_this_platform() {}
