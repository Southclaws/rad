use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use tokio_postgres::NoTls;

mod support;

use support::http_process::reserve_port_pair;
use support::s3::TestResult;

const SCHEMA: &str = r#"
tables:
  - id: 1
    name: users
    columns:
      - { id: 1, name: id, type: string, pk: true }
      - { id: 2, name: value, type: string }
"#;

#[tokio::test]
async fn read_instance_enforces_the_complete_mutation_contract() -> TestResult {
    let directory = tempfile::tempdir()?;
    let prefix = "read-only-contract";
    let writer = Process::start(directory.path(), prefix, "write", false).await?;
    seed(&writer).await?;
    writer.stop().await?;
    let before = inventory(directory.path())?;
    let project = tempfile::tempdir()?;

    let reader = Process::start(directory.path(), prefix, "read", true).await?;
    qualify_http_reads(&reader).await?;
    qualify_http_mutations(&reader).await?;
    qualify_admin(&reader).await?;
    qualify_postgres(&reader).await?;
    qualify_rust_client(&reader, project.path()).await?;
    qualify_http_reads(&reader).await?;
    reader.stop().await?;

    let after = inventory(directory.path())?;
    assert_reader_checkpoint_allowlist(&before, &after)?;

    let audit = Process::start(directory.path(), prefix, "write", false).await?;
    qualify_writer_state_unchanged(&audit).await?;
    audit.stop().await
}

async fn seed(writer: &Process) -> TestResult {
    writer
        .success(Method::POST, "/tables", Some(table_definition()))
        .await?;
    writer
        .success(Method::POST, "/execute", Some(create_program("original")))
        .await?;
    Ok(())
}

async fn qualify_http_reads(reader: &Process) -> TestResult {
    let info = reader.json(Method::GET, "/info", None).await?;
    require(
        info["access"] == "read",
        "database info did not report read access",
    )?;
    let tables = reader.json(Method::GET, "/tables", None).await?;
    require(
        tables["tables"]
            .as_array()
            .is_some_and(|tables| tables.len() == 1),
        "catalog inspection changed",
    )?;
    reader.json(Method::GET, "/schema", None).await?;
    reader
        .json(Method::GET, "/schema/transitions", None)
        .await?;
    let result = reader
        .success(Method::POST, "/execute", Some(query_program()))
        .await?;
    require(
        result["result"] == json!([{"id": "user-1", "value": "original"}]),
        "reader query did not return the committed row",
    )
}

async fn qualify_http_mutations(reader: &Process) -> TestResult {
    let cases = vec![
        Mutation::new(
            "PIR create",
            Method::POST,
            "/execute",
            create_program("forbidden"),
        ),
        Mutation::new("PIR update", Method::POST, "/execute", update_program()),
        Mutation::new("PIR delete", Method::POST, "/execute", delete_program()),
        Mutation::new(
            "effectful dry run",
            Method::POST,
            "/execute?dry-run=true",
            create_program("dry-run"),
        ),
        Mutation::new(
            "schema migration",
            Method::POST,
            "/schema/migrate",
            json!({"schema": SCHEMA, "current_version": 0, "current_hash": "ignored"}),
        ),
        Mutation::without_body(
            "transition cancellation",
            Method::POST,
            "/schema/transitions/not-a-transition/cancel",
        ),
        Mutation::new("table create", Method::POST, "/tables", table_definition()),
        Mutation::new(
            "table rename",
            Method::PATCH,
            "/tables/users",
            json!({"name": "forbidden"}),
        ),
        Mutation::without_body("table delete", Method::DELETE, "/tables/users"),
        Mutation::new(
            "column create",
            Method::POST,
            "/tables/users/columns",
            json!({"name": "extra", "type": "text", "nullable": true}),
        ),
        Mutation::new(
            "column rename",
            Method::PATCH,
            "/tables/users/columns/value",
            json!({"name": "forbidden"}),
        ),
        Mutation::without_body(
            "column delete",
            Method::DELETE,
            "/tables/users/columns/value",
        ),
        Mutation::new(
            "index create",
            Method::POST,
            "/tables/users/indexes",
            json!({"name": "forbidden", "columns": ["value"]}),
        ),
        Mutation::without_body(
            "index delete",
            Method::DELETE,
            "/tables/users/indexes/missing",
        ),
    ];
    for mutation in cases {
        let response = reader
            .request(mutation.method, mutation.path, mutation.body)
            .await?;
        assert_read_only(mutation.name, response).await?;
    }
    Ok(())
}

async fn qualify_admin(reader: &Process) -> TestResult {
    let scan = reader
        .client
        .get(format!("{}/api/kv/scan?prefix=", reader.admin_root))
        .send()
        .await?;
    require(
        scan.status().is_success(),
        "admin KV inspection was unavailable",
    )?;
    Ok(())
}

async fn qualify_postgres(reader: &Process) -> TestResult {
    let config = format!("host=127.0.0.1 port={} user=rad", reader.postgres_port);
    let (client, connection) = tokio_postgres::connect(&config, NoTls).await?;
    let connection = tokio::spawn(connection);
    let rows = client
        .query("SELECT id, value FROM users ORDER BY id", &[])
        .await?;
    require(
        rows.len() == 1,
        "PostgreSQL SELECT returned the wrong row count",
    )?;
    require(
        rows[0].get::<_, String>(0) == "user-1",
        "PostgreSQL SELECT returned the wrong database",
    )?;
    require(
        rows[0].get::<_, String>(1) == "original",
        "PostgreSQL SELECT returned mutated data",
    )?;

    for (name, sql) in [
        ("DDL", "CREATE TABLE forbidden (id text PRIMARY KEY)"),
        (
            "insert",
            "INSERT INTO users (id, value) VALUES ('pg-insert', 'no')",
        ),
        (
            "update",
            "UPDATE users SET value = 'no' WHERE id = 'user-1'",
        ),
        ("delete", "DELETE FROM users WHERE id = 'user-1'"),
    ] {
        let error = client.execute(sql, &[]).await.expect_err(name);
        assert_sqlstate(name, &error, "25006")?;
    }
    client.batch_execute("BEGIN").await?;
    let error = client
        .execute("INSERT INTO users (id, value) VALUES ('tx', 'no')", &[])
        .await
        .expect_err("transactional insert");
    assert_sqlstate("explicit transaction", &error, "25006")?;
    client.batch_execute("ROLLBACK").await?;
    drop(client);
    connection.await??;

    Ok(())
}

async fn qualify_rust_client(reader: &Process, directory: &Path) -> TestResult {
    let config = directory.join("rad.config.yaml");
    let schema = directory.join("rad.schema.yaml");
    std::fs::write(
        &config,
        format!(
            "database_url: rad://127.0.0.1:{}\ngenerate: []\n",
            reader.http_port
        ),
    )?;
    std::fs::write(&schema, SCHEMA)?;
    let output = Command::new(env!("CARGO_BIN_EXE_rad"))
        .args([
            "schema",
            "--config",
            config.to_str().ok_or("config path is not UTF-8")?,
            "--file",
            schema.to_str().ok_or("schema path is not UTF-8")?,
            "migrate",
            "--non-interactive",
            "--output",
            "json",
        ])
        .output()?;
    require(
        !output.status.success(),
        "Rust client mutation unexpectedly succeeded",
    )?;
    let json_start = output
        .stderr
        .iter()
        .position(|byte| *byte == b'{')
        .ok_or("Rust client error output contained no JSON object")?;
    let error: Value = serde_json::from_slice(&output.stderr[json_start..]).map_err(|decode| {
        format!(
            "Rust client did not emit JSON: {decode}: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })?;
    require(
        error["error"]["code"] == "invalid",
        "Rust client lost the problem class",
    )?;
    require(
        error["error"]["reason"] == "read_only",
        "Rust client lost read_only",
    )?;
    require(
        error["error"]["http_status"] == 403,
        "Rust client lost HTTP status",
    )
}

async fn qualify_writer_state_unchanged(writer: &Process) -> TestResult {
    let tables = writer.json(Method::GET, "/tables", None).await?;
    require(
        tables["tables"]
            .as_array()
            .is_some_and(|tables| tables.len() == 1),
        "catalog changed after reader mutations",
    )?;
    let result = writer
        .success(Method::POST, "/execute", Some(query_program()))
        .await?;
    require(
        result["result"] == json!([{"id": "user-1", "value": "original"}]),
        "application data changed after reader mutations",
    )
}

async fn assert_read_only(name: &str, response: reqwest::Response) -> TestResult {
    let status = response.status();
    let body: Value = response.json().await?;
    require(
        status == StatusCode::FORBIDDEN,
        &format!("{name} returned HTTP {status}: {body}"),
    )?;
    require(
        body["code"] == "invalid",
        &format!("{name} returned the wrong problem class: {body}"),
    )?;
    require(
        body["reason"] == "read_only",
        &format!("{name} returned the wrong reason: {body}"),
    )
}

fn assert_sqlstate(name: &str, error: &tokio_postgres::Error, expected: &str) -> TestResult {
    let actual = error.as_db_error().map(|error| error.code().code());
    require(
        actual == Some(expected),
        &format!("{name} returned SQLSTATE {actual:?}, expected {expected}"),
    )
}

fn assert_reader_checkpoint_allowlist(
    before: &BTreeMap<String, u64>,
    after: &BTreeMap<String, u64>,
) -> TestResult {
    let mut changes = Vec::new();
    for (path, size) in after {
        if before.get(path) != Some(size) {
            changes.push(path.clone());
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            changes.push(format!("removed:{path}"));
        }
    }
    require(
        !changes.is_empty(),
        "reader created no checkpoint manifests",
    )?;
    let forbidden = changes
        .iter()
        .filter(|path| !path.contains("/manifest/"))
        .cloned()
        .collect::<Vec<_>>();
    require(
        forbidden.is_empty(),
        &format!("reader wrote objects outside the checkpoint manifest allow-list: {forbidden:?}"),
    )
}

fn inventory(root: &Path) -> TestResult<BTreeMap<String, u64>> {
    fn visit(base: &Path, path: &Path, files: &mut BTreeMap<String, u64>) -> TestResult {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                visit(base, &path, files)?;
            } else {
                files.insert(
                    path.strip_prefix(base)?
                        .to_string_lossy()
                        .replace('\\', "/"),
                    entry.metadata()?.len(),
                );
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files)?;
    Ok(files)
}

struct Mutation {
    name: &'static str,
    method: Method,
    path: &'static str,
    body: Option<Value>,
}

impl Mutation {
    fn new(name: &'static str, method: Method, path: &'static str, body: Value) -> Self {
        Self {
            name,
            method,
            path,
            body: Some(body),
        }
    }

    fn without_body(name: &'static str, method: Method, path: &'static str) -> Self {
        Self {
            name,
            method,
            path,
            body: None,
        }
    }
}

struct Process {
    child: Child,
    http_port: u16,
    postgres_port: u16,
    root: String,
    admin_root: String,
    client: Client,
}

impl Process {
    async fn start(directory: &Path, prefix: &str, role: &str, postgres: bool) -> TestResult<Self> {
        let (http_port, public, admin) = reserve_port_pair()?;
        drop((public, admin));
        let postgres_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let postgres_port = postgres_listener.local_addr()?.port();
        drop(postgres_listener);
        let mut arguments = vec![
            "serve".to_owned(),
            "--addr".to_owned(),
            format!("127.0.0.1:{http_port}"),
            "--storage".to_owned(),
            "file".to_owned(),
            "--db".to_owned(),
            directory
                .to_str()
                .ok_or("temporary path is not UTF-8")?
                .to_owned(),
            "--storage-path".to_owned(),
            prefix.to_owned(),
            "--catalog-mode".to_owned(),
            "direct".to_owned(),
            "--role".to_owned(),
            role.to_owned(),
            "--reader-poll-interval-ms".to_owned(),
            "50".to_owned(),
        ];
        if postgres {
            arguments.extend([
                "--frontend".to_owned(),
                "postgres".to_owned(),
                "--postgres-addr".to_owned(),
                format!("127.0.0.1:{postgres_port}"),
            ]);
        }
        let mut process = Self {
            child: Command::new(env!("CARGO_BIN_EXE_rad"))
                .args(arguments)
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .spawn()?,
            http_port,
            postgres_port,
            root: format!("http://127.0.0.1:{http_port}"),
            admin_root: format!("http://127.0.0.1:{}", http_port + 1),
            client: Client::builder().timeout(Duration::from_secs(10)).build()?,
        };
        process.wait_for_health().await?;
        Ok(process)
    }

    async fn wait_for_health(&mut self) -> TestResult {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Err(format!("Rad exited before readiness: {status}").into());
            }
            if self
                .client
                .get(format!("{}/healthz", self.root))
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

    async fn json(&self, method: Method, path: &str, body: Option<Value>) -> TestResult<Value> {
        let response = self.request(method, path, body).await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        require(
            status.is_success(),
            &format!(
                "Rad returned HTTP {status}: {}",
                String::from_utf8_lossy(&bytes)
            ),
        )?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn success(&self, method: Method, path: &str, body: Option<Value>) -> TestResult<Value> {
        self.json(method, path, body).await
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> TestResult<reqwest::Response> {
        let request = self.client.request(method, format!("{}{path}", self.root));
        Ok(match body {
            Some(body) => request.json(&body),
            None => request,
        }
        .send()
        .await?)
    }

    async fn stop(mut self) -> TestResult {
        let expect_success = terminate(&mut self.child)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait()? {
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

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(unix)]
fn terminate(child: &mut Child) -> TestResult<bool> {
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()?;
    require(status.success(), "could not signal Rad")?;
    Ok(true)
}

#[cfg(not(unix))]
fn terminate(child: &mut Child) -> TestResult<bool> {
    child.kill()?;
    Ok(false)
}

fn table_definition() -> Value {
    json!({
        "name": "users",
        "columns": [
            {"name": "id", "type": "text"},
            {"name": "value", "type": "text"}
        ],
        "primary_key": ["id"]
    })
}

fn create_program(value: &str) -> Value {
    json!({"statements": [{
        "name": "create", "kind": "create", "table": "users",
        "relation": rows_relation(json!([["user-1", value]]))
    }]})
}

fn update_program() -> Value {
    json!({"statements": [{
        "name": "update", "kind": "update", "table": "users",
        "relation": rows_relation(json!([["user-1", "forbidden"]]))
    }]})
}

fn delete_program() -> Value {
    json!({"statements": [{
        "name": "delete", "kind": "delete", "table": "users",
        "relation": {
            "nodes": {"row": {"kind": "rows", "scope": "input", "columns": [{"name": "id", "type": "text"}], "rows": [["user-1"]]}},
            "root": {"node": "row", "cardinality": "many"}
        }
    }]})
}

fn rows_relation(rows: Value) -> Value {
    json!({
        "nodes": {"row": {"kind": "rows", "scope": "input", "columns": [
            {"name": "id", "type": "text"}, {"name": "value", "type": "text"}
        ], "rows": rows}},
        "root": {"node": "row", "cardinality": "many"}
    })
}

fn query_program() -> Value {
    json!({"statements": [{
        "name": "query", "kind": "query", "relation": {
            "nodes": {
                "users": {"kind": "scan", "table": "users", "scope": "user"},
                "ordered": {"kind": "order", "input": "users", "terms": [{"expr": {"kind": "col", "scope": "user", "column": "id"}}]}
            },
            "root": {"node": "ordered", "cardinality": "many"}
        }
    }]})
}

fn require(condition: bool, detail: &str) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(detail.into())
    }
}
