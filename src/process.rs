//! Thin process assembly around the reusable engine, scheduler, and HTTP API.

use std::env;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use slatedb::object_store::ObjectStore;
use slatedb::object_store::aws::AmazonS3Builder;
use slatedb::object_store::local::LocalFileSystem;
use slatedb::object_store::memory::InMemory;

use crate::engine::catalog::Catalog;
use crate::engine::catalog::model::Mode;
use crate::engine::exec::Engine;
use crate::engine::kv::slatedb::{
    ObjectCachePreload, Options as SlateOptions, ReaderStore, Store as SlateStore,
};
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
        path: PathBuf,
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
    /// Bound on the orderly storage close; `None` waits indefinitely. Commits are
    /// durable before they are acknowledged, so abandoning a close that cannot
    /// finish — a destroyed bucket retries longer than any termination grace —
    /// forfeits only background housekeeping, never acknowledged data.
    pub close_timeout: Option<Duration>,
    pub reader_poll_interval: Duration,
    pub capture_workload_corpus: bool,
    /// Listen address for the instance-to-instance API. Requires
    /// `relay_token_file`: an unauthenticated port that accepts evidence is
    /// not a configuration this process will serve.
    pub internal_address: Option<String>,
    /// Base URL of the instance this one reports its statistics to.
    pub relay_target: Option<String>,
    pub relay_token_file: Option<PathBuf>,
    /// Certificate and key the internal listener serves. Absent means the
    /// internal API is plain HTTP, which is the development posture.
    pub internal_tls_certificate: Option<PathBuf>,
    pub internal_tls_key: Option<PathBuf>,
    /// Authority a relaying instance verifies the writer against. Absent means
    /// the target is plain HTTP.
    pub relay_authority: Option<PathBuf>,
    /// What this instance calls itself when it relays. Every instance in a
    /// database needs a distinct name, or the receiver cannot tell their
    /// batch sequences apart.
    pub instance_id: Option<String>,
    pub role: Role,
    /// How long readiness reports unavailable before the listeners stop, so an
    /// orchestrator can move traffic elsewhere while requests still succeed.
    pub shutdown_drain: Duration,
    pub slate: SlateOptions,
    pub storage: StorageConfig,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        ensure_storage_path_environment()?;
        let address = normalize_address(&env_or("RAD_ADDR", "0.0.0.0:7237"));
        let slate = slate_options_from_env()?;
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
        let capture_workload_corpus = matches!(
            env::var("RAD_CAPTURE_WORKLOAD_CORPUS").ok().as_deref(),
            Some("1" | "true" | "yes")
        );
        let internal_address = env::var("RAD_INTERNAL_ADDR")
            .ok()
            .filter(|value| !value.is_empty())
            .map(|value| normalize_address(&value));
        let relay_target = env::var("RAD_RELAY_TARGET")
            .ok()
            .filter(|value| !value.is_empty());
        let relay_token_file = env::var("RAD_RELAY_TOKEN_FILE")
            .ok()
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let internal_tls_certificate = env_path("RAD_INTERNAL_TLS_CERT");
        let internal_tls_key = env_path("RAD_INTERNAL_TLS_KEY");
        let relay_authority = env_path("RAD_RELAY_CA");
        let instance_id = env::var("RAD_INSTANCE_ID")
            .ok()
            .filter(|value| !value.is_empty());
        let role = env_or("RAD_ROLE", "write").parse()?;
        let close_timeout = close_timeout_from_env()?;
        let shutdown_drain = shutdown_drain_from_env()?;
        let storage_path = env_or("RAD_STORAGE_PATH", "rad");
        let object_storage = env_or("RAD_STORAGE", "file");
        let storage = match object_storage.as_str() {
            "memory" => StorageConfig::Memory { path: storage_path },
            "file" => StorageConfig::File {
                path: PathBuf::from(storage_path),
            },
            "s3" => StorageConfig::S3 {
                bucket: required_env("RAD_S3_BUCKET")?,
                path: env_or("RAD_S3_PREFIX", &storage_path),
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
        let config = Self {
            address,
            admin_address,
            catalog_mode,
            capture_workload_corpus,
            close_timeout,
            frontend,
            instance_id,
            internal_address,
            internal_tls_certificate,
            internal_tls_key,
            postgres_address,
            reader_poll_interval,
            relay_authority,
            relay_target,
            relay_token_file,
            role,
            shutdown_drain,
            slate,
            storage,
        };
        config.validate()?;
        Ok(config)
    }

    /// Reject a relay configuration that is half-given.
    ///
    /// Each half without the other is a mistake with a silent consequence: an
    /// address alone would serve evidence to anything that could reach the
    /// port, and a target alone would gather evidence that every attempt to
    /// send is refused for.
    pub fn validate(&self) -> Result<()> {
        validate_slate_options(&self.slate)?;
        if self.internal_address.is_some() && self.relay_token_file.is_none() {
            return Err(
                "RAD_INTERNAL_ADDR requires RAD_RELAY_TOKEN_FILE: the internal API is never served unauthenticated"
                    .into(),
            );
        }
        if self.relay_target.is_some() && self.relay_token_file.is_none() {
            return Err(
                "RAD_RELAY_TARGET requires RAD_RELAY_TOKEN_FILE: the receiving instance rejects an unauthenticated batch"
                    .into(),
            );
        }
        // A certificate without its key, or a key without its certificate,
        // cannot serve. Failing here rather than at the first handshake keeps
        // a misconfiguration from looking like a network fault.
        if self.internal_tls_certificate.is_some() != self.internal_tls_key.is_some() {
            return Err(
                "RAD_INTERNAL_TLS_CERT and RAD_INTERNAL_TLS_KEY are given together or not at all"
                    .into(),
            );
        }
        Ok(())
    }

    /// The TLS material for the internal listener, if it serves TLS.
    pub fn internal_tls(&self) -> Option<crate::internal::TlsFiles> {
        let certificate = self.internal_tls_certificate.clone()?;
        let key = self.internal_tls_key.clone()?;
        Some(crate::internal::TlsFiles {
            certificate,
            key,
            authority: self.relay_authority.clone(),
        })
    }
}

pub(crate) fn ensure_storage_path_environment() -> Result {
    if env::var_os("RAD_DATA_DIR").is_some() {
        return Err("RAD_DATA_DIR is not supported; use RAD_STORAGE_PATH".into());
    }
    Ok(())
}

/// Construct the production runtime and serve until `shutdown` resolves.
///
/// Durable schema work starts before the listener and is stopped before the
/// store closes. Store close is awaited even when the HTTP server exits
/// with an error, preserving the storage orderly-shutdown contract.
///
/// Only the probe endpoints answer on the public address until the runtime is
/// built, so a slow or unavailable object store never publishes a partially
/// initialized database.
pub async fn serve(config: Config, shutdown: impl Future<Output = ()> + Send + 'static) -> Result {
    config.validate()?;
    let planner_mode = internal_test_planner_mode()?;
    tracing::info!(
        target: "rad",
        event = "process.started",
        component = "process",
        role = %config.role,
        message = "Rad process started"
    );
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
    let relay_token = match &config.relay_token_file {
        Some(path) => Some(Arc::new(crate::internal::Token::load(path)?)),
        None => None,
    };
    let internal_listener = match (&config.internal_address, &relay_token) {
        (Some(address), Some(_)) => Some(tokio::net::TcpListener::bind(address).await?),
        _ => None,
    };
    // Loaded before the listener serves, so unusable material stops the
    // process rather than surfacing as a handshake failure per connection.
    let internal_certificate = match config.internal_tls() {
        Some(files) => Some(crate::internal::RotatingCertificate::load(files)?),
        None => None,
    };

    let (started_sender, started_receiver) = tokio::sync::watch::channel(false);
    let startup_probes = tokio::spawn(crate::http::serve(
        startup_listener,
        crate::http::probe_router(health.clone()),
        wait_for_stop(started_receiver),
    ));
    let started = start_runtime(&config, &health, planner_mode).await;
    let _ = started_sender.send(true);
    let handover = joined_server(startup_probes.await);
    let Runtime {
        store,
        runtime_fence_writer,
        location,
        catalog,
        engine,
        mode,
        jobs,
        statistics,
    } = started?;
    handover?;

    let public_address = listener.local_addr()?;
    let bound_admin_address = admin_listener.local_addr()?;
    tracing::info!(
        target: "rad",
        event = "process.ready",
        component = "process",
        role = %config.role,
        catalog_mode = ?mode,
        storage = %location,
        public_address = %public_address,
        admin_address = %bound_admin_address,
        execution_concurrency = default_execution_concurrency(),
        message = "Rad is ready"
    );
    if let Some(listener) = &internal_listener {
        let scheme = if internal_certificate.is_some() {
            "https"
        } else {
            "http"
        };
        tracing::info!(
            target: "rad",
            event = "listener.started",
            component = "internal_http",
            scheme,
            address = %listener.local_addr()?,
            message = "internal HTTP listener started"
        );
    }
    if let Some(listener) = &postgres_listener {
        tracing::info!(
            target: "rad",
            event = "listener.started",
            component = "postgres",
            address = %listener.local_addr()?,
            message = "PostgreSQL listener started"
        );
    }
    health.serve();

    let (stop_sender, stop_receiver) = tokio::sync::watch::channel(false);
    let shutdown_sender = stop_sender.clone();
    let shutdown_health = health.clone();
    let shutdown_drain = config.shutdown_drain;
    let shutdown_task = tokio::spawn(async move {
        shutdown.await;
        tracing::info!(
            target: "rad",
            event = "process.draining",
            component = "process",
            message = "Rad is draining"
        );
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
            runtime_fence_writer,
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
    let admin_statistics = statistics.clone();
    servers.spawn(async move {
        crate::http::serve(
            admin_listener,
            crate::admin::router_with_statistics(admin_store, admin_statistics),
            wait_for_stop(admin_stop),
        )
        .await
    });
    if let Some(listener) = internal_listener
        && let Some(token) = relay_token
        && let Some(statistics) = &statistics
    {
        let internal_stop = stop_receiver.clone();
        let internal_ingest = statistics.ingest();
        let confidential = internal_certificate.is_some();
        servers.spawn(async move {
            crate::internal::serve(
                listener,
                crate::internal::router(internal_ingest, token, confidential),
                internal_certificate,
                wait_for_stop(internal_stop),
            )
            .await
        });
    }
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
    if let Some(statistics) = statistics {
        statistics.shutdown().await;
    }
    let scheduler_result = match jobs {
        Some(jobs) => jobs.shutdown().await,
        None => Ok(()),
    };
    let close_result = close_store(store.as_ref(), config.close_timeout).await;

    server_result?;
    scheduler_result?;
    close_result?;
    tracing::info!(
        target: "rad",
        event = "process.stopped",
        component = "process",
        message = "Rad stopped in order"
    );
    Ok(())
}

async fn close_store(store: &dyn TransactionalKv, timeout: Option<Duration>) -> Result {
    let Some(limit) = timeout else {
        return Ok(store.close().await?);
    };
    match tokio::time::timeout(limit, store.close()).await {
        Ok(result) => Ok(result?),
        Err(_) => Err(format!(
            "orderly storage close did not finish within {limit:?}; abandoning storage housekeeping"
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
    runtime_fence_writer: Option<Arc<dyn Kv>>,
    location: String,
    catalog: Arc<Catalog>,
    engine: Arc<Engine>,
    mode: Mode,
    jobs: Option<Arc<SchemaJobRunner>>,
    statistics: Option<Arc<crate::scheduler::statistics::StatisticsRunner>>,
}

/// Where this instance's own evidence goes.
///
/// A writer publishes to storage. A reader cannot, so it hands its evidence to
/// the instance that can; without a target it keeps observing and its evidence
/// stays local, which costs the fleet a better planner but nothing else.
fn statistics_sink(
    config: &Config,
    slate: Arc<crate::scheduler::statistics::SlateStatistics>,
) -> Result<Option<Arc<dyn crate::scheduler::statistics::StatisticsSink>>> {
    if config.role == Role::Write {
        return Ok(Some(slate));
    }
    let Some(target) = &config.relay_target else {
        return Ok(None);
    };
    let token = crate::internal::Token::load(
        config
            .relay_token_file
            .as_ref()
            .ok_or("a relay target requires a relay token file")?,
    )?;
    let transport = crate::internal::HttpTransport::new(
        target,
        token.header_value(),
        config.relay_authority.as_deref(),
    )?;
    Ok(Some(Arc::new(crate::scheduler::relay::RelaySink::new(
        Arc::new(transport),
        Arc::new(crate::runtime::SystemRuntime),
        instance_id(config),
    ))))
}

/// What this instance calls itself when it relays. A container runtime sets
/// `HOSTNAME` to the pod name, which is already distinct per instance.
///
/// A duplicate name is survivable rather than silent: a source is identified
/// by name *and* boot nonce, and the nonce is fresh per process, so two
/// instances sharing a name still have their sequences counted apart.
fn instance_id(config: &Config) -> String {
    config
        .instance_id
        .clone()
        .or_else(|| env::var("HOSTNAME").ok().filter(|value| !value.is_empty()))
        .unwrap_or_else(|| "rad".into())
}

/// Any failure closes storage before returning, so a process that never publishes
/// still leaves its storage location cleanly.
///
/// A preflight monitors a remote store while it opens and records the cause
/// of a held startup. SlateDB retries an unavailable object store without
/// limit, so the open does not return an error to classify.
async fn start_runtime(
    config: &Config,
    health: &Arc<Health>,
    planner_mode: crate::engine::planner::PlannerMode,
) -> Result<Runtime> {
    let built = build_objects(&config.storage)?;
    let preflight = built.remote.then(|| {
        tokio::spawn(diagnose_startup_hold(
            built.objects.clone(),
            built.path.clone(),
            health.clone(),
        ))
    });
    let opened = open_slate_storage(
        &built,
        config.role,
        config.reader_poll_interval,
        config.slate.clone(),
    )
    .await;
    if let Some(preflight) = preflight {
        preflight.abort();
    }
    open_runtime(config, opened?, planner_mode).await
}

async fn open_runtime(
    config: &Config,
    opened: OpenedStorage,
    planner_mode: crate::engine::planner::PlannerMode,
) -> Result<Runtime> {
    let store = opened.store;
    if let Err(error) = admit_storage_compatibility(store.as_ref(), config.role).await {
        return close_after_error(store.as_ref(), config.close_timeout, error).await;
    }
    let catalog = Arc::new(Catalog::new(store.clone()));
    let mode = match open_catalog_mode(&catalog, config.catalog_mode, config.role).await {
        Ok(mode) => mode,
        Err(error) => return close_after_error(store.as_ref(), config.close_timeout, error).await,
    };
    // Every instance reads published statistics. A reader sends its local
    // observations to the writer and plans only from stored evidence.
    let slate_statistics = Arc::new(crate::scheduler::statistics::SlateStatistics::new(
        store.clone(),
    ));
    let sink = statistics_sink(config, slate_statistics.clone())?;
    let capture_programs = config.capture_workload_corpus
        && sink.as_ref().is_some_and(|sink| sink.carries_user_values());
    if config.capture_workload_corpus && !capture_programs {
        tracing::warn!(
            target: "rad",
            event = "workload_corpus.disabled",
            component = "statistics",
            error_kind = "configuration",
            error_reason = "sink_rejects_user_values",
            message = "workload corpus capture is disabled"
        );
    }
    let statistics = Some(crate::scheduler::statistics::StatisticsRunner::start(
        Arc::new(crate::runtime::SystemRuntime),
        crate::scheduler::statistics::StatisticsConfig {
            capture_programs,
            ..Default::default()
        },
        Some(slate_statistics.clone()),
        sink,
    ));
    let execution_cpu_limit = default_execution_cpu_limit();
    let engine = match config.role {
        Role::Read => Engine::read_only(store.clone()),
        Role::Write => Engine::new(store.clone()),
    }
    .with_planner_mode(planner_mode)
    .with_execution_capacity(
        execution_concurrency_for(execution_cpu_limit, execution_cpu_limit),
        execution_cpu_limit,
    );
    let engine = Arc::new(match &statistics {
        Some(statistics) => engine
            .with_observer(statistics.collector())
            .with_statistics_provider(statistics.clone()),
        None => engine,
    });
    if let Some(statistics) = &statistics {
        statistics.attach_engine(engine.clone());
    }
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
        runtime_fence_writer: opened.runtime_fence_writer,
        location: opened.location,
        catalog,
        engine,
        mode,
        jobs,
        statistics,
    })
}

fn default_execution_concurrency() -> usize {
    let cpu_limit = default_execution_cpu_limit();
    execution_concurrency_for(cpu_limit, cpu_limit)
}

fn default_execution_cpu_limit() -> usize {
    let available = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    let quota = crate::telemetry::process_cpu_limit()
        .filter(|limit| limit.is_finite() && *limit > 0.0)
        .map(|limit| limit.ceil() as usize)
        .unwrap_or(available);
    available.min(quota).max(1)
}

fn execution_concurrency_for(available: usize, quota: usize) -> usize {
    available.min(quota).max(1).saturating_mul(4).clamp(4, 64)
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
            tracing::warn!(
                target: "rad",
                event = "process.startup_held",
                component = "storage",
                error_kind = "storage_unavailable",
                error_reason = hold.as_str(),
                message = "storage holds process startup"
            );
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
    crate::logging::install(crate::logging::Config::from_env()?)?;
    let result = serve(Config::from_env()?, shutdown_signal()).await;
    if let Err(error) = &result {
        crate::logging::terminal_failure(error.as_ref());
    }
    crate::logging::shutdown();
    result
}

struct OpenedStorage {
    store: Arc<dyn TransactionalKv>,
    runtime_fence_writer: Option<Arc<dyn Kv>>,
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
            StorageConfig::File { path } => {
                let (directory, database) = local_storage_path(path)?;
                let objects: Arc<dyn ObjectStore> =
                    Arc::new(LocalFileSystem::new_with_prefix(&directory)?);
                let location = directory.join(&database).display().to_string();
                (database, objects, location, false)
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
                let objects: Arc<dyn ObjectStore> =
                    crate::engine::kv::slatedb::observe_remote_object_store(Arc::new(
                        builder.build()?,
                    ));
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

async fn open_slate_storage(
    built: &BuiltObjects,
    role: Role,
    reader_poll_interval: Duration,
    options: SlateOptions,
) -> Result<OpenedStorage> {
    let path = built.path.clone();
    let objects = built.objects.clone();
    match role {
        Role::Write => {
            let writer = Arc::new(SlateStore::open_with_options(path, objects, options).await?);
            let store: Arc<dyn TransactionalKv> = writer.clone();
            let writer: Arc<dyn Kv> = writer;
            Ok(OpenedStorage {
                store,
                runtime_fence_writer: Some(writer),
                location: built.location.clone(),
            })
        }
        Role::Read => {
            let store: Arc<dyn TransactionalKv> = Arc::new(
                ReaderStore::open_with_options(path, objects, reader_poll_interval, options)
                    .await?,
            );
            Ok(OpenedStorage {
                store,
                runtime_fence_writer: None,
                location: built.location.clone(),
            })
        }
    }
}

fn local_storage_path(path: &Path) -> Result<(PathBuf, String)> {
    let database = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or("RAD_STORAGE_PATH must name a local database directory")?
        .to_owned();
    let directory = path
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(directory)?;
    Ok((directory.canonicalize()?, database))
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
    runtime_fence_writer: Option<Arc<dyn Kv>>,
    jobs: Option<Arc<SchemaJobRunner>>,
    health: Arc<Health>,
    drain: Duration,
    mut stop: tokio::sync::watch::Receiver<bool>,
    stop_sender: tokio::sync::watch::Sender<bool>,
) -> std::io::Result<()> {
    let token = Bytes::from(uuid::Uuid::new_v4().into_bytes().to_vec());
    let mut interval = tokio::time::interval(crate::health::STORAGE_OBSERVATION_INTERVAL);
    let mut outage_started: Option<Instant> = None;
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
                    crate::telemetry::scheduler_worker_lost();
                    tracing::error!(
                        target: "rad",
                        event = "scheduler.worker_lost",
                        component = "schema_scheduler",
                        error_kind = "worker_lost",
                        error_reason = "worker_stopped",
                        message = "schema scheduler worker stopped"
                    );
                    return stop_after_drain(
                        &health,
                        &stop_sender,
                        drain,
                        "scheduler_worker_lost",
                        "schema scheduler worker stopped",
                    )
                    .await;
                }
                // The fence key holds runtime state only: a writer rewrites
                // it to hold the storage location, and a reader reads it to
                // observe that storage still answers.
                let fence_key = crate::engine::kv::keys::runtime_writer_fence_key();
                let observation = match &runtime_fence_writer {
                    Some(writer) => Kv::put(
                        writer.as_ref(),
                        Bytes::from(fence_key),
                        token.clone(),
                    ).await,
                    None => Kv::get(store.as_ref(), &fence_key).await.map(|_| ()),
                };
                let Err(error) = observation else {
                    crate::telemetry::storage_observed(true);
                    if health.observe_storage() {
                        if let Some(started) = outage_started.take() {
                            crate::telemetry::storage_outage_finished(
                                "observation_failed",
                                started.elapsed(),
                            );
                        }
                        tracing::info!(
                            target: "rad",
                            event = "storage.recovered",
                            component = "storage",
                            message = "storage is available"
                        );
                    }
                    continue;
                };
                if error.kind() == KvErrorKind::Unavailable {
                    crate::telemetry::storage_observed(false);
                    if health.observe_storage_unavailable() {
                        outage_started = Some(Instant::now());
                        crate::telemetry::storage_outage_started("observation_failed");
                        tracing::warn!(
                            target: "rad",
                            event = "storage.unavailable",
                            component = "storage",
                            error_kind = "unavailable",
                            error_reason = "observation_failed",
                            message = "storage is unavailable"
                        );
                    }
                    continue;
                }
                let (reason, message) = if error.closure() == Some(Closure::Fenced) {
                    health.observe_fenced();
                    crate::telemetry::storage_fenced("writer_ownership_lost");
                    tracing::error!(
                        target: "rad",
                        event = "storage.fenced",
                        component = "storage",
                        error_kind = "fenced",
                        error_reason = "writer_ownership_lost",
                        message = "writer lost storage ownership"
                    );
                    ("writer_ownership_lost", "writer lost storage ownership")
                } else {
                    health.observe_task_failure();
                    tracing::error!(
                        target: "rad",
                        event = "storage.failed",
                        component = "storage",
                        error_kind = "unusable",
                        error_reason = "observation_failed",
                        message = "storage is unusable"
                    );
                    ("storage_unusable", "storage is unusable")
                };
                return stop_after_drain(&health, &stop_sender, drain, reason, message).await;
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
    reason: &'static str,
    message: &'static str,
) -> std::io::Result<()> {
    health.drain();
    tokio::time::sleep(drain).await;
    let _ = stop_sender.send(true);
    Err(std::io::Error::other(TerminalRuntimeError {
        reason,
        message,
    }))
}

#[derive(Debug)]
pub(crate) struct TerminalRuntimeError {
    pub reason: &'static str,
    pub message: &'static str,
}

impl std::fmt::Display for TerminalRuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for TerminalRuntimeError {}

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
        Err(close) => Err(format!("{original}; orderly storage close also failed: {close}").into()),
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

fn internal_test_planner_mode() -> Result<crate::engine::planner::PlannerMode> {
    if !matches!(
        env::var("RAD_INTERNAL_TESTING").ok().as_deref(),
        Some("1" | "true" | "yes")
    ) {
        return Ok(crate::engine::planner::PlannerMode::Cost);
    }
    env_or("RAD_INTERNAL_TEST_PLANNER_MODE", "cost")
        .parse()
        .map_err(Into::into)
}

fn slate_options_from_env() -> Result<SlateOptions> {
    Ok(SlateOptions {
        decoded_cache_size_mib: parse_env("RAD_SLATE_DECODED_CACHE_SIZE_MIB", "128")?,
        scan_cache_blocks: parse_bool_env("RAD_SLATE_SCAN_CACHE_BLOCKS", false)?,
        scan_read_ahead_kib: parse_env("RAD_SLATE_SCAN_READ_AHEAD_KIB", "256")?,
        scan_max_fetch_tasks: parse_env("RAD_SLATE_SCAN_MAX_FETCH_TASKS", "4")?,
        flush_interval: Duration::from_millis(parse_env("RAD_SLATE_FLUSH_INTERVAL_MS", "100")?),
        l0_sst_size_mib: parse_env("RAD_SLATE_L0_SST_SIZE_MIB", "64")?,
        max_wal_flushes_before_l0_flush: parse_env(
            "RAD_SLATE_MAX_WAL_FLUSHES_BEFORE_L0_FLUSH",
            "4096",
        )?,
        l0_max_ssts: parse_env("RAD_SLATE_L0_MAX_SSTS", "8")?,
        l0_max_ssts_per_key: parse_env("RAD_SLATE_L0_MAX_SSTS_PER_KEY", "8")?,
        l0_flush_parallelism: parse_env("RAD_SLATE_L0_FLUSH_PARALLELISM", "4")?,
        max_unflushed_mib: parse_env("RAD_SLATE_MAX_UNFLUSHED_MIB", "1024")?,
        min_filter_keys: parse_env("RAD_SLATE_MIN_FILTER_KEYS", "1000")?,
        bloom_bits_per_key: parse_env("RAD_SLATE_BLOOM_BITS_PER_KEY", "10")?,
        sst_block_size_kib: parse_env("RAD_SLATE_SST_BLOCK_SIZE_KIB", "4")?,
        object_cache_path: env_path("RAD_SLATE_OBJECT_CACHE_PATH"),
        object_cache_size_mib: parse_env("RAD_SLATE_OBJECT_CACHE_SIZE_MIB", "16384")?,
        object_cache_part_size_kib: parse_env("RAD_SLATE_OBJECT_CACHE_PART_SIZE_KIB", "4096")?,
        object_cache_cache_on_flush: parse_bool_env("RAD_SLATE_OBJECT_CACHE_ON_FLUSH", false)?,
        object_cache_cache_on_compaction: parse_bool_env(
            "RAD_SLATE_OBJECT_CACHE_ON_COMPACTION",
            false,
        )?,
        object_cache_preload: match env_or("RAD_SLATE_OBJECT_CACHE_PRELOAD", "none").as_str() {
            "none" => ObjectCachePreload::None,
            "l0" => ObjectCachePreload::L0,
            "all" => ObjectCachePreload::All,
            value => {
                return Err(format!(
                    "unknown RAD_SLATE_OBJECT_CACHE_PRELOAD {value:?} (none, l0, or all)"
                )
                .into());
            }
        },
    })
}

fn parse_env<T>(name: &str, fallback: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    env_or(name, fallback)
        .parse()
        .map_err(|error| format!("invalid {name}: {error}").into())
}

fn parse_bool_env(name: &str, fallback: bool) -> Result<bool> {
    match env::var(name).ok().filter(|value| !value.is_empty()) {
        None => Ok(fallback),
        Some(value) => match value.as_str() {
            "1" | "true" | "yes" => Ok(true),
            "0" | "false" | "no" => Ok(false),
            _ => Err(format!("invalid {name}: expected true or false").into()),
        },
    }
}

fn validate_slate_options(options: &SlateOptions) -> Result {
    if options.decoded_cache_size_mib < 16 {
        return Err("Slate decoded cache size must be at least 16 MiB".into());
    }
    if options.scan_read_ahead_kib == 0 {
        return Err("Slate scan read-ahead size must be greater than zero".into());
    }
    if options.scan_max_fetch_tasks == 0 {
        return Err("Slate scan fetch task count must be greater than zero".into());
    }
    if options.flush_interval.is_zero() {
        return Err("Slate flush interval must be greater than zero".into());
    }
    if options.l0_sst_size_mib == 0 {
        return Err("Slate L0 SST size must be greater than zero".into());
    }
    if options.max_wal_flushes_before_l0_flush == 0 {
        return Err("Slate WAL flush limit must be greater than zero".into());
    }
    if options.l0_max_ssts == 0 || options.l0_max_ssts_per_key == 0 {
        return Err("Slate L0 SST limits must be greater than zero".into());
    }
    if options.l0_max_ssts_per_key > options.l0_max_ssts {
        return Err("Slate per-key L0 SST limit must not exceed the total L0 SST limit".into());
    }
    if options.l0_flush_parallelism == 0 {
        return Err("Slate L0 flush parallelism must be greater than zero".into());
    }
    if options.max_unflushed_mib < options.l0_sst_size_mib {
        return Err("Slate maximum unflushed size must be at least the L0 SST size".into());
    }
    if options.bloom_bits_per_key == 0 {
        return Err("Slate Bloom filter bits per key must be greater than zero".into());
    }
    if !matches!(options.sst_block_size_kib, 1 | 2 | 4 | 8 | 16 | 32 | 64) {
        return Err("Slate SST block size must be 1, 2, 4, 8, 16, 32, or 64 KiB".into());
    }
    if options.object_cache_size_mib == 0 {
        return Err("Slate object cache size must be greater than zero".into());
    }
    if options.object_cache_part_size_kib == 0 {
        return Err("Slate object cache part size must be greater than zero".into());
    }
    Ok(())
}

fn env_or(name: &str, fallback: &str) -> String {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
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
    #[cfg(windows)]
    {
        // CTRL_BREAK is the only console signal a supervisor can deliver to
        // one specific process group, so orderly shutdown on Windows must
        // accept it alongside CTRL_C.
        let terminate = async {
            if let Ok(mut signal) = tokio::signal::windows::ctrl_break() {
                signal.recv().await;
            }
        };
        tokio::select! {
            () = interrupt => {},
            () = terminate => {},
        }
    }
    #[cfg(not(any(unix, windows)))]
    interrupt.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::kv::slatedb::Options as SlateOptions;

    #[test]
    fn execution_concurrency_uses_four_slots_per_cpu() {
        assert_eq!(execution_concurrency_for(8, 4), 16);
        assert_eq!(execution_concurrency_for(1, 1), 4);
        assert_eq!(execution_concurrency_for(128, 128), 64);
    }

    #[tokio::test]
    async fn memory_process_starts_and_closes_all_runtime_components() {
        let config = Config {
            address: "127.0.0.1:0".into(),
            admin_address: Some("127.0.0.1:0".into()),
            close_timeout: Some(Duration::from_secs(30)),
            catalog_mode: Some(Mode::Schema),
            slate: SlateOptions::default(),
            capture_workload_corpus: false,
            frontend: Some(Frontend::Postgres),
            postgres_address: "127.0.0.1:0".into(),
            instance_id: None,
            internal_address: None,
            internal_tls_certificate: None,
            internal_tls_key: None,
            reader_poll_interval: Duration::from_millis(10),
            relay_authority: None,
            relay_target: None,
            relay_token_file: None,
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
            path: directory.path().join("database"),
        };
        serve(
            Config {
                address: "127.0.0.1:0".into(),
                admin_address: None,
                close_timeout: None,
                catalog_mode: Some(Mode::Direct),
                slate: SlateOptions::default(),
                capture_workload_corpus: false,
                frontend: None,
                postgres_address: "127.0.0.1:0".into(),
                instance_id: None,
                internal_address: None,
                internal_tls_certificate: None,
                internal_tls_key: None,
                reader_poll_interval: Duration::from_millis(10),
                relay_authority: None,
                relay_target: None,
                relay_token_file: None,
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
                slate: SlateOptions::default(),
                capture_workload_corpus: false,
                frontend: None,
                postgres_address: "127.0.0.1:0".into(),
                instance_id: None,
                internal_address: None,
                internal_tls_certificate: None,
                internal_tls_key: None,
                reader_poll_interval: Duration::from_millis(10),
                relay_authority: None,
                relay_target: None,
                relay_token_file: None,
                role: Role::Write,
                shutdown_drain: Duration::ZERO,
                storage,
            },
            std::future::ready(()),
        )
        .await
        .unwrap();
    }

    fn relay_config(internal: Option<&str>, target: Option<&str>, token: Option<&str>) -> Config {
        Config {
            address: "127.0.0.1:0".into(),
            admin_address: None,
            close_timeout: None,
            catalog_mode: None,
            slate: SlateOptions::default(),
            capture_workload_corpus: false,
            frontend: None,
            instance_id: None,
            internal_address: internal.map(str::to_owned),
            internal_tls_certificate: None,
            internal_tls_key: None,
            postgres_address: "127.0.0.1:0".into(),
            reader_poll_interval: Duration::from_millis(10),
            relay_authority: None,
            relay_target: target.map(str::to_owned),
            relay_token_file: token.map(PathBuf::from),
            role: Role::Write,
            shutdown_drain: Duration::ZERO,
            storage: StorageConfig::Memory {
                path: "relay-config".into(),
            },
        }
    }

    /// Half a relay configuration is a mistake whose consequence is silent:
    /// an address alone serves evidence to anything that reaches the port, and
    /// a target alone gathers evidence every send is refused for. Both are
    /// refused at boot rather than at the first batch.
    #[test]
    fn a_half_configured_relay_is_refused() {
        assert!(
            relay_config(Some("127.0.0.1:7239"), None, None)
                .validate()
                .is_err()
        );
        assert!(
            relay_config(None, Some("http://writer:7239"), None)
                .validate()
                .is_err()
        );

        assert!(
            relay_config(Some("127.0.0.1:7239"), None, Some("/run/token"))
                .validate()
                .is_ok()
        );
        assert!(
            relay_config(None, Some("http://writer:7239"), Some("/run/token"))
                .validate()
                .is_ok()
        );
        // Neither half is the ordinary single-instance case, which must not
        // require a secret it has no use for.
        assert!(relay_config(None, None, None).validate().is_ok());
    }

    #[test]
    fn decoded_cache_capacity_has_a_safe_minimum() {
        let mut config = relay_config(None, None, None);
        config.slate.decoded_cache_size_mib = 15;
        assert!(config.validate().is_err());
        config.slate.decoded_cache_size_mib = 16;
        assert!(config.validate().is_ok());
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
