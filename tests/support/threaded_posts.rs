#![allow(dead_code)]

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use super::benchmark::{self, LoadStats, Manifest, QueryCase, TableData};
use super::http_process::RadProcess;
use super::s3::TestResult;

pub const FIXTURE_ROOT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/benchmarks/threaded_posts"
);
const GENERATOR_VERSION: &str = "threaded-posts-dataset-v1";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub struct DatasetShape {
    pub balanced_threads: usize,
    pub balanced_depth: usize,
    pub deep_replies: usize,
    pub wide_replies: usize,
}

#[derive(Deserialize)]
struct ManifestWithDataset {
    dataset: DatasetShape,
}

impl DatasetShape {
    pub fn rows(self) -> usize {
        self.balanced_threads * ((1_usize << (self.balanced_depth + 1)) - 1)
            + self.deep_replies
            + self.wide_replies
            + 2
    }

    fn validate(self) -> TestResult {
        if self.balanced_threads == 0 {
            return Err("threaded-posts dataset requires a balanced thread".into());
        }
        if self.balanced_depth >= usize::BITS as usize - 1 {
            return Err("threaded-posts balanced_depth is too large".into());
        }
        if self.deep_replies == 0 || self.wide_replies == 0 {
            return Err("threaded-posts dataset requires deep and wide replies".into());
        }
        Ok(())
    }
}

pub fn load_benchmark() -> TestResult<Manifest> {
    benchmark::load_manifest(&root())
}

pub fn shape(manifest: &Manifest) -> TestResult<DatasetShape> {
    let source = std::fs::read(root().join("benchmark.yaml"))?;
    let shape = serde_yaml::from_slice::<ManifestWithDataset>(&source)?.dataset;
    shape.validate()?;
    let declared = manifest
        .tables
        .iter()
        .find(|table| table.name == "posts")
        .map(|table| table.rows)
        .ok_or("threaded-posts benchmark omitted posts table")?;
    if declared != shape.rows() {
        return Err(format!(
            "threaded-posts manifest declares {declared} rows but its shape generates {}",
            shape.rows()
        )
        .into());
    }
    Ok(shape)
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
    shape: DatasetShape,
    batch_rows: usize,
) -> TestResult<LoadStats> {
    shape.validate()?;
    benchmark::load_tables(server, dataset(shape), batch_rows).await
}

pub(crate) fn dataset(shape: DatasetShape) -> Vec<TableData> {
    let mut rows = Vec::with_capacity(shape.rows());
    let mut sequence = 0_u64;

    for thread in 0..shape.balanced_threads {
        let nodes = (1_usize << (shape.balanced_depth + 1)) - 1;
        let root = format!("balanced-{thread:02}-0000");
        for node in 0..nodes {
            let id = format!("balanced-{thread:02}-{node:04}");
            let parent = (node > 0).then(|| format!("balanced-{thread:02}-{:04}", (node - 1) / 2));
            push_post(&mut rows, &mut sequence, id, &root, parent);
        }
    }

    let deep_root = "deep-0000".to_owned();
    for node in 0..=shape.deep_replies {
        let id = format!("deep-{node:04}");
        let parent = (node > 0).then(|| format!("deep-{:04}", node - 1));
        push_post(&mut rows, &mut sequence, id, &deep_root, parent);
    }

    let wide_root = "wide-0000".to_owned();
    for node in 0..=shape.wide_replies {
        let id = format!("wide-{node:04}");
        let parent = (node > 0).then(|| wide_root.clone());
        push_post(&mut rows, &mut sequence, id, &wide_root, parent);
    }

    vec![TableData {
        name: "posts",
        columns: vec![
            column("post_id", "text"),
            nullable_column("root_post_id", "text"),
            nullable_column("reply_to_post_id", "text"),
            column("author_id", "text"),
            column("post_body", "text"),
            column("post_score", "int64"),
            column("post_created_at", "int64"),
        ],
        rows,
    }]
}

fn push_post(
    rows: &mut Vec<Vec<Value>>,
    sequence: &mut u64,
    id: String,
    root: &str,
    parent: Option<String>,
) {
    let is_root = parent.is_none();
    rows.push(vec![
        json!(id),
        if is_root { Value::Null } else { json!(root) },
        parent.map_or(Value::Null, Value::String),
        json!(format!("author-{:04}", *sequence % 257)),
        json!(format!("deterministic benchmark post {sequence}")),
        json!(((*sequence * 37) % 10_000).to_string()),
        json!((1_700_000_000_u64 + *sequence).to_string()),
    ]);
    *sequence += 1;
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
