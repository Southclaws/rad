use std::cell::RefCell;

use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use base64::Engine as _;
use headers::{ETag, Header as _, IfNoneMatch};

use super::generated::server::{DataApi, ExecuteOptionsResponse, QueryResponse};
use super::server::Api;
use super::{generated, problem};
use crate::engine::exec::{ConditionalQueryResult, Error, ErrorKind, ErrorReason, QueryValidator};
use crate::engine::frontend;
use crate::protocol::generated::lir;
use crate::service::error::{Failure, InvalidFailure, InvalidReason, Stage};

const ACCEPT_QUERY: &str = "\"application/vnd.rad.lir+json\"";
const CACHE_CONTROL: &str = "private, no-cache";
const VARY: &str = "Accept, Authorization, Content-Encoding, Content-Type";
const ALLOW: &str = "OPTIONS, POST, QUERY";

#[derive(Default)]
struct ResponseMetadata {
    entity_tag: Option<String>,
    options: bool,
}

tokio::task_local! {
    // The generated response enum contains only the status and body. Each
    // request scope holds its dynamic response headers until the generated
    // handler creates the response. Separate scopes prevent concurrent
    // requests from exchanging validators.
    static RESPONSE_METADATA: RefCell<ResponseMetadata>;
}

pub(super) fn router(api: Api) -> axum::Router {
    generated::server::data_api_router(api)
}

pub(super) async fn response_headers(request: Request, next: Next) -> Response {
    let query_request = request.method().as_str() == "QUERY" && request.uri().path() == "/execute";
    let options_request = request.method() == Method::OPTIONS && request.uri().path() == "/execute";
    if !query_request && !options_request {
        return next.run(request).await;
    }
    if query_request && !accepts_json(request.headers()) {
        crate::telemetry::conditional_query_finished("error");
        let mut response = render_problem(ResponseProblemKind::NotAcceptable);
        set_query_response_headers(&mut response, None);
        return response;
    }
    RESPONSE_METADATA
        .scope(RefCell::new(ResponseMetadata::default()), async move {
            let mut response = next.run(request).await;
            let metadata = RESPONSE_METADATA.with(|metadata| metadata.take());
            if metadata.options {
                response
                    .headers_mut()
                    .insert(header::ALLOW, HeaderValue::from_static(ALLOW));
                response.headers_mut().insert(
                    HeaderName::from_static("accept-query"),
                    HeaderValue::from_static(ACCEPT_QUERY),
                );
            }
            if let Some(entity_tag) = metadata.entity_tag {
                let Ok(entity_tag) = HeaderValue::from_str(&entity_tag) else {
                    return render_problem(ResponseProblemKind::Internal);
                };
                set_query_response_headers(&mut response, Some(entity_tag));
            } else if query_request {
                set_query_response_headers(&mut response, None);
            }
            if query_request {
                let outcome = match response.status() {
                    StatusCode::OK => "changed",
                    StatusCode::NOT_MODIFIED => "unchanged",
                    _ => "error",
                };
                crate::telemetry::conditional_query_finished(outcome);
            }
            response
        })
        .await
}

fn set_query_response_headers(response: &mut Response, entity_tag: Option<HeaderValue>) {
    let headers = response.headers_mut();
    if let Some(entity_tag) = entity_tag {
        headers.insert(header::ETAG, entity_tag);
    }
    headers.insert(
        HeaderName::from_static("accept-query"),
        HeaderValue::from_static(ACCEPT_QUERY),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL),
    );
    headers.insert(header::VARY, HeaderValue::from_static(VARY));
}

#[async_trait::async_trait]
impl DataApi for Api {
    async fn query(&self, if_none_match: Option<String>, body: bytes::Bytes) -> QueryResponse {
        let condition = match parse_if_none_match(if_none_match.as_deref()) {
            Ok(condition) => condition,
            Err(()) => {
                return QueryResponse::BadRequest(query_problem(ResponseProblemKind::BadRequest));
            }
        };
        let query = match serde_json::from_slice::<lir::Query>(&body) {
            Ok(query) => query,
            Err(error) => {
                return QueryResponse::BadRequest(invalid_problem(
                    StatusCode::BAD_REQUEST,
                    format!("invalid LIR query: {error}"),
                ));
            }
        };
        let result = frontend::execute_lir_conditional(&self.engine, query, |validator| {
            condition
                .as_ref()
                .is_some_and(|condition| !condition.precondition_passes(&entity_tag(validator)))
        })
        .await;
        match result {
            Ok(ConditionalQueryResult::Changed { result, validator }) => {
                let result = match super::result::encode_datum(&result) {
                    Ok(result) => result,
                    Err(error) => {
                        let error = Error::source_with_reason(
                            ErrorKind::Internal,
                            ErrorReason::Internal,
                            "encode QUERY result",
                            error,
                        );
                        return query_error(&error);
                    }
                };
                set_entity_tag(&validator);
                QueryResponse::Ok(result.into())
            }
            Ok(ConditionalQueryResult::Unchanged { validator }) => {
                set_entity_tag(&validator);
                crate::telemetry::conditional_query_avoided_execution();
                QueryResponse::NotModified
            }
            Err(error) => query_error(&error),
        }
    }

    async fn execute_options(&self) -> ExecuteOptionsResponse {
        RESPONSE_METADATA.with(|metadata| metadata.borrow_mut().options = true);
        ExecuteOptionsResponse::NoContent
    }
}

fn parse_if_none_match(value: Option<&str>) -> Result<Option<IfNoneMatch>, ()> {
    let Some(value) = value else {
        return Ok(None);
    };
    if !valid_if_none_match(value) {
        return Err(());
    }
    let value = HeaderValue::from_str(value).map_err(|_| ())?;
    let mut values = std::iter::once(&value);
    IfNoneMatch::decode(&mut values).map(Some).map_err(|_| ())
}

fn valid_if_none_match(value: &str) -> bool {
    let value = value.trim();
    if value == "*" {
        return true;
    }
    let mut quoted = false;
    let mut start = 0;
    let mut found = false;
    for (index, character) in value.char_indices() {
        if character == '"' {
            quoted = !quoted;
        } else if character == ',' && !quoted {
            let tag = value[start..index].trim();
            if tag.parse::<ETag>().is_err() {
                return false;
            }
            found = true;
            start = index + 1;
        }
    }
    !quoted && value[start..].trim().parse::<ETag>().is_ok() && (found || !value.is_empty())
}

fn entity_tag(validator: &QueryValidator) -> ETag {
    validator_text(validator)
        .parse()
        .expect("a query validator is a valid entity tag")
}

fn validator_text(validator: &QueryValidator) -> String {
    let digest = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(validator.as_bytes());
    format!("W/\"rad-query-{digest}\"")
}

fn set_entity_tag(validator: &QueryValidator) {
    RESPONSE_METADATA.with(|metadata| {
        metadata.borrow_mut().entity_tag = Some(validator_text(validator));
    });
}

fn accepts_json(headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(header::ACCEPT) else {
        return true;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    value.split(',').any(|range| {
        let mut parts = range.trim().split(';');
        let media = parts.next().unwrap_or_default().trim();
        let rejected = parts.any(|parameter| {
            let Some((name, value)) = parameter.trim().split_once('=') else {
                return false;
            };
            name.trim().eq_ignore_ascii_case("q")
                && value
                    .trim()
                    .parse::<f32>()
                    .is_ok_and(|quality| quality == 0.0)
        });
        !rejected
            && (media == "*/*"
                || media.eq_ignore_ascii_case("application/json")
                || media.eq_ignore_ascii_case("application/*"))
    })
}

fn query_error(error: &Error) -> QueryResponse {
    let problem = problem::ResponseProblem::from_failure(Failure::from_exec(error));
    match problem.status {
        StatusCode::BAD_REQUEST => QueryResponse::BadRequest(problem.body),
        StatusCode::UNPROCESSABLE_ENTITY => QueryResponse::UnprocessableEntity(problem.body),
        StatusCode::INTERNAL_SERVER_ERROR => QueryResponse::InternalServerError(problem.body),
        status => QueryResponse::Default(status, problem.body),
    }
}

#[derive(Clone, Copy)]
enum ResponseProblemKind {
    BadRequest,
    NotAcceptable,
    Internal,
}

fn query_problem(kind: ResponseProblemKind) -> generated::types::Problem {
    match kind {
        ResponseProblemKind::BadRequest => {
            invalid_problem(StatusCode::BAD_REQUEST, "malformed If-None-Match header")
        }
        ResponseProblemKind::NotAcceptable => invalid_problem(
            StatusCode::NOT_ACCEPTABLE,
            "the Accept header does not permit application/json",
        ),
        ResponseProblemKind::Internal => {
            problem::ResponseProblem::internal_transport(
                "QUERY response validator is not a valid HTTP header",
            )
            .body
        }
    }
}

fn invalid_problem(status: StatusCode, detail: impl Into<String>) -> generated::types::Problem {
    problem::ResponseProblem::invalid(
        InvalidFailure {
            stage: Stage::Schema,
            reason: InvalidReason::SchemaViolation,
            detail: detail.into(),
            location: None,
            diagnostics: Vec::new(),
        },
        status,
    )
    .body
}

fn render_problem(kind: ResponseProblemKind) -> Response {
    let status = match kind {
        ResponseProblemKind::BadRequest => StatusCode::BAD_REQUEST,
        ResponseProblemKind::NotAcceptable => StatusCode::NOT_ACCEPTABLE,
        ResponseProblemKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let mut response = (status, axum::Json(query_problem(kind))).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    response
}
