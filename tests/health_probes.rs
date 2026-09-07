//! The orchestrator probe contract, exercised through the shipped process.

#![cfg(unix)]

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod support;

use support::http_process::reserve_port_pair;
use support::s3::TestResult;

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Port reservation scans a fixed range, so two concurrent tests would pick the
/// same free port. Holding this across the reserve-and-start window lets the
/// next scan see the previous child already listening.
static STARTING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn probes_track_the_process_from_startup_through_an_orderly_drain() -> TestResult {
    let directory = tempfile::tempdir()?;
    let starting = STARTING.lock().await;
    let port = reserve_port()?;
    let mut process = Process(spawn_writer(directory.path(), port, 3_000)?);
    wait_for_pass(port, "/startupz", &mut process.0).await?;
    drop(starting);

    for path in ["/startupz", "/readyz", "/livez"] {
        let (status, reason) = probe(port, path).await?;
        assert_eq!(status, 200, "{path} failed while serving: {reason}");
    }
    assert_eq!(probe(port, "/readyz").await?.1, "serving");
    assert_eq!(probe(port, "/livez").await?.1, "live");

    terminate(&mut process.0)?;

    // Readiness must be withdrawn before the listeners stop, and the database
    // must still answer while an orchestrator moves traffic away.
    let (ready_status, ready_reason) = probe(port, "/readyz").await?;
    assert_eq!(ready_status, 503, "readiness survived the shutdown signal");
    assert_eq!(ready_reason, "draining");
    let (live_status, live_reason) = probe(port, "/livez").await?;
    assert_eq!(live_status, 200, "a draining process failed liveness");
    assert_eq!(live_reason, "draining");
    assert_eq!(
        reqwest::get(format!("http://127.0.0.1:{port}/healthz"))
            .await?
            .status()
            .as_u16(),
        200,
        "the API stopped answering before the drain window elapsed"
    );

    wait_for_exit(&mut process.0).await
}

#[tokio::test]
async fn unreachable_storage_holds_startup_open_without_publishing_the_database() -> TestResult {
    // A listener that accepts nothing leaves every object-store request
    // outstanding, which is what a cold or unreachable bucket looks like.
    let black_hole = TcpListener::bind("127.0.0.1:0")?;
    let endpoint = format!("http://{}", black_hole.local_addr()?);
    let starting = STARTING.lock().await;
    let port = reserve_port()?;
    let mut process = Process(spawn_unreachable_writer(port, &endpoint)?);
    wait_for_probe(port, "/livez", &mut process.0).await?;
    drop(starting);

    let (startup_status, startup_reason) = probe(port, "/startupz").await?;
    assert_eq!(startup_status, 503, "startup passed without open storage");
    assert_eq!(startup_reason, "starting");
    assert_eq!(probe(port, "/readyz").await?, (503, "starting".to_owned()));
    assert_eq!(probe(port, "/livez").await?, (200, "live".to_owned()));

    // The preflight diagnoses why startup is held once its probe times out
    // against the black hole.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (status, reason) = probe(port, "/startupz").await?;
        assert_eq!(status, 503, "startup passed while storage is unreachable");
        if reason == "storage_unreachable" {
            break;
        }
        assert_eq!(reason, "starting", "unexpected startup hold diagnosis");
        if Instant::now() >= deadline {
            return Err("startup hold was never diagnosed as unreachable".into());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    // Nothing but the probes is published, so no client can reach a database
    // that has not finished initializing.
    for path in ["/healthz", "/info", "/schema"] {
        let status = reqwest::get(format!("http://127.0.0.1:{port}{path}"))
            .await?
            .status();
        assert_eq!(
            status.as_u16(),
            404,
            "{path} answered before storage opened"
        );
    }

    assert!(
        process.0.try_wait()?.is_none(),
        "unreachable storage stopped the process instead of holding startup open"
    );
    Ok(())
}

fn reserve_port() -> TestResult<u16> {
    let (port, public, admin) = reserve_port_pair()?;
    drop((public, admin));
    Ok(port)
}

fn spawn_writer(directory: &std::path::Path, port: u16, drain_ms: u64) -> TestResult<Child> {
    Ok(Command::new(env!("CARGO_BIN_EXE_rad"))
        .arg("serve")
        .arg("--addr")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--storage")
        .arg("file")
        .arg("--storage-path")
        .arg(directory.join("probes"))
        .args(["--catalog-mode", "direct", "--role", "write"])
        .env("RAD_SHUTDOWN_DRAIN_MS", drain_ms.to_string())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?)
}

fn spawn_unreachable_writer(port: u16, endpoint: &str) -> TestResult<Child> {
    Ok(Command::new(env!("CARGO_BIN_EXE_rad"))
        .args([
            "serve",
            "--addr",
            &format!("127.0.0.1:{port}"),
            "--storage",
            "s3",
            "--storage-path",
            "probes",
            "--catalog-mode",
            "direct",
            "--role",
            "write",
            "--s3-bucket",
            "unreachable",
            "--s3-region",
            "us-east-1",
            "--s3-endpoint",
            endpoint,
        ])
        .env("AWS_ACCESS_KEY_ID", "unreachable")
        .env("AWS_SECRET_ACCESS_KEY", "unreachable")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?)
}

async fn probe(port: u16, path: &str) -> TestResult<(u16, String)> {
    let response = reqwest::get(format!("http://127.0.0.1:{port}{path}")).await?;
    let status = response.status().as_u16();
    let body = response.json::<serde_json::Value>().await?;
    let reason = body["reason"]
        .as_str()
        .ok_or_else(|| format!("{path} returned no probe reason: {body}"))?
        .to_owned();
    Ok((status, reason))
}

/// Wait until the process answers `path` at all, whatever the probe's verdict.
async fn wait_for_probe(port: u16, path: &str, process: &mut Child) -> TestResult {
    wait_until(port, path, process, |status| status.is_some()).await
}

/// Wait until the probe at `path` passes.
async fn wait_for_pass(port: u16, path: &str, process: &mut Child) -> TestResult {
    wait_until(port, path, process, |status| status == Some(200)).await
}

async fn wait_until(
    port: u16,
    path: &str,
    process: &mut Child,
    accept: impl Fn(Option<u16>) -> bool,
) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = process.try_wait()? {
            return Err(format!("Rad exited before serving probes: {status}").into());
        }
        let answered = reqwest::get(format!("http://127.0.0.1:{port}{path}"))
            .await
            .ok()
            .map(|response| response.status().as_u16());
        if accept(answered) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("Rad never answered {path} as expected").into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_exit(process: &mut Child) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = process.try_wait()? {
            if status.success() {
                return Ok(());
            }
            return Err(format!("Rad exited unsuccessfully: {status}").into());
        }
        if Instant::now() >= deadline {
            return Err("Rad did not stop after its drain window".into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn terminate(process: &mut Child) -> TestResult {
    let status = Command::new("kill")
        .args(["-TERM", &process.id().to_string()])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("failed to signal Rad process {}: {status}", process.id()).into())
    }
}
