use std::sync::Arc;

use super::generated::server::{
    GetLivezResponse, GetReadyzResponse, GetStartupzResponse, ProbesApi, probes_api_router,
};
use super::generated::types::ProbeStatus;
use crate::health::Health;

#[derive(Clone)]
pub(super) struct Probes {
    health: Arc<Health>,
}

impl Probes {
    pub(super) fn new(health: Arc<Health>) -> Self {
        Self { health }
    }
}

/// A router serving only the probe endpoints, which the process publishes while
/// it starts so an orchestrator never routes a client to a half-built database.
pub fn router(health: Arc<Health>) -> axum::Router {
    probes_api_router(Probes::new(health))
}

fn status(reason: &str) -> ProbeStatus {
    ProbeStatus {
        reason: reason.to_owned(),
    }
}

#[async_trait::async_trait]
impl ProbesApi for Probes {
    async fn get_startupz(&self) -> GetStartupzResponse {
        let startup = self.health.startup();
        if startup.passed() {
            GetStartupzResponse::Ok(status(startup.as_str()))
        } else {
            GetStartupzResponse::ServiceUnavailable(status(self.health.startup_hold().as_str()))
        }
    }

    async fn get_readyz(&self) -> GetReadyzResponse {
        let readiness = self.health.ready();
        let body = status(readiness.as_str());
        if readiness.passed() {
            GetReadyzResponse::Ok(body)
        } else {
            GetReadyzResponse::ServiceUnavailable(body)
        }
    }

    async fn get_livez(&self) -> GetLivezResponse {
        let liveness = self.health.live();
        let body = status(liveness.as_str());
        if liveness.passed() {
            GetLivezResponse::Ok(body)
        } else {
            GetLivezResponse::ServiceUnavailable(body)
        }
    }
}
