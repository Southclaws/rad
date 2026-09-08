use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{router, router_with_location, serve};
use crate::engine::catalog::model::Mode;
use crate::engine::exec::Engine;
use crate::engine::kv::fault::{FaultAction, FaultController, FaultRule, FaultingKv, Operation};
use crate::engine::kv::slatedb::Store;
use crate::engine::kv::{ErrorKind as KvErrorKind, TransactionalKv};
use crate::health::Health;

async fn test_router(name: &str, mode: Mode) -> axum::Router {
    let store = Arc::new(Store::memory(name).await.unwrap());
    router(Arc::new(Engine::new(store)), mode)
}

async fn fault_router(name: &str, operation: Operation, kind: KvErrorKind) -> axum::Router {
    let store: Arc<dyn TransactionalKv> = Arc::new(Store::memory(name).await.unwrap());
    let faulting: Arc<dyn TransactionalKv> = Arc::new(FaultingKv::new(
        store,
        FaultController::new(vec![FaultRule {
            operation,
            occurrence: 1,
            action: FaultAction::ErrorBefore(kind),
        }]),
    ));
    router(Arc::new(Engine::new(faulting)), Mode::Direct)
}

fn post_json(uri: &str, body: Value) -> Request<Body> {
    request_json(Method::POST, uri, body)
}

fn post_json_with_diagnostics(uri: &str, body: Value, level: &str) -> Request<Body> {
    let mut request = post_json(uri, body);
    request
        .headers_mut()
        .insert(crate::diagnostics::HEADER, level.parse().unwrap());
    request
}

fn request_json(method: Method, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn request(method: Method, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

fn one_row_program() -> Value {
    json!({
        "statements": [{
            "kind": "query",
            "name": "one",
            "relation": {
                "nodes": {
                    "row": {
                        "kind": "rows",
                        "scope": "row",
                        "columns": [{"name": "value", "type": "int64"}],
                        "rows": [["1"]]
                    }
                },
                "root": {"node": "row", "cardinality": "exactly_one"}
            }
        }]
    })
}

fn create_table_program() -> Value {
    json!({
        "statements": [{
            "kind": "create_table",
            "name": "create_samples",
            "table": {
                "name": "samples",
                "columns": [{"name": "id", "type": "int64"}],
                "primary_key": ["id"]
            }
        }]
    })
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn contains_json_string(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(value) => value == expected,
        Value::Array(values) => values
            .iter()
            .any(|value| contains_json_string(value, expected)),
        Value::Object(fields) => fields
            .values()
            .any(|value| contains_json_string(value, expected)),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

#[tokio::test]
async fn health_reports_the_immutable_catalog_mode() {
    let response = test_router("http-health", Mode::Schema)
        .await
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await,
        json!({"status": "ok", "access": "write", "mode": "schema"})
    );
}

#[tokio::test]
async fn a_serving_router_passes_every_probe() {
    let router = test_router("http-probes-serving", Mode::Schema).await;

    for (path, reason) in [
        ("/startupz", "started"),
        ("/readyz", "serving"),
        ("/livez", "live"),
    ] {
        let response = router
            .clone()
            .oneshot(request(Method::GET, path))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(
            json_body(response).await,
            json!({"reason": reason}),
            "{path}"
        );
    }
}

#[tokio::test]
async fn metrics_route_is_not_found_without_an_installed_metric_provider() {
    let response = test_router("http-metrics-disabled", Mode::Schema)
        .await
        .oneshot(request(Method::GET, "/metrics"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_starting_process_serves_probes_and_nothing_else() {
    let router = super::probe_router(Health::starting(Duration::from_secs(15)));

    let startup = router
        .clone()
        .oneshot(request(Method::GET, "/startupz"))
        .await
        .unwrap();
    assert_eq!(startup.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json_body(startup).await, json!({"reason": "starting"}));

    let ready = router
        .clone()
        .oneshot(request(Method::GET, "/readyz"))
        .await
        .unwrap();
    assert_eq!(ready.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json_body(ready).await, json!({"reason": "starting"}));

    let live = router
        .clone()
        .oneshot(request(Method::GET, "/livez"))
        .await
        .unwrap();
    assert_eq!(live.status(), StatusCode::OK);
    assert_eq!(json_body(live).await, json!({"reason": "live"}));

    let execute = router
        .oneshot(post_json("/execute", one_row_program()))
        .await
        .unwrap();
    assert_eq!(execute.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn draining_withdraws_readiness_while_the_api_still_answers() {
    let store = Arc::new(Store::memory("http-probes-draining").await.unwrap());
    let health = Health::serving();
    let router = super::router_with_health(
        Arc::new(Engine::new(store)),
        Mode::Direct,
        "memory:///draining",
        health.clone(),
    );
    health.drain();

    let ready = router
        .clone()
        .oneshot(request(Method::GET, "/readyz"))
        .await
        .unwrap();
    assert_eq!(ready.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json_body(ready).await, json!({"reason": "draining"}));

    let live = router
        .clone()
        .oneshot(request(Method::GET, "/livez"))
        .await
        .unwrap();
    assert_eq!(live.status(), StatusCode::OK);

    let execute = router
        .oneshot(post_json("/execute", one_row_program()))
        .await
        .unwrap();
    assert_eq!(execute.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_fenced_writer_reports_why_it_is_unready() {
    let health = Health::serving();
    health.observe_fenced();
    let router = super::probe_router(health);

    let ready = router
        .clone()
        .oneshot(request(Method::GET, "/readyz"))
        .await
        .unwrap();
    assert_eq!(ready.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json_body(ready).await, json!({"reason": "fenced"}));

    let live = router
        .oneshot(request(Method::GET, "/livez"))
        .await
        .unwrap();
    assert_eq!(live.status(), StatusCode::OK);
}

#[tokio::test]
async fn public_api_allows_the_admin_origin_and_json_preflights() {
    let router = test_router("http-cors", Mode::Direct).await;

    let preflight = router
        .clone()
        .oneshot(
            Request::options("/execute")
                .header(header::ORIGIN, "http://127.0.0.1:7238")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "content-type")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        preflight.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "*"
    );
    assert!(
        preflight.headers()[header::ACCESS_CONTROL_ALLOW_METHODS]
            .to_str()
            .unwrap()
            .contains("POST")
    );
    assert_eq!(
        preflight.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS],
        "content-type"
    );

    let response = router
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
}

#[tokio::test]
async fn execute_preserves_full_width_integers_over_http() {
    let response = test_router("http-int64", Mode::Direct)
        .await
        .oneshot(post_json(
            "/execute",
            json!({
                "statements": [{
                    "kind": "query",
                    "name": "answer",
                    "relation": {
                        "nodes": {
                            "row": {
                                "kind": "rows",
                                "scope": "literal",
                                "columns": [{"name": "value", "type": "int64"}],
                                "rows": [["9007199254740993"]]
                            }
                        },
                        "root": {"node": "row", "cardinality": "exactly_one"}
                    }
                }]
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    assert_eq!(
        json_body(response).await,
        json!({
            "result": {"value": 9_007_199_254_740_993_i64},
            "statements": [{"name": "answer", "affected": 1, "control": null}]
        })
    );
}

#[tokio::test]
async fn malformed_pir_is_a_typed_bad_request_problem() {
    let response = test_router("http-malformed-pir", Mode::Direct)
        .await
        .oneshot(post_json("/execute", json!({})))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/problem+json"
    );
    let body = json_body(response).await;
    assert_eq!(body["type"], "urn:rad:problem:invalid");
    assert_eq!(body["code"], "invalid");
    assert_eq!(body["reason"], "schema_violation");
    assert_eq!(body["stage"], "schema");
    assert_eq!(body["status"], 400);
}

#[tokio::test]
async fn malformed_json_uses_the_rad_problem_union() {
    let response = test_router("http-malformed-json", Mode::Direct)
        .await
        .oneshot(
            Request::post("/execute")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json_body(response).await;
    assert_eq!(body["type"], "urn:rad:problem:invalid");
    assert_eq!(body["code"], "invalid");
    assert_eq!(body["reason"], "schema_violation");
    assert_eq!(body["status"], 400);
}

#[tokio::test]
async fn execute_preserves_request_validation_at_the_direct_codec_boundary() {
    let router = test_router("http-execute-validation", Mode::Direct).await;
    let duplicate_query = router
        .clone()
        .oneshot(post_json(
            "/execute?show-plan=true&show-plan=false",
            one_row_program(),
        ))
        .await
        .unwrap();
    assert_eq!(duplicate_query.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(duplicate_query).await["code"], "invalid");

    let wrong_media_type = router
        .clone()
        .oneshot(
            Request::post("/execute")
                .header(header::CONTENT_TYPE, "text/plain")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        wrong_media_type.status(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    assert_eq!(json_body(wrong_media_type).await["code"], "invalid");

    let oversized = router
        .oneshot(
            Request::post("/execute")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(vec![b' '; 4 * 1024 * 1024 + 1]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(json_body(oversized).await["code"], "invalid");
}

#[tokio::test]
async fn binding_failure_retains_its_semantic_reason() {
    let response = test_router("http-unknown-table", Mode::Direct)
        .await
        .oneshot(post_json(
            "/execute",
            json!({
                "statements": [{
                    "name": "q",
                    "kind": "query",
                    "relation": {
                        "nodes": {
                            "g": {"kind": "scan", "table": "ghosts", "scope": "g"}
                        },
                        "root": {"node": "g", "cardinality": "many"}
                    }
                }]
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = json_body(response).await;
    assert_eq!(body["code"], "invalid");
    assert_eq!(body["reason"], "unknown_table");
    assert_eq!(body["stage"], "binding");
    assert!(body["detail"].as_str().unwrap().contains("unknown table"));
}

#[tokio::test]
async fn graph_preflight_failure_retains_its_semantic_reason() {
    let response = test_router("http-shared-node", Mode::Direct)
        .await
        .oneshot(post_json(
            "/execute",
            json!({
                "statements": [{
                    "name": "q",
                    "kind": "query",
                    "relation": {
                        "nodes": {
                            "row": {
                                "kind": "rows",
                                "scope": "row",
                                "columns": [{"name": "id", "type": "text"}],
                                "rows": [["a"]]
                            },
                            "both": {
                                "kind": "concatenate",
                                "scope": "both",
                                "inputs": ["row", "row"]
                            }
                        },
                        "root": {"node": "both", "cardinality": "many"}
                    }
                }]
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = json_body(response).await;
    assert_eq!(body["code"], "invalid");
    assert_eq!(body["reason"], "shared_node");
    assert_eq!(body["stage"], "preflight");
    assert!(body["detail"].as_str().unwrap().contains("duplicate scope"));
}

#[tokio::test]
async fn execution_failure_is_a_typed_problem_outside_in() {
    let response = test_router("http-division-by-zero", Mode::Direct)
        .await
        .oneshot(post_json(
            "/execute",
            json!({
                "statements": [{
                    "name": "q",
                    "kind": "query",
                    "relation": {
                        "nodes": {
                            "r": {"kind": "rows", "scope": "r",
                                "columns": [
                                    {"name": "num", "type": "int64"},
                                    {"name": "den", "type": "int64"}
                                ],
                                "rows": [["1", "0"]]},
                            "p": {"kind": "project", "input": "r", "fields": [{
                                "as": "q",
                                "expr": {"kind": "binary", "op": "div",
                                    "left": {"kind": "col", "scope": "r", "column": "num"},
                                    "right": {"kind": "col", "scope": "r", "column": "den"}}
                            }]},
                            "s": {"kind": "slice", "input": "p", "limit": 1}
                        },
                        "root": {"node": "s", "cardinality": "scalar"}
                    }
                }]
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/problem+json"
    );
    let body = json_body(response).await;
    assert_eq!(body["type"], "urn:rad:problem:execution_failed");
    assert_eq!(body["code"], "execution_failed");
    assert_eq!(body["reason"], "division_by_zero");
    assert_eq!(body["stage"], "execution");
    assert_eq!(body["status"], 422);
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("division by zero")
    );
}

#[tokio::test]
async fn missing_transition_is_a_typed_not_found_problem_outside_in() {
    let response = test_router("http-transition-not-found", Mode::Direct)
        .await
        .oneshot(request(Method::GET, "/schema/transitions/does-not-exist"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/problem+json"
    );
    let body = json_body(response).await;
    assert_eq!(body["type"], "urn:rad:problem:not_found");
    assert_eq!(body["code"], "not_found");
    assert_eq!(body["reason"], "schema_transition_not_found");
    assert_eq!(body["status"], 404);
    assert_eq!(body["resource"]["kind"], "schema_transition");
    assert_eq!(body["resource"]["name"], "does-not-exist");
}

#[tokio::test]
async fn storage_conflict_is_a_typed_retryable_problem_outside_in() {
    let response = fault_router("http-conflict", Operation::Commit, KvErrorKind::Conflict)
        .await
        .oneshot(post_json("/execute", create_table_program()))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/problem+json"
    );
    let body = json_body(response).await;
    assert_eq!(body["type"], "urn:rad:problem:conflict");
    assert_eq!(body["code"], "conflict");
    assert_eq!(body["reason"], "serializable_conflict");
    assert_eq!(body["stage"], "execution");
    assert_eq!(body["status"], 409);
}

#[tokio::test]
async fn internal_storage_failure_is_diagnostic_outside_in() {
    let response = fault_router("http-internal", Operation::Begin, KvErrorKind::Internal)
        .await
        .oneshot(post_json("/execute", one_row_program()))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/problem+json"
    );
    let body = json_body(response).await;
    assert_eq!(body["type"], "urn:rad:problem:internal");
    assert_eq!(body["code"], "internal");
    assert_eq!(body["reason"], "storage_unavailable");
    assert_eq!(body["stage"], "storage");
    assert_eq!(body["status"], 500);
    assert_eq!(
        body["detail"],
        "exec storage: fault injection: Begin returned Internal"
    );
    assert!(
        body["incident"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert!(body.get("execution").is_none());
    assert!(body.get("conflict").is_none());
}

#[tokio::test]
async fn dry_run_and_show_plan_are_transport_options() {
    let response = test_router("http-plan", Mode::Direct)
        .await
        .oneshot(post_json(
            "/execute?dry-run=true&show-plan=true",
            json!({
                "statements": [{
                    "kind": "query",
                    "name": "answer",
                    "relation": {
                        "nodes": {
                            "row": {
                                "kind": "rows",
                                "scope": "literal",
                                "columns": [{"name": "value", "type": "int64"}],
                                "rows": [["42"]]
                            }
                        },
                        "root": {"node": "row", "cardinality": "exactly_one"}
                    }
                }]
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["result"], Value::Null);
    assert!(body["plan"]["statements"].is_array());
}

#[tokio::test]
async fn program_diagnostics_are_an_opt_in_response_extension() {
    crate::diagnostics::set_max_level(crate::diagnostics::Level::Summary);
    let policy = test_router("http-program-diagnostic-policy", Mode::Direct).await;
    let denied = policy
        .clone()
        .oneshot(post_json_with_diagnostics(
            "/execute",
            one_row_program(),
            "detailed",
        ))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let invalid = policy
        .oneshot(post_json_with_diagnostics(
            "/execute",
            one_row_program(),
            "trace",
        ))
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);

    crate::diagnostics::set_max_level(crate::diagnostics::Level::Full);
    let app = test_router("http-program-diagnostics", Mode::Direct).await;

    let plain = app
        .clone()
        .oneshot(post_json("/execute", one_row_program()))
        .await
        .unwrap();
    assert!(json_body(plain).await.get("_rad").is_none());

    let summary = app
        .clone()
        .oneshot(post_json_with_diagnostics(
            "/execute",
            one_row_program(),
            "summary",
        ))
        .await
        .unwrap();
    let summary = json_body(summary).await;
    let diagnostic = &summary["_rad"]["diagnostics"];
    assert_eq!(diagnostic["format"], crate::diagnostics::FORMAT);
    assert_eq!(diagnostic["status"], "success");
    assert_eq!(diagnostic["resultRows"], 1);
    assert!(
        diagnostic["programFingerprint"]
            .as_str()
            .unwrap()
            .starts_with("p1:family:sha256:")
    );
    assert!(diagnostic.get("submittedProgram").is_none());
    assert!(diagnostic.get("loweredProgram").is_none());

    let detailed = app
        .clone()
        .oneshot(post_json_with_diagnostics(
            "/execute",
            one_row_program(),
            "detailed",
        ))
        .await
        .unwrap();
    let detailed = json_body(detailed).await;
    assert!(detailed.get("plan").is_none());
    assert!(detailed["_rad"]["diagnostics"]["plans"].is_array());
    assert!(detailed["_rad"]["diagnostics"]["loweredProgramFamily"].is_object());
    assert!(!contains_json_string(&detailed["_rad"]["diagnostics"], "1"));
    assert!(
        detailed["_rad"]["diagnostics"]
            .get("submittedProgram")
            .is_none()
    );

    let full = app
        .oneshot(post_json_with_diagnostics(
            "/execute",
            one_row_program(),
            "full",
        ))
        .await
        .unwrap();
    let full = json_body(full).await;
    assert_eq!(
        full["_rad"]["diagnostics"]["submittedProgram"]["statements"][0]["relation"]["nodes"]["row"]
            ["rows"][0][0],
        "1"
    );
    crate::diagnostics::set_max_level(crate::diagnostics::Level::Summary);
}

#[tokio::test]
async fn program_diagnostics_extend_problem_responses() {
    let response = test_router("http-program-diagnostics-problem", Mode::Direct)
        .await
        .oneshot(post_json_with_diagnostics("/execute", json!({}), "summary"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json_body(response).await;
    assert_eq!(body["_rad"]["diagnostics"]["status"], "error");
}

#[tokio::test]
async fn catalog_authority_follows_the_catalog_mode() {
    let program = json!({
        "statements": [{
            "kind": "create_table",
            "name": "create_widgets",
            "table": {
                "name": "widgets",
                "columns": [{"name": "id", "type": "int64"}],
                "primary_key": ["id"]
            }
        }]
    });

    let direct = test_router("http-direct-catalog", Mode::Direct)
        .await
        .oneshot(post_json("/execute", program.clone()))
        .await
        .unwrap();
    assert_eq!(direct.status(), StatusCode::OK);

    let schema = test_router("http-schema-catalog", Mode::Schema)
        .await
        .oneshot(post_json("/execute", program))
        .await
        .unwrap();
    assert_eq!(schema.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = json_body(schema).await;
    assert_eq!(body["code"], "invalid");

    let schema_crud = test_router("http-schema-catalog-crud", Mode::Schema)
        .await
        .oneshot(post_json(
            "/tables",
            json!({
                "name": "widgets",
                "columns": [{"name": "id", "type": "int64"}],
                "primary_key": ["id"]
            }),
        ))
        .await
        .unwrap();
    assert_eq!(schema_crud.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let direct_schema = test_router("http-direct-schema-channel", Mode::Direct).await;
    let current = json_body(
        direct_schema
            .clone()
            .oneshot(request(Method::GET, "/schema"))
            .await
            .unwrap(),
    )
    .await;
    let migrated = direct_schema
        .oneshot(post_json(
            "/schema/migrate",
            json!({
                "schema": "tables: [{id: 1, name: things, columns: [{id: 1, name: id, type: int64, pk: true}]}]",
                "current_version": current["schema_version"],
                "current_hash": current["schema_hash"]
            }),
        ))
        .await
        .unwrap();
    assert_eq!(migrated.status(), StatusCode::OK);
    assert_eq!(json_body(migrated).await["state"], "ready");
}

#[tokio::test]
async fn metadata_and_schema_migration_cover_the_generated_surface() {
    let app = test_router("http-schema-surface", Mode::Schema).await;

    let info = app
        .clone()
        .oneshot(request(Method::GET, "/info"))
        .await
        .unwrap();
    assert_eq!(info.status(), StatusCode::OK);
    let info = json_body(info).await;
    assert_eq!(info["mode"], "schema");
    assert_eq!(info["schema_version"], 0);

    let current = app
        .clone()
        .oneshot(request(Method::GET, "/schema"))
        .await
        .unwrap();
    assert_eq!(current.status(), StatusCode::OK);
    let current = json_body(current).await;

    let desired = r#"
tables:
  - id: 1
    name: users
    columns:
      - { id: 1, name: id, type: string, pk: true }
"#;
    let diff = app
        .clone()
        .oneshot(post_json("/schema/diff", json!({"schema": desired})))
        .await
        .unwrap();
    assert_eq!(diff.status(), StatusCode::OK);
    let diff = json_body(diff).await;
    assert_eq!(diff["current_version"], 0);
    assert_eq!(diff["program"]["statements"][0]["kind"], "create_table");
    assert!(!diff["desired_hash"].as_str().unwrap().is_empty());

    let migrated = app
        .clone()
        .oneshot(post_json(
            "/schema/migrate",
            json!({
                "schema": desired,
                "current_version": current["schema_version"],
                "current_hash": current["schema_hash"]
            }),
        ))
        .await
        .unwrap();
    assert_eq!(migrated.status(), StatusCode::OK);
    let migrated = json_body(migrated).await;
    assert_eq!(migrated["state"], "ready");
    assert_eq!(migrated["schema_version"], 1);

    let compatible = app
        .clone()
        .oneshot(post_json(
            "/schema/compatibility",
            json!({
                "schema_version": migrated["schema_version"],
                "schema_hash": migrated["schema_hash"]
            }),
        ))
        .await
        .unwrap();
    assert_eq!(compatible.status(), StatusCode::NO_CONTENT);

    let outdated = app
        .clone()
        .oneshot(post_json(
            "/schema/compatibility",
            json!({"schema_version": 0, "schema_hash": current["schema_hash"]}),
        ))
        .await
        .unwrap();
    assert_eq!(outdated.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        json_body(outdated).await["reason"],
        "schema_client_outdated"
    );

    let server_outdated = app
        .clone()
        .oneshot(post_json(
            "/schema/compatibility",
            json!({
                "schema_version": migrated["schema_version"].as_i64().unwrap() + 1,
                "schema_hash": migrated["schema_hash"]
            }),
        ))
        .await
        .unwrap();
    assert_eq!(server_outdated.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        json_body(server_outdated).await["reason"],
        "schema_server_outdated"
    );

    let diverged = app
        .oneshot(post_json(
            "/schema/compatibility",
            json!({
                "schema_version": migrated["schema_version"],
                "schema_hash": "sha256:not-the-same-history"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(diverged.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        json_body(diverged).await["reason"],
        "schema_history_diverged"
    );
}

#[tokio::test]
async fn imperative_catalog_crud_runs_as_engine_programs() {
    let app = test_router("http-catalog-crud", Mode::Direct).await;

    let created = app
        .clone()
        .oneshot(post_json(
            "/tables",
            json!({
                "name": "widgets",
                "columns": [
                    {"name": "id", "type": "int64"},
                    {"name": "label", "type": "text", "nullable": true}
                ],
                "primary_key": ["id"]
            }),
        ))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::OK);
    assert_eq!(json_body(created).await["name"], "widgets");

    let renamed = app
        .clone()
        .oneshot(request_json(
            Method::PATCH,
            "/tables/widgets",
            json!({"name": "gadgets"}),
        ))
        .await
        .unwrap();
    assert_eq!(renamed.status(), StatusCode::OK);
    assert_eq!(json_body(renamed).await["name"], "gadgets");

    let with_column = app
        .clone()
        .oneshot(post_json(
            "/tables/gadgets/columns",
            json!({"name": "note", "type": "text", "nullable": true}),
        ))
        .await
        .unwrap();
    assert_eq!(with_column.status(), StatusCode::OK);

    let renamed_column = app
        .clone()
        .oneshot(request_json(
            Method::PATCH,
            "/tables/gadgets/columns/note",
            json!({"name": "description"}),
        ))
        .await
        .unwrap();
    assert_eq!(renamed_column.status(), StatusCode::OK);

    let with_index = app
        .clone()
        .oneshot(post_json(
            "/tables/gadgets/indexes",
            json!({"name": "gadgets_description", "columns": ["description"]}),
        ))
        .await
        .unwrap();
    assert_eq!(with_index.status(), StatusCode::OK);

    let without_index = app
        .clone()
        .oneshot(request(
            Method::DELETE,
            "/tables/gadgets/indexes/gadgets_description",
        ))
        .await
        .unwrap();
    assert_eq!(without_index.status(), StatusCode::OK);

    let without_column = app
        .clone()
        .oneshot(request(
            Method::DELETE,
            "/tables/gadgets/columns/description",
        ))
        .await
        .unwrap();
    assert_eq!(without_column.status(), StatusCode::OK);

    let listed = app
        .clone()
        .oneshot(request(Method::GET, "/tables"))
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let listed = json_body(listed).await;
    assert_eq!(listed["tables"][0]["name"], "gadgets");
    assert_eq!(listed["tables"][0]["columns"].as_array().unwrap().len(), 2);

    let deleted = app
        .oneshot(request(Method::DELETE, "/tables/gadgets"))
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn transition_administration_lists_inspects_and_cancels_durable_work() {
    let app = test_router("http-transition-admin", Mode::Direct).await;
    let created = app
        .clone()
        .oneshot(post_json(
            "/tables",
            json!({
                "name": "events",
                "columns": [{"name": "id", "type": "int64"}],
                "primary_key": ["id"]
            }),
        ))
        .await
        .unwrap();
    let table_id = json_body(created).await["id"].clone();

    let started = app
        .clone()
        .oneshot(post_json(
            "/execute",
            json!({"statements": [{
                "kind": "start_index_build",
                "name": "build_events_id",
                "table_id": table_id,
                "index": {"name": "events_id_lookup", "columns": ["id"]}
            }]}),
        ))
        .await
        .unwrap();
    assert_eq!(started.status(), StatusCode::OK);
    let started = json_body(started).await;
    let transition = started["statements"][0]["control"]["transition_id"]
        .as_str()
        .unwrap();

    let listed = app
        .clone()
        .oneshot(request(
            Method::GET,
            "/schema/transitions?kind=index_build&state=building",
        ))
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    assert_eq!(
        json_body(listed).await["transitions"][0]["transition_id"],
        transition
    );

    let inspected = app
        .clone()
        .oneshot(request(
            Method::GET,
            &format!("/schema/transitions/{transition}"),
        ))
        .await
        .unwrap();
    assert_eq!(inspected.status(), StatusCode::OK);

    let cancelled = app
        .clone()
        .oneshot(request(
            Method::POST,
            &format!("/schema/transitions/{transition}/cancel"),
        ))
        .await
        .unwrap();
    assert_eq!(cancelled.status(), StatusCode::OK);
    assert_eq!(json_body(cancelled).await["state"], "cancelled");

    let missing = app
        .oneshot(request(
            Method::GET,
            "/schema/transitions/tr-does-not-exist",
        ))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let missing = json_body(missing).await;
    assert_eq!(missing["reason"], "schema_transition_not_found");
    assert_eq!(missing["resource"]["kind"], "schema_transition");
}

#[tokio::test]
async fn generated_router_serves_over_a_real_tcp_listener_and_shuts_down() {
    let store = Arc::new(Store::memory("http-listener").await.unwrap());
    let engine = Arc::new(Engine::new(store));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(serve(
        listener,
        router_with_location(engine, Mode::Direct, "memory:///listener"),
        async move {
            let _ = stopped.await;
        },
    ));

    let response = reqwest::get(format!("http://{address}/info"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.json::<Value>().await.unwrap();
    assert_eq!(body["location"], "memory:///listener");

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn statistics_endpoint_reports_no_collector_when_none_is_installed() {
    let router = test_router("http-statistics-absent", Mode::Direct).await;
    let response = router
        .oneshot(Request::get("/statistics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    // Distinct from an empty body: this instance never collects.
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = json_body(response).await;
    assert_eq!(body["code"], "not_found");
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("no statistics collector")
    );
}

/// An instance that publishes to storage never sends, so its send counters
/// are absent rather than zero. Reporting zero would read as a relay that is
/// failing to send, which is a different thing from one that does not exist.
#[tokio::test]
async fn statistics_endpoint_distinguishes_a_writer_from_a_failing_relay() {
    let store = Arc::new(Store::memory("http-statistics-relay-absent").await.unwrap());
    let runner = crate::scheduler::statistics::StatisticsRunner::start(
        Arc::new(crate::runtime::SystemRuntime),
        crate::scheduler::statistics::StatisticsConfig {
            publish_interval: Duration::from_millis(10),
            ..Default::default()
        },
        None,
        None,
    );
    let engine = Arc::new(
        Engine::new(store.clone())
            .with_observer(runner.collector())
            .with_statistics_provider(runner.clone()),
    );
    let response = router(engine, Mode::Direct)
        .oneshot(
            Request::builder()
                .uri("/statistics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();

    let relay = &body["relay"];
    for absent in ["state", "sent", "abandoned", "rejected", "holding"] {
        assert!(
            relay[absent].is_null(),
            "an instance that does not relay reported {absent}: {relay}"
        );
    }
    // Receiving is always reported: every instance can receive, and zero is
    // the answer rather than the absence of one.
    for present in [
        "received",
        "receivedAlreadyApplied",
        "receivedRejected",
        "receivedSaturated",
        "sources",
        "mergedObservations",
        "refusedStaleFamilies",
    ] {
        assert_eq!(
            relay[present].as_i64(),
            Some(0),
            "{present} was not reported as zero: {relay}"
        );
    }
    assert!(body["corpus"]["maintenance"].is_null());
    runner.shutdown().await;
}

#[tokio::test]
async fn statistics_endpoint_reports_owner_corpus_storage() {
    use crate::engine::exec::observe::ProgramRecord;
    use crate::engine::planner::models::{
        ColumnGroupSynopsis, ColumnSynopsis, MostCommonColumnGroup, MostCommonValue,
        SynopsisCoverage, SynopsisModel, SynopsisValue,
    };
    use crate::scheduler::statistics::{SlateStatistics, StatisticsBatch, StatisticsSink as _};

    let store = Arc::new(
        Store::memory("http-statistics-corpus-maintenance")
            .await
            .unwrap(),
    );
    let slate = Arc::new(SlateStatistics::new(store.clone()));
    let at_unix_micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64;
    assert!(
        slate
            .publish(StatisticsBatch {
                programs: vec![ProgramRecord {
                    canonical: vec![1, 2, 3],
                    content_hash: [7; 16],
                    at_unix_micros,
                    statements: 1,
                    outcomes: Vec::new(),
                }],
                synopses: vec![SynopsisModel {
                    table: crate::engine::catalog::identity::SchemaId::new(7).unwrap(),
                    observed_rows: 10,
                    coverage: SynopsisCoverage::Complete,
                    sample_size: 10,
                    changes_since_collection: 0,
                    table_existence_generation: 2,
                    collected_at_unix_micros: at_unix_micros,
                    catalog_version: 3,
                    columns: vec![ColumnSynopsis {
                        column: crate::engine::catalog::identity::SchemaId::new(8).unwrap(),
                        value_generation: 4,
                        null_fraction: 0.0,
                        null_count: 0,
                        distinct: 2,
                        distinct_is_exact: true,
                        average_width: 4,
                        maximum_width: Some(4),
                        minimum: Some("\"cold\"".into()),
                        maximum: Some("\"hot\"".into()),
                        most_common_values: vec![MostCommonValue {
                            value: SynopsisValue::Text("hot".into()),
                            frequency: 8,
                            maximum_error: 1,
                        }],
                        range_distribution: None,
                        degree_sequence: None,
                    }],
                    column_groups: vec![ColumnGroupSynopsis {
                        columns: vec![
                            crate::engine::catalog::identity::SchemaId::new(8).unwrap(),
                            crate::engine::catalog::identity::SchemaId::new(9).unwrap(),
                        ],
                        value_generations: vec![4, 5],
                        null_count: 1,
                        distinct: 3,
                        distinct_is_exact: true,
                        most_common_values: vec![MostCommonColumnGroup {
                            values: vec![
                                SynopsisValue::Text("hot".into()),
                                SynopsisValue::Bool(true),
                            ],
                            frequency: 6,
                            maximum_error: 1,
                        }],
                        degree_sequence: None,
                    }],
                    predicate_conditioned_degrees: Vec::new(),
                }],
                ..StatisticsBatch::default()
            })
            .await
            .is_ok()
    );
    let runner = crate::scheduler::statistics::StatisticsRunner::start(
        Arc::new(crate::runtime::SystemRuntime),
        crate::scheduler::statistics::StatisticsConfig {
            publish_interval: Duration::from_millis(10),
            capture_programs: true,
            ..Default::default()
        },
        Some(slate.clone()),
        Some(slate),
    );
    let engine = Arc::new(
        Engine::new(store)
            .with_observer(runner.collector())
            .with_statistics_provider(runner.clone()),
    );
    let app = router(engine, Mode::Direct);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let response = app
            .clone()
            .oneshot(Request::get("/statistics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = json_body(response).await;
        let maintenance = &body["corpus"]["maintenance"];
        if !maintenance.is_null() {
            assert_eq!(maintenance["retainedExecutions"], 1);
            assert_eq!(maintenance["retainedPrograms"], 1);
            assert_eq!(maintenance["retainedProgramBytes"], 3);
            assert_eq!(maintenance["expiredExecutions"], 0);
            assert_eq!(maintenance["prunedExecutions"], 0);
            assert_eq!(body["synopses"][0]["table"], 7);
            assert_eq!(body["synopses"][0]["coverage"], "complete");
            assert_eq!(body["synopses"][0]["columns"][0]["valueGeneration"], 4);
            let common = &body["synopses"][0]["columns"][0]["mostCommonValues"][0];
            assert_eq!(common["value"], "\"hot\"");
            assert_eq!(common["frequency"], 8);
            assert_eq!(common["lowerFrequency"], 7);
            assert_eq!(common["maximumError"], 1);
            let group = &body["synopses"][0]["columnGroups"][0];
            assert_eq!(group["columns"], serde_json::json!([8, 9]));
            assert_eq!(group["valueGenerations"], serde_json::json!([4, 5]));
            assert_eq!(group["nullCount"], 1);
            assert_eq!(group["distinct"], 3);
            let common = &group["mostCommonValues"][0];
            assert_eq!(common["values"], serde_json::json!(["\"hot\"", "true"]));
            assert_eq!(common["frequency"], 6);
            assert_eq!(common["lowerFrequency"], 5);
            assert_eq!(common["maximumError"], 1);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "corpus maintenance was not published: {body}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    runner.shutdown().await;
}

/// A relaying instance reports what its transport is doing, and a batch that
/// arrives is visible on the receiving side. Both are the only way an operator
/// can tell a working channel from one silently losing evidence.
#[tokio::test]
async fn statistics_endpoint_reports_both_directions_of_the_relay() {
    use crate::scheduler::relay::{ObservationBatch, RELAY_FORMAT};

    let store = Arc::new(Store::memory("http-statistics-relay-live").await.unwrap());
    let runner = crate::scheduler::statistics::StatisticsRunner::start(
        Arc::new(crate::runtime::SystemRuntime),
        crate::scheduler::statistics::StatisticsConfig {
            publish_interval: Duration::from_millis(10),
            ..Default::default()
        },
        None,
        None,
    );
    let engine = Arc::new(
        Engine::new(store.clone())
            .with_observer(runner.collector())
            .with_statistics_provider(runner.clone()),
    );
    let router = router(engine, Mode::Direct);

    let batch = ObservationBatch {
        format: RELAY_FORMAT,
        instance: "reader-1".into(),
        boot: "boot-1".into(),
        sequence: 1,
        sent_at_micros: 0,
        families: Vec::new(),
        frequency: Vec::new(),
        corpus: Vec::new(),
    };
    assert_eq!(
        runner.ingest().submit(batch.clone()),
        crate::scheduler::relay::IngestOutcome::Accepted
    );
    // The same sequence again: at-least-once delivery makes a repeat ordinary
    // traffic, and it must be reported as such rather than as a fault.
    runner.ingest().submit(batch);
    runner.ingest().submit(ObservationBatch {
        format: RELAY_FORMAT + 1,
        instance: "reader-1".into(),
        boot: "boot-1".into(),
        sequence: 2,
        sent_at_micros: 0,
        families: Vec::new(),
        frequency: Vec::new(),
        corpus: Vec::new(),
    });

    let response = router
        .oneshot(
            Request::builder()
                .uri("/statistics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();

    let relay = &body["relay"];
    assert_eq!(relay["received"].as_i64(), Some(1), "{relay}");
    assert_eq!(relay["receivedAlreadyApplied"].as_i64(), Some(1), "{relay}");
    assert_eq!(relay["receivedRejected"].as_i64(), Some(1), "{relay}");
    assert_eq!(relay["sources"].as_i64(), Some(1), "{relay}");
    runner.shutdown().await;
}

#[tokio::test]
async fn statistics_endpoint_publishes_the_planner_models_it_holds() {
    let store = Arc::new(Store::memory("http-statistics-live").await.unwrap());
    let source = Arc::new(crate::scheduler::statistics::SlateStatistics::new(
        store.clone(),
    ));
    let runner = crate::scheduler::statistics::StatisticsRunner::start(
        Arc::new(crate::runtime::SystemRuntime),
        crate::scheduler::statistics::StatisticsConfig {
            publish_interval: Duration::from_millis(10),
            ..Default::default()
        },
        Some(source),
        None,
    );
    let engine = Arc::new(
        Engine::new(store.clone())
            .with_observer(runner.collector())
            .with_statistics_provider(runner.clone()),
    );
    let router = router(engine.clone(), Mode::Direct);

    let created = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/tables")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "name": "items",
                        "columns": [{"name": "id", "type": "text"}],
                        "primary_key": ["id"],
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::OK);

    let executed = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/execute")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "result": "read",
                        "statements": [{
                            "name": "read",
                            "kind": "query",
                            "relation": {
                                "nodes": {
                                    "s": {"kind": "scan", "table": "items", "scope": "i"},
                                    "o": {"kind": "order", "input": "s", "terms": [
                                        {"expr": {"kind": "col", "scope": "i", "column": "id"}}]},
                                },
                                "root": {"node": "o", "cardinality": "many"},
                            },
                        }],
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(executed.status(), StatusCode::OK);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let response = router
            .clone()
            .oneshot(Request::get("/statistics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        if body["absorbed"].as_i64().unwrap() > 0 && !body["physicalCost"].is_null() {
            assert_eq!(body["dropped"], 0);
            let models = body["models"].as_array().unwrap();
            assert!(!models.is_empty(), "a statement was executed: {body}");
            let model = &models[0];
            // The contract's own consistency rules, checked on real output.
            for entry in models {
                assert!(
                    entry["qErrorP50UpperBound"].as_f64().unwrap()
                        <= entry["qErrorMax"].as_f64().unwrap().max(0.0),
                    "a quantile bound exceeded the exact maximum: {entry}"
                );
                assert!(
                    entry["rowsP50UpperBound"].as_i64().unwrap()
                        <= entry["rowsMax"].as_i64().unwrap(),
                    "a row bound exceeded the exact maximum: {entry}"
                );
                assert!(
                    entry["retainedExecutions"].as_i64().unwrap() >= 1,
                    "a modelled family was measured at least once: {entry}"
                );
                assert!(entry["family"].as_str().unwrap().starts_with('c'));
            }
            assert!(model["frequency"].as_i64().unwrap() >= 1);
            let physical = &body["physicalCost"];
            assert_eq!(physical["basis"], "backend_physical_telemetry");
            assert_eq!(physical["backend"], "slatedb");
            assert_eq!(physical["telemetryFormat"], 1);
            assert_eq!(physical["capabilities"]["requestLatency"], true);
            assert_eq!(physical["capabilities"]["requestBytes"], false);
            assert_eq!(physical["capabilities"]["accessLocality"], false);
            assert!(!physical["requests"].as_array().unwrap().is_empty());
            let statement = models
                .iter()
                .find(|model| model["kind"] == "statement")
                .expect("statement model");
            assert_eq!(statement["resourceCost"]["basis"], "logical_kv_work");
            assert_eq!(statement["resourceCost"]["observedExecutions"], 1);
            assert_eq!(statement["resourceCost"]["scans"]["maximum"], 1);
            assert!(
                models
                    .iter()
                    .filter(|model| model["kind"] == "relation")
                    .all(|model| model["resourceCost"].is_null())
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "statistics never published"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    runner.shutdown().await;
    TransactionalKv::close(store.as_ref()).await.unwrap();
}
