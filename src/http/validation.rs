use axum::Json;
use axum::body::{Body, to_bytes};
use axum::http::header;
use axum::response::{IntoResponse, Response};

use super::generated::server::ProblemDetails;
use super::problem::ResponseProblem;
use crate::service::error::{InvalidDiagnostic, InvalidFailure, InvalidReason, Location, Stage};

const MAX_GENERATED_PROBLEM_BYTES: usize = 128 * 1024;

/// Convert pre-handler failures emitted by the generator into Rad's public
/// Problem union. The generator cannot infer an application's Problem schema,
/// so this is the one transport-specific normalization seam.
pub(super) async fn normalize_generated_rejection(response: Response) -> Response {
    if !is_problem(&response) {
        return response;
    }

    let status = response.status();
    let (parts, body) = response.into_parts();
    let Ok(bytes) = to_bytes(body, MAX_GENERATED_PROBLEM_BYTES).await else {
        return render(ResponseProblem::internal_transport());
    };
    let Ok(problem) = serde_json::from_slice::<ProblemDetails>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    if !problem
        .r#type
        .starts_with("https://openapi-to-rust.dev/problems/")
    {
        return Response::from_parts(parts, Body::from(bytes));
    }
    if status.is_server_error() {
        return render(ResponseProblem::internal_transport());
    }

    let location = problem.errors.first().map(|error| Location {
        pointer: Some(error.location.clone()),
        ..Location::default()
    });
    let detail = problem
        .errors
        .first()
        .map(|error| format!("{}: {}", error.location, error.message))
        .unwrap_or(problem.title);
    let diagnostics = problem
        .errors
        .into_iter()
        .map(|error| InvalidDiagnostic {
            reason: InvalidReason::SchemaViolation,
            detail: error.message,
            location: Some(Location {
                pointer: Some(error.location),
                ..Location::default()
            }),
        })
        .collect();
    render(ResponseProblem::invalid(
        InvalidFailure {
            stage: Stage::Schema,
            reason: InvalidReason::SchemaViolation,
            detail,
            location,
            diagnostics,
        },
        status,
    ))
}

fn is_problem(response: &Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("application/problem+json"))
}

fn render(problem: ResponseProblem) -> Response {
    let mut response = (problem.status, Json(problem.body)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/problem+json"),
    );
    response
}
