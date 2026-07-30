use std::env;
use std::io::{BufRead, BufReader, Write as _};
use std::process::{Command, Stdio};
use std::sync::Arc;

use bytes::Bytes;
use rad::engine::kv::slatedb::Store;
use rad::engine::kv::{IsolationLevel, Kv, TransactionalKv};
use slatedb::object_store::ObjectStore;
use slatedb::object_store::local::LocalFileSystem;
use tempfile::TempDir;

mod support;

use support::s3::{RustFs, S3Config, TestResult, object_store};

const READY: &str = "RAD_STORAGE_CRASH_READY";
const BASELINE_KEY: &[u8] = b"crash/baseline";
const FIRST_KEY: &[u8] = b"crash/first";
const SECOND_KEY: &[u8] = b"crash/second";

#[derive(Clone, Copy, Debug)]
enum CrashPoint {
    ActiveTransaction,
    CommitAcknowledged,
}

impl CrashPoint {
    fn as_env(self) -> &'static str {
        match self {
            Self::ActiveTransaction => "active-transaction",
            Self::CommitAcknowledged => "commit-acknowledged",
        }
    }

    fn from_env(value: &str) -> Self {
        match value {
            "active-transaction" => Self::ActiveTransaction,
            "commit-acknowledged" => Self::CommitAcknowledged,
            other => panic!("unknown crash point {other}"),
        }
    }
}

#[tokio::test]
async fn local_backend_recovers_from_real_process_death() -> TestResult {
    for point in [
        CrashPoint::ActiveTransaction,
        CrashPoint::CommitAcknowledged,
    ] {
        let directory = TempDir::new()?;
        let objects: Arc<dyn ObjectStore> =
            Arc::new(LocalFileSystem::new_with_prefix(directory.path())?);
        let path = format!("local-{}", point.as_env());
        seed(objects.clone(), &path).await?;

        kill_child_at(point, &path, |command| {
            command
                .env("RAD_CRASH_BACKEND", "local")
                .env("RAD_CRASH_LOCAL_ROOT", directory.path());
        })?;

        audit_after_crash(objects, &path, point).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires a Docker daemon or RAD_TEST_S3_ENDPOINT"]
async fn rustfs_backend_recovers_from_real_process_death() -> TestResult {
    let rustfs = RustFs::start_or_external().await?;
    for point in [
        CrashPoint::ActiveTransaction,
        CrashPoint::CommitAcknowledged,
    ] {
        let path = format!("rustfs-{}", point.as_env());
        seed(object_store(&rustfs.config)?, &path).await?;

        kill_child_at(point, &path, |command| {
            command
                .env("RAD_CRASH_BACKEND", "s3")
                .env("RAD_CRASH_S3_ENDPOINT", &rustfs.config.endpoint)
                .env("RAD_CRASH_S3_BUCKET", &rustfs.config.bucket)
                .env("RAD_CRASH_S3_REGION", &rustfs.config.region)
                .env("RAD_CRASH_S3_ACCESS_KEY_ID", &rustfs.config.access_key)
                .env("RAD_CRASH_S3_SECRET_ACCESS_KEY", &rustfs.config.secret_key);
        })?;

        audit_after_crash(object_store(&rustfs.config)?, &path, point).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "internal child-process probe"]
async fn crash_probe_child() -> TestResult {
    let point = CrashPoint::from_env(&env::var("RAD_CRASH_POINT")?);
    let path = env::var("RAD_CRASH_PATH")?;
    let objects = child_object_store()?;
    let store = Store::open(path, objects).await?;
    let transaction = store.begin(IsolationLevel::SerializableSnapshot).await?;
    transaction.put(Bytes::from_static(FIRST_KEY), Bytes::from_static(b"first"))?;
    transaction.put(
        Bytes::from_static(SECOND_KEY),
        Bytes::from_static(b"second"),
    )?;

    if matches!(point, CrashPoint::CommitAcknowledged) {
        transaction.commit().await?;
    }

    println!("{READY} {}", point.as_env());
    std::io::stdout().flush()?;
    std::future::pending::<()>().await;
    Ok(())
}

fn child_object_store() -> TestResult<Arc<dyn ObjectStore>> {
    match env::var("RAD_CRASH_BACKEND")?.as_str() {
        "local" => Ok(Arc::new(LocalFileSystem::new_with_prefix(env::var(
            "RAD_CRASH_LOCAL_ROOT",
        )?)?)),
        "s3" => object_store(&S3Config {
            endpoint: env::var("RAD_CRASH_S3_ENDPOINT")?,
            bucket: env::var("RAD_CRASH_S3_BUCKET")?,
            region: env::var("RAD_CRASH_S3_REGION")?,
            access_key: env::var("RAD_CRASH_S3_ACCESS_KEY_ID")?,
            secret_key: env::var("RAD_CRASH_S3_SECRET_ACCESS_KEY")?,
        }),
        backend => Err(format!("unknown crash backend {backend}").into()),
    }
}

fn kill_child_at(
    point: CrashPoint,
    path: &str,
    configure: impl FnOnce(&mut Command),
) -> TestResult {
    let executable = env::current_exe()?;
    let mut command = Command::new(executable);
    command
        .arg("crash_probe_child")
        .arg("--exact")
        .arg("--ignored")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env("RAD_CRASH_POINT", point.as_env())
        .env("RAD_CRASH_PATH", path)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    configure(&mut command);

    let mut child = command.spawn()?;
    let stdout = child.stdout.take().expect("child stdout is piped");
    let mut reached = false;
    let marker = format!("{READY} {}", point.as_env());
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        if line.contains(&marker) {
            reached = true;
            break;
        }
    }
    if !reached {
        let status = child.wait()?;
        return Err(format!(
            "crash probe exited before {} checkpoint: {status}",
            point.as_env()
        )
        .into());
    }

    child.kill()?;
    let status = child.wait()?;
    if status.success() {
        return Err("crash probe unexpectedly exited successfully".into());
    }
    Ok(())
}

async fn seed(objects: Arc<dyn ObjectStore>, path: &str) -> TestResult {
    let store = Store::open(path, objects).await?;
    store
        .put(
            Bytes::from_static(BASELINE_KEY),
            Bytes::from_static(b"durable"),
        )
        .await?;
    store.close().await?;
    Ok(())
}

async fn audit_after_crash(
    objects: Arc<dyn ObjectStore>,
    path: &str,
    point: CrashPoint,
) -> TestResult {
    for _ in 0..2 {
        let store = Store::open(path, objects.clone()).await?;
        assert_eq!(
            store.get(BASELINE_KEY).await?,
            Some(Bytes::from_static(b"durable"))
        );
        let expected = match point {
            CrashPoint::ActiveTransaction => None,
            CrashPoint::CommitAcknowledged => Some(Bytes::from_static(b"first")),
        };
        assert_eq!(store.get(FIRST_KEY).await?, expected);
        let expected = match point {
            CrashPoint::ActiveTransaction => None,
            CrashPoint::CommitAcknowledged => Some(Bytes::from_static(b"second")),
        };
        assert_eq!(store.get(SECOND_KEY).await?, expected);
        store.close().await?;
    }
    Ok(())
}
