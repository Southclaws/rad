use std::time::Duration;

use reqwest::{Response, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use url::Url;

use crate::http::generated::types::{
    Problem, SchemaDiffResult, SchemaMigrateRequest, SchemaMigrateResult, SchemaRequest,
    SchemaState, TransitionControl, TransitionState,
};
use crate::process::Result;

pub struct Client {
    base: Url,
    http: reqwest::Client,
}

impl Client {
    pub fn connect(connection: &str) -> Result<Self> {
        let base = connection_url(connection)?;
        Ok(Self {
            base,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
        })
    }

    pub async fn schema(&self) -> Result<SchemaState> {
        self.get("schema").await
    }

    pub async fn schema_diff(&self, schema: String) -> Result<SchemaDiffResult> {
        self.post("schema/diff", &SchemaRequest { schema }).await
    }

    pub async fn schema_migrate(
        &self,
        schema: String,
        current_version: i64,
        current_hash: String,
        accept_data_loss: bool,
    ) -> Result<SchemaMigrateResult> {
        let request = SchemaMigrateRequest {
            accept_data_loss: accept_data_loss.then_some(true),
            current_hash,
            current_version,
            schema,
        };
        self.post("schema/migrate", &request).await
    }

    pub async fn wait_for_migration(
        &self,
        mut migration: SchemaMigrateResult,
    ) -> Result<SchemaMigrateResult> {
        if migration.state.as_str() == "ready" {
            return Ok(migration);
        }
        if migration.transition_ids.is_empty() {
            return Err("converging migration has no observable transition work".into());
        }
        loop {
            let mut all_ready = true;
            for transition in &migration.transition_ids {
                let transition: TransitionControl = self
                    .get(&format!("schema/transitions/{transition}"))
                    .await?;
                match transition.state {
                    TransitionState::Ready => {}
                    TransitionState::Failed | TransitionState::Cancelled => {
                        return Err(format!(
                            "schema transition {:?} ended in state {}: {}",
                            transition.transition_id,
                            transition.state,
                            transition.last_error.unwrap_or_default()
                        )
                        .into());
                    }
                    _ => all_ready = false,
                }
            }
            if all_ready {
                let state = self.schema().await?;
                if state.schema_hash != migration.desired_hash {
                    return Err(format!(
                        "schema transitions published but current hash is {}, want {}",
                        state.schema_hash, migration.desired_hash
                    )
                    .into());
                }
                migration.schema = state.schema;
                migration.schema_hash = state.schema_hash;
                migration.schema_version = state.schema_version;
                migration.state = crate::http::generated::types::SchemaMigrateResultState::Ready;
                return Ok(migration);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self.http.get(self.endpoint(path)?).send().await?;
        decode(response).await
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        let response = self
            .http
            .post(self.endpoint(path)?)
            .json(body)
            .send()
            .await?;
        decode(response).await
    }

    fn endpoint(&self, path: &str) -> Result<Url> {
        Ok(self.base.join(path)?)
    }
}

async fn decode<T: DeserializeOwned>(response: Response) -> Result<T> {
    let status = response.status();
    if status.is_success() {
        return Ok(response.json().await?);
    }
    let bytes = response.bytes().await?;
    match serde_json::from_slice::<Problem>(&bytes) {
        Ok(problem) => Err(ApiError { status, problem }.into()),
        Err(error) => Err(format!(
            "Rad returned HTTP {status}, but its problem body could not be decoded: {error}"
        )
        .into()),
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    problem: Problem,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (code, reason, detail) = match &self.problem {
            Problem::InvalidProblem(problem) => (
                "invalid",
                problem.reason.as_str(),
                problem.detail.as_deref(),
            ),
            Problem::ExecutionFailedProblem(problem) => (
                "execution_failed",
                problem.reason.as_str(),
                problem.detail.as_deref(),
            ),
            Problem::NotFoundProblem(problem) => (
                "not_found",
                problem.reason.as_str(),
                problem.detail.as_deref(),
            ),
            Problem::ConflictProblem(problem) => (
                "conflict",
                problem.reason.as_str(),
                problem.detail.as_deref(),
            ),
            Problem::InternalProblem(problem) => (
                "internal",
                problem.reason.as_str(),
                problem.detail.as_deref(),
            ),
        };
        write!(
            formatter,
            "{} ({code}/{reason}, HTTP {})",
            detail.unwrap_or("request failed"),
            self.status
        )
    }
}

impl std::error::Error for ApiError {}

fn connection_url(connection: &str) -> Result<Url> {
    let url = Url::parse(connection)?;
    let scheme = match url.scheme() {
        "rad" => "http",
        "rads" => "https",
        scheme => {
            return Err(
                format!("connection URI must use rad:// or rads://, got {scheme:?}").into(),
            );
        }
    };
    if url.host_str().is_none() {
        return Err(format!("connection URI {connection:?} has no host").into());
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return Err(
            format!("connection URI must be rad(s)://host[:port], got {connection:?}").into(),
        );
    }
    let authority = &url[url::Position::BeforeHost..url::Position::AfterPort];
    let port = if url.port().is_none() { ":7237" } else { "" };
    Ok(Url::parse(&format!("{scheme}://{authority}{port}/"))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rad_urls_map_to_http_with_the_default_port() {
        for (input, expected) in [
            ("rad://localhost", "http://localhost:7237/"),
            ("rad://localhost:9000", "http://localhost:9000/"),
            ("rad://db.internal", "http://db.internal:7237/"),
            ("rad://10.0.0.5", "http://10.0.0.5:7237/"),
            ("rad://[::1]", "http://[::1]:7237/"),
            ("rad://[::1]:9000", "http://[::1]:9000/"),
            ("rad://localhost/", "http://localhost:7237/"),
            ("rads://db.example.com", "https://db.example.com:7237/"),
            ("rads://db.example.com:8443", "https://db.example.com:8443/"),
        ] {
            assert_eq!(connection_url(input).unwrap().as_str(), expected, "{input}");
        }
    }

    #[test]
    fn connection_urls_reject_other_schemes_and_non_authority_components() {
        for value in [
            "http://localhost",
            "postgres://localhost",
            "rad://",
            "rad://user@localhost",
            "rad://user:pw@localhost",
            "rad://localhost/db",
            "rad://localhost?tls=true",
        ] {
            assert!(connection_url(value).is_err(), "accepted {value}");
        }
    }
}
