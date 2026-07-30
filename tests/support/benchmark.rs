#![allow(dead_code)]

use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::http_process::RadProcess;
use super::s3::TestResult;

pub const FORMAT: &str = "rad-s3-http-benchmark-v1";

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub name: String,
    pub description: String,
    pub schema: String,
    pub batch_rows: usize,
    pub tables: Vec<TableCount>,
    pub queries: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct TableCount {
    pub name: String,
    pub rows: usize,
}

#[derive(Clone, Debug, Deserialize)]
pub struct QueryCase {
    pub name: String,
    pub description: String,
    pub warmup: usize,
    pub iterations: usize,
    pub expect_rows: usize,
    #[serde(default)]
    pub expect_result: Option<Value>,
    #[serde(default)]
    pub expect_first: Option<Value>,
    #[serde(default)]
    pub expect_last: Option<Value>,
    pub program: Value,
}

pub struct LoadStats {
    pub rows: usize,
    pub batches: usize,
    pub logical_bytes: usize,
}

pub struct TableData {
    pub name: &'static str,
    pub columns: Vec<Value>,
    pub rows: Vec<Vec<Value>>,
}

pub fn load_manifest(root: &Path) -> TestResult<Manifest> {
    let source = std::fs::read(root.join("benchmark.yaml"))?;
    let manifest: Manifest = serde_yaml::from_slice(&source)?;
    if manifest.format != FORMAT {
        return Err(format!("unknown benchmark format {:?}", manifest.format).into());
    }
    if manifest.batch_rows == 0 {
        return Err("benchmark batch_rows must be positive".into());
    }
    Ok(manifest)
}

pub fn schema(root: &Path, manifest: &Manifest) -> TestResult<String> {
    Ok(std::fs::read_to_string(root.join(&manifest.schema))?)
}

pub fn queries(root: &Path, manifest: &Manifest) -> TestResult<Vec<QueryCase>> {
    manifest
        .queries
        .iter()
        .map(|path| Ok(serde_json::from_slice(&std::fs::read(root.join(path))?)?))
        .collect()
}

pub fn fixture_hash(
    root: &Path,
    generator_version: &str,
    manifest: &Manifest,
) -> TestResult<String> {
    let mut hasher = Sha256::new();
    hasher.update(generator_version);
    hasher.update(std::fs::read(root.join("benchmark.yaml"))?);
    hasher.update(std::fs::read(root.join(&manifest.schema))?);
    for query in &manifest.queries {
        hasher.update(std::fs::read(root.join(query))?);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub async fn load_tables(
    server: &RadProcess,
    tables: Vec<TableData>,
    batch_rows: usize,
) -> TestResult<LoadStats> {
    let mut stats = LoadStats {
        rows: 0,
        batches: 0,
        logical_bytes: 0,
    };
    for table in tables {
        for (batch, rows) in table.rows.chunks(batch_rows).enumerate() {
            let rows = rows.to_vec();
            stats.logical_bytes += serde_json::to_vec(&rows)?.len();
            stats.rows += rows.len();
            stats.batches += 1;
            let response = server
                .execute(&json!({
                    "statements": [{
                        "name": format!("load-{}-{batch}", table.name),
                        "kind": "create",
                        "table": table.name,
                        "relation": {
                            "nodes": {
                                "rows": {
                                    "kind": "rows",
                                    "scope": "input",
                                    "columns": table.columns,
                                    "rows": rows
                                }
                            },
                            "root": { "node": "rows", "cardinality": "many" }
                        }
                    }]
                }))
                .await?;
            let affected = response["statements"][0]["affected"]
                .as_u64()
                .ok_or("load response omitted affected row count")?;
            if affected != rows.len() as u64 {
                return Err(format!(
                    "load of {} affected {affected} rows, expected {}",
                    table.name,
                    rows.len()
                )
                .into());
            }
        }
    }
    Ok(stats)
}

pub fn result_rows(response: &Value) -> usize {
    match &response["result"] {
        Value::Array(rows) => rows.len(),
        Value::Null => 0,
        _ => 1,
    }
}
