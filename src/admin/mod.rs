//! Embedded administration UI and its private storage inspection API.

mod assets;
mod kv;
mod render;

use std::sync::Arc;

use axum::Router;

use crate::engine::kv::TransactionalKv;

pub fn router(store: Arc<dyn TransactionalKv>) -> Router {
    Router::new()
        .merge(kv::router(store))
        .merge(assets::router())
}

#[cfg(test)]
mod tests;
