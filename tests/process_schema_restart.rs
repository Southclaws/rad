//! Black-box recovery through the shipped process and public HTTP API.

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use reqwest::{Client, StatusCode};
use serde_json::{Value, json};

const INITIAL_SCHEMA: &str = r#"
tables:
  - id: 1
    name: items
    columns:
      - { id: 1, name: id, type: string, pk: true }
      - { id: 2, name: value, type: string }
"#;

const DESIRED_SCHEMA: &str = r#"
tables:
  - id: 1
    name: items
    columns:
      - { id: 1, name: id, type: string, pk: true }
      - { id: 2, name: value, type: int64, index: true }
"#;

#[tokio::test]
async fn process_restart_converges_a_dependent_schema_transition_graph() {
    let directory = tempfile::tempdir().unwrap();
    let address = unused_loopback_address();
    let base = format!("http://{address}");
    let client = Client::new();

    let mut first = Process::start(directory.path(), &address, Some("schema"));
    wait_until_ready(&client, &base, &mut first).await;

    let initial = migrate(&client, &base, INITIAL_SCHEMA).await;
    assert_eq!(initial["state"], "ready");

    let rows = (0..12_000)
        .map(|ordinal| {
            vec![
                Value::String(format!("item-{ordinal}")),
                Value::String(ordinal.to_string()),
            ]
        })
        .collect::<Vec<_>>();
    let inserted = post_json(
        &client,
        &format!("{base}/execute"),
        &json!({
            "statements": [{
                "name": "seed_items",
                "kind": "create",
                "table": "items",
                "relation": {
                    "nodes": {
                        "input": {
                            "kind": "rows",
                            "scope": "input",
                            "columns": [
                                {"name": "id", "type": "text"},
                                {"name": "value", "type": "text"}
                            ],
                            "rows": rows
                        }
                    },
                    "root": {"node": "input", "cardinality": "many"}
                }
            }]
        }),
    )
    .await;
    assert_eq!(inserted.status(), StatusCode::OK);

    let migrating = migrate(&client, &base, DESIRED_SCHEMA).await;
    assert_eq!(migrating["state"], "converging");
    assert_eq!(migrating["transition_ids"].as_array().unwrap().len(), 2);
    let desired_hash = migrating["desired_hash"].as_str().unwrap().to_owned();

    let transitions = get_json(&client, &format!("{base}/schema/transitions")).await;
    let transitions = transitions["transitions"].as_array().unwrap();
    assert_eq!(transitions.len(), 2);
    assert!(
        transitions
            .iter()
            .any(|transition| !transition["prerequisites"].as_array().unwrap().is_empty()),
        "the index build must durably depend on the column replacement"
    );

    first.kill_and_wait();

    let mut second = Process::start(directory.path(), &address, None);
    wait_until_ready(&client, &base, &mut second).await;
    wait_for_schema_hash(&client, &base, &desired_hash, &mut second).await;

    let queried = post_json(
        &client,
        &format!("{base}/execute?show-plan=true"),
        &json!({
            "result": "find_42",
            "statements": [{
                "name": "find_42",
                "kind": "query",
                "relation": {
                    "nodes": {
                        "items": {"kind": "scan", "table": "items", "scope": "items"},
                        "matching": {
                            "kind": "filter",
                            "input": "items",
                            "predicate": {
                                "kind": "binary",
                                "op": "eq",
                                "left": {"kind": "col", "scope": "items", "column": "value"},
                                "right": {"kind": "lit", "value": {"type": "int64", "value": "42"}}
                            }
                        },
                        "ordered": {
                            "kind": "order",
                            "input": "matching",
                            "terms": [{"expr": {"kind": "col", "scope": "items", "column": "id"}}]
                        }
                    },
                    "root": {"node": "ordered", "cardinality": "many"}
                }
            }]
        }),
    )
    .await;
    let status = queried.status();
    let queried = queried.json::<Value>().await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "query failed after recovery: {queried}"
    );
    assert_eq!(queried["result"], json!([{"id": "item-42", "value": 42}]));
    assert!(queried["plan"]["statements"].is_array());

    second.kill_and_wait();
}

async fn migrate(client: &Client, base: &str, schema: &str) -> Value {
    let current = get_json(client, &format!("{base}/schema")).await;
    let response = post_json(
        client,
        &format!("{base}/schema/migrate"),
        &json!({
            "schema": schema,
            "current_version": current["schema_version"],
            "current_hash": current["schema_hash"]
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.unwrap()
}

async fn wait_until_ready(client: &Client, base: &str, child: &mut Process) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        child.assert_running();
        if let Ok(response) = client.get(format!("{base}/health")).send().await
            && response.status() == StatusCode::OK
        {
            return;
        }
        assert!(Instant::now() < deadline, "Rad did not become healthy");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_schema_hash(client: &Client, base: &str, expected: &str, child: &mut Process) {
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut last_schema = Value::Null;
    let mut last_transitions = Value::Null;
    loop {
        child.assert_running();
        if let Ok(response) = client.get(format!("{base}/schema")).send().await
            && response.status() == StatusCode::OK
        {
            let body = response.json::<Value>().await.unwrap();
            if body["schema_hash"] == expected {
                return;
            }
            last_schema = body;
        }
        if let Ok(response) = client
            .get(format!("{base}/schema/transitions"))
            .send()
            .await
            && response.status() == StatusCode::OK
        {
            last_transitions = response.json::<Value>().await.unwrap();
            let failed = last_transitions["transitions"]
                .as_array()
                .is_some_and(|transitions| {
                    transitions.iter().any(|transition| {
                        matches!(transition["state"].as_str(), Some("failed" | "cancelled"))
                    })
                });
            assert!(
                !failed,
                "recovered schema transition failed: {last_transitions}"
            );
        }
        assert!(
            Instant::now() < deadline,
            "restarted Rad process did not converge to schema {expected}; last schema: {last_schema}; last transitions: {last_transitions}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn get_json(client: &Client, url: &str) -> Value {
    let response = client.get(url).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.unwrap()
}

async fn post_json(client: &Client, url: &str, body: &Value) -> reqwest::Response {
    client.post(url).json(body).send().await.unwrap()
}

fn unused_loopback_address() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().to_string()
}

struct Process {
    child: Child,
}

impl Process {
    fn start(directory: &Path, address: &str, mode: Option<&str>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rad"));
        command
            .arg("serve")
            .env("RAD_ADDR", address)
            .env("RAD_STORAGE", "file")
            .env("RAD_DATA_DIR", directory)
            .env("RAD_STORAGE_PATH", "database")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        if let Some(mode) = mode {
            command.env("RAD_CATALOG_MODE", mode);
        }
        Self {
            child: command.spawn().unwrap(),
        }
    }

    fn assert_running(&mut self) {
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "Rad process exited before the recovery assertion completed"
        );
    }

    fn kill_and_wait(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        self.child.wait().unwrap();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
