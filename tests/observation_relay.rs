//! A reader's evidence reaching a writer, over real sockets and real
//! processes. Everything below the transport is covered by unit tests; what
//! this qualifies is that the pieces are wired together and that the port
//! behaves the same way in a process as it does in a router test.

mod support;

use std::time::{Duration, Instant};

use support::http_process::{RadProcess, reserve_extra_port, reserve_port_pair};
use support::multi_replica;
use support::s3::TestResult;

fn total_executions(statistics: &serde_json::Value) -> i64 {
    statistics["models"]
        .as_array()
        .map(|models| {
            models
                .iter()
                .map(|model| model["retainedExecutions"].as_i64().unwrap_or(0))
                .sum()
        })
        .unwrap_or(0)
}

#[test]
fn subprocess_port_allocators_advance_after_a_reservation_is_released() -> TestResult {
    let (first_extra, first_listener) = reserve_extra_port()?;
    drop(first_listener);
    let (second_extra, second_listener) = reserve_extra_port()?;
    drop(second_listener);
    assert_ne!(first_extra, second_extra);

    let (first_public, first_listener, first_admin) = reserve_port_pair()?;
    drop((first_listener, first_admin));
    let (second_public, second_listener, second_admin) = reserve_port_pair()?;
    drop((second_listener, second_admin));
    assert_ne!(first_public, second_public);
    Ok(())
}

/// The writer already holds evidence from its own seeding, so what qualifies
/// the relay is growth beyond that. Statistics are advisory and published on
/// the writer's own schedule, so wait for it rather than assume a timing.
async fn wait_for_relayed_growth(
    writer: &RadProcess,
    baseline: i64,
    expected: i64,
) -> TestResult<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let statistics = writer.statistics().await?;
        if total_executions(&statistics) - baseline >= expected {
            return Ok(statistics);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the writer gained {} executions, not the {expected} the reader served: {statistics}",
                total_executions(&statistics) - baseline
            )
            .into());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test]
async fn a_readers_queries_reach_the_writers_statistics() -> TestResult {
    let directory = tempfile::tempdir()?;
    let token_file = directory.path().join("relay-token");
    std::fs::write(&token_file, "a-database-scoped-secret\n")?;

    let (writer, reader, internal) =
        RadProcess::start_file_relay_pair(directory.path(), "relay", &token_file).await?;
    multi_replica::seed(&writer).await?;
    let baseline = total_executions(&writer.statistics().await?);

    // Only the reader serves these, so evidence for them can reach the writer
    // by no route other than the relay.
    for _ in 0..12 {
        reader.execute(&multi_replica::query_program()).await?;
    }
    wait_for_relayed_growth(&writer, baseline, 12).await?;

    // The internal port is authenticated and serves nothing else. Its
    // liveness answer is deliberately reachable without the secret.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let unauthenticated = client
        .post(format!("{internal}/internal/statistics/observations"))
        .json(&serde_json::json!({"format": 1}))
        .send()
        .await?;
    assert_eq!(unauthenticated.status(), reqwest::StatusCode::UNAUTHORIZED);

    let livez = client
        .get(format!("{internal}/internal/livez"))
        .send()
        .await?;
    assert!(livez.status().is_success(), "internal liveness: {livez:?}");

    // The client port never gained the internal API, and the internal port
    // never gained the client API.
    assert_eq!(writer.get_status("/internal/livez").await?, 404);
    let client_route = client.get(format!("{internal}/healthz")).send().await?;
    assert_eq!(client_route.status(), reqwest::StatusCode::NOT_FOUND);

    reader.stop().await?;
    writer.stop().await
}

/// A relay that cannot reach anything must cost the fleet a better planner and
/// nothing else: the reader keeps serving, and keeps reporting ready.
#[tokio::test]
async fn a_reader_whose_writer_is_unreachable_keeps_serving() -> TestResult {
    let directory = tempfile::tempdir()?;
    let token_file = directory.path().join("relay-token");
    std::fs::write(&token_file, "a-database-scoped-secret")?;

    let writer = RadProcess::start_file(directory.path(), "relay-down").await?;
    multi_replica::seed(&writer).await?;

    // Nothing listens on the target, so every batch fails to send.
    let reader = RadProcess::start_file_reader_relaying(
        directory.path(),
        "relay-down",
        "http://127.0.0.1:1",
        &token_file,
    )
    .await?;

    for _ in 0..8 {
        reader.execute(&multi_replica::query_program()).await?;
    }
    assert_eq!(reader.get_status("/readyz").await?, 200);
    assert_eq!(reader.get_status("/livez").await?, 200);
    let rows = reader.execute(&multi_replica::query_program()).await?;
    assert!(
        rows["result"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty()),
        "a reader with an unreachable relay stopped answering: {rows}"
    );

    reader.stop().await?;
    writer.stop().await
}
