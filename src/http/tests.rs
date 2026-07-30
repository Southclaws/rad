use std::sync::Arc;

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

#[tokio::test]
async fn health_reports_the_immutable_catalog_mode() {
    let response = test_router("http-health", Mode::Schema)
        .await
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await,
        json!({"status": "ok", "mode": "schema"})
    );
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
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
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
async fn malformed_json_from_the_generated_router_uses_the_rad_problem_union() {
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
async fn internal_storage_failure_is_redacted_outside_in() {
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
    assert_eq!(body["reason"], "internal");
    assert_eq!(body["status"], 500);
    assert_eq!(body["detail"], "internal error");
    assert!(body.get("execution").is_none());
    assert!(body.get("conflict").is_none());
    assert!(!body.to_string().contains("injected"));
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
