//! Thin process assembly around the reusable engine, scheduler, and HTTP API.

use std::env;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use slatedb::object_store::ObjectStore;
use slatedb::object_store::aws::AmazonS3Builder;
use slatedb::object_store::local::LocalFileSystem;

use crate::engine::catalog::Catalog;
use crate::engine::catalog::model::Mode;
use crate::engine::exec::Engine;
use crate::engine::kv::TransactionalKv;
use crate::engine::kv::slatedb::Store;
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub address: String,
    pub catalog_mode: Option<Mode>,
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
    let (store, location) = open_storage(&config.storage).await?;
    let store = Arc::new(store);
    let catalog = Catalog::new(store.clone());
    let mode = match catalog.init_mode(config.catalog_mode).await {
        Ok(mode) => mode,
        Err(error) => return close_after_error(&store, error).await,
    };
    let engine = Arc::new(Engine::new(store.clone()));
    let jobs = match SchemaJobRunner::start(engine.clone(), SchemaJobConfig::default()) {
        Ok(jobs) => jobs,
        Err(error) => return close_after_error(&store, error).await,
    };
    jobs.observe_catalog(&catalog);

    let public_address = listener.local_addr()?;
    let bound_admin_address = admin_listener.local_addr()?;
    eprintln!("rad serving on {public_address} (storage: {location}, catalog: {mode:?})");
    eprintln!("admin UI on http://{bound_admin_address}");

    let (stop_sender, stop_receiver) = tokio::sync::watch::channel(false);
    let shutdown_sender = stop_sender.clone();
    let shutdown_task = tokio::spawn(async move {
        shutdown.await;
        let _ = shutdown_sender.send(true);
    });
    let mut public_server = Box::pin(crate::http::serve(
        listener,
        crate::http::router_with_location(engine, mode, location),
        wait_for_stop(stop_receiver.clone()),
    ));
    let admin_store: Arc<dyn TransactionalKv> = store.clone();
    let mut admin_server = Box::pin(crate::http::serve(
        admin_listener,
        crate::admin::router(admin_store),
        wait_for_stop(stop_receiver),
    ));
    let server_result = tokio::select! {
        result = &mut public_server => {
            let _ = stop_sender.send(true);
            combine_servers(result, admin_server.await)
        }
        result = &mut admin_server => {
            let _ = stop_sender.send(true);
            combine_servers(result, public_server.await)
        }
    };
    shutdown_task.abort();
    let scheduler_result = jobs.shutdown().await;
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

pub async fn run() -> Result {
    serve(Config::from_env()?, shutdown_signal()).await
}

async fn open_storage(config: &StorageConfig) -> Result<(Store, String)> {
    match config {
        StorageConfig::Memory { path } => Ok((Store::memory(path).await?, "memory:///".into())),
        StorageConfig::File { directory, path } => {
            std::fs::create_dir_all(directory)?;
            let directory = directory.canonicalize()?;
            let objects: Arc<dyn ObjectStore> =
                Arc::new(LocalFileSystem::new_with_prefix(&directory)?);
            let store = Store::open(path.clone(), objects).await?;
            Ok((store, directory.join(path).display().to_string()))
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
            let store = Store::open(path.clone(), objects).await?;
            Ok((store, format!("s3://{bucket}/{path}")))
        }
    }
}

async fn close_after_error(
    store: &Store,
    error: impl std::error::Error + Send + Sync + 'static,
) -> Result {
    let original = error.to_string();
    match store.close().await {
        Ok(()) => Err(Box::new(error)),
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
