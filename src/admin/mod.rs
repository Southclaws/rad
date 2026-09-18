//! Embedded administration UI and its private storage inspection API.

mod assets;
mod kv;
mod render;
mod statistics;

use std::sync::Arc;

use axum::Router;

use crate::engine::kv::TransactionalKv;
use crate::scheduler::statistics::StatisticsRunner;

pub fn router(store: Arc<dyn TransactionalKv>) -> Router {
    router_with_statistics(store, None)
}

pub fn router_with_statistics(
    store: Arc<dyn TransactionalKv>,
    runner: Option<Arc<StatisticsRunner>>,
) -> Router {
    router_with_statistics_and_auth(store, runner, None)
}

pub fn router_with_statistics_and_auth(
    store: Arc<dyn TransactionalKv>,
    runner: Option<Arc<StatisticsRunner>>,
    authenticator: Option<Arc<crate::auth::Authenticator>>,
) -> Router {
    let router = Router::new()
        .merge(kv::router(store))
        .merge(statistics::router(runner))
        .merge(assets::router());
    match authenticator {
        Some(authenticator) => router.layer(axum::middleware::from_fn_with_state(
            authenticator,
            crate::http::auth::require_admin,
        )),
        None => router,
    }
}

#[cfg(test)]
mod tests;
