#![allow(dead_code)]

use std::env;
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use reqwest::{Client, Response};
use serde_json::{Value, json};

use super::s3::{S3Config, TestResult};

pub struct RadProcess {
    child: Child,
    pub base: String,
    client: Client,
}

const PORT_PAIR_COUNT: usize = 9_000;
const EXTRA_PORT_COUNT: usize = 10_000;
const DEFAULT_REQUEST_TIMEOUT_SECONDS: u64 = 180;
const PROCESS_READY_TIMEOUT: Duration = Duration::from_secs(60);
static NEXT_PORT_PAIR: AtomicUsize = AtomicUsize::new(0);
static NEXT_EXTRA_PORT: AtomicUsize = AtomicUsize::new(0);

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
        Self::start_s3_role(config, endpoint, prefix, "write", "structural", false).await
    }

    pub async fn start_s3_corpus_writer(
        config: &S3Config,
        endpoint: &str,
        prefix: &str,
    ) -> TestResult<Self> {
        Self::start_s3_role(config, endpoint, prefix, "write", "cost", true).await
    }

    pub async fn start_s3_reader(
        config: &S3Config,
        endpoint: &str,
        prefix: &str,
    ) -> TestResult<Self> {
        Self::start_s3_role(config, endpoint, prefix, "read", "cost", false).await
    }

    pub async fn start_s3_reader_mode(
        config: &S3Config,
        endpoint: &str,
        prefix: &str,
        planner_mode: &str,
    ) -> TestResult<Self> {
        Self::start_s3_role(config, endpoint, prefix, "read", planner_mode, false).await
    }

    async fn start_s3_role(
        config: &S3Config,
        endpoint: &str,
        prefix: &str,
        role: &str,
        planner_mode: &str,
        capture_workload_corpus: bool,
    ) -> TestResult<Self> {
        let (port, public, admin) = reserve_port_pair()?;
        drop((public, admin));

        let mut command = rad_command();
        command
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
            .env("RAD_INTERNAL_TESTING", "true")
            .env("RAD_INTERNAL_TEST_PLANNER_MODE", planner_mode)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        if capture_workload_corpus {
            command.env("RAD_CAPTURE_WORKLOAD_CORPUS", "true");
        }
        let child = command.spawn()?;
        let mut process = Self {
            child,
            base: format!("http://127.0.0.1:{port}"),
            client: benchmark_client()?,
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
            .arg("serve")
            .arg("--addr")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--storage")
            .arg("file")
            .arg("--storage-path")
            .arg(directory.join(prefix))
            .args([
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
            client: benchmark_client()?,
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

    pub async fn plan(&self, program: &Value) -> TestResult<Value> {
        self.post_json("/execute?show-plan=true&dry-run=true", program)
            .await
    }

    pub async fn execute_with_plan(&self, program: &Value) -> TestResult<Value> {
        self.post_json("/execute?show-plan=true", program).await
    }

    pub async fn statistics(&self) -> TestResult<Value> {
        self.get_json("/statistics").await
    }

    pub async fn replay_corpus(&self, limit: usize) -> TestResult<Value> {
        let mut url = reqwest::Url::parse(&self.base)?;
        let admin_port = url
            .port_or_known_default()
            .ok_or("Rad public URL has no port")?
            .checked_add(1)
            .ok_or("Rad admin port overflow")?;
        url.set_port(Some(admin_port))
            .map_err(|_| "Rad admin URL rejected its port")?;
        url.set_path("/api/statistics/corpus/replay");
        decode(
            self.client
                .post(url)
                .json(&json!({"limit": limit}))
                .send()
                .await?,
        )
        .await
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
        let deadline = Instant::now() + PROCESS_READY_TIMEOUT;
        loop {
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
            if Instant::now() >= deadline {
                return Err(format!(
                    "Rad did not become ready within {} seconds",
                    PROCESS_READY_TIMEOUT.as_secs()
                )
                .into());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

fn benchmark_client() -> TestResult<Client> {
    Ok(Client::builder()
        .timeout(Duration::from_secs(request_timeout_seconds()?))
        .build()?)
}

pub(crate) fn request_timeout_seconds() -> TestResult<u64> {
    let configured = env::var("RAD_BENCHMARK_REQUEST_TIMEOUT_SECONDS").ok();
    request_timeout_seconds_from(configured.as_deref())
}

fn request_timeout_seconds_from(configured: Option<&str>) -> TestResult<u64> {
    let seconds = match configured {
        Some(value) => value.parse::<u64>().map_err(|error| {
            format!("RAD_BENCHMARK_REQUEST_TIMEOUT_SECONDS is invalid: {error}")
        })?,
        None => DEFAULT_REQUEST_TIMEOUT_SECONDS,
    };
    if seconds == 0 {
        return Err("RAD_BENCHMARK_REQUEST_TIMEOUT_SECONDS must be positive".into());
    }
    Ok(seconds)
}

/// A port for a listener that is not part of the public/admin pair.
///
/// Drawn from a range the pair allocator never touches: the pair allocator
/// steps by two, so any port adjacent to a reserved pair is another pair's
/// public port and would collide as soon as two tests run together.
pub(crate) fn reserve_extra_port() -> TestResult<(u16, TcpListener)> {
    for _ in 0..EXTRA_PORT_COUNT {
        let offset = NEXT_EXTRA_PORT.fetch_add(1, Ordering::Relaxed) % EXTRA_PORT_COUNT;
        let port = 30_000 + offset as u16;
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => return Ok((port, listener)),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err("could not reserve a port outside the public and admin pair range".into())
}

pub(crate) fn reserve_port_pair() -> TestResult<(u16, TcpListener, TcpListener)> {
    for _ in 0..PORT_PAIR_COUNT {
        let offset = NEXT_PORT_PAIR.fetch_add(1, Ordering::Relaxed) % PORT_PAIR_COUNT;
        let port = 12_000 + (offset as u16 * 2);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_timeout_uses_the_default_and_validates_an_override() {
        assert_eq!(
            request_timeout_seconds_from(None).unwrap(),
            DEFAULT_REQUEST_TIMEOUT_SECONDS
        );
        assert_eq!(request_timeout_seconds_from(Some("240")).unwrap(), 240);
        assert!(request_timeout_seconds_from(Some("0")).is_err());
        assert!(request_timeout_seconds_from(Some("invalid")).is_err());
    }
}
