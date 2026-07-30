use std::sync::Arc;

use axum::http::StatusCode;

use super::generated::server::{DataApi, ExecuteResponse};
use super::generated::types::Program;
use super::{generated, problem, result, validation};
use crate::engine::catalog::model::Mode;
use crate::engine::exec::{CatalogPolicy, Engine, Error, ErrorKind, ErrorReason, ProgramOptions};
use crate::engine::frontend;
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
}

/// Build the generated API router. Callers may add a fallback or nest this
/// router when the static admin application is embedded later.
pub fn router(engine: Arc<Engine>, mode: Mode) -> axum::Router {
    router_with_location(engine, mode, "")
}

pub fn router_with_location(
    engine: Arc<Engine>,
    mode: Mode,
    location: impl Into<Arc<str>>,
) -> axum::Router {
    let api = Api::with_location(engine, mode, location);
    generated::server::build_router(api.clone(), api.clone(), api.clone(), api.clone(), api)
        .layer(axum::middleware::map_response(
            validation::normalize_generated_rejection,
        ))
        .layer(axum::middleware::from_fn(super::cors::allow_admin_origin))
}

#[async_trait::async_trait]
impl DataApi for Api {
    async fn execute(
        &self,
        show_plan: Option<bool>,
        dry_run: Option<bool>,
        body: Option<Program>,
    ) -> ExecuteResponse {
        let Some(body) = body else {
            return invalid_request("request body is required");
        };
        let program = match serde_json::from_value::<pir::Program>(body) {
            Ok(program) => program,
            Err(error) => return invalid_request(format!("invalid PIR program: {error}")),
        };

        let options = ProgramOptions {
            catalog: self.catalog_policy(),
            dry_run: dry_run.unwrap_or(false),
            collect_plan: show_plan.unwrap_or(false),
            ..ProgramOptions::default()
        };
        let result = match frontend::execute_pir_with_options(&self.engine, program, options).await
        {
            Ok(result) => result,
            Err(error) => {
                return execute_problem(problem::ResponseProblem::from_failure((&error).into()));
            }
        };
        match result::encode(&result) {
            Ok(result) => ExecuteResponse::Ok(result),
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
}

fn invalid_request(detail: impl Into<String>) -> ExecuteResponse {
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

fn execute_problem(problem: problem::ResponseProblem) -> ExecuteResponse {
    match problem.status {
        StatusCode::BAD_REQUEST => ExecuteResponse::BadRequest(problem.body),
        StatusCode::CONFLICT => ExecuteResponse::Conflict(problem.body),
        StatusCode::UNPROCESSABLE_ENTITY => ExecuteResponse::UnprocessableEntity(problem.body),
        status => ExecuteResponse::Default(status, problem.body),
    }
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
