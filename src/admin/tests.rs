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

#[tokio::test]
async fn statistics_endpoint_reports_disabled_and_live_states() {
    let store = Arc::new(Store::memory("admin-stats-off").await.unwrap());
    // A reader runs no collector, which is distinct from a collector that has
    // seen nothing yet.
    let absent = router(store.clone())
        .oneshot(Request::get("/statistics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(absent.status(), StatusCode::NOT_FOUND);

    let runner = crate::scheduler::statistics::StatisticsRunner::start(
        Arc::new(crate::runtime::SystemRuntime),
        crate::scheduler::statistics::StatisticsConfig {
            publish_interval: std::time::Duration::from_millis(10),
            ..Default::default()
        },
        None,
        None,
    );
    let engine = crate::engine::exec::Engine::new(store.clone()).with_observer(runner.collector());
    let catalog = crate::engine::catalog::Catalog::new(store.clone());
    catalog
        .create_table(crate::engine::catalog::model::TableDraft {
            id: None,
            name: "items".into(),
            columns: vec![crate::engine::catalog::model::ColumnDraft {
                id: None,
                name: "id".into(),
                scalar_type: crate::engine::catalog::model::ScalarType::Text,
                nullable: false,
                format: String::new(),
                default: None,
            }],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    engine
        .execute_program(
            crate::engine::exec::Program {
                statements: vec![crate::engine::exec::Statement::Query {
                    name: "read".into(),
                    relation: crate::engine::lir::Query {
                        root: crate::engine::lir::Relation::Order {
                            input: Box::new(crate::engine::lir::Relation::Scan {
                                table: "items".into(),
                                scope: "i".into(),
                            }),
                            terms: vec![crate::engine::lir::OrderTerm {
                                expression: crate::engine::lir::Expr::Column {
                                    scope: "i".into(),
                                    name: "id".into(),
                                },
                                descending: false,
                            }],
                        },
                        cardinality: crate::engine::lir::RootCardinality::Many,
                        bindings: Default::default(),
                    },
                }],
                result: Some("read".into()),
            },
            crate::engine::exec::CatalogPolicy::Forbidden,
        )
        .await
        .unwrap();

    let app = super::router_with_statistics(store, Some(runner.clone()));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let live = json(app.clone(), "/statistics").await;
        if live["absorbed"] == 1 {
            // The statement's own family plus every attributed relation
            // inside it are modelled separately.
            assert!(live["trackedFamilies"].as_u64().unwrap() >= 1);
            assert_eq!(live["models"][0]["retainedExecutions"], 1);
            let statement = live["models"]
                .as_array()
                .unwrap()
                .iter()
                .find(|model| model["kind"] == "statement")
                .expect("statement model");
            assert_eq!(statement["resourceCost"]["basis"], "logical_kv_work");
            assert_eq!(statement["resourceCost"]["observedExecutions"], 1);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "never published: {live}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    runner.shutdown().await;
}

#[tokio::test]
async fn corpus_erasure_requires_the_storage_owner() {
    let store = Arc::new(Store::memory("admin-corpus-erase").await.unwrap());
    let absent = router(store.clone())
        .oneshot(
            Request::delete("/api/statistics/corpus")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(absent.status(), StatusCode::CONFLICT);

    let slate = Arc::new(crate::scheduler::statistics::SlateStatistics::new(
        store.clone(),
    ));
    let at_unix_micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64;
    assert!(
        crate::scheduler::statistics::StatisticsSink::publish(
            slate.as_ref(),
            crate::scheduler::statistics::StatisticsBatch {
                programs: vec![crate::engine::exec::observe::ProgramRecord {
                    canonical: vec![1, 2, 3],
                    content_hash: [9; 16],
                    at_unix_micros,
                    statements: 1,
                    outcomes: Vec::new(),
                }],
                ..Default::default()
            },
        )
        .await
        .is_ok()
    );
    let runner = crate::scheduler::statistics::StatisticsRunner::start(
        Arc::new(crate::runtime::SystemRuntime),
        crate::scheduler::statistics::StatisticsConfig::default(),
        Some(slate.clone()),
        Some(slate),
    );
    let app = super::router_with_statistics(store, Some(runner.clone()));
    let response = app
        .clone()
        .oneshot(
            Request::delete("/api/statistics/corpus")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["deleted"], 2);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let live = json(app.clone(), "/statistics").await;
        let maintenance = &live["corpus"]["maintenance"];
        if maintenance["erasedExecutions"] == 1 && maintenance["erasedPrograms"] == 1 {
            assert_eq!(maintenance["retainedExecutions"], 0);
            assert_eq!(maintenance["retainedPrograms"], 0);
            assert_eq!(maintenance["retainedProgramBytes"], 0);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "corpus erasure was not published: {live}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    runner.shutdown().await;
}

#[tokio::test]
async fn corpus_replay_is_bounded_and_returns_an_aggregate_report() {
    let store = Arc::new(Store::memory("admin-corpus-replay").await.unwrap());
    let slate = Arc::new(crate::scheduler::statistics::SlateStatistics::new(
        store.clone(),
    ));
    let runner = crate::scheduler::statistics::StatisticsRunner::start(
        Arc::new(crate::runtime::SystemRuntime),
        crate::scheduler::statistics::StatisticsConfig::default(),
        Some(slate.clone()),
        Some(slate),
    );
    runner.attach_engine(Arc::new(crate::engine::exec::Engine::new(store.clone())));
    let app = super::router_with_statistics(store, Some(runner.clone()));

    let response = app
        .clone()
        .oneshot(
            Request::post("/api/statistics/corpus/replay")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"limit":10}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["executionsConsidered"], 0);
    assert_eq!(body["observedPlanRegret"]["statementsCompared"], 0);
    assert_eq!(
        body["observedPlanRegret"]["basis"],
        "historical_p50_execution_time_for_same_family_row_class_and_access_stamp"
    );
    assert_eq!(body["baseline"]["estimator"], "active");
    assert_eq!(
        body["candidate"]["estimator"],
        "family_feedback_with_active_fallback"
    );

    let response = app
        .oneshot(
            Request::post("/api/statistics/corpus/replay")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"limit":10001}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    runner.shutdown().await;
}
