use std::env;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

mod support;

use support::access_paths;
use support::benchmark::{self, Manifest, QueryCase};
use support::commerce;
use support::http_process::{RadProcess, request_timeout_seconds};
use support::s3::{RustFs, TestResult};
use support::threaded_posts::{self, DatasetShape};
use support::toxiproxy::ToxiProxy;

#[derive(Clone, Copy, Debug, Default)]
struct Traffic {
    uploaded: u64,
    downloaded: u64,
}

impl Traffic {
    fn delta(self, before: Self) -> Self {
        Self {
            uploaded: self.uploaded.saturating_sub(before.uploaded),
            downloaded: self.downloaded.saturating_sub(before.downloaded),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BenchmarkProfile {
    Full,
    Smoke,
}

impl BenchmarkProfile {
    fn load() -> TestResult<Self> {
        let configured = env::var("RAD_BENCHMARK_PROFILE").ok();
        Self::parse(configured.as_deref())
    }

    fn parse(configured: Option<&str>) -> TestResult<Self> {
        match configured.unwrap_or("full") {
            "full" => Ok(Self::Full),
            "smoke" => Ok(Self::Smoke),
            value => {
                Err(format!("RAD_BENCHMARK_PROFILE must be full or smoke, got {value:?}").into())
            }
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Smoke => "smoke",
        }
    }

    fn default_pairs(self) -> usize {
        match self {
            Self::Full => 12,
            Self::Smoke => 2,
        }
    }

    fn query_counts(self, query: &QueryCase) -> (usize, usize) {
        match self {
            Self::Full => (query.warmup, query.iterations),
            Self::Smoke => (query.warmup.min(1), query.iterations.min(2)),
        }
    }
}

#[derive(Debug)]
struct ModeFailure {
    stage: &'static str,
    query: Option<String>,
    completed_queries: Vec<Value>,
    statistics_preparation_ms: Option<u64>,
    message: String,
}

impl ModeFailure {
    fn new(
        stage: &'static str,
        query: Option<&str>,
        completed_queries: &[Value],
        statistics_preparation_ms: Option<u64>,
        error: impl std::fmt::Display,
    ) -> Self {
        Self {
            stage,
            query: query.map(ToOwned::to_owned),
            completed_queries: completed_queries.to_vec(),
            statistics_preparation_ms,
            message: error.to_string(),
        }
    }

    fn view(&self, pair: usize, mode: &str) -> Value {
        json!({
            "pair": pair,
            "mode": mode,
            "stage": self.stage,
            "query": self.query,
            "message": self.message,
            "statistics_preparation_ms": self.statistics_preparation_ms,
            "completed_queries": self.completed_queries,
        })
    }
}

struct ModeContext<'a> {
    rustfs: &'a RustFs,
    proxy: &'a ToxiProxy,
    prefix: &'a str,
    benchmark: &'a Manifest,
    queries: &'a [QueryCase],
    probe: &'a QueryCase,
    expected_statistics_snapshot_identity: &'a str,
    profile: BenchmarkProfile,
}

#[tokio::test]
#[ignore = "requires a Docker daemon; emits measurements rather than enforcing timing thresholds"]
async fn commerce_workload_records_stable_outside_in_s3_measurements() -> TestResult {
    let manifest = commerce::load_benchmark()?;
    let dataset = Dataset::Commerce(commerce::counts(&manifest)?);
    let schema = commerce::schema(&manifest)?;
    let queries = commerce::queries(&manifest)?;
    let fixture_hash = commerce::fixture_hash(&manifest)?;
    run_workload(manifest, schema, queries, fixture_hash, dataset).await
}

#[tokio::test]
#[ignore = "requires a Docker daemon; emits measurements rather than enforcing timing thresholds"]
async fn threaded_posts_workload_records_recursive_relation_s3_measurements() -> TestResult {
    let manifest = threaded_posts::load_benchmark()?;
    let dataset = Dataset::ThreadedPosts(threaded_posts::shape(&manifest)?);
    let schema = threaded_posts::schema(&manifest)?;
    let queries = threaded_posts::queries(&manifest)?;
    let fixture_hash = threaded_posts::fixture_hash(&manifest)?;
    run_workload(manifest, schema, queries, fixture_hash, dataset).await
}

#[tokio::test]
#[ignore = "requires a Docker daemon; emits measurements rather than enforcing timing thresholds"]
async fn access_paths_workload_records_statistics_and_plan_measurements() -> TestResult {
    let manifest = access_paths::load_benchmark()?;
    let dataset = Dataset::AccessPaths(access_paths::rows(&manifest)?);
    let schema = access_paths::schema(&manifest)?;
    let queries = access_paths::queries(&manifest)?;
    let fixture_hash = access_paths::fixture_hash(&manifest)?;
    run_workload(manifest, schema, queries, fixture_hash, dataset).await
}

enum Dataset {
    AccessPaths(usize),
    Commerce(commerce::Counts),
    ThreadedPosts(DatasetShape),
}

async fn run_workload(
    benchmark: Manifest,
    schema: String,
    queries: Vec<QueryCase>,
    fixture_hash: String,
    dataset: Dataset,
) -> TestResult {
    let profile = BenchmarkProfile::load()?;
    let request_timeout_seconds = request_timeout_seconds()?;
    let queries = configured_queries(queries)?;
    let probe = queries
        .first()
        .ok_or("benchmark workload must contain one query")?;
    let rustfs = RustFs::start_or_external().await?;
    let proxy_name = format!("rustfs-benchmark-{}", benchmark.name);
    let proxy = ToxiProxy::start(&rustfs.config.endpoint, &proxy_name).await?;
    let prefix = format!("benchmark-{}-{}", benchmark.name, &fixture_hash[..12]);
    let server = RadProcess::start_s3(&rustfs.config, &proxy.endpoint, &prefix).await?;
    let migration_started = Instant::now();
    server.migrate(&schema).await?;
    let migration_ms = millis(migration_started.elapsed());

    let traffic_before = traffic(&proxy.metrics().await?, &proxy.name)?;
    let write_started = Instant::now();
    let load = match dataset {
        Dataset::AccessPaths(rows) => {
            access_paths::load_dataset(&server, rows, benchmark.batch_rows).await?
        }
        Dataset::Commerce(counts) => {
            commerce::load_dataset(&server, counts, benchmark.batch_rows).await?
        }
        Dataset::ThreadedPosts(shape) => {
            threaded_posts::load_dataset(&server, shape, benchmark.batch_rows).await?
        }
    };
    let http_write_elapsed = write_started.elapsed();
    let close_started = Instant::now();
    server.stop().await?;
    let close_elapsed = close_started.elapsed();
    let durable_write_elapsed = write_started.elapsed();
    let traffic_after = wait_for_quiet_traffic(&proxy).await?;
    let write_traffic = traffic_after.delta(traffic_before);
    if write_traffic.uploaded == 0 {
        return Err(format!(
            "{} load produced no observable S3 upload traffic",
            benchmark.name
        )
        .into());
    }

    let reopen_started = Instant::now();
    let server =
        RadProcess::start_s3_corpus_writer(&rustfs.config, &proxy.endpoint, &prefix).await?;
    let reopen_ms = millis(reopen_started.elapsed());
    let statistics_preparation_ms = prepare_statistics(&server, &benchmark).await?;
    for query in &queries {
        let response = server.execute(&query.program).await?;
        verify_rows(query, &response)?;
    }
    let (statistics_seed, statistics_settled) = settled_statistics(&server).await?;
    server.stop().await?;

    let replay_server =
        RadProcess::start_s3_reader(&rustfs.config, &proxy.endpoint, &prefix).await?;
    let replay_statistics_preparation_ms = prepare_statistics(&replay_server, &benchmark).await?;
    let expected_statistics_snapshot_identity =
        planning_snapshot_identity(&replay_server, probe).await?;
    let replay = replay_server.replay_corpus(1_000).await?;
    if replay["statisticsSnapshotIdentity"] != expected_statistics_snapshot_identity {
        return Err(format!(
            "corpus replay used statistics identity {}, expected {:?}",
            replay["statisticsSnapshotIdentity"], expected_statistics_snapshot_identity
        )
        .into());
    }
    replay_server.stop().await?;

    let pair_count = configured_pair_count(profile)?;
    let acceptance_eligible = profile == BenchmarkProfile::Full && pair_count == 12;
    let mode_context = ModeContext {
        rustfs: &rustfs,
        proxy: &proxy,
        prefix: &prefix,
        benchmark: &benchmark,
        queries: &queries,
        probe,
        expected_statistics_snapshot_identity: &expected_statistics_snapshot_identity,
        profile,
    };
    let mut pairs = Vec::with_capacity(pair_count);
    for pair in 0..pair_count {
        let order = if pair % 2 == 0 {
            ["structural", "cost"]
        } else {
            ["cost", "structural"]
        };
        let mut modes = Vec::new();
        for mode in order {
            match run_mode(&mode_context, pair, mode).await {
                Ok(report) => modes.push(report),
                Err(failure) => {
                    let partial_pair = json!({
                        "index": pair,
                        "order": order,
                        "modes": modes,
                        "failure": failure.view(pair, mode),
                    });
                    write_failure_artifact(
                        &benchmark,
                        &fixture_hash,
                        profile,
                        pair_count,
                        request_timeout_seconds,
                        &expected_statistics_snapshot_identity,
                        &replay,
                        &pairs,
                        partial_pair,
                    )?;
                    return Err(format!(
                        "benchmark {:?} failed in pair {pair}, mode {mode}, stage {}: {}",
                        benchmark.name, failure.stage, failure.message
                    )
                    .into());
                }
            }
        }
        pairs.push(json!({
            "index": pair,
            "order": order,
            "modes": modes,
        }));
    }
    let confidence = paired_confidence(&pairs, &queries, &replay, acceptance_eligible)?;

    let write_seconds = durable_write_elapsed.as_secs_f64();
    let report = json!({
        "format": benchmark::RESULT_FORMAT,
        "workload": benchmark.name,
        "description": benchmark.description,
        "fixture_hash": fixture_hash,
        "run": {
            "id": env::var("RAD_BENCHMARK_RUN_ID").ok(),
            "pair_id": env::var("RAD_BENCHMARK_PAIR_ID").ok(),
            "source_revision": env::var("RAD_SOURCE_REVISION").ok(),
            "runner": env::var("RAD_BENCHMARK_RUNNER").ok(),
            "build_profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            "benchmark_profile": profile.name(),
            "query_filter": env::var("RAD_BENCHMARK_QUERY").ok(),
            "request_timeout_seconds": request_timeout_seconds
        },
        "planner": {
            "modes": ["structural", "cost"],
            "plan_capture": "executed_show_plan",
            "pairs": pair_count,
            "order": "alternating",
            "acceptance_eligible": acceptance_eligible,
            "statistics_snapshot_identity": expected_statistics_snapshot_identity
        },
        "target": {
            "os": env::consts::OS,
            "architecture": env::consts::ARCH,
            "backend": "s3-rustfs",
            "transport": "http",
            "network": "toxiproxy-transparent"
        },
        "schema_migration_ms": migration_ms,
        "reopen_ms": reopen_ms,
        "statistics_preparation_ms": statistics_preparation_ms,
        "writes": {
            "rows": load.rows,
            "batches": load.batches,
            "logical_row_bytes": load.logical_bytes,
            "http_write_ms": millis(http_write_elapsed),
            "close_flush_ms": millis(close_elapsed),
            "durable_elapsed_ms": millis(durable_write_elapsed),
            "rows_per_second": load.rows as f64 / write_seconds,
            "s3_uploaded_bytes": write_traffic.uploaded,
            "s3_downloaded_bytes": write_traffic.downloaded,
            "network_write_amplification": ratio(write_traffic.uploaded, load.logical_bytes)
        },
        "pairs": pairs,
        "confidence": confidence,
        "replay": replay,
        "replay_statistics_preparation_ms": replay_statistics_preparation_ms,
        "statistics_seed": {
            "settled": statistics_settled,
            "snapshot": statistics_seed
        }
    });
    let encoded = serde_json::to_vec_pretty(&report)?;
    println!("{}", String::from_utf8_lossy(&encoded));
    write_artifact(&benchmark.name, profile, "result", &encoded)?;
    Ok(())
}

fn configured_queries(queries: Vec<QueryCase>) -> TestResult<Vec<QueryCase>> {
    let Ok(name) = env::var("RAD_BENCHMARK_QUERY") else {
        return Ok(queries);
    };
    let selected = queries
        .into_iter()
        .filter(|query| query.name == name)
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Err(format!("RAD_BENCHMARK_QUERY does not match query {name:?}").into());
    }
    Ok(selected)
}

async fn run_mode(
    context: &ModeContext<'_>,
    pair: usize,
    mode: &str,
) -> Result<Value, ModeFailure> {
    let no_reports = Vec::new();
    let metrics = context.proxy.metrics().await.map_err(|error| {
        ModeFailure::new("physical_measurement", None, &no_reports, None, error)
    })?;
    let traffic_before = traffic(&metrics, &context.proxy.name).map_err(|error| {
        ModeFailure::new("physical_measurement", None, &no_reports, None, error)
    })?;
    let server = RadProcess::start_s3_reader_mode(
        &context.rustfs.config,
        &context.proxy.endpoint,
        context.prefix,
        mode,
    )
    .await
    .map_err(|error| ModeFailure::new("process_start", None, &no_reports, None, error))?;
    let statistics_preparation_ms = match wait_for_statistics_identity(
        &server,
        context.benchmark,
        context.probe,
        context.expected_statistics_snapshot_identity,
    )
    .await
    {
        Ok(elapsed) => elapsed,
        Err(error) => {
            let message = match server.stop().await {
                Ok(()) => error.to_string(),
                Err(stop_error) => format!("{error}; process stop failed: {stop_error}"),
            };
            return Err(ModeFailure::new(
                "statistics_identity",
                None,
                &no_reports,
                None,
                message,
            ));
        }
    };
    let mut reports = Vec::with_capacity(context.queries.len());
    for query in context.queries {
        match run_query(&server, query, context.profile).await {
            Ok(report) => reports.push(report),
            Err(error) => {
                let message = match server.stop().await {
                    Ok(()) => error.to_string(),
                    Err(stop_error) => format!("{error}; process stop failed: {stop_error}"),
                };
                return Err(ModeFailure::new(
                    "query_execution",
                    Some(&query.name),
                    &reports,
                    Some(statistics_preparation_ms),
                    message,
                ));
            }
        }
    }
    let (statistics, statistics_settled) = settled_statistics(&server).await.map_err(|error| {
        ModeFailure::new(
            "statistics_settle",
            None,
            &reports,
            Some(statistics_preparation_ms),
            error,
        )
    })?;
    server.stop().await.map_err(|error| {
        ModeFailure::new(
            "process_stop",
            None,
            &reports,
            Some(statistics_preparation_ms),
            error,
        )
    })?;
    let traffic_after = wait_for_quiet_traffic(context.proxy)
        .await
        .map_err(|error| {
            ModeFailure::new(
                "physical_measurement",
                None,
                &reports,
                Some(statistics_preparation_ms),
                error,
            )
        })?;
    let physical = traffic_after.delta(traffic_before);
    Ok(json!({
        "pair": pair,
        "mode": mode,
        "statistics_preparation_ms": statistics_preparation_ms,
        "queries": reports,
        "physical_run": {
            "s3_uploaded_bytes": physical.uploaded,
            "s3_downloaded_bytes": physical.downloaded
        },
        "statistics": {
            "settled": statistics_settled,
            "snapshot": statistics
        }
    }))
}

async fn run_query(
    server: &RadProcess,
    query: &QueryCase,
    profile: BenchmarkProfile,
) -> TestResult<Value> {
    let (warmup, iterations) = profile.query_counts(query);
    for _ in 0..warmup {
        let response = server.execute(&query.program).await?;
        verify_rows(query, &response)?;
    }
    let mut samples = Vec::with_capacity(iterations);
    let mut result_sha256 = None;
    for _ in 0..iterations {
        let started = Instant::now();
        let response = server.execute(&query.program).await?;
        samples.push(started.elapsed().as_micros() as u64);
        verify_rows(query, &response)?;
        let digest = benchmark::result_hash(&response)?;
        match &result_sha256 {
            Some(expected) if expected != &digest => {
                return Err(format!(
                    "benchmark query {:?} returned different results across measurements",
                    query.name
                )
                .into());
            }
            Some(_) => {}
            None => result_sha256 = Some(digest),
        }
    }
    samples.sort_unstable();
    let plan_started = Instant::now();
    let plan_response = server.execute_with_plan(&query.program).await?;
    let plan_capture_http_us = plan_started.elapsed().as_micros() as u64;
    verify_rows(query, &plan_response)?;
    let plan = plan_response
        .get("plan")
        .filter(|plan| !plan.is_null())
        .ok_or_else(|| format!("benchmark query {:?} returned no plan", query.name))?;
    let statements = plan["statements"]
        .as_array()
        .ok_or_else(|| format!("benchmark query {:?} returned an invalid plan", query.name))?;
    if statements.is_empty()
        || statements
            .iter()
            .any(|statement| statement["measurement"].is_null())
    {
        return Err(format!(
            "benchmark query {:?} returned no executed plan measurement",
            query.name
        )
        .into());
    }
    Ok(json!({
        "name": query.name,
        "description": query.description,
        "warmup": warmup,
        "iterations": iterations,
        "result_rows": query.expect_rows,
        "result_sha256": result_sha256,
        "plan": plan,
        "plan_capture_http_us": plan_capture_http_us,
        "cache_preparation": {
            "mode": "query_warmup",
            "executions": warmup
        },
        "latency_us": {
            "samples": samples,
            "min": samples[0],
            "median": percentile(&samples, 50),
            "p95": percentile(&samples, 95),
            "max": samples[samples.len() - 1]
        }
    }))
}

fn configured_pair_count(profile: BenchmarkProfile) -> TestResult<usize> {
    let pair_count = match env::var("RAD_BENCHMARK_PAIRS") {
        Ok(value) => value
            .parse::<usize>()
            .map_err(|error| format!("RAD_BENCHMARK_PAIRS is invalid: {error}"))?,
        Err(env::VarError::NotPresent) => profile.default_pairs(),
        Err(error) => return Err(error.into()),
    };
    if pair_count == 0 {
        return Err("RAD_BENCHMARK_PAIRS must be positive".into());
    }
    Ok(pair_count)
}

async fn planning_snapshot_identity(server: &RadProcess, probe: &QueryCase) -> TestResult<String> {
    let response = server.plan(&probe.program).await?;
    response["plan"]["statements"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|statement| statement["view"]["statisticsSnapshotIdentity"].as_str())
        .next()
        .map(ToOwned::to_owned)
        .ok_or_else(|| "benchmark probe plan omitted statistics snapshot identity".into())
}

async fn wait_for_statistics_identity(
    server: &RadProcess,
    benchmark: &Manifest,
    probe: &QueryCase,
    expected: &str,
) -> TestResult<u64> {
    let started = Instant::now();
    prepare_statistics(server, benchmark).await?;
    let deadline = started + statistics_timeout(benchmark);
    loop {
        let actual = planning_snapshot_identity(server, probe).await?;
        if actual == expected {
            return Ok(millis(started.elapsed()));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "benchmark {:?} received statistics identity {actual:?}, expected {expected:?}",
                benchmark.name
            )
            .into());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[allow(clippy::too_many_arguments)]
fn write_failure_artifact(
    benchmark: &Manifest,
    fixture_hash: &str,
    profile: BenchmarkProfile,
    pair_count: usize,
    request_timeout_seconds: u64,
    expected_statistics_snapshot_identity: &str,
    replay: &Value,
    completed_pairs: &[Value],
    partial_pair: Value,
) -> TestResult {
    let report = json!({
        "format": benchmark::FAILURE_FORMAT,
        "status": "failed",
        "workload": benchmark.name,
        "description": benchmark.description,
        "fixture_hash": fixture_hash,
        "run": {
            "id": env::var("RAD_BENCHMARK_RUN_ID").ok(),
            "pair_id": env::var("RAD_BENCHMARK_PAIR_ID").ok(),
            "source_revision": env::var("RAD_SOURCE_REVISION").ok(),
            "runner": env::var("RAD_BENCHMARK_RUNNER").ok(),
            "build_profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            "benchmark_profile": profile.name(),
            "request_timeout_seconds": request_timeout_seconds
        },
        "planner": {
            "modes": ["structural", "cost"],
            "pairs_requested": pair_count,
            "pairs_completed": completed_pairs.len(),
            "order": "alternating",
            "statistics_snapshot_identity": expected_statistics_snapshot_identity
        },
        "target": {
            "os": env::consts::OS,
            "architecture": env::consts::ARCH,
            "backend": "s3-rustfs",
            "transport": "http",
            "network": "toxiproxy-transparent"
        },
        "replay": replay,
        "pairs": completed_pairs,
        "partial_pair": partial_pair,
    });
    let encoded = serde_json::to_vec_pretty(&report)?;
    println!("{}", String::from_utf8_lossy(&encoded));
    write_artifact(&benchmark.name, profile, "failure", &encoded)
}

fn write_artifact(
    workload: &str,
    profile: BenchmarkProfile,
    kind: &str,
    encoded: &[u8],
) -> TestResult {
    let Ok(directory) = env::var("RAD_BENCHMARK_ARTIFACT_DIR") else {
        return Ok(());
    };
    std::fs::create_dir_all(&directory)?;
    let profile_suffix = match profile {
        BenchmarkProfile::Full => "",
        BenchmarkProfile::Smoke => "-smoke",
    };
    let kind_suffix = match kind {
        "result" => "",
        "failure" => "-failure",
        value => return Err(format!("unknown benchmark artifact kind {value:?}").into()),
    };
    std::fs::write(
        std::path::Path::new(&directory).join(format!(
            "{workload}-s3-http{profile_suffix}{kind_suffix}.json"
        )),
        encoded,
    )?;
    Ok(())
}

fn paired_confidence(
    pairs: &[Value],
    queries: &[QueryCase],
    replay: &Value,
    acceptance_eligible: bool,
) -> TestResult<Value> {
    let mut query_reports = Vec::with_capacity(queries.len());
    let mut aggregate_ratios = vec![0.0; pairs.len()];
    let mut result_identity = true;
    let mut plan_identity = true;
    let mut statistics_identity = true;
    let mut resource_gate = true;
    let mut bytes_gate = true;
    let mut latency_gate = true;
    let mut structural_planning_samples = Vec::new();
    let mut cost_planning_samples = Vec::new();
    let replay_identity = replay["statisticsSnapshotIdentity"].as_str();

    for query in queries {
        let mut median_ratios = Vec::with_capacity(pairs.len());
        let mut p95_ratios = Vec::with_capacity(pairs.len());
        let mut changed = false;
        let mut logical_pairs = Vec::with_capacity(pairs.len());
        let mut kv_logical_pairs = Vec::with_capacity(pairs.len());
        let mut join_logical_pairs = Vec::with_capacity(pairs.len());
        let mut join_operator_pairs = Vec::with_capacity(pairs.len());
        let mut byte_pairs = Vec::with_capacity(pairs.len());
        let mut planning_pairs = Vec::with_capacity(pairs.len());
        let mut expected_result = None;
        let mut expected_structural_plan = None;
        let mut expected_cost_plan = None;
        for (pair_index, pair) in pairs.iter().enumerate() {
            let structural = paired_query(pair, "structural", &query.name)?;
            let cost = paired_query(pair, "cost", &query.name)?;
            result_identity &= structural["result_sha256"] == cost["result_sha256"];
            if let Some(expected) = &expected_result {
                result_identity &= expected == &structural["result_sha256"];
            } else {
                expected_result = Some(structural["result_sha256"].clone());
            }
            let median_ratio = ratio_f64(
                cost["latency_us"]["median"].as_u64(),
                structural["latency_us"]["median"].as_u64(),
            )?;
            let p95_ratio = ratio_f64(
                cost["latency_us"]["p95"].as_u64(),
                structural["latency_us"]["p95"].as_u64(),
            )?;
            median_ratios.push(median_ratio);
            p95_ratios.push(p95_ratio);
            aggregate_ratios[pair_index] += median_ratio / queries.len() as f64;

            let structural_statement = first_plan_statement(structural)?;
            let cost_statement = first_plan_statement(cost)?;
            if let Some(expected) = &expected_structural_plan {
                plan_identity &= expected == &structural_statement["view"]["fingerprint"];
            } else {
                expected_structural_plan =
                    Some(structural_statement["view"]["fingerprint"].clone());
            }
            if let Some(expected) = &expected_cost_plan {
                plan_identity &= expected == &cost_statement["view"]["fingerprint"];
            } else {
                expected_cost_plan = Some(cost_statement["view"]["fingerprint"].clone());
            }
            let structural_snapshot =
                structural_statement["view"]["statisticsSnapshotIdentity"].as_str();
            let cost_snapshot = cost_statement["view"]["statisticsSnapshotIdentity"].as_str();
            statistics_identity &= structural_snapshot.is_some()
                && structural_snapshot == cost_snapshot
                && structural_snapshot == replay_identity;
            let pair_changed = structural_statement["view"]["fingerprint"]
                != cost_statement["view"]["fingerprint"];
            changed |= pair_changed;
            let structural_kv = &structural_statement["measurement"]["logicalKv"];
            let cost_kv = &cost_statement["measurement"]["logicalKv"];
            let structural_kv_work = logical_row_operations(structural_kv);
            let cost_kv_work = logical_row_operations(cost_kv);
            let structural_joins = &structural_statement["measurement"]["joinOperators"];
            let cost_joins = &cost_statement["measurement"]["joinOperators"];
            let structural_join_work = join_logical_row_operations(structural_joins);
            let cost_join_work = join_logical_row_operations(cost_joins);
            let structural_work = structural_kv_work.saturating_add(structural_join_work);
            let cost_work = cost_kv_work.saturating_add(cost_join_work);
            let structural_bytes = structural_kv["bytesRead"].as_u64().unwrap_or(0);
            let cost_bytes = cost_kv["bytesRead"].as_u64().unwrap_or(0);
            let structural_planning = structural_statement["measurement"]["planningMicros"]
                .as_u64()
                .unwrap_or(0);
            let cost_planning = cost_statement["measurement"]["planningMicros"]
                .as_u64()
                .unwrap_or(0);
            if pair_changed {
                resource_gate &= cost_work < structural_work;
                bytes_gate &= cost_bytes as f64 <= structural_bytes as f64 * 1.05;
            }
            structural_planning_samples.push(structural_planning);
            cost_planning_samples.push(cost_planning);
            logical_pairs.push(json!({"structural": structural_work, "cost": cost_work}));
            kv_logical_pairs.push(json!({"structural": structural_kv_work, "cost": cost_kv_work}));
            join_logical_pairs
                .push(json!({"structural": structural_join_work, "cost": cost_join_work}));
            join_operator_pairs.push(json!({
                "structural": structural_joins,
                "cost": cost_joins
            }));
            byte_pairs.push(json!({"structural": structural_bytes, "cost": cost_bytes}));
            planning_pairs.push(json!({
                "structural_micros": structural_planning,
                "cost_micros": cost_planning
            }));
        }
        let median_ci = bootstrap_interval(&median_ratios);
        let p95_ci = bootstrap_interval(&p95_ratios);
        latency_gate &= median_ci.2 <= 1.10 && p95_ci.2 <= 1.15;
        query_reports.push(json!({
            "name": query.name,
            "plan_changed": changed,
            "median_ratio": confidence_json(median_ci),
            "p95_ratio": confidence_json(p95_ci),
            "logical_row_operations": logical_pairs,
            "kv_logical_row_operations": kv_logical_pairs,
            "join_logical_row_operations": join_logical_pairs,
            "join_operators": join_operator_pairs,
            "bytes_read": byte_pairs,
            "planning": planning_pairs
        }));
    }
    let workload = bootstrap_interval(&aggregate_ratios);
    let workload_gate = workload.2 < 1.0;
    structural_planning_samples.sort_unstable();
    cost_planning_samples.sort_unstable();
    let structural_planning_p95 = percentile(&structural_planning_samples, 95);
    let cost_planning_p95 = percentile(&cost_planning_samples, 95);
    let planning_gate = cost_planning_p95
        <= structural_planning_p95
            .saturating_add(100)
            .max(structural_planning_p95.saturating_mul(120) / 100);
    let replay_has_evidence = replay["planner"]["statementsCompared"]
        .as_u64()
        .is_some_and(|count| count > 0);
    let replay_coverage = replay["planner"]["costEvidenceCoverage"]
        .as_u64()
        .zip(replay["planner"]["structuralEvidenceCoverage"].as_u64())
        .is_some_and(|(cost, structural)| cost >= structural);
    let replay_regret = replay["planner"]["costPredictedWorkloadRegret"]
        .as_u64()
        .zip(replay["planner"]["structuralPredictedWorkloadRegret"].as_u64())
        .is_some_and(|(cost, structural)| cost <= structural);
    Ok(json!({
        "method": "fixed_seed_paired_bootstrap",
        "resamples": 10_000,
        "confidence_level": 0.95,
        "workload_median_ratio": confidence_json(workload),
        "queries": query_reports,
        "planning_p95_micros": {
            "structural": structural_planning_p95,
            "cost": cost_planning_p95
        },
        "gates": {
            "acceptance_protocol": acceptance_eligible,
            "result_identity": result_identity,
            "plan_identity": plan_identity,
            "statistics_identity": statistics_identity,
            "changed_plan_logical_work": resource_gate,
            "changed_plan_bytes": bytes_gate,
            "planning_overhead": planning_gate,
            "per_query_latency": latency_gate,
            "workload_latency": workload_gate,
            "replay_has_evidence": replay_has_evidence,
            "replay_evidence_coverage": replay_coverage,
            "replay_predicted_workload_regret": replay_regret,
            "cost_mode_default": acceptance_eligible
                && result_identity
                && plan_identity
                && statistics_identity
                && resource_gate
                && bytes_gate
                && planning_gate
                && latency_gate
                && workload_gate
                && replay_has_evidence
                && replay_coverage
                && replay_regret
        }
    }))
}

fn paired_query<'a>(pair: &'a Value, mode: &str, query: &str) -> TestResult<&'a Value> {
    pair["modes"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|report| report["mode"] == mode)
        .and_then(|report| report["queries"].as_array())
        .into_iter()
        .flatten()
        .find(|report| report["name"] == query)
        .ok_or_else(|| format!("pair has no {mode} report for query {query:?}").into())
}

fn first_plan_statement(query: &Value) -> TestResult<&Value> {
    query["plan"]["statements"]
        .as_array()
        .and_then(|statements| statements.first())
        .ok_or_else(|| "query report has no plan statement".into())
}

fn logical_row_operations(kv: &Value) -> u64 {
    kv["gets"]
        .as_u64()
        .unwrap_or(0)
        .saturating_add(kv["iterated"].as_u64().unwrap_or(0))
}

fn join_logical_row_operations(operators: &Value) -> u64 {
    operators
        .as_array()
        .into_iter()
        .flatten()
        .map(|operator| {
            [
                "buildRows",
                "probeRows",
                "lookupRequests",
                "keyComparisons",
                "residualPredicateEvaluations",
                "expandedRows",
                "filterRowsScanned",
                "filterInsertions",
                "filterChecks",
            ]
            .into_iter()
            .map(|field| operator[field].as_u64().unwrap_or(0))
            .fold(0u64, u64::saturating_add)
        })
        .fold(0u64, u64::saturating_add)
}

fn ratio_f64(numerator: Option<u64>, denominator: Option<u64>) -> TestResult<f64> {
    let numerator = numerator.ok_or("paired metric has no numerator")?;
    let denominator = denominator
        .filter(|value| *value > 0)
        .ok_or("paired metric has no positive denominator")?;
    Ok(numerator as f64 / denominator as f64)
}

fn bootstrap_interval(samples: &[f64]) -> (f64, f64, f64) {
    let point = median_f64(samples.to_vec());
    let mut state = 0x6a09_e667_f3bc_c909u64;
    let mut resampled = Vec::with_capacity(10_000);
    for _ in 0..10_000 {
        let mut draw = Vec::with_capacity(samples.len());
        for _ in 0..samples.len() {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            draw.push(samples[(state as usize) % samples.len()]);
        }
        resampled.push(median_f64(draw));
    }
    resampled.sort_by(f64::total_cmp);
    (point, resampled[249], resampled[9_749])
}

fn median_f64(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(f64::total_cmp);
    let middle = samples.len() / 2;
    if samples.len().is_multiple_of(2) {
        (samples[middle - 1] + samples[middle]) / 2.0
    } else {
        samples[middle]
    }
}

fn confidence_json((estimate, lower, upper): (f64, f64, f64)) -> Value {
    json!({"estimate": estimate, "lower": lower, "upper": upper})
}

#[test]
fn paired_bootstrap_is_deterministic_and_contains_the_point_estimate() {
    let samples = [0.80, 0.90, 1.00, 1.10, 1.20];
    let first = bootstrap_interval(&samples);
    let second = bootstrap_interval(&samples);
    assert_eq!(first, second);
    assert_eq!(first.0, 1.0);
    assert!(first.1 <= first.0);
    assert!(first.2 >= first.0);
}

#[test]
fn benchmark_profiles_have_explicit_acceptance_and_smoke_controls() {
    assert_eq!(
        BenchmarkProfile::parse(None).unwrap(),
        BenchmarkProfile::Full
    );
    assert_eq!(
        BenchmarkProfile::parse(Some("full")).unwrap(),
        BenchmarkProfile::Full
    );
    assert_eq!(
        BenchmarkProfile::parse(Some("smoke")).unwrap(),
        BenchmarkProfile::Smoke
    );
    assert_eq!(BenchmarkProfile::Full.default_pairs(), 12);
    assert_eq!(BenchmarkProfile::Smoke.default_pairs(), 2);
    assert!(BenchmarkProfile::parse(Some("short")).is_err());
}

#[test]
fn mode_failure_keeps_the_failed_query_and_completed_reports() {
    let completed = vec![json!({"name": "complete"})];
    let failure = ModeFailure::new(
        "query_execution",
        Some("failed"),
        &completed,
        Some(25),
        "request timeout",
    );
    let view = failure.view(3, "cost");
    assert_eq!(view["pair"], 3);
    assert_eq!(view["mode"], "cost");
    assert_eq!(view["query"], "failed");
    assert_eq!(view["completed_queries"], json!(completed));
}

async fn prepare_statistics(server: &RadProcess, benchmark: &Manifest) -> TestResult<u64> {
    let started = Instant::now();
    let deadline = started + statistics_timeout(benchmark);
    loop {
        let statistics = server.statistics().await?;
        let synopses = statistics["synopses"].as_array().map_or(0, Vec::len);
        let models = statistics["models"].as_array().map_or(0, Vec::len);
        if models > 0 && synopses >= benchmark.statistics.required_synopses {
            return Ok(millis(started.elapsed()));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "benchmark {:?} received {models} statistics models and {synopses} synopses; expected at least one model and {} synopses",
                benchmark.name, benchmark.statistics.required_synopses,
            )
            .into());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn statistics_timeout(benchmark: &Manifest) -> Duration {
    Duration::from_secs(match benchmark.statistics.timeout_seconds {
        0 => 180,
        configured => configured,
    })
}

async fn settled_statistics(server: &RadProcess) -> TestResult<(Value, bool)> {
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut previous = None;
    let mut unchanged_since = Instant::now();
    loop {
        let statistics = server.statistics().await?;
        let marker = statistics_marker(&statistics);
        if previous.as_ref() == Some(&marker) {
            if unchanged_since.elapsed() >= Duration::from_millis(1_250) {
                return Ok((statistics, true));
            }
        } else {
            previous = Some(marker);
            unchanged_since = Instant::now();
        }
        if Instant::now() >= deadline {
            return Ok((statistics, false));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn statistics_marker(statistics: &Value) -> (u64, u64) {
    let absorbed = statistics["absorbed"].as_u64().unwrap_or(0);
    let executions = statistics["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|model| model["retainedExecutions"].as_u64())
        .fold(0_u64, u64::saturating_add);
    (absorbed, executions)
}

fn verify_rows(query: &QueryCase, response: &Value) -> TestResult {
    let actual = benchmark::result_rows(response);
    if actual != query.expect_rows {
        return Err(format!(
            "benchmark query {:?} returned {actual} rows, expected {}: {response}",
            query.name, query.expect_rows
        )
        .into());
    }
    if let Some(expected) = &query.expect_result
        && &response["result"] != expected
    {
        return Err(format!(
            "benchmark query {:?} returned the wrong result: expected {expected}, got {}",
            query.name, response["result"]
        )
        .into());
    }
    if let Some(expected) = &query.expect_first
        && response["result"].as_array().and_then(|rows| rows.first()) != Some(expected)
    {
        return Err(format!(
            "benchmark query {:?} returned the wrong first row: expected {expected}, got {}",
            query.name, response["result"]
        )
        .into());
    }
    if let Some(expected) = &query.expect_last
        && response["result"].as_array().and_then(|rows| rows.last()) != Some(expected)
    {
        return Err(format!(
            "benchmark query {:?} returned the wrong last row: expected {expected}, got {}",
            query.name, response["result"]
        )
        .into());
    }
    Ok(())
}

async fn wait_for_quiet_traffic(proxy: &ToxiProxy) -> TestResult<Traffic> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut previous = traffic(&proxy.metrics().await?, &proxy.name)?;
    let mut unchanged = 0;
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let current = traffic(&proxy.metrics().await?, &proxy.name)?;
        if current.uploaded == previous.uploaded && current.downloaded == previous.downloaded {
            unchanged += 1;
            if unchanged == 3 {
                return Ok(current);
            }
        } else {
            unchanged = 0;
            previous = current;
        }
    }
    Ok(previous)
}

fn traffic(metrics: &str, proxy: &str) -> TestResult<Traffic> {
    let mut result = Traffic::default();
    for line in metrics.lines() {
        let (name_and_labels, value) = match line.split_once(' ') {
            Some(parts) if !line.starts_with('#') => parts,
            _ => continue,
        };
        if !name_and_labels.contains(&format!("proxy=\"{proxy}\"")) {
            continue;
        }
        let parsed = value.trim().parse::<f64>()? as u64;
        if name_and_labels.starts_with("toxiproxy_proxy_received_bytes_total")
            && name_and_labels.contains("direction=\"upstream\"")
        {
            result.uploaded = result.uploaded.saturating_add(parsed);
        }
        if name_and_labels.starts_with("toxiproxy_proxy_received_bytes_total")
            && name_and_labels.contains("direction=\"downstream\"")
        {
            result.downloaded = result.downloaded.saturating_add(parsed);
        }
    }
    Ok(result)
}

fn percentile(samples: &[u64], percentile: usize) -> u64 {
    let index = (samples.len() - 1) * percentile / 100;
    samples[index]
}

fn ratio(numerator: u64, denominator: usize) -> f64 {
    numerator as f64 / denominator as f64
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis() as u64
}

#[test]
fn prometheus_traffic_parser_selects_the_named_proxy_and_direction() {
    let metrics = r#"
# HELP toxiproxy_proxy_received_bytes_total bytes
toxiproxy_proxy_received_bytes_total{direction="upstream",listener="x",proxy="rustfs-benchmark",upstream="s3"} 1200
toxiproxy_proxy_received_bytes_total{direction="downstream",listener="x",proxy="rustfs-benchmark",upstream="s3"} 3400
toxiproxy_proxy_received_bytes_total{direction="upstream",listener="x",proxy="other",upstream="s3"} 9999
"#;
    let parsed = traffic(metrics, "rustfs-benchmark").unwrap();
    assert_eq!(parsed.uploaded, 1200);
    assert_eq!(parsed.downloaded, 3400);
}

#[test]
fn statistics_marker_tracks_absorbed_observations_and_retained_executions() {
    let marker = statistics_marker(&json!({
        "absorbed": 7,
        "models": [
            {"retainedExecutions": 3},
            {"retainedExecutions": 5},
            {"kind": "unretained"}
        ]
    }));
    assert_eq!(marker, (7, 8));
}

#[test]
fn access_paths_fixture_has_locked_distributions_and_valid_programs() {
    let benchmark = access_paths::load_benchmark().unwrap();
    assert_eq!(benchmark.name, "access-paths");
    assert_eq!(benchmark.batch_rows, 500);
    assert_eq!(access_paths::rows(&benchmark).unwrap(), 1_000);

    let schema = access_paths::schema(&benchmark).unwrap();
    let parsed = rad::engine::catalog::schema::parse("schema.yaml", schema.as_bytes()).unwrap();
    assert_eq!(parsed.tables.len(), 1);
    assert_eq!(parsed.tables[0].def.columns.len(), 9);
    assert_eq!(parsed.tables[0].def.indexes.len(), 4);

    let tables = access_paths::dataset(1_000);
    let rows = &tables[0].rows;
    assert_eq!(rows.len(), 1_000);
    assert_eq!(
        rows.iter().filter(|row| row[2] == json!("hot")).count(),
        700
    );
    assert_eq!(
        rows.iter().filter(|row| row[2] == json!("needle")).count(),
        1
    );
    assert_eq!(rows.iter().filter(|row| row[5].is_null()).count(), 800);
    assert!(rows.iter().all(|row| {
        (row[3] == json!("north") && row[4] == json!("consumer"))
            || (row[3] == json!("south") && row[4] == json!("business"))
    }));

    let queries = access_paths::queries(&benchmark).unwrap();
    assert_eq!(queries.len(), 18);
    for query in queries {
        assert!(query.iterations > 0, "{} has no measurements", query.name);
        serde_json::from_value::<rad::protocol::generated::pir::Program>(query.program)
            .unwrap_or_else(|error| panic!("{} is not valid PIR: {error}", query.name));
    }
    assert_eq!(
        access_paths::fixture_hash(&benchmark).unwrap(),
        "c686761bda17ed0ae5162104b492d9b5bd7087668a8039c3322e975e6886a43e"
    );
}

#[test]
fn commerce_fixture_has_the_locked_schema_counts_and_program_shapes() {
    let benchmark = commerce::load_benchmark().unwrap();
    assert_eq!(benchmark.name, "commerce");
    assert_eq!(benchmark.batch_rows, 200);
    assert_eq!(
        commerce::counts(&benchmark).unwrap(),
        commerce::Counts {
            customers: 200,
            products: 100,
            orders: 1000,
            order_items: 3000,
        }
    );
    let schema = commerce::schema(&benchmark).unwrap();
    let parsed = rad::engine::catalog::schema::parse("schema.yaml", schema.as_bytes()).unwrap();
    assert_eq!(parsed.tables.len(), 4);
    let queries = commerce::queries(&benchmark).unwrap();
    assert_eq!(queries.len(), 5);
    for query in queries {
        assert!(query.iterations > 0, "{} has no measurements", query.name);
        serde_json::from_value::<rad::protocol::generated::pir::Program>(query.program)
            .unwrap_or_else(|error| panic!("{} is not valid PIR: {error}", query.name));
    }
    assert_eq!(
        commerce::fixture_hash(&benchmark).unwrap(),
        "f500326003a6b7fdddc6a7d2890b68b30784e8c006bf4a2b631a7c781495a5f3"
    );
}

#[test]
fn generated_threads_preserve_both_backreference_invariants() {
    let shape = DatasetShape {
        balanced_threads: 2,
        balanced_depth: 3,
        deep_replies: 5,
        wide_replies: 7,
    };
    let tables = threaded_posts::dataset(shape);
    assert_eq!(tables[0].columns[1]["nullable"], true);
    assert_eq!(tables[0].columns[2]["nullable"], true);
    let rows = &tables[0].rows;
    assert_eq!(rows.len(), shape.rows());

    let ids = rows
        .iter()
        .map(|row| row[0].as_str().unwrap())
        .collect::<std::collections::HashSet<_>>();
    for row in rows {
        match (&row[1], &row[2]) {
            (Value::Null, Value::Null) => {}
            (Value::String(root), Value::String(parent)) => {
                assert!(ids.contains(root.as_str()), "missing root {root}");
                assert!(ids.contains(parent.as_str()), "missing parent {parent}");
            }
            backreferences => panic!("partial backreferences: {backreferences:?}"),
        }
    }
}

#[test]
fn threaded_posts_fixture_has_locked_shapes_and_valid_recursive_programs() {
    let benchmark = threaded_posts::load_benchmark().unwrap();
    assert_eq!(benchmark.name, "threaded-posts");
    assert_eq!(benchmark.batch_rows, 200);
    assert_eq!(
        threaded_posts::shape(&benchmark).unwrap(),
        DatasetShape {
            balanced_threads: 12,
            balanced_depth: 6,
            deep_replies: 128,
            wide_replies: 1024,
        }
    );
    let schema = threaded_posts::schema(&benchmark).unwrap();
    let parsed = rad::engine::catalog::schema::parse("schema.yaml", schema.as_bytes()).unwrap();
    assert_eq!(parsed.tables.len(), 1);
    assert_eq!(parsed.tables[0].def.columns.len(), 7);
    let queries = threaded_posts::queries(&benchmark).unwrap();
    assert_eq!(queries.len(), 4);
    for query in queries {
        assert!(query.iterations > 0, "{} has no measurements", query.name);
        serde_json::from_value::<rad::protocol::generated::pir::Program>(query.program)
            .unwrap_or_else(|error| panic!("{} is not valid PIR: {error}", query.name));
    }
    assert_eq!(
        threaded_posts::fixture_hash(&benchmark).unwrap(),
        "1b16c0ce03e6f5adbe2cde490b65181675e0e970c668692a1a4b0f262accb1ad"
    );
}
