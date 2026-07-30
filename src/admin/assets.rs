use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

const INDEX: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/admin/dist/index.html"
));
const APP_JS: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/admin/dist/assets/app.js"
));
const APP_CSS: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/admin/dist/assets/app.css"
));

pub(super) fn router() -> Router {
    Router::new()
        .route("/", get(index))
        .route("/assets/app.js", get(javascript))
        .route("/assets/app.css", get(stylesheet))
        .fallback(fallback)
}

async fn index() -> Response {
    asset("text/html; charset=utf-8", INDEX)
}

async fn javascript() -> Response {
    asset("text/javascript; charset=utf-8", APP_JS)
}

async fn stylesheet() -> Response {
    asset("text/css; charset=utf-8", APP_CSS)
}

async fn fallback(request: Request) -> Response {
    if request.uri().path().starts_with("/api/") || request.uri().path().starts_with("/assets/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    index().await
}

fn asset(content_type: &'static str, contents: &'static [u8]) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(Bytes::from_static(contents)))
        .expect("static admin response headers are valid")
}
