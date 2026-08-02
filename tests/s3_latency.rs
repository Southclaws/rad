use std::env;

use serde_json::json;

mod support;

use support::commerce::{self, Counts};
use support::http_process::RadProcess;
use support::s3::{RustFs, TestResult};
use support::toxiproxy::ToxiProxy;

const DEFAULT_SEED: u64 = 0x5241_442d_5333_4c41;

#[tokio::test]
#[ignore = "requires a Docker daemon"]
async fn seeded_s3_latency_preserves_http_write_reopen_and_query_correctness() -> TestResult {
    let seed = env::var("RAD_TEST_S3_LATENCY_SEED")
        .ok()
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(DEFAULT_SEED);
    let latencies = latency_schedule(seed);
    let benchmark = commerce::load_benchmark()?;
    let schema = commerce::schema(&benchmark)?;
    let rustfs = RustFs::start_or_external().await?;
    let proxy = ToxiProxy::start_seeded(&rustfs.config.endpoint, "rustfs-latency", seed).await?;
    let prefix = format!("s3-latency-{seed:016x}");

    proxy.set_latency(latencies[0], latencies[0] / 2).await?;
    let server = RadProcess::start_s3(&rustfs.config, &proxy.endpoint, &prefix).await?;
    server.migrate(&schema).await?;
    let loaded = commerce::load_dataset(&server, Counts::LATENCY, 32).await?;
    assert_eq!(loaded.rows, 284);
    audit(&server, Counts::LATENCY).await?;
    server.stop().await?;

    proxy.set_latency(latencies[1], latencies[1] / 2).await?;
    let server = RadProcess::start_s3(&rustfs.config, &proxy.endpoint, &prefix).await?;
    audit(&server, Counts::LATENCY).await?;
    update_one_order(&server).await?;
    audit_updated_order(&server).await?;
    server.stop().await?;

    proxy.set_latency(latencies[2], latencies[2] / 2).await?;
    let server = RadProcess::start_s3(&rustfs.config, &proxy.endpoint, &prefix).await?;
    audit(&server, Counts::LATENCY).await?;
    audit_updated_order(&server).await?;
    server.stop().await?;

    let trace = json!({
        "format": "rad-s3-latency-v1",
        "seed": seed,
        "latency_ms": latencies,
        "jitter_ms": latencies.map(|latency| latency / 2),
        "rows": loaded.rows,
        "batches": loaded.batches,
        "reopens": 2
    });
    println!("{}", serde_json::to_string(&trace)?);
    if let Ok(directory) = env::var("RAD_TEST_ARTIFACT_DIR") {
        std::fs::create_dir_all(&directory)?;
        std::fs::write(
            std::path::Path::new(&directory).join(format!("s3-latency-{seed}.json")),
            serde_json::to_vec_pretty(&trace)?,
        )?;
    }
    Ok(())
}

async fn audit(server: &RadProcess, counts: Counts) -> TestResult {
    let response = server
        .execute(&json!({
            "statements": [{
                "name": "latency-audit",
                "kind": "query",
                "relation": {
                    "nodes": {
                        "items": { "kind": "scan", "table": "order_items", "scope": "i" },
                        "products": { "kind": "scan", "table": "products", "scope": "p" },
                        "joined": {
                            "kind": "join", "left": "items", "right": "products", "join": "inner",
                            "on": {
                                "kind": "binary", "op": "eq",
                                "left": { "kind": "col", "scope": "i", "column": "item_product_id" },
                                "right": { "kind": "col", "scope": "p", "column": "product_id" }
                            }
                        },
                        "totals": {
                            "kind": "aggregate", "input": "joined", "scope": "a",
                            "aggs": [
                                { "fn": "count", "as": "item_count", "arg": { "kind": "col", "scope": "i", "column": "item_order_id" } },
                                { "fn": "sum", "as": "quantity", "arg": { "kind": "col", "scope": "i", "column": "item_quantity" } }
                            ]
                        }
                    },
                    "root": { "node": "totals", "cardinality": "exactly_one" }
                }
            }]
        }))
        .await?;
    let expected_quantity = (0..counts.order_items)
        .map(|index| 1 + index % 4)
        .sum::<usize>();
    if response["result"]
        != json!({ "item_count": counts.order_items, "quantity": expected_quantity })
    {
        return Err(format!("latency audit returned the wrong result: {response}").into());
    }
    Ok(())
}

async fn update_one_order(server: &RadProcess) -> TestResult {
    let response = server
        .execute(&json!({
            "statements": [{
                "name": "update-after-reopen",
                "kind": "update",
                "table": "orders",
                "relation": {
                    "nodes": {
                        "row": {
                            "kind": "rows", "scope": "input",
                            "columns": [
                                { "name": "order_id", "type": "text" },
                                { "name": "order_customer_id", "type": "text" },
                                { "name": "order_status", "type": "text" },
                                { "name": "order_created_at", "type": "int64" },
                                { "name": "order_total", "type": "int64" }
                            ],
                            "rows": [["order-000007", "customer-0007", "refunded", "1700000007", "7777"]]
                        }
                    },
                    "root": { "node": "row", "cardinality": "many" }
                }
            }]
        }))
        .await?;
    if response["statements"][0]["affected"] != 1 {
        return Err(format!("latency update affected the wrong row count: {response}").into());
    }
    Ok(())
}

async fn audit_updated_order(server: &RadProcess) -> TestResult {
    let response = server
        .execute(&json!({
            "statements": [{
                "name": "read-after-reopen",
                "kind": "query",
                "relation": {
                    "nodes": {
                        "orders": { "kind": "scan", "table": "orders", "scope": "o" },
                        "selected": {
                            "kind": "filter", "input": "orders",
                            "predicate": {
                                "kind": "binary", "op": "eq",
                                "left": { "kind": "col", "scope": "o", "column": "order_id" },
                                "right": { "kind": "lit", "value": { "type": "text", "value": "order-000007" } }
                            }
                        }
                    },
                    "root": { "node": "selected", "cardinality": "exactly_one" }
                }
            }]
        }))
        .await?;
    if response["result"]["order_status"] != "refunded" || response["result"]["order_total"] != 7777
    {
        return Err(format!("updated order did not survive reopen: {response}").into());
    }
    Ok(())
}

fn latency_schedule(seed: u64) -> [u64; 3] {
    let mut state = seed;
    std::array::from_fn(|_| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        5 + state % 36
    })
}

#[test]
fn latency_schedule_is_seeded_bounded_and_reproducible() {
    let first = latency_schedule(DEFAULT_SEED);
    assert_eq!(first, latency_schedule(DEFAULT_SEED));
    assert_ne!(first, latency_schedule(DEFAULT_SEED + 1));
    assert!(first.into_iter().all(|latency| (5..=40).contains(&latency)));
}

#[test]
fn http_process_reserves_public_and_admin_ports_together() {
    let (port, public, admin) = support::http_process::reserve_port_pair().unwrap();
    assert!((12_000..30_000).contains(&port));
    assert_eq!(public.local_addr().unwrap().port(), port);
    assert_eq!(admin.local_addr().unwrap().port(), port + 1);
    assert_eq!(
        std::net::TcpListener::bind(("127.0.0.1", port))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AddrInUse
    );
    assert_eq!(
        std::net::TcpListener::bind(("127.0.0.1", port + 1))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AddrInUse
    );
}

#[tokio::test]
async fn toxiproxy_seed_rejects_values_its_signed_flag_cannot_replay() {
    for seed in [0, u64::MAX] {
        let error = ToxiProxy::start_seeded("not-an-endpoint", "unused", seed)
            .await
            .err()
            .expect("invalid seed should fail before Docker startup");
        assert!(error.to_string().contains("between 1 and i64::MAX"));
    }
}
