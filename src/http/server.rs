use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::extract::{RawQuery, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use serde::Deserialize;

use super::probes::Probes;
use super::{generated, problem, query, result, validation};
use crate::engine::catalog::model::Mode;
use crate::engine::exec::{CatalogPolicy, Engine, Error, ErrorKind, ErrorReason, ProgramOptions};
use crate::engine::frontend;
use crate::health::Health;
use crate::protocol::generated::pir;
use crate::service::error::{Failure, InvalidFailure, InvalidReason, Stage};

#[derive(Clone)]
pub struct Api {
    pub(super) engine: Arc<Engine>,
    pub(super) mode: Mode,
    pub(super) location: Arc<str>,
}

impl Api {
    pub fn new(engine: Arc<Engine>, mode: Mode) -> Self {
        Self::with_location(engine, mode, "")
    }

    pub fn with_location(engine: Arc<Engine>, mode: Mode, location: impl Into<Arc<str>>) -> Self {
        Self {
            engine,
            mode,
            location: location.into(),
        }
    }

    fn catalog_policy(&self) -> CatalogPolicy {
        match self.mode {
            Mode::Direct => CatalogPolicy::RevisionPerStatement,
            Mode::Schema => CatalogPolicy::Forbidden,
        }
    }

    async fn execute_program(
        &self,
        show_plan: Option<bool>,
        dry_run: Option<bool>,
        program: pir::Program,
    ) -> Response {
        let show_plan = show_plan.unwrap_or(false);
        let diagnostic_measurements = crate::logging::request_context().diagnostics.is_some();
        let options = ProgramOptions {
            catalog: self.catalog_policy(),
            dry_run: dry_run.unwrap_or(false),
            collect_plan: show_plan || diagnostic_measurements,
            ..ProgramOptions::default()
        };
        let mut value = match frontend::execute_pir_with_options(&self.engine, program, options)
            .await
        {
            Ok(value) => value,
            Err(error) => {
                return execute_problem(problem::ResponseProblem::from_failure((&error).into()));
            }
        };
        if !show_plan {
            value.plans.clear();
        }
        match result::encode(&value) {
            Ok(body) => json_response(StatusCode::OK, "application/json", body),
            Err(error) => {
                let error = Error::source_with_reason(
                    ErrorKind::Internal,
                    ErrorReason::Internal,
                    "encode HTTP program result",
                    error,
                );
                execute_problem(problem::ResponseProblem::from_failure(Failure::from_exec(
                    &error,
                )))
            }
        }
    }

    pub(super) fn write_problem(&self) -> Option<problem::ResponseProblem> {
        self.engine
            .require_write()
            .err()
            .map(|error| engine_problem(&error))
    }
}

/// Build the generated API router. Callers may add a fallback or nest this
/// router when the static admin application is embedded later.
pub fn router(engine: Arc<Engine>, mode: Mode) -> axum::Router {
    router_with_location(engine, mode, "")
}

/// Build the API router for an engine with no separate runtime reporting its
/// health, such as an embedded or in-process server. The probe endpoints then
/// answer from a health record that is already serving.
pub fn router_with_location(
    engine: Arc<Engine>,
    mode: Mode,
    location: impl Into<Arc<str>>,
) -> axum::Router {
    router_with_health(engine, mode, location, Health::serving())
}

pub fn router_with_health(
    engine: Arc<Engine>,
    mode: Mode,
    location: impl Into<Arc<str>>,
    health: Arc<Health>,
) -> axum::Router {
    let api = Api::with_location(engine, mode, location);
    generated::server::administration_api_router(api.clone())
        .merge(generated::server::catalog_api_router(api.clone()))
        .merge(execute_router(api.clone()))
        .merge(query::router(api.clone()))
        .merge(generated::server::meta_api_router(api.clone()))
        .merge(generated::server::probes_api_router(Probes::new(health)))
        .merge(generated::server::schema_api_router(api))
        .route(
            "/metrics",
            axum::routing::get(crate::telemetry::prometheus_metrics),
        )
        .layer(axum::middleware::map_response(
            validation::normalize_generated_rejection,
        ))
        .layer(axum::middleware::from_fn(query::response_headers))
        .layer(axum::middleware::from_fn(super::cors::allow_admin_origin))
        .layer(axum::middleware::from_fn(super::context::log_request))
}

const MAX_EXECUTE_BODY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Default, Deserialize)]
struct ExecuteQuery {
    #[serde(rename = "show-plan")]
    show_plan: Option<bool>,
    #[serde(rename = "dry-run")]
    dry_run: Option<bool>,
}

fn execute_router(api: Api) -> axum::Router {
    axum::Router::new()
        .route("/execute", axum::routing::post(execute_handler))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_EXECUTE_BODY_BYTES))
        .with_state(api)
}

async fn execute_handler(
    State(api): State<Api>,
    RawQuery(raw_query): RawQuery,
    request: Request,
) -> Response {
    let query = match decode_execute_query(raw_query.as_deref()) {
        Ok(query) => query,
        Err(error) => return error.into_response(),
    };
    let program = match decode_execute_program(request).await {
        Ok(Some(program)) => program,
        Ok(None) => return invalid_request("request body is required"),
        Err(ExecuteDecodeError::Request(error)) => return error.into_response(),
        Err(ExecuteDecodeError::Program(error)) => {
            return invalid_request(format!("invalid PIR program: {error}"));
        }
    };
    api.execute_program(query.show_plan, query.dry_run, program)
        .await
}

fn decode_execute_query(
    raw: Option<&str>,
) -> Result<ExecuteQuery, generated::server::RequestValidationRejection> {
    let raw = raw.unwrap_or_default();
    let bytes = raw.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && (index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit())
        {
            return Err(generated::server::validation::malformed_parameter("/query"));
        }
        index += if bytes[index] == b'%' { 3 } else { 1 };
    }
    let query = serde_urlencoded::from_str::<ExecuteQuery>(raw)
        .map_err(|_| generated::server::validation::malformed_parameter("/query"))?;
    Ok(query)
}

enum ExecuteDecodeError {
    Request(generated::server::RequestValidationRejection),
    Program(serde_json::Error),
}

async fn decode_execute_program(
    request: Request,
) -> Result<Option<pir::Program>, ExecuteDecodeError> {
    let (parts, body) = request.into_parts();
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    let is_json = content_type.is_some_and(json_media_type);
    if !is_json {
        if content_type.is_none() {
            let bytes = read_execute_body(body).await?;
            if bytes.is_empty() {
                return Ok(None);
            }
        }
        return Err(ExecuteDecodeError::Request(
            generated::server::validation::unsupported_media_type(),
        ));
    }
    let bytes = read_execute_body(body).await?;
    if bytes.is_empty() {
        return Ok(None);
    }
    // The OpenAPI Program component accepts arbitrary JSON because the PIR
    // schema is independent. Decode the normative generated PIR type here so
    // the repeated request path does not allocate an intermediate generic
    // JSON tree.
    match serde_json::from_slice(&bytes) {
        Ok(program) => Ok(Some(program)),
        Err(error) if error.is_data() => Err(ExecuteDecodeError::Program(error)),
        Err(_) => Err(ExecuteDecodeError::Request(
            generated::server::validation::malformed_request(),
        )),
    }
}

async fn read_execute_body(body: Body) -> Result<axum::body::Bytes, ExecuteDecodeError> {
    to_bytes(body, MAX_EXECUTE_BODY_BYTES)
        .await
        .map_err(|error| {
            let source = std::error::Error::source(&error);
            let rejection =
                if source.is_some_and(|source| source.is::<http_body_util::LengthLimitError>()) {
                    generated::server::validation::request_body_too_large()
                } else {
                    generated::server::validation::malformed_request()
                };
            ExecuteDecodeError::Request(rejection)
        })
}

fn json_media_type(value: &str) -> bool {
    value.parse::<mime::Mime>().is_ok_and(|value| {
        value.type_() == mime::APPLICATION
            && value.subtype() == mime::JSON
            && value.suffix().is_none()
    })
}

fn json_response(status: StatusCode, content_type: &'static str, body: Vec<u8>) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static(content_type),
    );
    response
}

fn invalid_request(detail: impl Into<String>) -> Response {
    execute_problem(problem::ResponseProblem::invalid(
        InvalidFailure {
            stage: Stage::Schema,
            reason: InvalidReason::SchemaViolation,
            detail: detail.into(),
            location: None,
            diagnostics: Vec::new(),
        },
        StatusCode::BAD_REQUEST,
    ))
}

fn execute_problem(problem: problem::ResponseProblem) -> Response {
    let mut response = (problem.status, axum::Json(problem.body)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/problem+json"),
    );
    response
}

pub(super) fn engine_problem(error: &Error) -> problem::ResponseProblem {
    problem::ResponseProblem::from_failure(Failure::from_exec(error))
}

pub(super) fn internal_problem(
    context: &'static str,
    error: impl std::error::Error + Send + Sync + 'static,
) -> problem::ResponseProblem {
    let error =
        Error::source_with_reason(ErrorKind::Internal, ErrorReason::Internal, context, error);
    engine_problem(&error)
}
