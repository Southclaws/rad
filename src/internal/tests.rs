use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt as _;

use super::*;
use crate::scheduler::relay::{ObservationBatch, RELAY_FORMAT, RelayIngest};

const SECRET: &str = "a-database-scoped-secret";

/// Certificate material for a test, standing in for what cert-manager issues.
pub(super) struct Material {
    pub certificate: Vec<u8>,
    pub key: Vec<u8>,
}

pub(super) fn self_signed(name: &str) -> Material {
    let issued = rcgen::generate_simple_self_signed(vec![name.to_owned()]).unwrap();
    Material {
        certificate: issued.cert.pem().into_bytes(),
        key: issued.signing_key.serialize_pem().into_bytes(),
    }
}

fn listener(capacity: usize) -> (Router, Arc<RelayIngest>) {
    let (ingest, receiver) = RelayIngest::channel(capacity);
    // The runner would drain this; holding it open keeps the queue bounded by
    // capacity rather than by nothing.
    std::mem::forget(receiver);
    (
        router(ingest.clone(), Arc::new(Token::from_value(SECRET)), false),
        ingest,
    )
}

fn corpus_listener(
    confidential: bool,
    capture: bool,
) -> (
    Router,
    Arc<RelayIngest>,
    tokio::sync::mpsc::Receiver<ObservationBatch>,
) {
    let (ingest, receiver) = RelayIngest::channel_with_corpus(8, capture);
    (
        router(
            ingest.clone(),
            Arc::new(Token::from_value(SECRET)),
            confidential,
        ),
        ingest,
        receiver,
    )
}

fn batch(sequence: u64) -> ObservationBatch {
    ObservationBatch {
        format: RELAY_FORMAT,
        instance: "reader-1".into(),
        boot: "boot-1".into(),
        sequence,
        sent_at_micros: 0,
        families: Vec::new(),
        frequency: Vec::new(),
        corpus: Vec::new(),
    }
}

fn submission(token: Option<&str>, batch: &ObservationBatch) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri("/internal/statistics/observations")
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    request
        .body(Body::from(serde_json::to_vec(batch).unwrap()))
        .unwrap()
}

async fn status(router: &Router, request: Request<Body>) -> StatusCode {
    router.clone().oneshot(request).await.unwrap().status()
}

/// The port carries evidence that steers a planner, so reaching it is not
/// enough. Every way of not presenting the secret must be refused the same
/// way, and refusal must happen before the body is read.
#[tokio::test]
async fn a_submission_without_the_secret_is_refused() {
    let (router, ingest) = listener(8);
    for token in [
        None,
        Some("wrong"),
        Some(""),
        Some("a-database-scoped-secre"),
    ] {
        assert_eq!(
            status(&router, submission(token, &batch(1))).await,
            StatusCode::UNAUTHORIZED,
            "token {token:?} was accepted"
        );
    }
    // A prefix of the secret is not the secret, and neither is a superset.
    assert_eq!(
        status(
            &router,
            submission(Some(&format!("{SECRET}-extra")), &batch(1))
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        ingest.counters().accepted,
        0,
        "an unauthorized batch reached the models"
    );
}

#[tokio::test]
async fn a_submission_with_the_secret_is_accepted() {
    let (router, ingest) = listener(8);
    assert_eq!(
        status(&router, submission(Some(SECRET), &batch(1))).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(ingest.counters().accepted, 1);
}

#[tokio::test]
async fn plaintext_and_disabled_receivers_reject_corpus() {
    let mut corpus = batch(1);
    corpus
        .corpus
        .push(crate::engine::exec::observe::ProgramRecord {
            canonical: br#"{"statements":[]}"#.to_vec(),
            content_hash: [3; 16],
            at_unix_micros: 1,
            statements: 0,
            outcomes: Vec::new(),
        });

    let (plaintext, plaintext_ingest, mut plaintext_received) = corpus_listener(false, true);
    assert_eq!(
        status(&plaintext, submission(Some(SECRET), &corpus)).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(plaintext_ingest.counters().accepted, 0);
    assert!(plaintext_received.try_recv().is_err());

    let (disabled, disabled_ingest, mut disabled_received) = corpus_listener(true, false);
    assert_eq!(
        status(&disabled, submission(Some(SECRET), &corpus)).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(disabled_ingest.counters().accepted, 0);
    assert!(disabled_received.try_recv().is_err());

    let (enabled, enabled_ingest, mut enabled_received) = corpus_listener(true, true);
    assert_eq!(
        status(&enabled, submission(Some(SECRET), &corpus)).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(enabled_ingest.counters().accepted, 1);
    assert_eq!(
        enabled_received
            .try_recv()
            .expect("corpus batch")
            .corpus
            .len(),
        1
    );
}

/// A sender that repeats a sequence must be told its evidence has arrived, so
/// it stops rather than retrying forever.
#[tokio::test]
async fn a_repeated_sequence_is_accepted_without_second_admission() {
    let (router, ingest) = listener(8);
    for _ in 0..3 {
        assert_eq!(
            status(&router, submission(Some(SECRET), &batch(1))).await,
            StatusCode::ACCEPTED
        );
    }
    let counters = ingest.counters();
    assert_eq!(counters.accepted, 1);
    assert_eq!(counters.already_admitted, 2);
}

/// Rolling upgrades run two versions at once. The older instance refuses what
/// it cannot read rather than guessing at it.
#[tokio::test]
async fn a_batch_in_another_wire_format_is_refused() {
    let (router, ingest) = listener(8);
    let mut wrong = batch(1);
    wrong.format = RELAY_FORMAT + 1;
    assert_eq!(
        status(&router, submission(Some(SECRET), &wrong)).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(ingest.counters().format_mismatch, 1);
}

#[tokio::test]
async fn a_body_that_is_not_a_batch_is_refused() {
    let (router, _ingest) = listener(8);
    let request = Request::builder()
        .method("POST")
        .uri("/internal/statistics/observations")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {SECRET}"))
        .body(Body::from("{\"nonsense\":true}"))
        .unwrap();
    assert_eq!(status(&router, request).await, StatusCode::BAD_REQUEST);
}

/// A full queue must ask the sender to try again rather than drop its
/// evidence, and must leave no record of the batch it refused.
#[tokio::test]
async fn a_saturated_receiver_asks_the_sender_to_retry() {
    let (router, ingest) = listener(1);
    assert_eq!(
        status(&router, submission(Some(SECRET), &batch(1))).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        status(&router, submission(Some(SECRET), &batch(2))).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(ingest.counters().saturated, 1);
    assert_eq!(
        ingest.counters().accepted,
        1,
        "the refused batch was recorded as admitted"
    );
}

/// A faulty sender must not be able to make the receiver allocate without
/// bound.
#[tokio::test]
async fn an_oversized_batch_is_refused() {
    let (router, _ingest) = listener(8);
    let request = Request::builder()
        .method("POST")
        .uri("/internal/statistics/observations")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {SECRET}"))
        .body(Body::from(vec![b'x'; 8 * 1024 * 1024]))
        .unwrap();
    assert_eq!(
        status(&router, request).await,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

/// A relaying instance must reach the writer over TLS, verify it against the
/// authority, and be refused when it cannot. This runs the real listener and
/// the real transport over a real socket, because that is the only place the
/// handshake and the certificate's names are actually exercised.
#[tokio::test]
async fn a_relay_over_tls_verifies_the_writer_and_refuses_a_stranger() {
    use crate::scheduler::relay::{ObservationTransport as _, TransportError};

    let directory = tempfile::tempdir().unwrap();
    let authority = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let certificate = directory.path().join("tls.crt");
    let key = directory.path().join("tls.key");
    let ca = directory.path().join("ca.crt");
    std::fs::write(&certificate, authority.cert.pem()).unwrap();
    std::fs::write(&key, authority.signing_key.serialize_pem()).unwrap();
    std::fs::write(&ca, authority.cert.pem()).unwrap();

    let (ingest, _receiver) = RelayIngest::channel(8);
    let rotating = super::RotatingCertificate::load(super::TlsFiles {
        certificate,
        key,
        authority: Some(ca.clone()),
    })
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    let served = tokio::spawn(super::serve(
        listener,
        router(ingest.clone(), Arc::new(Token::from_value(SECRET)), true),
        Some(rotating),
        async move {
            let _ = stopped.changed().await;
        },
    ));

    let target = format!("https://localhost:{}", address.port());
    let batch = ObservationBatch {
        format: RELAY_FORMAT,
        instance: "reader-1".into(),
        boot: "boot-1".into(),
        sequence: 1,
        sent_at_micros: 0,
        families: Vec::new(),
        frequency: vec![(
            crate::engine::lir::fingerprint::Fingerprint {
                canonicalization_version: crate::engine::lir::fingerprint::CANONICALIZATION_VERSION,
                hash_algorithm: crate::engine::lir::fingerprint::HASH_SHA256_128,
                digest: [7; 16],
            },
            3,
        )],
        corpus: Vec::new(),
    };

    let trusting =
        super::HttpTransport::new(&target, format!("Bearer {SECRET}"), Some(&ca)).unwrap();
    assert!(
        trusting.submit(&batch).await.is_ok(),
        "a reader could not reach the writer over TLS"
    );
    assert_eq!(ingest.counters().accepted, 1);

    // The writer's certificate must chain to the authority the reader was
    // given. That the public roots are *also* excluded cannot be shown here
    // without a publicly issued certificate; it is why the client is built
    // with only these roots rather than these roots as well.
    let stranger = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let elsewhere = directory.path().join("stranger.crt");
    std::fs::write(&elsewhere, stranger.cert.pem()).unwrap();
    let distrusting =
        super::HttpTransport::new(&target, format!("Bearer {SECRET}"), Some(&elsewhere)).unwrap();
    assert_eq!(
        distrusting.submit(&batch).await,
        Err(TransportError::Unavailable),
        "a writer presenting an unrelated authority's certificate was accepted"
    );

    // TLS proves who the writer is; it does not decide who may submit.
    let unauthenticated =
        super::HttpTransport::new(&target, "Bearer wrong".into(), Some(&ca)).unwrap();
    assert_eq!(
        unauthenticated.submit(&batch).await,
        Err(TransportError::Rejected)
    );

    let _ = stop.send(true);
    let _ = served.await;
}

/// The authority is renewed like any other certificate. A reader that pinned
/// it at boot would keep trusting an expired anchor and lose the writer months
/// later, with nothing in the moment to explain it.
#[tokio::test]
async fn a_reader_adopts_a_rotated_authority_without_restarting() {
    use crate::scheduler::relay::{ObservationTransport as _, TransportError};

    let directory = tempfile::tempdir().unwrap();
    let ca = directory.path().join("ca.crt");

    // The writer is reissued under a new authority, as a CA rotation does.
    let second = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let certificate = directory.path().join("tls.crt");
    let key = directory.path().join("tls.key");
    std::fs::write(&certificate, second.cert.pem()).unwrap();
    std::fs::write(&key, second.signing_key.serialize_pem()).unwrap();

    // The reader still holds the previous authority.
    let first = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    std::fs::write(&ca, first.cert.pem()).unwrap();

    let (ingest, _receiver) = RelayIngest::channel(8);
    let rotating = super::RotatingCertificate::load(super::TlsFiles {
        certificate,
        key,
        authority: Some(ca.clone()),
    })
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    let served = tokio::spawn(super::serve(
        listener,
        router(ingest.clone(), Arc::new(Token::from_value(SECRET)), true),
        Some(rotating),
        async move {
            let _ = stopped.changed().await;
        },
    ));

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
    let transport = super::HttpTransport::new(
        &format!("https://localhost:{}", address.port()),
        format!("Bearer {SECRET}"),
        Some(&ca),
    )
    .unwrap();

    // The stale authority cannot verify the reissued writer.
    assert_eq!(
        transport.submit(&batch).await,
        Err(TransportError::Unavailable),
        "an authority that does not sign the writer was accepted"
    );

    // The rotation reaches the mounted file, and the next batch succeeds with
    // no restart and no new transport.
    std::fs::write(&ca, second.cert.pem()).unwrap();
    assert!(
        transport.submit(&batch).await.is_ok(),
        "the reader never adopted the rotated authority"
    );
    assert_eq!(ingest.counters().accepted, 1);

    let _ = stop.send(true);
    let _ = served.await;
}

/// An orchestrator has to be able to ask whether the listener serves without
/// holding a database secret.
#[tokio::test]
async fn liveness_needs_no_secret() {
    let (router, _ingest) = listener(8);
    let request = Request::builder()
        .uri("/internal/livez")
        .body(Body::empty())
        .unwrap();
    assert_eq!(status(&router, request).await, StatusCode::OK);
}

/// The internal listener serves nothing else. A client route appearing here
/// would be reachable on a port whose exposure was decided for advisory data.
#[tokio::test]
async fn the_internal_listener_serves_nothing_else() {
    let (router, _ingest) = listener(8);
    for path in [
        "/",
        "/healthz",
        "/readyz",
        "/execute",
        "/tables",
        "/statistics",
    ] {
        let request = Request::builder().uri(path).body(Body::empty()).unwrap();
        assert_eq!(
            status(&router, request).await,
            StatusCode::NOT_FOUND,
            "{path} is served on the internal listener"
        );
    }
}
