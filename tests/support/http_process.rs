#![allow(dead_code)]

use std::net::TcpListener;
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

/// On Windows the child leads its own process group so `terminate` can
/// deliver CTRL_BREAK to it alone; `Child::kill` would `TerminateProcess`
/// and forfeit the orderly-shutdown exit status this suite asserts.
fn rad_command() -> Command {
    let command = Command::new(env!("CARGO_BIN_EXE_rad"));
    #[cfg(windows)]
    let command = {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        let mut command = command;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
        command
    };
    command
}

impl RadProcess {
    pub async fn start_s3(config: &S3Config, endpoint: &str, prefix: &str) -> TestResult<Self> {
        Self::start_s3_role(config, endpoint, prefix, "write").await
    }

    pub async fn start_s3_reader(
        config: &S3Config,
        endpoint: &str,
        prefix: &str,
    ) -> TestResult<Self> {
        Self::start_s3_role(config, endpoint, prefix, "read").await
    }

    async fn start_s3_role(
        config: &S3Config,
        endpoint: &str,
        prefix: &str,
        role: &str,
    ) -> TestResult<Self> {
        let (port, public, admin) = reserve_port_pair()?;
        drop((public, admin));

        let child = rad_command()
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
                "--role",
                role,
                "--reader-poll-interval-ms",
                "100",
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

    pub async fn start_file(directory: &std::path::Path, prefix: &str) -> TestResult<Self> {
        Self::start_file_role(directory, prefix, "write").await
    }

    pub async fn start_file_reader(directory: &std::path::Path, prefix: &str) -> TestResult<Self> {
        Self::start_file_role(directory, prefix, "read").await
    }

    async fn start_file_role(
        directory: &std::path::Path,
        prefix: &str,
        role: &str,
    ) -> TestResult<Self> {
        let (port, public, admin) = reserve_port_pair()?;
        drop((public, admin));
        Self::spawn_file(directory, prefix, role, port, &[]).await
    }

    async fn spawn_file(
        directory: &std::path::Path,
        prefix: &str,
        role: &str,
        port: u16,
        extra: &[&str],
    ) -> TestResult<Self> {
        let child = rad_command()
            .args([
                "serve",
                "--addr",
                &format!("127.0.0.1:{port}"),
                "--storage",
                "file",
                "--db",
                directory.to_str().ok_or("temporary path is not UTF-8")?,
                "--storage-path",
                prefix,
                "--catalog-mode",
                "schema",
                "--role",
                role,
                "--reader-poll-interval-ms",
                "100",
            ])
            .args(extra)
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

    /// A writer that also serves the instance-to-instance API, and a reader
    /// that reports its statistics to it.
    pub async fn start_file_relay_pair(
        directory: &std::path::Path,
        prefix: &str,
        token_file: &std::path::Path,
    ) -> TestResult<(Self, Self, String)> {
        let (writer_port, public, admin) = reserve_port_pair()?;
        let (internal_port, internal) = reserve_extra_port()?;
        drop((public, admin, internal));
        let internal_address = format!("127.0.0.1:{internal_port}");
        let token = token_file
            .to_str()
            .ok_or("temporary path is not UTF-8")?
            .to_owned();

        let writer = Self::spawn_file(
            directory,
            prefix,
            "write",
            writer_port,
            &[
                "--internal-addr",
                &internal_address,
                "--relay-token-file",
                &token,
            ],
        )
        .await?;

        let (reader_port, public, admin) = reserve_port_pair()?;
        drop((public, admin));
        let target = format!("http://{internal_address}");
        let reader = Self::spawn_file(
            directory,
            prefix,
            "read",
            reader_port,
            &[
                "--relay-target",
                &target,
                "--relay-token-file",
                &token,
                "--instance-id",
                "relay-reader",
            ],
        )
        .await?;
        Ok((writer, reader, target))
    }

    /// A reader that reports to a target which may or may not answer.
    pub async fn start_file_reader_relaying(
        directory: &std::path::Path,
        prefix: &str,
        target: &str,
        token_file: &std::path::Path,
    ) -> TestResult<Self> {
        let (port, public, admin) = reserve_port_pair()?;
        drop((public, admin));
        let token = token_file
            .to_str()
            .ok_or("temporary path is not UTF-8")?
            .to_owned();
        Self::spawn_file(
            directory,
            prefix,
            "read",
            port,
            &["--relay-target", target, "--relay-token-file", &token],
        )
        .await
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

    pub async fn statistics(&self) -> TestResult<Value> {
        self.get_json("/statistics").await
    }

    pub async fn get_status(&self, path: &str) -> TestResult<u16> {
        Ok(self
            .client
            .get(format!("{}{path}", self.base))
            .send()
            .await?
            .status()
            .as_u16())
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
                .get(format!("{}/healthz", self.base))
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

/// A port for a listener that is not part of the public/admin pair.
///
/// Drawn from a range the pair allocator never touches: the pair allocator
/// steps by two, so any port adjacent to a reserved pair is another pair's
/// public port and would collide as soon as two tests run together.
pub(crate) fn reserve_extra_port() -> TestResult<(u16, TcpListener)> {
    for port in 30_000..40_000 {
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => return Ok((port, listener)),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err("could not reserve a port outside the public and admin pair range".into())
}

pub(crate) fn reserve_port_pair() -> TestResult<(u16, TcpListener, TcpListener)> {
    for port in (12_000..30_000).step_by(2) {
        let public = match TcpListener::bind(("127.0.0.1", port)) {
            Ok(public) => public,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => return Err(error.into()),
        };
        let admin_port = port + 1;
        match TcpListener::bind(("127.0.0.1", admin_port)) {
            Ok(admin) => return Ok((port, public, admin)),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err("could not reserve adjacent public and admin ports outside the ephemeral range".into())
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

// The one sanctioned `unsafe` block in the package (`unsafe_code = "deny"`
// in Cargo.toml documents the carve-out): `GenerateConsoleCtrlEvent` has no
// safe wrapper, takes two scalar arguments, and carries no memory-safety
// obligations.
#[cfg(windows)]
#[allow(unsafe_code)]
fn terminate(child: &mut Child) -> TestResult {
    let delivered = unsafe {
        windows_sys::Win32::System::Console::GenerateConsoleCtrlEvent(
            windows_sys::Win32::System::Console::CTRL_BREAK_EVENT,
            child.id(),
        )
    };
    if delivered == 0 {
        return Err(format!(
            "failed to signal Rad process {}: {}",
            child.id(),
            std::io::Error::last_os_error()
        )
        .into());
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn terminate(child: &mut Child) -> TestResult {
    child.kill()?;
    Ok(())
}
