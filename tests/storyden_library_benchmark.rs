use std::borrow::Cow;
use std::env;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client as PostgresClient, NoTls, Row, Statement};

mod support;

use support::http_process::{reserve_extra_port, reserve_port_pair};
use support::s3::TestResult;

const NODE_PROPERTIES_SQL: &str =
    include_str!("benchmarks/storyden_library/queries/node_properties.sql");
const PROPERTY_SORT_SQL: &str =
    include_str!("benchmarks/storyden_library/queries/property_sort.sql");
const SUBTREE_SQL: &str = include_str!("benchmarks/storyden_library/queries/subtree.sql");
const PROPERTY_SORT_CHILDREN: usize = 12;
const SUBTREE_ROWS: usize = 49;

#[derive(Clone, Copy)]
enum Workload {
    NodeProperties,
    PropertySort,
    Subtree,
}

impl Workload {
    const ALL: [Self; 3] = [Self::NodeProperties, Self::PropertySort, Self::Subtree];

    fn name(self) -> &'static str {
        match self {
            Self::NodeProperties => "node_properties",
            Self::PropertySort => "property_sort",
            Self::Subtree => "subtree",
        }
    }

    fn sql(self, scenario: &Scenario) -> Cow<'static, str> {
        match self {
            Self::NodeProperties => Cow::Borrowed(NODE_PROPERTIES_SQL),
            Self::PropertySort => {
                Cow::Owned(PROPERTY_SORT_SQL.replace("{{IDS}}", &scenario.quoted_sort_child_ids()))
            }
            Self::Subtree => Cow::Borrowed(SUBTREE_SQL),
        }
    }

    fn parameters(self, scenario: &Scenario) -> Vec<String> {
        match self {
            Self::NodeProperties => vec![scenario.root_id(), scenario.root_id()],
            Self::PropertySort => vec!["score".to_owned()],
            Self::Subtree => vec![
                scenario.root_id(),
                "account-owner".to_owned(),
                "published".to_owned(),
                "review".to_owned(),
            ],
        }
    }

    fn expected_rows(self) -> usize {
        match self {
            Self::NodeProperties => 4,
            Self::PropertySort => PROPERTY_SORT_CHILDREN,
            Self::Subtree => SUBTREE_ROWS,
        }
    }
}

struct Config {
    warmup: usize,
    iterations: usize,
    workloads: Vec<Workload>,
}

impl Config {
    fn load() -> TestResult<Self> {
        let warmup = positive_usize("RAD_STORYDEN_WARMUP", 8)?;
        let iterations = positive_usize("RAD_STORYDEN_ITERATIONS", 16)?;
        let configured_workload = env::var("RAD_STORYDEN_QUERY");
        let workloads = match configured_workload.as_deref() {
            Ok("node_properties") => vec![Workload::NodeProperties],
            Ok("property_sort") => vec![Workload::PropertySort],
            Ok("subtree") => vec![Workload::Subtree],
            Ok("all") | Err(env::VarError::NotPresent) => Workload::ALL.to_vec(),
            Ok(value) => {
                return Err(format!(
                    "RAD_STORYDEN_QUERY must be all, node_properties, property_sort, or subtree; got {value:?}"
                )
                .into());
            }
            Err(error) => return Err(error.to_string().into()),
        };
        Ok(Self {
            warmup,
            iterations,
            workloads,
        })
    }

    fn scenario_count(&self) -> usize {
        (self.warmup + self.iterations) * 2
    }
}

#[derive(Clone, Copy)]
struct Scenario {
    index: usize,
}

impl Scenario {
    fn root_id(self) -> String {
        format!("node-{:05}-root", self.index)
    }

    fn parent_id(self) -> String {
        format!("node-{:05}-parent", self.index)
    }

    fn sort_child_id(self, child: usize) -> String {
        format!("node-{:05}-child-{child:02}", self.index)
    }

    fn level_two_id(self, branch: usize, child: usize) -> String {
        format!("node-{:05}-level-two-{branch:02}-{child:02}", self.index)
    }

    fn level_three_id(self, branch: usize, child: usize, leaf: usize) -> String {
        format!(
            "node-{:05}-level-three-{branch:02}-{child:02}-{leaf:02}",
            self.index
        )
    }

    fn quoted_sort_child_ids(self) -> String {
        (0..PROPERTY_SORT_CHILDREN)
            .map(|child| quoted(&self.sort_child_id(child)))
            .collect::<Vec<_>>()
            .join(",")
    }
}

struct BenchmarkTarget {
    child: Option<Child>,
    database_url: String,
    _directory: Option<tempfile::TempDir>,
}

impl BenchmarkTarget {
    async fn start() -> TestResult<Self> {
        if let Ok(database_url) = env::var("RAD_STORYDEN_DATABASE_URL") {
            return Ok(Self {
                child: None,
                database_url,
                _directory: None,
            });
        }

        let directory = tempfile::tempdir()?;
        let (http_port, public, admin) = reserve_port_pair()?;
        let (postgres_port, postgres) = reserve_extra_port()?;
        drop((public, admin, postgres));
        let child = Command::new(env!("CARGO_BIN_EXE_rad"))
            .args([
                "serve",
                "--addr",
                &format!("127.0.0.1:{http_port}"),
                "--storage",
                "file",
                "--storage-path",
            ])
            .arg(directory.path().join("storyden-library"))
            .args([
                "--catalog-mode",
                "direct",
                "--frontend",
                "postgres",
                "--postgres-addr",
                &format!("127.0.0.1:{postgres_port}"),
                "--slate-commit-durability",
                "memory",
                "--metrics",
                "false",
                "--diagnostics",
                "off",
                "--log-level",
                "error",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        let mut target = Self {
            child: Some(child),
            database_url: format!(
                "postgresql://rad@127.0.0.1:{postgres_port}/postgres?sslmode=disable"
            ),
            _directory: Some(directory),
        };
        target.wait_until_ready(http_port).await?;
        Ok(target)
    }

    async fn wait_until_ready(&mut self, http_port: u16) -> TestResult {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(child) = self.child.as_mut()
                && let Some(status) = child.try_wait()?
            {
                return Err(format!("Rad exited before readiness: {status}").into());
            }
            if client
                .get(format!("http://127.0.0.1:{http_port}/healthz"))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("Rad did not become ready".into());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn stop(&mut self) -> TestResult {
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        let expect_success = terminate(child)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = child.try_wait()? {
                self.child = None;
                return if status.success() || !expect_success {
                    Ok(())
                } else {
                    Err(format!("Rad exited unsuccessfully: {status}").into())
                };
            }
            if Instant::now() >= deadline {
                return Err("Rad did not stop".into());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

impl Drop for BenchmarkTarget {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[tokio::test]
#[ignore = "emits release measurements and does not enforce timing thresholds"]
async fn storyden_library_records_file_postgres_measurements() -> TestResult {
    if cfg!(debug_assertions) {
        return Err("run the Storyden library benchmark with --release".into());
    }

    let config = Config::load()?;
    let scenarios = (0..config.scenario_count())
        .map(|index| Scenario { index })
        .collect::<Vec<_>>();
    let mut target = BenchmarkTarget::start().await?;
    let (client, connection) = tokio_postgres::connect(&target.database_url, NoTls).await?;
    let connection = tokio::spawn(connection);
    create_schema(&client).await?;
    load_fixture(&client, &scenarios).await?;

    let mut queries = Vec::with_capacity(config.workloads.len());
    for workload in config.workloads.iter().copied() {
        queries.push(
            measure_workload(
                &client,
                workload,
                &scenarios,
                config.warmup,
                config.iterations,
            )
            .await?,
        );
    }

    let report = json!({
        "format": "rad.storyden-library-benchmark.v1",
        "workload": "storyden-library",
        "run": {
            "build_profile": "release",
            "warmup": config.warmup,
            "iterations": config.iterations,
            "source_revision": env::var("RAD_SOURCE_REVISION").ok()
        },
        "target": {
            "os": env::consts::OS,
            "architecture": env::consts::ARCH,
            "backend": "file",
            "transport": "postgres",
            "commit_durability": "memory",
            "metrics": false,
            "diagnostics": "off"
        },
        "cache_preparation": {
            "mode": "distinct_fixture_root_per_phase",
            "scenario_count": scenarios.len()
        },
        "fixture": {
            "accounts": 1,
            "property_schemas": 1,
            "property_schema_fields": 2,
            "nodes": scenarios.len() * 53,
            "properties": scenarios.len() * PROPERTY_SORT_CHILDREN * 2
        },
        "queries": queries
    });
    let encoded = serde_json::to_vec_pretty(&report)?;
    println!("{}", String::from_utf8_lossy(&encoded));
    write_artifact(&encoded)?;

    drop(client);
    connection.await??;
    target.stop().await
}

async fn measure_workload(
    client: &PostgresClient,
    workload: Workload,
    scenarios: &[Scenario],
    warmup: usize,
    iterations: usize,
) -> TestResult<Value> {
    let prepare = measure_prepare(client, workload, scenarios, warmup, iterations).await?;
    let execute = measure_execute(client, workload, scenarios, warmup, iterations).await?;
    let full_wire_offset = warmup + iterations;
    let full_wire = measure_full_wire(
        client,
        workload,
        &scenarios[full_wire_offset..],
        warmup,
        iterations,
    )
    .await?;
    Ok(json!({
        "name": workload.name(),
        "expected_rows": workload.expected_rows(),
        "prepare_us": summarize(prepare),
        "execute_us": summarize(execute),
        "full_wire_us": summarize(full_wire)
    }))
}

async fn measure_prepare(
    client: &PostgresClient,
    workload: Workload,
    scenarios: &[Scenario],
    warmup: usize,
    iterations: usize,
) -> TestResult<Vec<u64>> {
    let mut statements = Vec::with_capacity(warmup + iterations);
    let mut samples = Vec::with_capacity(iterations);
    for scenario in &scenarios[..warmup + iterations] {
        let sql = workload.sql(scenario);
        let started = Instant::now();
        let statement = client.prepare(sql.as_ref()).await?;
        let elapsed = started.elapsed();
        statements.push(statement);
        if statements.len() > warmup {
            samples.push(nanos(elapsed));
        }
    }
    std::hint::black_box(statements);
    Ok(samples)
}

async fn measure_execute(
    client: &PostgresClient,
    workload: Workload,
    scenarios: &[Scenario],
    warmup: usize,
    iterations: usize,
) -> TestResult<Vec<u64>> {
    let selected = &scenarios[..warmup + iterations];
    let statements = prepare_statements(client, workload, selected).await?;
    let mut samples = Vec::with_capacity(iterations);
    for (index, (scenario, statement)) in selected.iter().zip(&statements).enumerate() {
        let parameters = workload.parameters(scenario);
        let parameter_refs = parameter_refs(&parameters);
        let started = Instant::now();
        let rows = client.query(statement, &parameter_refs).await?;
        let elapsed = started.elapsed();
        verify_rows(workload, &rows)?;
        if index >= warmup {
            samples.push(nanos(elapsed));
        }
    }
    Ok(samples)
}

async fn prepare_statements(
    client: &PostgresClient,
    workload: Workload,
    scenarios: &[Scenario],
) -> TestResult<Vec<Statement>> {
    if matches!(workload, Workload::PropertySort) {
        let mut statements = Vec::with_capacity(scenarios.len());
        for scenario in scenarios {
            statements.push(client.prepare(workload.sql(scenario).as_ref()).await?);
        }
        return Ok(statements);
    }

    let statement = client.prepare(workload.sql(&scenarios[0]).as_ref()).await?;
    Ok(vec![statement; scenarios.len()])
}

async fn measure_full_wire(
    client: &PostgresClient,
    workload: Workload,
    scenarios: &[Scenario],
    warmup: usize,
    iterations: usize,
) -> TestResult<Vec<u64>> {
    let mut samples = Vec::with_capacity(iterations);
    for (index, scenario) in scenarios[..warmup + iterations].iter().enumerate() {
        let sql = workload.sql(scenario);
        let parameters = workload.parameters(scenario);
        let parameter_refs = parameter_refs(&parameters);
        let started = Instant::now();
        let rows = client.query(sql.as_ref(), &parameter_refs).await?;
        let elapsed = started.elapsed();
        verify_rows(workload, &rows)?;
        if index >= warmup {
            samples.push(nanos(elapsed));
        }
    }
    Ok(samples)
}

fn parameter_refs(parameters: &[String]) -> Vec<&(dyn ToSql + Sync)> {
    parameters
        .iter()
        .map(|value| value as &(dyn ToSql + Sync))
        .collect()
}

fn verify_rows(workload: Workload, rows: &[Row]) -> TestResult {
    if rows.len() != workload.expected_rows() {
        return Err(format!(
            "{} returned {} rows; expected {}",
            workload.name(),
            rows.len(),
            workload.expected_rows()
        )
        .into());
    }
    Ok(())
}

async fn create_schema(client: &PostgresClient) -> TestResult {
    for statement in [
        "CREATE TABLE accounts (id text PRIMARY KEY, handle text NOT NULL)",
        "CREATE TABLE property_schemas (id text PRIMARY KEY)",
        "CREATE TABLE property_schema_fields (id text PRIMARY KEY, schema_id text NOT NULL, name text NOT NULL, type text NOT NULL, sort bigint NOT NULL)",
        "CREATE TABLE nodes (id text PRIMARY KEY, account_id text NOT NULL, parent_node_id text NULL, property_schema_id text NULL, hide_child_tree boolean NOT NULL, visibility text NOT NULL, sort text NOT NULL)",
        "CREATE TABLE properties (id text PRIMARY KEY, node_id text NOT NULL, field_id text NOT NULL, value text NOT NULL)",
        "CREATE INDEX nodes_parent_node_id ON nodes (parent_node_id)",
        "CREATE INDEX nodes_property_schema_id ON nodes (property_schema_id)",
        "CREATE INDEX properties_node_id ON properties (node_id)",
        "CREATE INDEX properties_field_id ON properties (field_id)",
        "CREATE INDEX property_schema_fields_schema_id ON property_schema_fields (schema_id)",
    ] {
        client.batch_execute(statement).await?;
    }
    Ok(())
}

async fn load_fixture(client: &PostgresClient, scenarios: &[Scenario]) -> TestResult {
    client
        .batch_execute("INSERT INTO accounts (id, handle) VALUES ('account-owner', 'owner')")
        .await?;
    client
        .batch_execute("INSERT INTO property_schemas (id) VALUES ('schema-main')")
        .await?;
    client
        .batch_execute(
            "INSERT INTO property_schema_fields (id, schema_id, name, type, sort) VALUES ('field-score', 'schema-main', 'score', 'number', 0), ('field-title', 'schema-main', 'title', 'text', 1)",
        )
        .await?;

    let mut nodes = Vec::with_capacity(scenarios.len() * 53);
    let mut properties = Vec::with_capacity(scenarios.len() * PROPERTY_SORT_CHILDREN * 2);
    for scenario in scenarios {
        add_node(&mut nodes, &scenario.parent_id(), None, "parent");
        add_node(
            &mut nodes,
            &scenario.root_id(),
            Some(&scenario.parent_id()),
            "root",
        );
        for sibling in 0..3 {
            add_node(
                &mut nodes,
                &format!("node-{:05}-sibling-{sibling:02}", scenario.index),
                Some(&scenario.parent_id()),
                &format!("sibling-{sibling:02}"),
            );
        }
        for child in 0..PROPERTY_SORT_CHILDREN {
            let child_id = scenario.sort_child_id(child);
            add_node(
                &mut nodes,
                &child_id,
                Some(&scenario.root_id()),
                &format!("child-{child:02}"),
            );
            properties.push(format!(
                "({}, {}, 'field-score', {})",
                quoted(&format!("property-{:05}-score-{child:02}", scenario.index)),
                quoted(&child_id),
                quoted(&(PROPERTY_SORT_CHILDREN - child).to_string())
            ));
            properties.push(format!(
                "({}, {}, 'field-title', {})",
                quoted(&format!("property-{:05}-title-{child:02}", scenario.index)),
                quoted(&child_id),
                quoted(&format!("title-{child:02}"))
            ));
        }
        for branch in 0..3 {
            for child in 0..3 {
                let level_two_id = scenario.level_two_id(branch, child);
                add_node(
                    &mut nodes,
                    &level_two_id,
                    Some(&scenario.sort_child_id(branch)),
                    &format!("level-two-{branch:02}-{child:02}"),
                );
                for leaf in 0..3 {
                    add_node(
                        &mut nodes,
                        &scenario.level_three_id(branch, child, leaf),
                        Some(&level_two_id),
                        &format!("level-three-{branch:02}-{child:02}-{leaf:02}"),
                    );
                }
            }
        }
    }

    insert_values(
        client,
        "INSERT INTO nodes (id, account_id, parent_node_id, property_schema_id, hide_child_tree, visibility, sort) VALUES ",
        &nodes,
    )
    .await?;
    insert_values(
        client,
        "INSERT INTO properties (id, node_id, field_id, value) VALUES ",
        &properties,
    )
    .await
}

fn add_node(rows: &mut Vec<String>, id: &str, parent: Option<&str>, sort: &str) {
    let parent = parent.map_or_else(|| "NULL".to_owned(), quoted);
    rows.push(format!(
        "({}, 'account-owner', {parent}, 'schema-main', false, 'published', {})",
        quoted(id),
        quoted(sort)
    ));
}

async fn insert_values(client: &PostgresClient, prefix: &str, rows: &[String]) -> TestResult {
    for chunk in rows.chunks(250) {
        client
            .batch_execute(&format!("{prefix}{}", chunk.join(",")))
            .await?;
    }
    Ok(())
}

fn quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn summarize(mut samples: Vec<u64>) -> Value {
    samples.sort_unstable();
    let total = samples.iter().copied().map(u128::from).sum::<u128>();
    let sample_values = samples.iter().copied().map(micros).collect::<Vec<_>>();
    json!({
        "count": samples.len(),
        "samples": sample_values,
        "min": micros(samples[0]),
        "median": micros(percentile(&samples, 50)),
        "p95": micros(percentile(&samples, 95)),
        "max": micros(samples[samples.len() - 1]),
        "mean": total as f64 / samples.len() as f64 / 1_000.0
    })
}

fn percentile(samples: &[u64], percentile: usize) -> u64 {
    samples[(samples.len() - 1) * percentile / 100]
}

fn nanos(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

fn micros(nanos: u64) -> f64 {
    nanos as f64 / 1_000.0
}

fn positive_usize(name: &str, default: usize) -> TestResult<usize> {
    let value = match env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .map_err(|error| format!("{name} is invalid: {error}"))?,
        Err(env::VarError::NotPresent) => default,
        Err(error) => return Err(error.into()),
    };
    if value == 0 {
        return Err(format!("{name} must be positive").into());
    }
    Ok(value)
}

fn write_artifact(encoded: &[u8]) -> TestResult {
    let Ok(directory) = env::var("RAD_BENCHMARK_ARTIFACT_DIR") else {
        return Ok(());
    };
    std::fs::create_dir_all(&directory)?;
    std::fs::write(
        PathBuf::from(directory).join("storyden-library-file-postgres.json"),
        encoded,
    )?;
    Ok(())
}

#[cfg(unix)]
fn terminate(child: &mut Child) -> TestResult<bool> {
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()?;
    if !status.success() {
        return Err("could not signal Rad".into());
    }
    Ok(true)
}

#[cfg(not(unix))]
fn terminate(child: &mut Child) -> TestResult<bool> {
    child.kill()?;
    Ok(false)
}
