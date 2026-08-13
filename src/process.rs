//! Thin process assembly around the reusable engine, scheduler, and HTTP API.

use std::env;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use slatedb::object_store::ObjectStore;
use slatedb::object_store::aws::AmazonS3Builder;
use slatedb::object_store::local::LocalFileSystem;
use slatedb::object_store::memory::InMemory;

use crate::engine::catalog::Catalog;
use crate::engine::catalog::model::Mode;
use crate::engine::exec::Engine;
use crate::engine::kv::slatedb::{ReaderStore, Store};
use crate::engine::kv::{Kv, TransactionalKv};
use crate::scheduler::schema_jobs::{SchemaJobConfig, SchemaJobRunner};

pub type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StorageConfig {
    Memory {
        path: String,
    },
    File {
        directory: PathBuf,
        path: String,
    },
    S3 {
        bucket: String,
        path: String,
        region: Option<String>,
        endpoint: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Frontend {
    Postgres,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Role {
    Read,
    #[default]
    Write,
}

impl std::str::FromStr for Role {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "read" => Ok(Self::Read),
            "write" => Ok(Self::Write),
            _ => Err(format!("unknown role {value:?} (read or write)")),
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Read => "read",
            Self::Write => "write",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub address: String,
    pub catalog_mode: Option<Mode>,
    pub frontend: Option<Frontend>,
    pub postgres_address: String,
    pub reader_poll_interval: Duration,
    pub role: Role,
    pub storage: StorageConfig,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let address = normalize_address(&env_or("RAD_ADDR", "0.0.0.0:7237"));
        let catalog_mode = env::var("RAD_CATALOG_MODE")
            .ok()
            .filter(|value| !value.is_empty())
            .map(|value| value.parse())
            .transpose()?;
        let frontend = match env::var("RAD_FRONTEND").ok().as_deref() {
            None | Some("") => None,
            Some("postgres") => Some(Frontend::Postgres),
            Some(value) => {
                return Err(format!("unknown RAD_FRONTEND {value:?} (postgres)").into());
            }
        };
        let postgres_address = normalize_address(&env_or("RAD_POSTGRES_ADDR", "0.0.0.0:5432"));
        let reader_poll_interval = Duration::from_millis(
            env_or("RAD_READER_POLL_INTERVAL_MS", "1000")
                .parse::<u64>()
                .map_err(|error| format!("invalid RAD_READER_POLL_INTERVAL_MS: {error}"))?,
        );
        if reader_poll_interval.is_zero() {
            return Err("RAD_READER_POLL_INTERVAL_MS must be greater than zero".into());
        }
        let role = env_or("RAD_ROLE", "write").parse()?;
        let path = env_or("RAD_STORAGE_PATH", "rad");
        let storage = match env_or("RAD_STORAGE", "file").as_str() {
            "memory" => StorageConfig::Memory { path },
            "file" => StorageConfig::File {
                directory: PathBuf::from(env_or("RAD_DATA_DIR", "data")),
                path,
            },
            "s3" => StorageConfig::S3 {
                bucket: required_env("RAD_S3_BUCKET")?,
                path: env_or("RAD_S3_PREFIX", &path),
                region: env::var("RAD_S3_REGION")
                    .ok()
                    .filter(|value| !value.is_empty()),
                endpoint: env::var("RAD_S3_ENDPOINT")
                    .ok()
                    .filter(|value| !value.is_empty()),
            },
            backend => {
                return Err(
                    format!("unknown RAD_STORAGE {backend:?} (memory, file, or s3)").into(),
                );
            }
        };
        Ok(Self {
            address,
            catalog_mode,
            frontend,
            postgres_address,
            reader_poll_interval,
            role,
            storage,
        })
    }
}

/// Construct the production runtime and serve until `shutdown` resolves.
///
/// Durable schema work starts before the listener and is stopped before the
/// Slate store closes. Store close is awaited even when the HTTP server exits
/// with an error, preserving Slate's orderly-shutdown contract.
pub async fn serve(config: Config, shutdown: impl Future<Output = ()> + Send + 'static) -> Result {
    let listener = tokio::net::TcpListener::bind(&config.address).await?;
    let admin_address = admin_address(&config.address)?;
    let admin_listener = tokio::net::TcpListener::bind(&admin_address).await?;
    let postgres_listener = match config.frontend {
        Some(Frontend::Postgres) => {
            Some(tokio::net::TcpListener::bind(&config.postgres_address).await?)
        }
        None => None,
    };
    let opened = open_storage(&config.storage, config.role, config.reader_poll_interval).await?;
    let store = opened.store;
    let writer = opened.writer;
    let location = opened.location;
    let catalog = Arc::new(Catalog::new(store.clone()));
    let mode = match open_catalog_mode(&catalog, config.catalog_mode, config.role).await {
        Ok(mode) => mode,
        Err(error) => return close_after_error(store.as_ref(), error).await,
    };
    let engine = Arc::new(match config.role {
        Role::Read => Engine::read_only(store.clone()),
        Role::Write => Engine::new(store.clone()),
    });
    let jobs = if config.role == Role::Write {
        let jobs = match SchemaJobRunner::start(engine.clone(), SchemaJobConfig::default()) {
            Ok(jobs) => jobs,
            Err(error) => return close_after_error(store.as_ref(), Box::new(error)).await,
        };
        jobs.observe_catalog(&catalog);
        Some(jobs)
    } else {
        None
    };

    let public_address = listener.local_addr()?;
    let bound_admin_address = admin_listener.local_addr()?;
    eprintln!(
        "rad serving on {public_address} (storage: {location}, catalog: {mode:?}, role: {})",
        config.role
    );
    eprintln!("admin UI on http://{bound_admin_address}");
    if let Some(listener) = &postgres_listener {
        eprintln!("postgres frontend on {}", listener.local_addr()?);
    }

    let (stop_sender, stop_receiver) = tokio::sync::watch::channel(false);
    let shutdown_sender = stop_sender.clone();
    let shutdown_task = tokio::spawn(async move {
        shutdown.await;
        let _ = shutdown_sender.send(true);
    });
    let mut servers = tokio::task::JoinSet::new();
    if let Some(writer) = writer {
        let writer_stop = stop_receiver.clone();
        let writer_stop_sender = stop_sender.clone();
        servers.spawn(async move {
            monitor_writer_fence(writer, writer_stop, writer_stop_sender).await
        });
    }
    let http_engine = engine.clone();
    let http_stop = stop_receiver.clone();
    servers.spawn(async move {
        crate::http::serve(
            listener,
            crate::http::router_with_location(http_engine, mode, location),
            wait_for_stop(http_stop),
        )
        .await
    });
    let admin_store = store.clone();
    let admin_stop = stop_receiver.clone();
    servers.spawn(async move {
        crate::http::serve(
            admin_listener,
            crate::admin::router(admin_store),
            wait_for_stop(admin_stop),
        )
        .await
    });
    if let Some(listener) = postgres_listener {
        let postgres_stop = stop_receiver;
        let postgres = crate::postgres::Server::new(engine, catalog.clone(), mode);
        servers
            .spawn(async move { crate::postgres::serve(listener, postgres, postgres_stop).await });
    }
    let mut server_result = Ok(());
    if let Some(result) = servers.join_next().await {
        server_result = joined_server(result);
        let _ = stop_sender.send(true);
    }
    while let Some(result) = servers.join_next().await {
        server_result = combine_servers(server_result, joined_server(result));
    }
    shutdown_task.abort();
    let scheduler_result = match jobs {
        Some(jobs) => jobs.shutdown().await,
        None => Ok(()),
    };
    let close_result = store.close().await;

    server_result?;
    scheduler_result?;
    close_result?;
    Ok(())
}

async fn wait_for_stop(mut receiver: tokio::sync::watch::Receiver<bool>) {
    if *receiver.borrow() {
        return;
    }
    while receiver.changed().await.is_ok() {
        if *receiver.borrow() {
            return;
        }
    }
}

fn combine_servers(first: std::io::Result<()>, second: std::io::Result<()>) -> std::io::Result<()> {
    first.and(second)
}

fn joined_server(
    result: std::result::Result<std::io::Result<()>, tokio::task::JoinError>,
) -> std::io::Result<()> {
    result.map_err(std::io::Error::other)?
}

pub async fn run() -> Result {
    serve(Config::from_env()?, shutdown_signal()).await
}

struct OpenedStorage {
    store: Arc<dyn TransactionalKv>,
    writer: Option<Arc<Store>>,
    location: String,
}

async fn open_storage(
    config: &StorageConfig,
    role: Role,
    reader_poll_interval: Duration,
) -> Result<OpenedStorage> {
    let (path, objects, location): (String, Arc<dyn ObjectStore>, String) = match config {
        StorageConfig::Memory { path } => {
            (path.clone(), Arc::new(InMemory::new()), "memory:///".into())
        }
        StorageConfig::File { directory, path } => {
            std::fs::create_dir_all(directory)?;
            let directory = directory.canonicalize()?;
            let objects: Arc<dyn ObjectStore> =
                Arc::new(LocalFileSystem::new_with_prefix(&directory)?);
            (
                path.clone(),
                objects,
                directory.join(path).display().to_string(),
            )
        }
        StorageConfig::S3 {
            bucket,
            path,
            region,
            endpoint,
        } => {
            let mut builder = AmazonS3Builder::from_env().with_bucket_name(bucket);
            if let Some(region) = region {
                builder = builder.with_region(region);
            }
            if let Some(endpoint) = endpoint {
                builder = builder
                    .with_endpoint(endpoint)
                    .with_allow_http(endpoint.starts_with("http://"))
                    .with_virtual_hosted_style_request(false);
            }
            let objects: Arc<dyn ObjectStore> = Arc::new(builder.build()?);
            (path.clone(), objects, format!("s3://{bucket}/{path}"))
        }
    };
    match role {
        Role::Write => {
            let writer = Arc::new(Store::open(path, objects).await?);
            let store: Arc<dyn TransactionalKv> = writer.clone();
            Ok(OpenedStorage {
                store,
                writer: Some(writer),
                location,
            })
        }
        Role::Read => {
            let store: Arc<dyn TransactionalKv> =
                Arc::new(ReaderStore::open(path, objects, reader_poll_interval).await?);
            Ok(OpenedStorage {
                store,
                writer: None,
                location,
            })
        }
    }
}

async fn open_catalog_mode(catalog: &Catalog, requested: Option<Mode>, role: Role) -> Result<Mode> {
    match role {
        Role::Write => Ok(catalog.init_mode(requested).await?),
        Role::Read => {
            let stored = catalog.mode().await?;
            if let Some(requested) = requested
                && requested != stored
            {
                return Err(format!(
                    "requested catalog mode {requested:?} does not match stored mode {stored:?}"
                )
                .into());
            }
            Ok(stored)
        }
    }
}

async fn monitor_writer_fence(
    writer: Arc<Store>,
    mut stop: tokio::sync::watch::Receiver<bool>,
    stop_sender: tokio::sync::watch::Sender<bool>,
) -> std::io::Result<()> {
    let token = Bytes::from(uuid::Uuid::new_v4().into_bytes().to_vec());
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return Ok(());
                }
            }
            _ = interval.tick() => {
                if let Err(error) = Kv::put(
                    writer.as_ref(),
                    Bytes::from_static(b"/rad/runtime/writer-fence"),
                    token.clone(),
                ).await {
                    let _ = stop_sender.send(true);
                    return Err(std::io::Error::other(format!(
                        "Slate writer lost ownership: {error}"
                    )));
                }
            }
        }
    }
}

async fn close_after_error(
    store: &dyn TransactionalKv,
    error: Box<dyn std::error::Error + Send + Sync>,
) -> Result {
    let original = error.to_string();
    match store.close().await {
        Ok(()) => Err(error),
        Err(close) => Err(format!("{original}; orderly Slate close also failed: {close}").into()),
    }
}

fn env_or(name: &str, fallback: &str) -> String {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

pub(crate) fn normalize_address(address: &str) -> String {
    address
        .strip_prefix(':')
        .map_or_else(|| address.to_owned(), |port| format!("0.0.0.0:{port}"))
}

fn admin_address(address: &str) -> Result<String> {
    let (host, port) = address
        .rsplit_once(':')
        .ok_or_else(|| format!("cannot derive admin address from {address:?}"))?;
    if host.is_empty() {
        return Err(format!("cannot derive admin address from {address:?}").into());
    }
    let port = port
        .parse::<u16>()
        .map_err(|error| format!("invalid listen port in {address:?}: {error}"))?;
    let admin_port = if port == 0 {
        0
    } else {
        port.checked_add(1)
            .ok_or_else(|| format!("cannot derive admin port after {port}"))?
    };
    Ok(format!("{host}:{admin_port}"))
}

fn required_env(name: &str) -> Result<String> {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("RAD_STORAGE=s3 requires {name}").into())
}

pub(crate) async fn shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    {
        let terminate = async {
            if let Ok(mut signal) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                signal.recv().await;
            }
        };
        tokio::select! {
            () = interrupt => {},
            () = terminate => {},
        }
    }
    #[cfg(not(unix))]
    interrupt.await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_process_starts_and_closes_all_runtime_components() {
        let config = Config {
            address: "127.0.0.1:0".into(),
            catalog_mode: Some(Mode::Schema),
            frontend: Some(Frontend::Postgres),
            postgres_address: "127.0.0.1:0".into(),
            reader_poll_interval: Duration::from_millis(10),
            role: Role::Write,
            storage: StorageConfig::Memory {
                path: "process-lifecycle".into(),
            },
        };
        serve(config, std::future::ready(())).await.unwrap();
    }

    #[tokio::test]
    async fn file_process_reopens_the_same_catalog_mode() {
        let directory = tempfile::tempdir().unwrap();
        let storage = StorageConfig::File {
            directory: directory.path().into(),
            path: "database".into(),
        };
        serve(
            Config {
                address: "127.0.0.1:0".into(),
                catalog_mode: Some(Mode::Direct),
                frontend: None,
                postgres_address: "127.0.0.1:0".into(),
                reader_poll_interval: Duration::from_millis(10),
                role: Role::Write,
                storage: storage.clone(),
            },
            std::future::ready(()),
        )
        .await
        .unwrap();
        serve(
            Config {
                address: "127.0.0.1:0".into(),
                catalog_mode: None,
                frontend: None,
                postgres_address: "127.0.0.1:0".into(),
                reader_poll_interval: Duration::from_millis(10),
                role: Role::Write,
                storage,
            },
            std::future::ready(()),
        )
        .await
        .unwrap();
    }

    #[test]
    fn colon_prefixed_listener_addresses_bind_all_interfaces() {
        assert_eq!(normalize_address(":7237"), "0.0.0.0:7237");
        assert_eq!(normalize_address("127.0.0.1:0"), "127.0.0.1:0");
    }

    #[test]
    fn admin_listener_uses_the_next_port_and_preserves_the_host() {
        assert_eq!(admin_address("0.0.0.0:7237").unwrap(), "0.0.0.0:7238");
        assert_eq!(admin_address("127.0.0.1:8000").unwrap(), "127.0.0.1:8001");
        assert_eq!(admin_address("[::1]:7237").unwrap(), "[::1]:7238");
        assert_eq!(admin_address("127.0.0.1:0").unwrap(), "127.0.0.1:0");
        assert!(admin_address("127.0.0.1:65535").is_err());
    }
}
