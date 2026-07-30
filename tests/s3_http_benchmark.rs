use std::env;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

mod support;

use support::benchmark::{self, Manifest, QueryCase};
use support::commerce;
use support::http_process::RadProcess;
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

enum Dataset {
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
    let server = RadProcess::start_s3(&rustfs.config, &proxy.endpoint, &prefix).await?;
    let reopen_ms = millis(reopen_started.elapsed());
    let mut query_reports = Vec::new();
    for query in queries {
        query_reports.push(run_query(&server, &query).await?);
    }
    server.stop().await?;

    let write_seconds = durable_write_elapsed.as_secs_f64();
    let report = json!({
        "format": "rad-s3-http-benchmark-result-v1",
        "workload": benchmark.name,
        "description": benchmark.description,
        "fixture_hash": fixture_hash,
        "source_revision": env::var("RAD_SOURCE_REVISION").ok(),
        "target": {
            "os": env::consts::OS,
            "architecture": env::consts::ARCH,
            "backend": "s3-rustfs",
            "transport": "http",
            "network": "toxiproxy-transparent"
        },
        "schema_migration_ms": migration_ms,
        "reopen_ms": reopen_ms,
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
        "queries": query_reports
    });
    let encoded = serde_json::to_vec_pretty(&report)?;
    println!("{}", String::from_utf8_lossy(&encoded));
    if let Ok(directory) = env::var("RAD_BENCHMARK_ARTIFACT_DIR") {
        std::fs::create_dir_all(&directory)?;
        std::fs::write(
            std::path::Path::new(&directory).join(format!("{}-s3-http.json", benchmark.name)),
            encoded,
        )?;
    }
    Ok(())
}

async fn run_query(server: &RadProcess, query: &QueryCase) -> TestResult<Value> {
    for _ in 0..query.warmup {
        let response = server.execute(&query.program).await?;
        verify_rows(query, &response)?;
    }
    let mut samples = Vec::with_capacity(query.iterations);
    for _ in 0..query.iterations {
        let started = Instant::now();
        let response = server.execute(&query.program).await?;
        samples.push(started.elapsed().as_micros() as u64);
        verify_rows(query, &response)?;
    }
    samples.sort_unstable();
    Ok(json!({
        "name": query.name,
        "description": query.description,
        "warmup": query.warmup,
        "iterations": query.iterations,
        "result_rows": query.expect_rows,
        "latency_us": {
            "min": samples[0],
            "median": percentile(&samples, 50),
            "p95": percentile(&samples, 95),
            "max": samples[samples.len() - 1]
        }
    }))
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
    assert_eq!(queries.len(), 4);
    for query in queries {
        assert!(query.iterations > 0, "{} has no measurements", query.name);
        serde_json::from_value::<rad::protocol::generated::pir::Program>(query.program)
            .unwrap_or_else(|error| panic!("{} is not valid PIR: {error}", query.name));
    }
    assert_eq!(
        commerce::fixture_hash(&benchmark).unwrap(),
        "8bcf3d742e965e3d4400355da578efacd6daa29d1d738e7c1cba8ab7dfb617f0"
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
