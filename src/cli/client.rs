use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::{RequestBuilder, Response, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use url::Url;

use crate::http::generated::types::{
    DatabaseInfo, Health, Problem, SchemaDiffResult, SchemaMigrateRequest, SchemaMigrateResult,
    SchemaRequest, SchemaState, TransitionControl, TransitionKind, TransitionList, TransitionState,
};
use crate::process::Result;

pub(super) struct Client {
    base: Url,
    http: reqwest::Client,
    access_token_file: Option<PathBuf>,
}

impl Client {
    pub(super) fn connect(connection: &str) -> Result<Self> {
        Self::connect_with_token_file(connection, None)
    }

    pub(super) fn connect_with_token_file(
        connection: &str,
        access_token_file: Option<&Path>,
    ) -> Result<Self> {
        let base = connection_url(connection)?;
        Ok(Self {
            base,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
            access_token_file: access_token_file.map(Path::to_path_buf),
        })
    }

    pub(super) async fn schema(&self) -> Result<SchemaState> {
        self.get("schema").await
    }

    pub(super) async fn health(&self) -> Result<Health> {
        self.get("healthz").await
    }

    pub(super) async fn info(&self) -> Result<DatabaseInfo> {
        self.get("info").await
    }

    pub(super) async fn schema_diff(&self, schema: String) -> Result<SchemaDiffResult> {
        self.post("schema/diff", &SchemaRequest { schema }).await
    }

    pub(super) async fn schema_migrate(
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

    pub(super) async fn wait_for_migration(
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

    pub(super) async fn transitions(
        &self,
        kind: Option<&TransitionKind>,
        state: Option<&TransitionState>,
    ) -> Result<TransitionList> {
        let mut endpoint = self.endpoint("schema/transitions")?;
        {
            let mut query = endpoint.query_pairs_mut();
            if let Some(kind) = kind {
                query.append_pair("kind", kind.as_str());
            }
            if let Some(state) = state {
                query.append_pair("state", state.as_str());
            }
        }
        let response = self.authorize(self.http.get(endpoint))?.send().await?;
        decode(response).await
    }

    pub(super) async fn transition(&self, transition: &str) -> Result<TransitionControl> {
        self.get(&format!("schema/transitions/{transition}")).await
    }

    pub(super) async fn cancel_transition(&self, transition: &str) -> Result<TransitionControl> {
        let request = self
            .http
            .post(self.endpoint(&format!("schema/transitions/{transition}/cancel"))?);
        let response = self.authorize(request)?.send().await?;
        decode(response).await
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let request = self.http.get(self.endpoint(path)?);
        let response = self.authorize(request)?.send().await?;
        decode(response).await
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        let request = self.http.post(self.endpoint(path)?).json(body);
        let response = self.authorize(request)?.send().await?;
        decode(response).await
    }

    fn authorize(&self, request: RequestBuilder) -> Result<RequestBuilder> {
        let Some(path) = &self.access_token_file else {
            return Ok(request);
        };
        let token = std::fs::read_to_string(path).map_err(|error| {
            format!(
                "could not read access token file {}: {error}",
                path.display()
            )
        })?;
        let token = token.trim();
        if token.is_empty() {
            return Err(format!("access token file {} is empty", path.display()).into());
        }
        let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|error| format!("access token file {} is invalid: {error}", path.display()))?;
        value.set_sensitive(true);
        Ok(request.header(AUTHORIZATION, value))
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
pub(super) struct ApiError {
    status: StatusCode,
    problem: Problem,
}

impl ApiError {
    pub(super) fn value(&self) -> serde_json::Value {
        let (code, reason, detail) = self.parts();
        serde_json::json!({
            "ok": false,
            "error": {
                "code": code,
                "reason": reason,
                "message": detail.unwrap_or("request failed"),
                "http_status": self.status.as_u16(),
                "problem": self.problem,
            }
        })
    }

    fn parts(&self) -> (&'static str, &str, Option<&str>) {
        match &self.problem {
            Problem::InvalidProblem(problem) => (
                "invalid",
                problem.reason.as_str(),
                problem.detail.as_deref(),
            ),
            Problem::UnauthenticatedProblem(problem) => (
                "unauthenticated",
                problem.reason.as_str(),
                problem.detail.as_deref(),
            ),
            Problem::ForbiddenProblem(problem) => (
                "forbidden",
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
                Some(problem.detail.as_str()),
            ),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (code, reason, detail) = self.parts();
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
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::http::HeaderMap;
    use axum::routing::get;

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

    #[tokio::test]
    async fn access_token_file_is_read_before_each_request() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let request_headers = Arc::clone(&received);
        let app = Router::new().route(
            "/healthz",
            get(move |headers: HeaderMap| {
                let request_headers = Arc::clone(&request_headers);
                async move {
                    request_headers.lock().unwrap().push(
                        headers
                            .get(AUTHORIZATION)
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .to_owned(),
                    );
                    axum::Json(serde_json::json!({
                        "access": "write",
                        "mode": "direct",
                        "status": "ok"
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let directory = tempfile::tempdir().unwrap();
        let token_file = directory.path().join("access-token");
        std::fs::write(&token_file, "first\n").unwrap();
        let client = Client::connect_with_token_file(
            &format!("rad://{address}"),
            Some(token_file.as_path()),
        )
        .unwrap();

        client.health().await.unwrap();
        std::fs::write(&token_file, "second\n").unwrap();
        client.health().await.unwrap();

        assert_eq!(
            received.lock().unwrap().as_slice(),
            ["Bearer first", "Bearer second"]
        );
        server.abort();
    }
}
