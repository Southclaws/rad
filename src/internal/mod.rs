//! The instance-to-instance API.
//!
//! A separate listener rather than another route on the public API, so
//! exposure, authentication, and network policy are decided by port. Nothing
//! here serves clients, and nothing a client can reach serves this.
//!
//! The listener binds only when both an address and a token file are
//! configured. Requiring both removes two failure modes at once: a port that
//! accepts unauthenticated evidence, and a single-node run that fails because
//! it did not supply a token it has no use for.

mod auth;
mod observations;
mod serve;
mod tls;
mod transport;

use std::sync::Arc;

use axum::Router;
use axum::routing::get;

pub use auth::{Token, TokenError};
pub use serve::serve;
pub use tls::{RotatingCertificate, TlsError, TlsFiles};
pub use transport::{HttpTransport, TransportBuildError};

use crate::scheduler::relay::RelayIngest;

/// Routes for the internal listener.
///
/// `/internal/livez` is deliberately unauthenticated and deliberately empty: an
/// orchestrator has to be able to ask whether the listener serves without
/// holding a database secret, and the answer tells it nothing else.
pub fn router(ingest: Arc<RelayIngest>, token: Arc<Token>, confidential: bool) -> Router {
    Router::new()
        .route("/internal/livez", get(async || "ok"))
        .merge(observations::router(ingest, token, confidential))
}

#[cfg(test)]
mod tests;
