//! HTTP transport for relayed observations.
//!
//! This is the only place that knows the relay travels over HTTP. It lives
//! outside the statistics layer on purpose: the layer holds an
//! [`ObservationTransport`], and what implements it — this, an in-process
//! handoff, or a simulated network — is not its concern.

use std::path::Path;
use std::time::Duration;

use crate::scheduler::relay::{ObservationBatch, ObservationTransport, TransportError};

/// A request that has not answered by now is treated as unavailable, and the
/// batch is retried. Statistics never make a query wait, so the only cost of
/// waiting is the delay before this instance tries again.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The trust anchors in force, and the client built from them.
struct Trusted {
    digest: u64,
    http: reqwest::Client,
}

pub struct HttpTransport {
    endpoint: String,
    authorization: String,
    /// Re-read rather than captured once. The authority is renewed like any
    /// other certificate, and a client that pinned it at boot would stop
    /// trusting the writer the moment the old one expired — months later,
    /// long after anything that would explain it.
    authority: Option<std::path::PathBuf>,
    trusted: std::sync::RwLock<Trusted>,
}

#[derive(Debug)]
pub enum TransportBuildError {
    Client(reqwest::Error),
    Authority(super::TlsError),
}

impl std::fmt::Display for TransportBuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(error) => write!(formatter, "cannot build the relay client: {error}"),
            Self::Authority(error) => {
                write!(formatter, "cannot trust the writer's authority: {error}")
            }
        }
    }
}

impl std::error::Error for TransportBuildError {}

impl HttpTransport {
    /// Build a client for `target`.
    ///
    /// `authority` is the certificate authority the writer's certificate must
    /// chain to. It is the *only* trusted root: the public web roots are
    /// irrelevant to an in-cluster peer, and accepting them would mean any
    /// certificate a public authority issued could impersonate the writer.
    pub fn new(
        target: &str,
        authorization: String,
        authority: Option<&Path>,
    ) -> Result<Self, TransportBuildError> {
        super::tls::install_crypto_provider();
        let trusted = Self::build(authority)?;
        Ok(Self {
            endpoint: format!(
                "{}/internal/statistics/observations",
                target.trim_end_matches('/')
            ),
            authorization,
            authority: authority.map(Path::to_path_buf),
            trusted: std::sync::RwLock::new(trusted),
        })
    }

    fn build(authority: Option<&Path>) -> Result<Trusted, TransportBuildError> {
        let mut builder = reqwest::Client::builder().timeout(REQUEST_TIMEOUT);
        let mut digest = 0;
        if let Some(authority) = authority {
            let pem = std::fs::read(authority).map_err(|error| {
                TransportBuildError::Authority(super::TlsError::Unreadable(
                    authority.to_path_buf(),
                    error,
                ))
            })?;
            digest = crate::fnv::fnv1a(&pem);
            let roots = super::tls::certificates(authority)
                .map_err(TransportBuildError::Authority)?
                .iter()
                .map(|certificate| reqwest::Certificate::from_der(certificate))
                .collect::<Result<Vec<_>, _>>()
                .map_err(TransportBuildError::Client)?;
            // Only this authority, not the public web roots as well. The
            // writer is an in-cluster peer, so a publicly trusted certificate
            // has no business satisfying this connection.
            builder = builder.tls_certs_only(roots);
        }
        Ok(Trusted {
            digest,
            http: builder.build().map_err(TransportBuildError::Client)?,
        })
    }

    /// Adopt a renewed authority. A failed read or unusable material leaves
    /// the current anchors in force: a half-written file during renewal must
    /// not cost this instance its ability to reach the writer.
    fn refresh_trust(&self) {
        let Some(authority) = &self.authority else {
            return;
        };
        let Ok(pem) = std::fs::read(authority) else {
            return;
        };
        let digest = crate::fnv::fnv1a(&pem);
        if self.trusted.read().expect("relay trust").digest == digest {
            return;
        }
        if let Ok(rebuilt) = Self::build(Some(authority)) {
            *self.trusted.write().expect("relay trust") = rebuilt;
        }
    }

    fn client(&self) -> reqwest::Client {
        self.trusted.read().expect("relay trust").http.clone()
    }
}

#[async_trait::async_trait]
impl ObservationTransport for HttpTransport {
    /// Both halves are required. Encryption without a verified peer protects
    /// the bytes from an observer but not from whoever answered, and a
    /// verified peer over plaintext protects them from nobody. Neither alone
    /// is confidentiality.
    fn is_confidential(&self) -> bool {
        self.endpoint.starts_with("https://") && self.authority.is_some()
    }

    async fn submit(&self, batch: &ObservationBatch) -> Result<(), TransportError> {
        // Checked per batch rather than on a timer: batches are sent on the
        // relay's own cadence, so this is one small read every few seconds and
        // needs no task of its own.
        self.refresh_trust();
        let response = self
            .client()
            .post(&self.endpoint)
            .header(reqwest::header::AUTHORIZATION, &self.authorization)
            .json(batch)
            .send()
            .await
            .map_err(|_| TransportError::Unavailable)?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        // Only a receiver that understood and refused makes retrying
        // pointless. Anything else — saturation, an outage, a proxy between
        // the two — is worth another attempt with the same sequence.
        Err(match status {
            reqwest::StatusCode::TOO_MANY_REQUESTS => TransportError::Backpressured,
            status if status.is_client_error() => TransportError::Rejected,
            _ => TransportError::Unavailable,
        })
    }
}
