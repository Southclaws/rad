#![allow(dead_code)]

use std::time::Duration;

use reqwest::Client;
use serde_json::{Value, json};
use testcontainers::core::{Host, IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

use super::s3::TestResult;

const IMAGE: &str = "ghcr.io/shopify/toxiproxy";
const IMAGE_TAG: &str =
    "2.12.0@sha256:9378ed52a28bc50edc1350f936f518f31fa95f0d15917d6eb40b8e376d1a214e";
const API_PORT: u16 = 8474;
const PROXY_PORT: u16 = 8666;

pub struct ToxiProxy {
    pub api: String,
    pub endpoint: String,
    pub name: String,
    client: Client,
    _container: ContainerAsync<GenericImage>,
}

impl ToxiProxy {
    pub async fn start(upstream_endpoint: &str, name: &str) -> TestResult<Self> {
        Self::start_seeded(upstream_endpoint, name, 1).await
    }

    pub async fn start_seeded(upstream_endpoint: &str, name: &str, seed: u64) -> TestResult<Self> {
        if seed == 0 || i64::try_from(seed).is_err() {
            return Err("Toxiproxy seed must be between 1 and i64::MAX".into());
        }
        let upstream = docker_upstream(upstream_endpoint)?;
        let container = GenericImage::new(IMAGE, IMAGE_TAG)
            .with_exposed_port(API_PORT.tcp())
            .with_exposed_port(PROXY_PORT.tcp())
            .with_wait_for(WaitFor::seconds(1))
            .with_host("host.testcontainers.internal", Host::HostGateway)
            .with_cmd([
                "-host=0.0.0.0".to_owned(),
                "-proxy-metrics".to_owned(),
                format!("-seed={seed}"),
            ])
            .start()
            .await?;
        let api_port = container.get_host_port_ipv4(API_PORT.tcp()).await?;
        let proxy_port = container.get_host_port_ipv4(PROXY_PORT.tcp()).await?;
        let proxy = Self {
            api: format!("http://127.0.0.1:{api_port}"),
            endpoint: format!("http://127.0.0.1:{proxy_port}"),
            name: name.to_owned(),
            client: Client::new(),
            _container: container,
        };
        proxy.create(&upstream).await?;
        Ok(proxy)
    }

    pub async fn add_toxic(&self, toxic: Value) -> TestResult {
        self.client
            .post(format!("{}/proxies/{}/toxics", self.api, self.name))
            .json(&toxic)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn remove_toxic(&self, name: &str) -> TestResult {
        self.client
            .delete(format!("{}/proxies/{}/toxics/{name}", self.api, self.name))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn set_latency(&self, latency_ms: u64, jitter_ms: u64) -> TestResult {
        for stream in ["upstream", "downstream"] {
            let name = format!("seeded-latency-{stream}");
            let _ = self.remove_toxic(&name).await;
            self.add_toxic(json!({
                "name": name,
                "type": "latency",
                "stream": stream,
                "toxicity": 1.0,
                "attributes": { "latency": latency_ms, "jitter": jitter_ms }
            }))
            .await?;
        }
        Ok(())
    }

    pub async fn metrics(&self) -> TestResult<String> {
        Ok(self
            .client
            .get(format!("{}/metrics", self.api))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?)
    }

    async fn create(&self, upstream: &str) -> TestResult {
        let body = json!({
            "name": self.name,
            "listen": format!("0.0.0.0:{PROXY_PORT}"),
            "upstream": upstream,
            "enabled": true
        });
        let mut last_error = None;
        for _ in 0..120 {
            match self
                .client
                .post(format!("{}/proxies", self.api))
                .json(&body)
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => return Ok(()),
                Ok(response) => last_error = Some(format!("HTTP {}", response.status())),
                Err(error) => last_error = Some(error.to_string()),
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Err(format!(
            "timed out creating Toxiproxy route: {}",
            last_error.unwrap_or_else(|| "no response".to_owned())
        )
        .into())
    }
}

fn docker_upstream(endpoint: &str) -> TestResult<String> {
    let url = reqwest::Url::parse(endpoint)?;
    let host = match url.host_str().ok_or("RustFS endpoint has no host")? {
        "127.0.0.1" | "localhost" => "host.testcontainers.internal",
        other => other,
    };
    let port = url
        .port_or_known_default()
        .ok_or("RustFS endpoint has no port")?;
    Ok(format!("{host}:{port}"))
}
