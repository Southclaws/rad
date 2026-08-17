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
use crate::engine::kv::{Closure, ErrorKind as KvErrorKind, Kv, TransactionalKv};
use crate::health::{Health, StartupHold};
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
    /// Admin listener address; `None` derives the next port on the public
    /// host. Orchestrated deployments pin this to loopback so the admin
    /// surface is reachable only through the orchestrator's own tunnel.
    pub admin_address: Option<String>,
    pub catalog_mode: Option<Mode>,
    pub frontend: Option<Frontend>,
    pub postgres_address: String,
    /// Bound on the orderly Slate close; `None` waits indefinitely. Commits are
    /// durable before they are acknowledged, so abandoning a close that cannot
    /// finish — a destroyed bucket retries longer than any termination grace —
    /// forfeits only background housekeeping, never acknowledged data.
    pub close_timeout: Option<Duration>,
    pub reader_poll_interval: Duration,
    pub role: Role,
    /// How long readiness reports unavailable before the listeners stop, so an
    /// orchestrator can move traffic elsewhere while requests still succeed.
    pub shutdown_drain: Duration,
    pub storage: StorageConfig,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let address = normalize_address(&env_or("RAD_ADDR", "0.0.0.0:7237"));
        let admin_address = admin_address_from_env();
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
        let close_timeout = close_timeout_from_env()?;
        let shutdown_drain = shutdown_drain_from_env()?;
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
            admin_address,
            catalog_mode,
            close_timeout,
            frontend,
            postgres_address,
            reader_poll_interval,
            role,
            shutdown_drain,
            storage,
        })
    }
}

/// Construct the production runtime and serve until `shutdown` resolves.
///
/// Durable schema work starts before the listener and is stopped before the
/// Slate store closes. Store close is awaited even when the HTTP server exits
/// with an error, preserving Slate's orderly-shutdown contract.
///
/// Only the probe endpoints answer on the public address until the runtime is
/// built, so a slow or unavailable object store never publishes a partially
/// initialized database.
pub async fn serve(config: Config, shutdown: impl Future<Output = ()> + Send + 'static) -> Result {
    let health = Health::starting(crate::health::STORAGE_FRESHNESS);
    let (listener, startup_listener) = bind_public(&config.address).await?;
    let admin_address = match &config.admin_address {
        Some(address) => address.clone(),
        None => admin_address(&config.address)?,
    };
    let admin_listener = tokio::net::TcpListener::bind(&admin_address).await?;
    let postgres_listener = match config.frontend {
        Some(Frontend::Postgres) => {
            Some(tokio::net::TcpListener::bind(&config.postgres_address).await?)
        }
        None => None,
    };

    let (started_sender, started_receiver) = tokio::sync::watch::channel(false);
    let startup_probes = tokio::spawn(crate::http::serve(
        startup_listener,
        crate::http::probe_router(health.clone()),
        wait_for_stop(started_receiver),
    ));
    let started = start_runtime(&config, &health).await;
    let _ = started_sender.send(true);
    let handover = joined_server(startup_probes.await);
    let Runtime {
        store,
        writer,
        location,
        catalog,
        engine,
        mode,
        jobs,
    } = started?;
    handover?;

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
    health.serve();

    let (stop_sender, stop_receiver) = tokio::sync::watch::channel(false);
    let shutdown_sender = stop_sender.clone();
    let shutdown_health = health.clone();
    let shutdown_drain = config.shutdown_drain;
    let shutdown_task = tokio::spawn(async move {
        shutdown.await;
        shutdown_health.drain();
        tokio::time::sleep(shutdown_drain).await;
        let _ = shutdown_sender.send(true);
    });
    let mut servers = tokio::task::JoinSet::new();
    let monitor_store = store.clone();
    let monitor_jobs = jobs.clone();
    let monitor_health = health.clone();
    let monitor_stop = stop_receiver.clone();
    let monitor_stop_sender = stop_sender.clone();
    servers.spawn(async move {
        monitor_runtime(
            monitor_store,
            writer,
            monitor_jobs,
            monitor_health,
            shutdown_drain,
            monitor_stop,
            monitor_stop_sender,
        )
        .await
    });
    let http_engine = engine.clone();
    let http_health = health.clone();
    let http_stop = stop_receiver.clone();
    servers.spawn(async move {
        crate::http::serve(
            listener,
            crate::http::router_with_health(http_engine, mode, location, http_health),
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
        health.drain();
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
    let close_result = close_store(store.as_ref(), config.close_timeout).await;

    server_result?;
    scheduler_result?;
    close_result?;
    Ok(())
}

async fn close_store(store: &dyn TransactionalKv, timeout: Option<Duration>) -> Result {
    let Some(limit) = timeout else {
        return Ok(store.close().await?);
    };
    match tokio::time::timeout(limit, store.close()).await {
        Ok(result) => Ok(result?),
        Err(_) => Err(format!(
            "orderly Slate close did not finish within {limit:?}; abandoning storage housekeeping"
        )
        .into()),
    }
}

/// Bind the public address once and return two handles on the same accept
/// queue: one for the startup probe server, one for the database itself. The
/// port is claimed before storage is touched and never released between the
/// two, so nothing can take it during the handover.
async fn bind_public(address: &str) -> Result<(tokio::net::TcpListener, tokio::net::TcpListener)> {
    let bound = tokio::net::TcpListener::bind(address).await?.into_std()?;
    let duplicate = bound.try_clone()?;
    Ok((
        tokio::net::TcpListener::from_std(bound)?,
        tokio::net::TcpListener::from_std(duplicate)?,
    ))
}

struct Runtime {
    store: Arc<dyn TransactionalKv>,
    writer: Option<Arc<Store>>,
    location: String,
    catalog: Arc<Catalog>,
    engine: Arc<Engine>,
    mode: Mode,
    jobs: Option<Arc<SchemaJobRunner>>,
}

/// Any failure closes Slate before returning, so a process that never publishes
/// still leaves its storage location cleanly.
///
/// A preflight monitors a remote store while it opens and records the cause
/// of a held startup. SlateDB retries an unavailable object store without
/// limit, so the open does not return an error to classify.
async fn start_runtime(config: &Config, health: &Arc<Health>) -> Result<Runtime> {
    let built = build_objects(&config.storage)?;
    let preflight = built.remote.then(|| {
        tokio::spawn(diagnose_startup_hold(
            built.objects.clone(),
            built.path.clone(),
            health.clone(),
        ))
    });
    let runtime = open_runtime(config, &built).await;
    if let Some(preflight) = preflight {
        preflight.abort();
    }
    runtime
}

async fn open_runtime(config: &Config, built: &BuiltObjects) -> Result<Runtime> {
    let opened = open_storage(built, config.role, config.reader_poll_interval).await?;
    let store = opened.store;
    if let Err(error) = admit_storage_compatibility(store.as_ref(), config.role).await {
        return close_after_error(store.as_ref(), config.close_timeout, error).await;
    }
    let catalog = Arc::new(Catalog::new(store.clone()));
    let mode = match open_catalog_mode(&catalog, config.catalog_mode, config.role).await {
        Ok(mode) => mode,
        Err(error) => return close_after_error(store.as_ref(), config.close_timeout, error).await,
    };
    let engine = Arc::new(match config.role {
        Role::Read => Engine::read_only(store.clone()),
        Role::Write => Engine::new(store.clone()),
    });
    let jobs = if config.role == Role::Write {
        let jobs = match SchemaJobRunner::start(engine.clone(), SchemaJobConfig::default()) {
            Ok(jobs) => jobs,
            Err(error) => {
                return close_after_error(store.as_ref(), config.close_timeout, Box::new(error))
                    .await;
            }
        };
        jobs.observe_catalog(&catalog);
        Some(Arc::new(jobs))
    } else {
        None
    };
    Ok(Runtime {
        store,
        writer: opened.writer,
        location: opened.location,
        catalog,
        engine,
        mode,
        jobs,
    })
}

async fn diagnose_startup_hold(objects: Arc<dyn ObjectStore>, path: String, health: Arc<Health>) {
    let prefix = slatedb::object_store::path::Path::from(path.as_str());
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        interval.tick().await;
        let listed = tokio::time::timeout(
            Duration::from_secs(4),
            objects.list_with_delimiter(Some(&prefix)),
        )
        .await;
        let hold = classify_startup_hold(listed);
        if health.observe_startup_hold(hold) && hold != StartupHold::Opening {
            eprintln!("startup held: {}", hold.as_str());
        }
    }
}

fn classify_startup_hold(
    listed: std::result::Result<
        slatedb::object_store::Result<slatedb::object_store::ListResult>,
        tokio::time::error::Elapsed,
    >,
) -> StartupHold {
    use slatedb::object_store::Error as StoreError;
    match listed {
        Ok(Ok(_)) => StartupHold::Opening,
        // A prefix listing never reports NotFound for an empty prefix; only a
        // missing bucket does.
        Ok(Err(StoreError::NotFound { .. })) => StartupHold::BucketMissing,
        Ok(Err(StoreError::Unauthenticated { .. } | StoreError::PermissionDenied { .. })) => {
            StartupHold::Unauthorized
        }
        Ok(Err(_)) | Err(_) => StartupHold::Unreachable,
    }
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

struct BuiltObjects {
    path: String,
    objects: Arc<dyn ObjectStore>,
    location: String,
    /// Whether the store is remote enough for a startup preflight to be
    /// meaningful: local backends fail fast on their own.
    remote: bool,
}

fn build_objects(config: &StorageConfig) -> Result<BuiltObjects> {
    let (path, objects, location, remote): (String, Arc<dyn ObjectStore>, String, bool) =
        match config {
            StorageConfig::Memory { path } => (
                path.clone(),
                Arc::new(InMemory::new()),
                "memory:///".into(),
                false,
            ),
            StorageConfig::File { directory, path } => {
                std::fs::create_dir_all(directory)?;
                let directory = directory.canonicalize()?;
                let objects: Arc<dyn ObjectStore> =
                    Arc::new(LocalFileSystem::new_with_prefix(&directory)?);
                (
                    path.clone(),
                    objects,
                    directory.join(path).display().to_string(),
                    false,
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
                (path.clone(), objects, format!("s3://{bucket}/{path}"), true)
            }
        };
    Ok(BuiltObjects {
        path,
        objects,
        location,
        remote,
    })
}

async fn open_storage(
    built: &BuiltObjects,
    role: Role,
    reader_poll_interval: Duration,
) -> Result<OpenedStorage> {
    let path = built.path.clone();
    let objects = built.objects.clone();
    match role {
        Role::Write => {
            let writer = Arc::new(Store::open(path, objects).await?);
            let store: Arc<dyn TransactionalKv> = writer.clone();
            Ok(OpenedStorage {
                store,
                writer: Some(writer),
                location: built.location.clone(),
            })
        }
        Role::Read => {
            let store: Arc<dyn TransactionalKv> =
                Arc::new(ReaderStore::open(path, objects, reader_poll_interval).await?);
            Ok(OpenedStorage {
                store,
                writer: None,
                location: built.location.clone(),
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

/// Keep the process's health current: refresh the storage observation that
/// readiness reads, hold the storage location for a writer, and watch the
/// background workers liveness depends on.
///
/// A storage fault that may recover only withdraws traffic, because restarting
/// the process returns it to the same object store. Losing the storage location
/// or the store itself is terminal: the process drains and stops.
async fn monitor_runtime(
    store: Arc<dyn TransactionalKv>,
    writer: Option<Arc<Store>>,
    jobs: Option<Arc<SchemaJobRunner>>,
    health: Arc<Health>,
    drain: Duration,
    mut stop: tokio::sync::watch::Receiver<bool>,
    stop_sender: tokio::sync::watch::Sender<bool>,
) -> std::io::Result<()> {
    let token = Bytes::from(uuid::Uuid::new_v4().into_bytes().to_vec());
    let mut interval = tokio::time::interval(crate::health::STORAGE_OBSERVATION_INTERVAL);
    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return Ok(());
                }
            }
            _ = interval.tick() => {
                if jobs.as_ref().is_some_and(|jobs| jobs.worker_lost()) {
                    health.observe_task_failure();
                    return stop_after_drain(
                        &health,
                        &stop_sender,
                        drain,
                        "the schema job runner stopped without being asked to".to_owned(),
                    )
                    .await;
                }
                // The fence key holds runtime state only: a writer rewrites
                // it to hold the storage location, and a reader reads it to
                // observe that storage still answers.
                let fence_key = crate::engine::kv::keys::runtime_writer_fence_key();
                let observation = match &writer {
                    Some(writer) => Kv::put(
                        writer.as_ref(),
                        Bytes::from(fence_key),
                        token.clone(),
                    ).await,
                    None => Kv::get(store.as_ref(), &fence_key).await.map(|_| ()),
                };
                let Err(error) = observation else {
                    health.observe_storage();
                    continue;
                };
                if error.kind() == KvErrorKind::Unavailable {
                    if health.observe_storage_unavailable() {
                        eprintln!("storage is unavailable, withdrawing traffic: {error}");
                    }
                    continue;
                }
                let message = if error.closure() == Some(Closure::Fenced) {
                    health.observe_fenced();
                    format!("Slate writer lost ownership: {error}")
                } else {
                    health.observe_task_failure();
                    format!("Slate storage is unusable: {error}")
                };
                return stop_after_drain(&health, &stop_sender, drain, message).await;
            }
        }
    }
}

/// Report the fault, withdraw traffic for the drain window, then stop every
/// server. The fault is still returned, so the process exits unsuccessfully and
/// a supervisor can act on it.
async fn stop_after_drain(
    health: &Health,
    stop_sender: &tokio::sync::watch::Sender<bool>,
    drain: Duration,
    message: String,
) -> std::io::Result<()> {
    eprintln!("{message}");
    health.drain();
    tokio::time::sleep(drain).await;
    let _ = stop_sender.send(true);
    Err(std::io::Error::other(message))
}

async fn admit_storage_compatibility(
    store: &dyn TransactionalKv,
    role: Role,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use crate::engine::kv::{IsolationLevel, TransactionView};
    let transaction = store.begin(IsolationLevel::Snapshot).await?;
    let writes = role == Role::Write;
    let result = {
        let mut view = TransactionView(&*transaction);
        crate::engine::catalog::store::admit_storage_compatibility(&mut view, writes).await
    };
    match result {
        Ok(()) if writes => transaction.commit().await.map_err(Into::into),
        Ok(()) => {
            transaction.rollback();
            Ok(())
        }
        Err(error) => {
            transaction.rollback();
            Err(Box::new(error).into())
        }
    }
}

async fn close_after_error<T>(
    store: &dyn TransactionalKv,
    timeout: Option<Duration>,
    error: Box<dyn std::error::Error + Send + Sync>,
) -> Result<T> {
    let original = error.to_string();
    match close_store(store, timeout).await {
        Ok(()) => Err(error),
        Err(close) => Err(format!("{original}; orderly Slate close also failed: {close}").into()),
    }
}

pub fn admin_address_from_env() -> Option<String> {
    env::var("RAD_ADMIN_ADDR")
        .ok()
        .filter(|value| !value.is_empty())
        .map(|value| normalize_address(&value))
}

pub fn close_timeout_from_env() -> Result<Option<Duration>> {
    let millis = env_or("RAD_CLOSE_TIMEOUT_MS", "0")
        .parse::<u64>()
        .map_err(|error| format!("invalid RAD_CLOSE_TIMEOUT_MS: {error}"))?;
    Ok((millis > 0).then(|| Duration::from_millis(millis)))
}

/// How long readiness reports unavailable before the listeners stop.
///
/// The window belongs to whoever runs the process: an orchestrator sets it long
/// enough for its own readiness period to notice, while an interactive `rad
/// serve` should stop as soon as it is interrupted. The default is therefore no
/// drain at all.
pub fn shutdown_drain_from_env() -> Result<Duration> {
    Ok(Duration::from_millis(
        env_or("RAD_SHUTDOWN_DRAIN_MS", "0")
            .parse::<u64>()
            .map_err(|error| format!("invalid RAD_SHUTDOWN_DRAIN_MS: {error}"))?,
    ))
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
            admin_address: Some("127.0.0.1:0".into()),
            close_timeout: Some(Duration::from_secs(30)),
            catalog_mode: Some(Mode::Schema),
            frontend: Some(Frontend::Postgres),
            postgres_address: "127.0.0.1:0".into(),
            reader_poll_interval: Duration::from_millis(10),
            role: Role::Write,
            shutdown_drain: Duration::ZERO,
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
                admin_address: None,
                close_timeout: None,
                catalog_mode: Some(Mode::Direct),
                frontend: None,
                postgres_address: "127.0.0.1:0".into(),
                reader_poll_interval: Duration::from_millis(10),
                role: Role::Write,
                shutdown_drain: Duration::ZERO,
                storage: storage.clone(),
            },
            std::future::ready(()),
        )
        .await
        .unwrap();
        serve(
            Config {
                address: "127.0.0.1:0".into(),
                admin_address: None,
                close_timeout: None,
                catalog_mode: None,
                frontend: None,
                postgres_address: "127.0.0.1:0".into(),
                reader_poll_interval: Duration::from_millis(10),
                role: Role::Write,
                shutdown_drain: Duration::ZERO,
                storage,
            },
            std::future::ready(()),
        )
        .await
        .unwrap();
    }

    #[test]
    fn startup_hold_classification_maps_every_preflight_outcome() {
        use slatedb::object_store::Error as StoreError;

        let unauthorized = classify_startup_hold(Ok(Err(StoreError::Unauthenticated {
            path: "rad".into(),
            source: "invalid key".into(),
        })));
        assert_eq!(unauthorized, StartupHold::Unauthorized);
        let denied = classify_startup_hold(Ok(Err(StoreError::PermissionDenied {
            path: "rad".into(),
            source: "access denied".into(),
        })));
        assert_eq!(denied, StartupHold::Unauthorized);
        let missing = classify_startup_hold(Ok(Err(StoreError::NotFound {
            path: "rad".into(),
            source: "NoSuchBucket".into(),
        })));
        assert_eq!(missing, StartupHold::BucketMissing);
        let unreachable = classify_startup_hold(Ok(Err(StoreError::Generic {
            store: "S3",
            source: "connection refused".into(),
        })));
        assert_eq!(unreachable, StartupHold::Unreachable);
    }

    #[tokio::test]
    async fn startup_hold_timeout_classifies_as_unreachable() {
        let elapsed = tokio::time::timeout(Duration::ZERO, std::future::pending::<()>())
            .await
            .expect_err("pending future cannot finish instantly");
        let listed: std::result::Result<
            slatedb::object_store::Result<slatedb::object_store::ListResult>,
            tokio::time::error::Elapsed,
        > = Err(elapsed);
        assert_eq!(classify_startup_hold(listed), StartupHold::Unreachable);
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
