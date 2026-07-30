#![allow(dead_code)]

use std::env;
use std::error::Error as StdError;
use std::sync::Arc;
use std::time::Duration;

use rusty_s3::actions::{CreateBucket, S3Action as _};
use rusty_s3::{Bucket, Credentials, UrlStyle};
use slatedb::object_store::ObjectStore;
use slatedb::object_store::aws::AmazonS3Builder;
use slatedb::object_store::{ClientOptions, RetryConfig};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use uuid::Uuid;

pub type TestResult<T = ()> = Result<T, Box<dyn StdError + Send + Sync>>;

const RUSTFS_IMAGE: &str = "rustfs/rustfs";
const RUSTFS_IMAGE_TAG: &str =
    "1.0.0-beta.11@sha256:84ce557a0245a06a9aae5516f55ee0f007fca78d41df356f419306fdc0cb168c";
const RUSTFS_ACCESS_KEY: &str = "rustfsadmin";
const RUSTFS_SECRET_KEY: &str = "ChangeMe123!";
const S3_REGION: &str = "us-east-1";

#[derive(Clone, Debug)]
pub struct S3Config {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
}

pub struct RustFs {
    pub config: S3Config,
    _container: Option<ContainerAsync<GenericImage>>,
}

impl RustFs {
    pub async fn start_or_external() -> TestResult<Self> {
        let access_key =
            env::var("RAD_TEST_S3_ACCESS_KEY_ID").unwrap_or_else(|_| RUSTFS_ACCESS_KEY.to_owned());
        let secret_key = env::var("RAD_TEST_S3_SECRET_ACCESS_KEY")
            .unwrap_or_else(|_| RUSTFS_SECRET_KEY.to_owned());
        let region = env::var("RAD_TEST_S3_REGION").unwrap_or_else(|_| S3_REGION.to_owned());

        let (endpoint, container) = match env::var("RAD_TEST_S3_ENDPOINT") {
            Ok(endpoint) => (endpoint, None),
            Err(_) => {
                let tag = env::var("RAD_TEST_RUSTFS_IMAGE_TAG")
                    .unwrap_or_else(|_| RUSTFS_IMAGE_TAG.to_owned());
                let running = GenericImage::new(RUSTFS_IMAGE.to_owned(), tag)
                    .with_exposed_port(9000.tcp())
                    .with_wait_for(WaitFor::seconds(2))
                    .with_env_var("RUSTFS_ACCESS_KEY", &access_key)
                    .with_env_var("RUSTFS_SECRET_KEY", &secret_key)
                    .with_cmd(["/data"])
                    .start()
                    .await?;
                let port = running.get_host_port_ipv4(9000.tcp()).await?;
                (format!("http://127.0.0.1:{port}"), Some(running))
            }
        };

        let bucket = env::var("RAD_TEST_S3_BUCKET")
            .unwrap_or_else(|_| format!("rad-qualification-{}", Uuid::new_v4().simple()));
        let config = S3Config {
            endpoint,
            bucket,
            region,
            access_key,
            secret_key,
        };
        create_bucket(&config).await?;
        Ok(Self {
            config,
            _container: container,
        })
    }
}

pub fn object_store(config: &S3Config) -> TestResult<Arc<dyn ObjectStore>> {
    object_store_at(config, &config.endpoint)
}

pub fn object_store_at(config: &S3Config, endpoint: &str) -> TestResult<Arc<dyn ObjectStore>> {
    Ok(Arc::new(
        AmazonS3Builder::new()
            .with_endpoint(endpoint)
            .with_bucket_name(&config.bucket)
            .with_region(&config.region)
            .with_access_key_id(&config.access_key)
            .with_secret_access_key(&config.secret_key)
            .with_virtual_hosted_style_request(false)
            .with_allow_http(endpoint.starts_with("http://"))
            .build()?,
    ))
}

#[allow(dead_code)]
pub fn fail_fast_object_store_at(
    config: &S3Config,
    endpoint: &str,
) -> TestResult<Arc<dyn ObjectStore>> {
    Ok(Arc::new(
        AmazonS3Builder::new()
            .with_endpoint(endpoint)
            .with_bucket_name(&config.bucket)
            .with_region(&config.region)
            .with_access_key_id(&config.access_key)
            .with_secret_access_key(&config.secret_key)
            .with_virtual_hosted_style_request(false)
            .with_client_options(
                ClientOptions::new()
                    .with_allow_http(endpoint.starts_with("http://"))
                    .with_connect_timeout(Duration::from_secs(2))
                    .with_timeout(Duration::from_secs(2)),
            )
            .with_retry(RetryConfig {
                max_retries: 0,
                retry_timeout: Duration::from_secs(2),
                ..Default::default()
            })
            .build()?,
    ))
}

pub async fn create_bucket(config: &S3Config) -> TestResult {
    let bucket = Bucket::new(
        config.endpoint.parse()?,
        UrlStyle::Path,
        config.bucket.clone(),
        config.region.clone(),
    )?;
    let credentials = Credentials::new(&config.access_key, &config.secret_key);
    let signed_url = CreateBucket::new(&bucket, &credentials).sign(Duration::from_secs(60));
    let client = reqwest::Client::new();
    let mut last_error = None;

    for _ in 0..120 {
        match client.put(signed_url.clone()).send().await {
            Ok(response)
                if response.status().is_success()
                    || response.status() == reqwest::StatusCode::CONFLICT =>
            {
                return Ok(());
            }
            Ok(response) => {
                last_error = Some(format!("RustFS returned {}", response.status()));
            }
            Err(error) => {
                last_error = Some(error.to_string());
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    Err(format!(
        "timed out creating S3 bucket {} at {}: {}",
        config.bucket,
        config.endpoint,
        last_error.unwrap_or_else(|| "no response".to_owned())
    )
    .into())
}
