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
      - { id: 2, name: status, type: string }
    indexes:
      - { columns: [status] }
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
    wait_for_row(reader_two, "user-2").await?;
    exercise_concurrent_index_reads(writer, reader_one, reader_two).await
}

async fn exercise_concurrent_index_reads(
    writer: &RadProcess,
    reader_one: &RadProcess,
    reader_two: &RadProcess,
) -> TestResult {
    let planned = reader_one
        .post_response("/execute?show-plan=true", &query_active_program())
        .await?;
    let status = planned.status();
    let planned: Value = planned.json().await?;
    if status != StatusCode::OK || !planned.to_string().contains("IndexRangeScan") {
        return Err(format!("active-user query did not use an index range scan: {planned}").into());
    }
    let write = async {
        for iteration in 0..64 {
            let status = if iteration % 2 == 0 {
                "active"
            } else {
                "inactive"
            };
            writer.execute(&update_program("user-1", status)).await?;
        }
        TestResult::Ok(())
    };
    tokio::try_join!(write, read_active(reader_one), read_active(reader_two))?;
    Ok(())
}

async fn read_active(reader: &RadProcess) -> TestResult {
    for _ in 0..64 {
        execute_read(reader, &query_active_program()).await?;
    }
    Ok(())
}

pub async fn execute_read(reader: &RadProcess, program: &Value) -> TestResult<Value> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let response = reader.post_response("/execute", program).await?;
        let status = response.status();
        let body: Value = response.json().await?;
        if status == StatusCode::OK {
            return Ok(body);
        }
        if status != StatusCode::CONFLICT
            || body["code"] != "conflict"
            || body["reason"] != "serializable_conflict"
        {
            return Err(format!("active-user query returned HTTP {status}: {body}").into());
        }
        if Instant::now() >= deadline {
            return Err("active-user query did not complete after snapshot conflicts".into());
        }
        tokio::task::yield_now().await;
    }
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

pub fn insert_program(id: &str) -> Value {
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
                        "columns": [
                            {"name": "id", "type": "text"},
                            {"name": "status", "type": "text"}
                        ],
                        "rows": [[id, "active"]]
                    }
                },
                "root": {"node": "row", "cardinality": "many"}
            }
        }]
    })
}

fn update_program(id: &str, status: &str) -> Value {
    json!({
        "statements": [{
            "name": "update-user",
            "kind": "update",
            "table": "users",
            "relation": {
                "nodes": {
                    "row": {
                        "kind": "rows",
                        "scope": "input",
                        "columns": [
                            {"name": "id", "type": "text"},
                            {"name": "status", "type": "text"}
                        ],
                        "rows": [[id, status]]
                    }
                },
                "root": {"node": "row", "cardinality": "many"}
            }
        }]
    })
}

fn query_active_program() -> Value {
    json!({
        "statements": [{
            "name": "active-users",
            "kind": "query",
            "relation": {
                "nodes": {
                    "users": {"kind": "scan", "table": "users", "scope": "user"},
                    "active": {
                        "kind": "filter",
                        "input": "users",
                        "predicate": {
                            "kind": "binary",
                            "op": "eq",
                            "left": {"kind": "col", "scope": "user", "column": "status"},
                            "right": {"kind": "lit", "value": {"type": "text", "value": "active"}}
                        }
                    },
                    "ordered": {
                        "kind": "order",
                        "input": "active",
                        "terms": [
                            {"expr": {"kind": "col", "scope": "user", "column": "id"}}
                        ]
                    }
                },
                "root": {"node": "ordered", "cardinality": "many"}
            }
        }]
    })
}

pub fn query_program() -> Value {
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
