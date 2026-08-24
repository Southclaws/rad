//! Ingest for relayed observations.

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};

use super::auth::Token;
use crate::scheduler::relay::{IngestOutcome, ObservationBatch, RelayIngest};

/// Largest batch accepted. A batch holds one instance's evidence for one
/// interval, so this is far above any honest sender and bounds what a faulty
/// one can make the receiver allocate.
const MAX_BATCH_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone)]
struct IngestState {
    ingest: Arc<RelayIngest>,
    token: Arc<Token>,
    confidential: bool,
}

pub(super) fn router(ingest: Arc<RelayIngest>, token: Arc<Token>, confidential: bool) -> Router {
    Router::new()
        .route("/internal/statistics/observations", post(observations))
        .layer(DefaultBodyLimit::max(MAX_BATCH_BYTES))
        .with_state(IngestState {
            ingest,
            token,
            confidential,
        })
}

/// Answer synchronously: the sender has to learn whether to retry, and only
/// this instance knows whether it took the batch.
async fn observations(
    State(state): State<IngestState>,
    headers: HeaderMap,
    body: Result<Json<ObservationBatch>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if !state.token.authorizes(&headers) {
        return reply(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let batch = match body {
        Ok(Json(batch)) => batch,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return reply(StatusCode::PAYLOAD_TOO_LARGE, "batch_too_large");
        }
        Err(_) => return reply(StatusCode::BAD_REQUEST, "malformed_batch"),
    };
    if !state.confidential && !batch.corpus.is_empty() {
        return reply(StatusCode::FORBIDDEN, "corpus_requires_tls");
    }
    match state.ingest.submit(batch) {
        IngestOutcome::Accepted => reply(StatusCode::ACCEPTED, "accepted"),
        // The sender need not distinguish this from acceptance: either way its
        // evidence has arrived and it must not send that sequence again.
        IngestOutcome::AlreadyAdmitted => reply(StatusCode::ACCEPTED, "already_admitted"),
        IngestOutcome::FormatMismatch => reply(StatusCode::BAD_REQUEST, "format_mismatch"),
        IngestOutcome::CorpusDisabled => reply(StatusCode::FORBIDDEN, "corpus_disabled"),
        IngestOutcome::CorpusOversize => {
            reply(StatusCode::PAYLOAD_TOO_LARGE, "corpus_document_too_large")
        }
        IngestOutcome::Saturated => reply(StatusCode::TOO_MANY_REQUESTS, "saturated"),
    }
}

fn reply(status: StatusCode, reason: &str) -> Response {
    (status, Json(serde_json::json!({ "reason": reason }))).into_response()
}
