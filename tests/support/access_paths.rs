#![allow(dead_code)]

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::benchmark::{self, LoadStats, Manifest, QueryCase, TableData};
use super::http_process::RadProcess;
use super::s3::TestResult;

pub const FIXTURE_ROOT: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/benchmarks/access_paths");
const GENERATOR_VERSION: &str = "access-paths-dataset-v1";

pub fn load_benchmark() -> TestResult<Manifest> {
    benchmark::load_manifest(&root())
}

pub fn rows(manifest: &Manifest) -> TestResult<usize> {
    let table = manifest
        .tables
        .iter()
        .find(|table| table.name == "events")
        .ok_or("access-paths benchmark omitted events table")?;
    if table.rows < 100 {
        return Err("access-paths dataset requires at least 100 events".into());
    }
    Ok(table.rows)
}

pub fn schema(manifest: &Manifest) -> TestResult<String> {
    benchmark::schema(&root(), manifest)
}

pub fn queries(manifest: &Manifest) -> TestResult<Vec<QueryCase>> {
    benchmark::queries(&root(), manifest)
}

pub fn fixture_hash(manifest: &Manifest) -> TestResult<String> {
    benchmark::fixture_hash(&root(), GENERATOR_VERSION, manifest)
}

pub async fn load_dataset(
    server: &RadProcess,
    rows: usize,
    batch_rows: usize,
) -> TestResult<LoadStats> {
    benchmark::load_tables(server, dataset(rows), batch_rows).await
}

pub(crate) fn dataset(row_count: usize) -> Vec<TableData> {
    let hot_end = row_count * 70 / 100;
    let warm_end = row_count * 90 / 100;
    let cold_end = row_count * 99 / 100;
    let rows = (0..row_count)
        .map(|index| {
            let tenant = index % 100;
            let category = if index + 1 == row_count {
                "needle"
            } else if index < hot_end {
                "hot"
            } else if index < warm_end {
                "warm"
            } else if index < cold_end {
                "cold"
            } else {
                "rare"
            };
            let north = tenant < 50;
            vec![
                json!(format!("event-{index:06}")),
                json!(format!("tenant-{tenant:03}")),
                json!(category),
                json!(if north { "north" } else { "south" }),
                json!(if north { "consumer" } else { "business" }),
                if index % 5 == 0 {
                    json!(format!("tag-{:02}", index % 20))
                } else {
                    Value::Null
                },
                json!((index % 1_000).to_string()),
                json!((1_700_000_000_u64 + index as u64).to_string()),
                json!(format!("payload-{}", "x".repeat(index % 64))),
            ]
        })
        .collect();

    vec![TableData {
        name: "events",
        columns: vec![
            column("event_id", "text"),
            column("tenant_id", "text"),
            column("category", "text"),
            column("region", "text"),
            column("segment", "text"),
            nullable_column("optional_tag", "text"),
            column("score", "int64"),
            column("created_at", "int64"),
            column("payload", "text"),
        ],
        rows,
    }]
}

fn column(name: &str, data_type: &str) -> Value {
    json!({ "name": name, "type": data_type })
}

fn nullable_column(name: &str, data_type: &str) -> Value {
    json!({ "name": name, "type": data_type, "nullable": true })
}

fn root() -> PathBuf {
    Path::new(FIXTURE_ROOT).to_owned()
}
