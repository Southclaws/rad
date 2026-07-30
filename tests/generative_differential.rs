//! Correct-by-construction query generation and automatic
//! shrinking. Every case runs through the chosen physical plan, a forced full
//! scan plan, unbatched nested correlation, and the independent reference
//! executor.

mod generative;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use generative::{
    Case, ModelCase, ProgramCase, SemanticModelCase, check, check_invalid, check_invalid_program,
    check_metamorphic, check_model, check_program, check_semantic_model, emit_fixture, minimize,
    minimize_invalid, minimize_invalid_program, minimize_metamorphic, minimize_model,
    minimize_program, minimize_semantic_model, nested_identity_case, recursive_case,
    recursive_from_decisions,
};

#[tokio::test]
async fn generated_queries_match_all_execution_paths() {
    if let Ok(replay) = env::var("RAD_GEN_REPLAY") {
        let decisions = replay
            .split(',')
            .filter(|value| !value.is_empty())
            .map(|value| {
                value
                    .parse::<u64>()
                    .expect("RAD_GEN_REPLAY contains u64 values")
            })
            .collect();
        if env::var("RAD_GEN_REPLAY_KIND").as_deref() == Ok("program") {
            check_program(&ProgramCase::generate(decisions))
                .await
                .expect("replayed generated PIR program");
            return;
        }
        if env::var("RAD_GEN_REPLAY_KIND").as_deref() == Ok("invalid_program") {
            check_invalid_program(&ProgramCase::generate(decisions))
                .await
                .expect("replayed generated invalid PIR program");
            return;
        }
        if env::var("RAD_GEN_REPLAY_KIND").as_deref() == Ok("metamorphic") {
            check_metamorphic(&Case::generate(decisions))
                .await
                .expect("replayed metamorphic case");
            return;
        }
        if env::var("RAD_GEN_REPLAY_KIND").as_deref() == Ok("invalid") {
            check_invalid(&Case::generate(decisions))
                .await
                .expect("replayed invalid-query case");
            return;
        }
        if env::var("RAD_GEN_REPLAY_KIND").as_deref() == Ok("model") {
            check_model(&ModelCase::generate(decisions))
                .await
                .expect("replayed independent model case");
            return;
        }
        if env::var("RAD_GEN_REPLAY_KIND").as_deref() == Ok("semantic_model") {
            check_semantic_model(&SemanticModelCase::generate(decisions))
                .await
                .expect("replayed independent semantic model case");
            return;
        }
        if env::var("RAD_GEN_REPLAY_KIND").as_deref() == Ok("nested_identity") {
            check(&nested_identity_case(0).regenerate(decisions))
                .await
                .expect("replayed nested identity case");
            return;
        }
        let case = if env::var("RAD_GEN_REPLAY_KIND").as_deref() == Ok("recursive") {
            recursive_from_decisions(decisions)
        } else {
            Case::generate(decisions)
        };
        check(&case).await.expect("replayed generated case");
        return;
    }

    let cases = env_usize("RAD_GEN_CASES", 32);
    let base_seed = env_u64("RAD_GEN_SEED", 0x7261_642d_7275_7374);
    for offset in 0..cases as u64 {
        let seed = base_seed.wrapping_add(offset);
        let case = Case::from_seed(seed);
        if let Err(original) = check(&case).await {
            let minimized = minimize(&case, env_usize("RAD_GEN_SHRINK_BUDGET", 2_000))
                .await
                .expect("shrink generated failure");
            let minimized_error = check(&minimized)
                .await
                .expect_err("minimized case must retain the failure");
            let replay = minimized
                .decisions
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let fixture = if env::var_os("RAD_GEN_EMIT").is_some() {
                emit_fixture(&minimized, seed, &minimized_error)
                    .await
                    .map(|path| format!("\nemitted: {}", path.display()))
                    .unwrap_or_else(|error| format!("\nfixture emission failed: {error}"))
            } else {
                String::new()
            };
            panic!(
                "generated differential failed at seed {seed}\noriginal: {original}\nminimized: {minimized_error}\nreplay:\nRAD_GEN_REPLAY='{replay}' cargo test --test generative_differential -- --nocapture{fixture}"
            );
        }
    }
    println!("checked {cases} generated differential cases from seed {base_seed}");
}

#[tokio::test]
async fn generated_near_valid_queries_retain_structured_reasons() {
    if env::var_os("RAD_GEN_REPLAY").is_some() {
        return;
    }
    let cases = env_usize("RAD_GEN_INVALID_CASES", 24);
    let base_seed = env_u64("RAD_GEN_INVALID_SEED", 0x696e_7661_6c69_642d);
    for offset in 0..cases as u64 {
        let seed = base_seed.wrapping_add(offset);
        let case = Case::from_seed(seed);
        if let Err(original) = check_invalid(&case).await {
            let minimized = minimize_invalid(&case, env_usize("RAD_GEN_SHRINK_BUDGET", 2_000))
                .await
                .expect("shrink invalid-query failure");
            let minimized_error = check_invalid(&minimized)
                .await
                .expect_err("minimized invalid-query case must retain the failure");
            let replay = minimized
                .decisions
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            panic!(
                "invalid-query campaign failed at seed {seed}\noriginal: {original}\nminimized: {minimized_error}\nreplay:\nRAD_GEN_REPLAY_KIND=invalid RAD_GEN_REPLAY='{replay}' cargo test --test generative_differential -- --nocapture"
            );
        }
    }
    println!("checked {cases} invalid-query cases from seed {base_seed}");
}

#[tokio::test]
async fn generated_nested_rows_obey_set_identity() {
    if env::var_os("RAD_GEN_REPLAY").is_some() {
        return;
    }
    let cases = env_usize("RAD_GEN_NESTED_IDENTITY_CASES", 16);
    let base_seed = env_u64("RAD_GEN_NESTED_IDENTITY_SEED", 0x6e65_7374_6564_7365);
    for offset in 0..cases as u64 {
        let seed = base_seed.wrapping_add(offset);
        let case = nested_identity_case(seed);
        if let Err(original) = check(&case).await {
            let minimized = minimize(&case, env_usize("RAD_GEN_SHRINK_BUDGET", 2_000))
                .await
                .expect("shrink nested identity failure");
            let minimized_error = check(&minimized)
                .await
                .expect_err("minimized nested identity case must retain the failure");
            let replay = minimized
                .decisions
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let fixture = if env::var_os("RAD_GEN_EMIT").is_some() {
                emit_fixture(&minimized, seed, &minimized_error)
                    .await
                    .map(|path| format!("\nemitted: {}", path.display()))
                    .unwrap_or_else(|error| format!("\nfixture emission failed: {error}"))
            } else {
                String::new()
            };
            panic!(
                "nested identity differential failed at seed {seed}\noriginal: {original}\nminimized: {minimized_error}\nreplay:\nRAD_GEN_REPLAY_KIND=nested_identity RAD_GEN_REPLAY='{replay}' cargo test --test generative_differential -- --nocapture{fixture}"
            );
        }
    }
}

#[tokio::test]
async fn generated_queries_match_independent_result_model() {
    if env::var_os("RAD_GEN_REPLAY").is_some() {
        return;
    }
    let cases = env_usize("RAD_GEN_MODEL_CASES", 48);
    let base_seed = env_u64("RAD_GEN_MODEL_SEED", 0x6d6f_6465_6c2d_6c69);
    for offset in 0..cases as u64 {
        let seed = base_seed.wrapping_add(offset);
        let case = ModelCase::from_seed(seed);
        if let Err(original) = check_model(&case).await {
            let minimized = minimize_model(&case, env_usize("RAD_GEN_SHRINK_BUDGET", 2_000))
                .await
                .expect("shrink independent model failure");
            let minimized_error = check_model(&minimized)
                .await
                .expect_err("minimized model case must retain the failure");
            let replay = minimized
                .decisions
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            panic!(
                "independent model differential failed at seed {seed}\noriginal: {original}\nminimized: {minimized_error}\nreplay:\nRAD_GEN_REPLAY_KIND=model RAD_GEN_REPLAY='{replay}' cargo test --test generative_differential -- --nocapture"
            );
        }
    }
    println!("checked {cases} independent model cases from seed {base_seed}");
}

#[tokio::test]
async fn generated_semantics_match_small_independent_models() {
    if env::var_os("RAD_GEN_REPLAY").is_some() {
        return;
    }
    let cases = env_usize("RAD_GEN_SEMANTIC_MODEL_CASES", 64);
    let base_seed = env_u64("RAD_GEN_SEMANTIC_MODEL_SEED", 0x7365_6d61_6e74_6963);
    for offset in 0..cases as u64 {
        let seed = base_seed.wrapping_add(offset);
        let case = SemanticModelCase::from_seed(seed);
        if let Err(original) = check_semantic_model(&case).await {
            let minimized =
                minimize_semantic_model(&case, env_usize("RAD_GEN_SHRINK_BUDGET", 2_000))
                    .await
                    .expect("shrink independent semantic model failure");
            let minimized_error = check_semantic_model(&minimized)
                .await
                .expect_err("minimized semantic model case must retain the failure");
            let replay = minimized
                .decisions
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            panic!(
                "independent semantic model failed at seed {seed}\noriginal: {original}\nminimized: {minimized_error}\nreplay:\nRAD_GEN_REPLAY_KIND=semantic_model RAD_GEN_REPLAY='{replay}' cargo test --test generative_differential -- --nocapture"
            );
        }
    }
    println!("checked {cases} small semantic models from seed {base_seed}");
}

#[tokio::test]
async fn generated_queries_preserve_metamorphic_equivalence() {
    if env::var_os("RAD_GEN_REPLAY").is_some() {
        return;
    }
    let cases = env_usize("RAD_GEN_METAMORPHIC_CASES", 24);
    let base_seed = env_u64("RAD_GEN_METAMORPHIC_SEED", 0x6d65_7461_6d6f_7270);
    for offset in 0..cases as u64 {
        let seed = base_seed.wrapping_add(offset);
        let case = Case::from_seed(seed);
        if let Err(original) = check_metamorphic(&case).await {
            let minimized = minimize_metamorphic(&case, env_usize("RAD_GEN_SHRINK_BUDGET", 2_000))
                .await
                .expect("shrink metamorphic failure");
            let minimized_error = check_metamorphic(&minimized)
                .await
                .expect_err("minimized metamorphic case must retain the failure");
            let replay = minimized
                .decisions
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            panic!(
                "metamorphic differential failed at seed {seed}\noriginal: {original}\nminimized: {minimized_error}\nreplay:\nRAD_GEN_REPLAY_KIND=metamorphic RAD_GEN_REPLAY='{replay}' cargo test --test generative_differential -- --nocapture"
            );
        }
    }
    println!("checked {cases} metamorphic cases from seed {base_seed}");
}

#[tokio::test]
async fn generated_pir_programs_match_reference_and_state_model() {
    if env::var_os("RAD_GEN_REPLAY").is_some() {
        return;
    }
    let cases = env_usize("RAD_GEN_PROGRAM_CASES", 32);
    let base_seed = env_u64("RAD_GEN_PROGRAM_SEED", 0x7069_722d_7072_6f67);
    for offset in 0..cases as u64 {
        let seed = base_seed.wrapping_add(offset);
        let case = ProgramCase::from_seed(seed);
        if let Err(original) = check_program(&case).await {
            let minimized = minimize_program(&case, env_usize("RAD_GEN_SHRINK_BUDGET", 2_000))
                .await
                .expect("shrink generated PIR failure");
            let minimized_error = check_program(&minimized)
                .await
                .expect_err("minimized PIR case must retain the failure");
            let replay = minimized
                .decisions
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            panic!(
                "generated PIR differential failed at seed {seed}\noriginal: {original}\nminimized: {minimized_error}\nreplay:\nRAD_GEN_REPLAY_KIND=program RAD_GEN_REPLAY='{replay}' cargo test --test generative_differential -- --nocapture"
            );
        }
    }
    println!("checked {cases} generated PIR programs from seed {base_seed}");
}

#[tokio::test]
async fn generated_invalid_pir_preserves_reason_and_atomicity_contracts() {
    if env::var_os("RAD_GEN_REPLAY").is_some() {
        return;
    }
    let cases = env_usize("RAD_GEN_INVALID_PROGRAM_CASES", 4);
    let base_seed = env_u64("RAD_GEN_INVALID_PROGRAM_SEED", 0x696e_7661_6c69_6470);
    for offset in 0..cases as u64 {
        let seed = base_seed.wrapping_add(offset);
        let case = ProgramCase::from_seed(seed);
        if let Err(original) = check_invalid_program(&case).await {
            let minimized =
                minimize_invalid_program(&case, env_usize("RAD_GEN_SHRINK_BUDGET", 2_000))
                    .await
                    .expect("shrink generated invalid PIR failure");
            let minimized_error = check_invalid_program(&minimized)
                .await
                .expect_err("minimized invalid PIR case must retain the failure");
            let replay = minimized
                .decisions
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            panic!(
                "generated invalid PIR campaign failed at seed {seed}\noriginal: {original}\nminimized: {minimized_error}\nreplay:\nRAD_GEN_REPLAY_KIND=invalid_program RAD_GEN_REPLAY='{replay}' cargo test --test generative_differential -- --nocapture"
            );
        }
    }
    println!("checked {cases} invalid PIR programs from seed {base_seed}");
}

#[tokio::test]
async fn generated_recursive_queries_match_all_execution_paths() {
    if env::var_os("RAD_GEN_REPLAY").is_some() {
        return;
    }
    let cases = env_usize("RAD_GEN_RECURSIVE_CASES", 24);
    let base_seed = env_u64("RAD_GEN_RECURSIVE_SEED", 0x7265_6375_7273_6976);
    for offset in 0..cases as u64 {
        let seed = base_seed.wrapping_add(offset);
        let case = recursive_case(seed);
        if let Err(original) = check(&case).await {
            let minimized = minimize(&case, env_usize("RAD_GEN_SHRINK_BUDGET", 2_000))
                .await
                .expect("shrink recursive generated failure");
            let replay = minimized
                .decisions
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let minimized_error = check(&minimized)
                .await
                .expect_err("minimized recursive case must retain the failure");
            let fixture = if env::var_os("RAD_GEN_EMIT").is_some() {
                emit_fixture(&minimized, seed, &minimized_error)
                    .await
                    .map(|path| format!("\nemitted: {}", path.display()))
                    .unwrap_or_else(|error| format!("\nfixture emission failed: {error}"))
            } else {
                String::new()
            };
            panic!(
                "recursive differential failed at seed {seed}\noriginal: {original}\nminimized: {minimized_error}\nreplay:\nRAD_GEN_REPLAY_KIND=recursive RAD_GEN_REPLAY='{replay}' cargo test --test generative_differential -- --nocapture{fixture}"
            );
        }
    }
    println!("checked {cases} generated recursive cases from seed {base_seed}");
}

#[derive(Clone, Copy)]
struct SoakFamily {
    test: &'static str,
    count_env: &'static str,
    count: usize,
    seed_env: &'static str,
    seed_offset: u64,
}

/// Drives the ordinary generated-test cases in fresh child processes. Keeping
/// each family in a child makes environment configuration race-free, gives a
/// crashing family a precise identity, and exercises the same shrinking and
/// fixture-emission path as required CI.
#[test]
#[ignore = "wall-clock confidence campaign"]
fn generative_semantic_soak() {
    let requested_seconds = required_positive_env_u64("RAD_GEN_SOAK_SECONDS");
    let root_seed = required_env_u64("RAD_GEN_SOAK_SEED");
    let artifact_dir = soak_artifact_directory(env::var_os("RAD_TEST_ARTIFACT_DIR"));
    let fixture_dir = artifact_dir.join("fixtures");
    fs::create_dir_all(&fixture_dir).expect("create generative artifact directory");

    let families = [
        SoakFamily {
            test: "generated_queries_match_all_execution_paths",
            count_env: "RAD_GEN_CASES",
            count: env_positive_usize("RAD_GEN_SOAK_CASES", 64),
            seed_env: "RAD_GEN_SEED",
            seed_offset: 0,
        },
        SoakFamily {
            test: "generated_queries_match_independent_result_model",
            count_env: "RAD_GEN_MODEL_CASES",
            count: env_positive_usize("RAD_GEN_SOAK_MODEL_CASES", 64),
            seed_env: "RAD_GEN_MODEL_SEED",
            seed_offset: 100_000,
        },
        SoakFamily {
            test: "generated_queries_preserve_metamorphic_equivalence",
            count_env: "RAD_GEN_METAMORPHIC_CASES",
            count: env_positive_usize("RAD_GEN_SOAK_METAMORPHIC_CASES", 32),
            seed_env: "RAD_GEN_METAMORPHIC_SEED",
            seed_offset: 200_000,
        },
        SoakFamily {
            test: "generated_near_valid_queries_retain_structured_reasons",
            count_env: "RAD_GEN_INVALID_CASES",
            count: env_positive_usize("RAD_GEN_SOAK_INVALID_CASES", 32),
            seed_env: "RAD_GEN_INVALID_SEED",
            seed_offset: 225_000,
        },
        SoakFamily {
            test: "generated_semantics_match_small_independent_models",
            count_env: "RAD_GEN_SEMANTIC_MODEL_CASES",
            count: env_positive_usize("RAD_GEN_SOAK_SEMANTIC_MODEL_CASES", 64),
            seed_env: "RAD_GEN_SEMANTIC_MODEL_SEED",
            seed_offset: 250_000,
        },
        SoakFamily {
            test: "generated_pir_programs_match_reference_and_state_model",
            count_env: "RAD_GEN_PROGRAM_CASES",
            count: env_positive_usize("RAD_GEN_SOAK_PROGRAM_CASES", 32),
            seed_env: "RAD_GEN_PROGRAM_SEED",
            seed_offset: 300_000,
        },
        SoakFamily {
            test: "generated_invalid_pir_preserves_reason_and_atomicity_contracts",
            count_env: "RAD_GEN_INVALID_PROGRAM_CASES",
            count: env_positive_usize("RAD_GEN_SOAK_INVALID_PROGRAM_CASES", 8),
            seed_env: "RAD_GEN_INVALID_PROGRAM_SEED",
            seed_offset: 325_000,
        },
        SoakFamily {
            test: "generated_recursive_queries_match_all_execution_paths",
            count_env: "RAD_GEN_RECURSIVE_CASES",
            count: env_positive_usize("RAD_GEN_SOAK_RECURSIVE_CASES", 32),
            seed_env: "RAD_GEN_RECURSIVE_SEED",
            seed_offset: 400_000,
        },
        SoakFamily {
            test: "generated_nested_rows_obey_set_identity",
            count_env: "RAD_GEN_NESTED_IDENTITY_CASES",
            count: env_positive_usize("RAD_GEN_SOAK_NESTED_CASES", 16),
            seed_env: "RAD_GEN_NESTED_IDENTITY_SEED",
            seed_offset: 500_000,
        },
    ];

    write_soak_context(&artifact_dir, root_seed, &families);
    let executable = env::current_exe().expect("locate generative test executable");
    let started = Instant::now();
    let deadline = Duration::from_secs(requested_seconds);
    let mut completed_iterations = 0_u64;
    let mut failure = None;

    while started.elapsed() < deadline {
        let iteration = completed_iterations + 1;
        let wave_seed = root_seed.wrapping_add(iteration.wrapping_mul(1_000_000));
        println!("generative iteration={iteration} wave_seed={wave_seed}");

        for family in families {
            let seed = wave_seed.wrapping_add(family.seed_offset);
            let status = Command::new(&executable)
                .arg(family.test)
                .arg("--exact")
                .arg("--nocapture")
                .env(family.count_env, family.count.to_string())
                .env(family.seed_env, seed.to_string())
                .env("RAD_GEN_EMIT", &fixture_dir)
                .env_remove("RAD_GEN_REPLAY")
                .env_remove("RAD_GEN_REPLAY_KIND")
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status();
            let status = match status {
                Ok(status) => status,
                Err(error) => {
                    failure = Some(format!(
                        "could not start {} at iteration {iteration}, seed {seed}: {error}",
                        family.test
                    ));
                    break;
                }
            };
            if !status.success() {
                failure = Some(format!(
                    "{} failed at iteration {iteration}, seed {seed}, status {status}",
                    family.test
                ));
                break;
            }
        }

        if failure.is_some() {
            break;
        }
        completed_iterations = iteration;
    }

    write_soak_result(
        &artifact_dir,
        root_seed,
        requested_seconds,
        started.elapsed(),
        completed_iterations,
        failure.as_deref(),
    );
    if let Some(failure) = failure {
        panic!("{failure}");
    }
}

fn write_soak_context(artifact_dir: &Path, root_seed: u64, families: &[SoakFamily]) {
    let cases = families
        .iter()
        .map(|family| (family.test.to_owned(), serde_json::json!(family.count)))
        .collect::<serde_json::Map<_, _>>();
    write_json(
        &artifact_dir.join("context.json"),
        &serde_json::json!({
            "format": "rad-generative-soak-context-v1",
            "revision": env::var("GITHUB_SHA").unwrap_or_else(|_| "local".to_owned()),
            "source_revision": env::var("RAD_SOURCE_REVISION").ok(),
            "base_revision": env::var("RAD_BASE_REVISION").ok().filter(|value| !value.is_empty()),
            "root_seed": root_seed,
            "families": cases,
        }),
    );
}

fn write_soak_result(
    artifact_dir: &Path,
    root_seed: u64,
    requested_seconds: u64,
    elapsed: Duration,
    completed_iterations: u64,
    failure: Option<&str>,
) {
    write_json(
        &artifact_dir.join("campaign.json"),
        &serde_json::json!({
            "format": "rad-generative-soak-v1",
            "revision": env::var("GITHUB_SHA").unwrap_or_else(|_| "local".to_owned()),
            "source_revision": env::var("RAD_SOURCE_REVISION").ok(),
            "base_revision": env::var("RAD_BASE_REVISION").ok().filter(|value| !value.is_empty()),
            "root_seed": root_seed,
            "requested_seconds": requested_seconds,
            "elapsed_milliseconds": elapsed.as_millis(),
            "completed_iterations": completed_iterations,
            "status": if failure.is_some() { "failed" } else { "passed" },
            "failure": failure,
        }),
    );
}

fn write_json(path: &Path, value: &serde_json::Value) {
    let mut bytes = serde_json::to_vec_pretty(value).expect("encode generative campaign evidence");
    bytes.push(b'\n');
    fs::write(path, bytes).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
}

fn soak_artifact_directory(configured: Option<std::ffi::OsString>) -> PathBuf {
    configured.map(PathBuf::from).unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("target/rad-test-artifacts/generative")
    })
}

#[test]
fn generative_soak_has_a_cross_platform_local_artifact_default() {
    assert_eq!(
        soak_artifact_directory(None),
        Path::new(env!("CARGO_MANIFEST_DIR")).join("target/rad-test-artifacts/generative")
    );
    assert_eq!(
        soak_artifact_directory(Some(std::ffi::OsString::from("chosen"))),
        PathBuf::from("chosen")
    );
}

fn required_positive_env_u64(name: &str) -> u64 {
    let value = required_env_u64(name);
    assert!(value > 0, "{name} must be greater than zero");
    value
}

fn required_env_u64(name: &str) -> u64 {
    env::var(name)
        .unwrap_or_else(|_| panic!("{name} must be set"))
        .parse()
        .unwrap_or_else(|_| panic!("{name} must be an unsigned integer"))
}

fn env_positive_usize(name: &str, default: usize) -> usize {
    let value = env_usize(name, default);
    assert!(value > 0, "{name} must be greater than zero");
    value
}

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .map(|value| {
            value
                .parse()
                .unwrap_or_else(|_| panic!("{name} must be an integer"))
        })
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .map(|value| {
            value
                .parse()
                .unwrap_or_else(|_| panic!("{name} must be an integer"))
        })
        .unwrap_or(default)
}
