#![allow(dead_code)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use reqwest::{Client, Response};
use serde_json::{Value, json};

use super::s3::{S3Config, TestResult};

pub struct RadProcess {
    child: Child,
    pub base: String,
    client: Client,
}

impl RadProcess {
    pub async fn start_s3(config: &S3Config, endpoint: &str, prefix: &str) -> TestResult<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        drop(listener);

        let child = Command::new(env!("CARGO_BIN_EXE_rad"))
            .args([
                "serve",
                "--addr",
                &format!("127.0.0.1:{port}"),
                "--storage",
                "s3",
                "--storage-path",
                prefix,
                "--catalog-mode",
                "schema",
                "--s3-bucket",
                &config.bucket,
                "--s3-region",
                &config.region,
                "--s3-endpoint",
                endpoint,
            ])
            .env("AWS_ACCESS_KEY_ID", &config.access_key)
            .env("AWS_SECRET_ACCESS_KEY", &config.secret_key)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?;
        let mut process = Self {
            child,
            base: format!("http://127.0.0.1:{port}"),
            client: Client::builder().timeout(Duration::from_secs(60)).build()?,
        };
        process.wait_until_ready().await?;
        Ok(process)
    }

    pub async fn migrate(&self, schema: &str) -> TestResult<Value> {
        let state = self.get_json("/schema").await?;
        let mut migration = self
            .post_json(
                "/schema/migrate",
                &json!({
                    "schema": schema,
                    "current_version": state["schema_version"],
                    "current_hash": state["schema_hash"]
                }),
            )
            .await?;
        if migration["state"] == "ready" {
            return Ok(migration);
        }
        let transitions = migration["transition_ids"]
            .as_array()
            .ok_or("converging migration omitted transition_ids")?;
        if transitions.is_empty() {
            return Err("converging migration has no observable transition work".into());
        }
        for _ in 0..600 {
            let mut ready = true;
            for transition in transitions {
                let id = transition
                    .as_str()
                    .ok_or("migration returned a non-string transition id")?;
                let control = self.get_json(&format!("/schema/transitions/{id}")).await?;
                match control["state"].as_str() {
                    Some("ready") => {}
                    Some("failed" | "cancelled") => {
                        return Err(format!(
                            "schema transition {id} ended in state {}: {}",
                            control["state"], control["last_error"]
                        )
                        .into());
                    }
                    _ => ready = false,
                }
            }
            if ready {
                let state = self.get_json("/schema").await?;
                if state["schema_hash"] != migration["desired_hash"] {
                    return Err("schema transitions published the wrong desired hash".into());
                }
                migration["schema"] = state["schema"].clone();
                migration["schema_hash"] = state["schema_hash"].clone();
                migration["schema_version"] = state["schema_version"].clone();
                migration["state"] = Value::String("ready".to_owned());
                return Ok(migration);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err("timed out waiting for schema migration".into())
    }

    pub async fn execute(&self, program: &Value) -> TestResult<Value> {
        self.post_json("/execute", program).await
    }

    pub async fn post_response(&self, path: &str, body: &Value) -> TestResult<Response> {
        Ok(self
            .client
            .post(format!("{}{path}", self.base))
            .json(body)
            .send()
            .await?)
    }

    pub async fn stop(mut self) -> TestResult {
        terminate(&mut self.child)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait()? {
                if status.success() {
                    return Ok(());
                }
                return Err(format!("Rad process exited unsuccessfully: {status}").into());
            }
            if Instant::now() >= deadline {
                self.child.kill()?;
                let _ = self.child.wait();
                return Err("Rad process did not stop after its shutdown signal".into());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn get_json(&self, path: &str) -> TestResult<Value> {
        decode(
            self.client
                .get(format!("{}{path}", self.base))
                .send()
                .await?,
        )
        .await
    }

    async fn post_json(&self, path: &str, body: &Value) -> TestResult<Value> {
        decode(self.post_response(path, body).await?).await
    }

    async fn wait_until_ready(&mut self) -> TestResult {
        for _ in 0..400 {
            if let Some(status) = self.child.try_wait()? {
                return Err(format!("Rad exited before readiness: {status}").into());
            }
            if self
                .client
                .get(format!("{}/health", self.base))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Err("Rad did not become ready".into())
    }
}

impl Drop for RadProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn decode(response: Response) -> TestResult<Value> {
    let status = response.status();
    let bytes = response.bytes().await?;
    if !status.is_success() {
        return Err(format!(
            "Rad returned HTTP {status}: {}",
            String::from_utf8_lossy(&bytes)
        )
        .into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(unix)]
fn terminate(child: &mut Child) -> TestResult {
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("failed to signal Rad process {0}: {status}", child.id()).into())
    }
}

#[cfg(not(unix))]
fn terminate(child: &mut Child) -> TestResult {
    child.kill()?;
    Ok(())
}
