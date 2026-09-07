#![cfg(unix)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod support;

use support::http_process::reserve_port_pair;
use support::s3::TestResult;

struct Processes(Vec<Child>);

impl Drop for Processes {
    fn drop(&mut self) {
        for process in &mut self.0 {
            let _ = process.kill();
            let _ = process.wait();
        }
    }
}

#[tokio::test]
async fn writer_fencing_stops_the_displaced_process() -> TestResult {
    let directory = tempfile::tempdir()?;
    let mut processes = Processes(Vec::new());
    let first_port = reserve_port()?;
    processes
        .0
        .push(spawn_writer(directory.path(), first_port)?);
    wait_for_health(first_port, &mut processes.0[0]).await?;

    let second_port = reserve_port()?;
    processes
        .0
        .push(spawn_writer(directory.path(), second_port)?);
    wait_for_health(second_port, &mut processes.0[1]).await?;

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = processes.0[0].try_wait()? {
            if status.success() {
                return Err("fenced writer exited as though it retained ownership".into());
            }
            break;
        }
        if Instant::now() >= deadline {
            return Err("fenced writer kept serving past the ownership-check interval".into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let health = reqwest::get(format!("http://127.0.0.1:{second_port}/healthz")).await?;
    if !health.status().is_success() {
        return Err(format!(
            "replacement writer did not serve the database: {}",
            health.status()
        )
        .into());
    }
    terminate(&mut processes.0[1])?;
    wait_for_exit(&mut processes.0[1]).await
}

fn reserve_port() -> TestResult<u16> {
    let (port, public, admin) = reserve_port_pair()?;
    drop((public, admin));
    Ok(port)
}

fn spawn_writer(directory: &std::path::Path, port: u16) -> TestResult<Child> {
    Ok(Command::new(env!("CARGO_BIN_EXE_rad"))
        .arg("serve")
        .arg("--addr")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--storage")
        .arg("file")
        .arg("--storage-path")
        .arg(directory.join("writer-fence"))
        .args(["--catalog-mode", "direct", "--role", "write"])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?)
}

async fn wait_for_health(port: u16, process: &mut Child) -> TestResult {
    let client = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = process.try_wait()? {
            return Err(format!("writer exited before readiness: {status}").into());
        }
        if client
            .get(format!("http://127.0.0.1:{port}/healthz"))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("writer did not become ready".into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_exit(process: &mut Child) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = process.try_wait()? {
            if status.success() {
                return Ok(());
            }
            return Err(format!("writer exited unsuccessfully: {status}").into());
        }
        if Instant::now() >= deadline {
            return Err("writer did not stop after SIGTERM".into());
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
        Err(format!("failed to signal writer {}: {status}", process.id()).into())
    }
}
