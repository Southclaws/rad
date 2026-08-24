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
    Router::new()
        .merge(kv::router(store))
        .merge(statistics::router(runner))
        .merge(assets::router())
}

#[cfg(test)]
mod tests;
