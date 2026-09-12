use std::net::SocketAddr;
use std::time::Instant;

use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, Request};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use opentelemetry::propagation::Extractor;
use tracing::Instrument as _;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

pub(crate) const APPLICATION_NAME_HEADER: &str = "x-rad-application-name";

pub(super) async fn log_request(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let client_ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|peer| peer.0.ip().to_string())
        .unwrap_or_default();
    let application_name = request
        .headers()
        .get(APPLICATION_NAME_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_default()
        .to_owned();
    if is_probe_path(&path) {
        let response = next.run(request).await;
        if !response.status().is_success() {
            tracing::debug!(
                target: "rad",
                event = "http.probe_failed",
                component = "http",
                client_ip,
                method = %method,
                path,
                status = response.status().as_u16(),
                duration_ms = started.elapsed().as_millis() as u64,
                message = "HTTP probe failed"
            );
        }
        return response;
    }
    let request_id = uuid::Uuid::new_v4().to_string();
    let diagnostic_level = match requested_diagnostic_level(request.headers()) {
        Ok(level) => level,
        Err((status, detail)) => return diagnostic_header_problem(status, detail),
    };
    let diagnostics = diagnostic_level.map(crate::diagnostics::ProgramDiagnosticRecorder::new);
    let parent = opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderExtractor(request.headers()))
    });
    let span = tracing::info_span!(
        target: "rad::telemetry",
        "http.request",
        otel.kind = "server",
        request_id,
        http.request.method = %method,
        url.path = path,
        client.address = client_ip,
        http.response.status_code = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
    );
    let _ = span.set_parent(parent);
    let (trace_id, span_id) = crate::telemetry::span_ids(&span);
    let context = crate::logging::RequestContext {
        transport: "http",
        request_id: request_id.clone(),
        transaction_id: String::new(),
        client_ip: client_ip.clone(),
        application_name: application_name.clone(),
        transaction_state: "none",
        trace_id: trace_id.clone(),
        span_id: span_id.clone(),
        diagnostics: diagnostics.clone(),
        parent_span: None,
    };
    let diagnostic_context = context.clone();
    crate::telemetry::http_started(method.as_str());
    let response =
        crate::logging::with_request_context(context, next.run(request).instrument(span.clone()))
            .await;
    let status = response.status().as_u16();
    let duration = started.elapsed();
    span.record("http.response.status_code", status);
    if status >= 500 {
        span.record("otel.status_code", "ERROR");
    }
    crate::telemetry::http_finished(method.as_str(), status, duration);
    tracing::debug!(
        target: "rad",
        event = "http.request_completed",
        component = "http",
        request_id,
        trace_id,
        span_id,
        client_ip,
        application_name,
        method = %method,
        path,
        status,
        duration_ms = duration.as_millis() as u64,
        message = "HTTP request completed"
    );
    match diagnostics {
        Some(diagnostics) => {
            let document = diagnostics.document(&diagnostic_context, status);
            add_diagnostics(response, document).await
        }
        None => response,
    }
}

fn requested_diagnostic_level(
    headers: &HeaderMap,
) -> Result<Option<crate::diagnostics::Level>, (StatusCode, &'static str)> {
    let Some(value) = headers.get(crate::diagnostics::HEADER) else {
        return Ok(None);
    };
    let level = value
        .to_str()
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|level| *level != crate::diagnostics::Level::Off)
        .ok_or((StatusCode::BAD_REQUEST, "invalid diagnostic level"))?;
    if level > crate::diagnostics::max_level() {
        return Err((StatusCode::FORBIDDEN, "diagnostic level is not permitted"));
    }
    Ok(Some(level))
}

fn diagnostic_header_problem(status: StatusCode, detail: &'static str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({
            "type": "urn:rad:problem:diagnostics",
            "title": "Diagnostic request rejected",
            "status": status.as_u16(),
            "detail": detail,
        })),
    )
        .into_response()
}

async fn add_diagnostics(response: Response, diagnostics: serde_json::Value) -> Response {
    let is_json = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            let media_type = value.split(';').next().unwrap_or_default();
            media_type == "application/json" || media_type.ends_with("+json")
        });
    if !is_json {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = to_bytes(body, usize::MAX).await else {
        return Response::from_parts(parts, Body::empty());
    };
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    let Some(fields) = value.as_object_mut() else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    fields.insert(
        "_rad".into(),
        serde_json::json!({ "diagnostics": diagnostics }),
    );
    let Ok(body) = serde_json::to_vec(&value) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    parts.headers.remove(CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(body))
}

struct HeaderExtractor<'a>(&'a HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(axum::http::HeaderName::as_str).collect()
    }
}

fn is_probe_path(path: &str) -> bool {
    matches!(
        path,
        "/healthz" | "/startupz" | "/readyz" | "/livez" | "/metrics"
    )
}

#[cfg(test)]
mod tests {
    use super::{APPLICATION_NAME_HEADER, is_probe_path};

    #[test]
    fn probe_paths_are_exact() {
        for path in ["/healthz", "/startupz", "/readyz", "/livez", "/metrics"] {
            assert!(is_probe_path(path));
        }
        assert!(!is_probe_path("/execute"));
        assert!(!is_probe_path("/readyz/details"));
    }

    #[test]
    fn application_name_header_is_vendor_scoped() {
        assert_eq!(APPLICATION_NAME_HEADER, "x-rad-application-name");
    }
}
