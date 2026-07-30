use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use bytes::Bytes;
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

use super::router;
use crate::engine::kv::slatedb::Store;
use crate::engine::kv::{Kv, TransactionalKv};

#[tokio::test]
async fn embedded_assets_and_spa_fallback_are_served() {
    let store = Arc::new(Store::memory("admin-assets").await.unwrap());
    let app = router(store.clone());

    let root = app
        .clone()
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(root.status(), StatusCode::OK);
    assert_eq!(
        root.headers()[header::CONTENT_TYPE],
        "text/html; charset=utf-8"
    );
    let html = root.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&html).contains("/assets/app.js"));

    let fallback = app
        .clone()
        .oneshot(Request::get("/tables/users").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(fallback.status(), StatusCode::OK);

    let missing_asset = app
        .clone()
        .oneshot(
            Request::get("/assets/missing.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_asset.status(), StatusCode::NOT_FOUND);

    drop(app);
    TransactionalKv::close(store.as_ref()).await.unwrap();
}

#[tokio::test]
async fn kv_scan_pages_and_get_preserves_raw_and_json_forms() {
    let store = Arc::new(Store::memory("admin-kv").await.unwrap());
    Kv::put(
        store.as_ref(),
        Bytes::from_static(b"/rad/test/a"),
        Bytes::from_static(br#"{"ok":true}"#),
    )
    .await
    .unwrap();
    Kv::put(
        store.as_ref(),
        Bytes::from_static(b"/rad/test/b"),
        Bytes::from_static(b"second"),
    )
    .await
    .unwrap();
    let app = router(store.clone());

    let first = json(app.clone(), "/api/kv/scan?prefix=%2Frad%2Ftest%2F&limit=1").await;
    assert_eq!(first["entries"].as_array().unwrap().len(), 1);
    assert_eq!(first["entries"][0]["keyDisplay"], "/rad/test/a");
    assert_eq!(first["truncated"], true);
    let cursor = first["nextAfter"].as_str().unwrap();

    let second_query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("prefix", "/rad/test/")
        .append_pair("after", cursor)
        .append_pair("limit", "1")
        .finish();
    let second = json(app.clone(), &format!("/api/kv/scan?{second_query}")).await;
    assert_eq!(second["entries"].as_array().unwrap().len(), 1);
    assert_eq!(second["entries"][0]["keyDisplay"], "/rad/test/b");
    assert_eq!(second["truncated"], false);

    let key = first["entries"][0]["key"].as_str().unwrap();
    let get_query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("key", key)
        .finish();
    let detail = json(app.clone(), &format!("/api/kv/get?{get_query}")).await;
    assert_eq!(detail["keyDisplay"], "/rad/test/a");
    assert_eq!(detail["valueJSON"]["ok"], true);
    assert!(detail["keyHex"].as_str().unwrap().contains("/rad/test/a"));

    let bad_cursor = app
        .clone()
        .oneshot(
            Request::get("/api/kv/scan?after=%25%25%25")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad_cursor.status(), StatusCode::BAD_REQUEST);

    drop(app);
    TransactionalKv::close(store.as_ref()).await.unwrap();
}

async fn json(app: axum::Router, uri: &str) -> serde_json::Value {
    let response = app
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}
