#![allow(dead_code)]

use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde_json::{Value, json};

use super::http_process::RadProcess;
use super::s3::TestResult;

const SCHEMA: &str = r#"
tables:
  - id: 1
    name: users
    columns:
      - { id: 1, name: id, type: string, pk: true }
"#;

pub async fn seed(writer: &RadProcess) -> TestResult {
    writer.migrate(SCHEMA).await?;
    writer.execute(&insert_program("user-1")).await?;
    Ok(())
}

pub async fn qualify_replicas(
    writer: &RadProcess,
    reader_one: &RadProcess,
    reader_two: &RadProcess,
) -> TestResult {
    assert_access(writer, "write").await?;
    assert_access(reader_one, "read").await?;
    assert_access(reader_two, "read").await?;
    wait_for_row(reader_one, "user-1").await?;
    wait_for_row(reader_two, "user-1").await?;

    assert_read_only(
        "reader PIR mutation",
        reader_one
            .post_response("/execute", &insert_program("forbidden"))
            .await?,
    )
    .await?;
    assert_read_only(
        "reader schema migration",
        reader_two
            .post_response(
                "/schema/migrate",
                &json!({"schema": "tables: []\n", "current_version": 0, "current_hash": "ignored"}),
            )
            .await?,
    )
    .await?;

    writer.execute(&insert_program("user-2")).await?;
    wait_for_row(reader_one, "user-2").await?;
    wait_for_row(reader_two, "user-2").await
}

async fn assert_access(process: &RadProcess, expected: &str) -> TestResult {
    let info: Value = reqwest::get(format!("{}/info", process.base))
        .await?
        .json()
        .await?;
    if info["access"] == expected {
        Ok(())
    } else {
        Err(format!("expected {expected} access, got {info}").into())
    }
}

async fn assert_read_only(name: &str, response: reqwest::Response) -> TestResult {
    let status = response.status();
    let problem: Value = response.json().await?;
    if status != StatusCode::FORBIDDEN
        || problem["code"] != "invalid"
        || problem["reason"] != "read_only"
    {
        return Err(format!("{name} returned HTTP {status}: {problem}").into());
    }
    Ok(())
}

async fn wait_for_row(reader: &RadProcess, id: &str) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let observation = match reader.execute(&query_program()).await {
            Ok(body) if body.to_string().contains(id) => return Ok(()),
            Ok(body) => body.to_string(),
            Err(error) => error.to_string(),
        };
        if Instant::now() >= deadline {
            return Err(format!(
                "reader did not observe committed row {id:?}; last observation: {observation}"
            )
            .into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn insert_program(id: &str) -> Value {
    json!({
        "statements": [{
            "name": "insert-user",
            "kind": "create",
            "table": "users",
            "relation": {
                "nodes": {
                    "row": {
                        "kind": "rows",
                        "scope": "literal",
                        "columns": [{"name": "id", "type": "text"}],
                        "rows": [[id]]
                    }
                },
                "root": {"node": "row", "cardinality": "many"}
            }
        }]
    })
}

fn query_program() -> Value {
    json!({
        "statements": [{
            "name": "list-users",
            "kind": "query",
            "relation": {
                "nodes": {
                    "users": {"kind": "scan", "table": "users", "scope": "user"},
                    "ordered": {
                        "kind": "order",
                        "input": "users",
                        "terms": [{
                            "expr": {"kind": "col", "scope": "user", "column": "id"}
                        }]
                    }
                },
                "root": {"node": "ordered", "cardinality": "many"}
            }
        }]
    })
}
