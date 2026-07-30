use std::time::{Duration, Instant};

use bytes::Bytes;
use rad::engine::kv::slatedb::Store;
use rad::engine::kv::{ErrorKind, IsolationLevel, Kv, TransactionalKv};
use serde_json::json;

mod support;

use support::s3::{RustFs, TestResult, fail_fast_object_store_at, object_store};
use support::toxiproxy::ToxiProxy;

const WINNER_KEY: &[u8] = b"network/winner";
const STALE_KEY: &[u8] = b"network/stale";

#[tokio::test]
#[ignore = "requires a Docker daemon"]
async fn rustfs_network_fault_matrix_preserves_commit_and_writer_safety() -> TestResult {
    let rustfs = RustFs::start_or_external().await?;
    let proxy = ToxiProxy::start(&rustfs.config.endpoint, "rustfs").await?;

    let faults = [
        FaultCase {
            name: "downstream-reset",
            toxic: json!({
                "name": "downstream-reset",
                "type": "reset_peer",
                "stream": "downstream",
                "toxicity": 1.0,
                "attributes": { "timeout": 0 }
            }),
            repair_after: Duration::from_millis(250),
            minimum_commit_delay: Duration::ZERO,
        },
        FaultCase {
            name: "upstream-blackhole",
            toxic: json!({
                "name": "upstream-blackhole",
                "type": "timeout",
                "stream": "upstream",
                "toxicity": 1.0,
                "attributes": { "timeout": 0 }
            }),
            repair_after: Duration::from_millis(2_500),
            minimum_commit_delay: Duration::from_secs(2),
        },
        FaultCase {
            name: "downstream-blackhole",
            toxic: json!({
                "name": "downstream-blackhole",
                "type": "timeout",
                "stream": "downstream",
                "toxicity": 1.0,
                "attributes": { "timeout": 0 }
            }),
            repair_after: Duration::from_millis(2_500),
            minimum_commit_delay: Duration::from_secs(2),
        },
    ];

    for fault in faults {
        exercise_commit_fault(&rustfs, &proxy, fault).await?;
    }
    Ok(())
}

struct FaultCase {
    name: &'static str,
    toxic: serde_json::Value,
    repair_after: Duration,
    minimum_commit_delay: Duration,
}

async fn exercise_commit_fault(rustfs: &RustFs, proxy: &ToxiProxy, fault: FaultCase) -> TestResult {
    let path = format!("toxiproxy-unknown-commit-{}", fault.name);
    let store = Store::open(
        path.clone(),
        fail_fast_object_store_at(&rustfs.config, &proxy.endpoint)?,
    )
    .await?;
    store
        .put(
            Bytes::from_static(b"network/baseline"),
            Bytes::from_static(b"durable"),
        )
        .await?;
    let transaction = store.begin(IsolationLevel::SerializableSnapshot).await?;
    transaction.put(
        Bytes::from_static(b"network/first"),
        Bytes::from_static(b"first"),
    )?;
    transaction.put(
        Bytes::from_static(b"network/second"),
        Bytes::from_static(b"second"),
    )?;

    proxy.add_toxic(fault.toxic).await?;
    assert_proxy_fault(&proxy.endpoint, fault.name).await?;
    let repair_after = fault.repair_after;
    let repair = async {
        tokio::time::sleep(repair_after).await;
        proxy.remove_toxic(fault.name).await
    };
    let commit_started = Instant::now();
    let commit = async {
        let outcome = tokio::time::timeout(Duration::from_secs(15), transaction.commit())
            .await
            .map_err(|_| {
                format!(
                    "Slate commit remained stuck after {} was repaired",
                    fault.name
                )
            });
        (outcome, commit_started.elapsed())
    };
    let ((outcome, commit_elapsed), repair) = tokio::join!(commit, repair);
    let outcome = outcome?;
    repair?;
    assert!(
        commit_elapsed >= fault.minimum_commit_delay,
        "{} did not delay durable commit: elapsed={commit_elapsed:?}, expected at least {:?}",
        fault.name,
        fault.minimum_commit_delay
    );
    if let Err(error) = &outcome {
        assert_eq!(
            error.kind(),
            ErrorKind::CommitOutcomeUnknown,
            "{} returned the wrong commit classification: {error}",
            fault.name
        );
    }
    let commit_class = if outcome.is_ok() {
        "acknowledged"
    } else {
        "unknown"
    };

    let stale = match store.begin(IsolationLevel::SerializableSnapshot).await {
        Ok(transaction) => {
            transaction.put(
                Bytes::from_static(STALE_KEY),
                Bytes::from_static(b"must-not-commit"),
            )?;
            Some(transaction)
        }
        Err(error) if error.kind() == ErrorKind::Closed => None,
        Err(error) => {
            return Err(format!(
                "{} could not stage the writer-fencing probe: {error}",
                fault.name
            )
            .into());
        }
    };

    let mut recovered_after = None;
    for pass in 0..2 {
        let recovered = Store::open(path.clone(), object_store(&rustfs.config)?).await?;
        assert_eq!(
            recovered.get(b"network/baseline").await?,
            Some(Bytes::from_static(b"durable"))
        );
        let first = recovered.get(b"network/first").await?;
        let second = recovered.get(b"network/second").await?;
        if outcome.is_ok() {
            assert_eq!(first, Some(Bytes::from_static(b"first")));
            assert_eq!(second, Some(Bytes::from_static(b"second")));
        }
        assert!(
            (first.is_none() && second.is_none())
                || (first == Some(Bytes::from_static(b"first"))
                    && second == Some(Bytes::from_static(b"second"))),
            "{} recovered a partial transaction: first={first:?}, second={second:?}",
            fault.name
        );
        let after = first.is_some();
        if let Some(previous) = recovered_after {
            assert_eq!(
                after, previous,
                "{} changed recovered outcome across reopen",
                fault.name
            );
        }
        recovered_after = Some(after);
        assert_eq!(recovered.get(STALE_KEY).await?, None);
        if pass == 0 {
            recovered
                .put(
                    Bytes::from_static(WINNER_KEY),
                    Bytes::from_static(b"winner"),
                )
                .await?;
        } else {
            assert_eq!(
                recovered.get(WINNER_KEY).await?,
                Some(Bytes::from_static(b"winner"))
            );
        }
        recovered.close().await?;
    }

    if let Some(stale) = stale {
        let stale_commit = match stale.commit().await {
            Ok(()) => {
                return Err(format!(
                    "{} allowed a superseded object-store writer to commit",
                    fault.name
                )
                .into());
            }
            Err(error) => error,
        };
        assert_eq!(
            stale_commit.kind(),
            ErrorKind::CommitOutcomeUnknown,
            "{} classified a stale writer commit incorrectly: {stale_commit}",
            fault.name
        );
    }

    let final_audit = Store::open(path, object_store(&rustfs.config)?).await?;
    assert_eq!(final_audit.get(STALE_KEY).await?, None);
    assert_eq!(
        final_audit.get(WINNER_KEY).await?,
        Some(Bytes::from_static(b"winner"))
    );
    final_audit.close().await?;

    let stale_error = match store.begin(IsolationLevel::SerializableSnapshot).await {
        Ok(transaction) => {
            transaction.rollback();
            return Err(format!(
                "{} left the superseded object-store writer usable after recovery",
                fault.name
            )
            .into());
        }
        Err(error) => error,
    };
    assert_eq!(
        stale_error.kind(),
        ErrorKind::Closed,
        "{} did not fence the superseded writer: {stale_error}",
        fault.name
    );
    store.close().await?;
    println!(
        "network_fault={} commit={} commit_ms={} recovered={} stale_writer=fenced",
        fault.name,
        commit_class,
        commit_elapsed.as_millis(),
        if recovered_after == Some(true) {
            "after"
        } else {
            "before"
        }
    );
    Ok(())
}

async fn assert_proxy_fault(endpoint: &str, name: &str) -> TestResult {
    let probe = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()?;
    if let Ok(response) = probe.get(endpoint).send().await {
        return Err(format!(
            "{name} did not disrupt a control request through the proxy (HTTP {})",
            response.status()
        )
        .into());
    }
    Ok(())
}
