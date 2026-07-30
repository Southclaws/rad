use std::sync::Arc;

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use super::render::{KeyDecoder, hex_dump};
use crate::engine::kv::key_encoding::prefix_end;
use crate::engine::kv::{KeyRange, Kv, TransactionalKv};

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1_000;

#[derive(Clone)]
struct AdminState {
    store: Arc<dyn TransactionalKv>,
}

pub(super) fn router(store: Arc<dyn TransactionalKv>) -> Router {
    Router::new()
        .route("/api/kv/scan", get(scan))
        .route("/api/kv/get", get(get_value))
        .with_state(AdminState { store })
}

#[derive(Default, Deserialize)]
struct ScanQuery {
    #[serde(default)]
    prefix: String,
    after: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct GetQuery {
    key: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KvEntry {
    key: String,
    key_display: String,
    value_size: usize,
    value_display: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanResponse {
    entries: Vec<KvEntry>,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_after: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GetResponse {
    key: String,
    key_display: String,
    key_hex: String,
    value_size: usize,
    value_display: String,
    value_hex: String,
    #[serde(rename = "valueJSON")]
    #[serde(skip_serializing_if = "Option::is_none")]
    value_json: Option<serde_json::Value>,
}

async fn scan(
    State(state): State<AdminState>,
    RawQuery(raw): RawQuery,
) -> Result<axum::Json<ScanResponse>, AdminError> {
    let query: ScanQuery = parse_query(raw.as_deref())?;
    let prefix = query.prefix.into_bytes();
    let start = match query.after.filter(|value| !value.is_empty()) {
        Some(after) => {
            let mut key = STANDARD
                .decode(after)
                .map_err(|error| AdminError::bad_request(format!("bad after cursor: {error}")))?;
            key.push(0);
            key
        }
        None => prefix.clone(),
    };
    let range = KeyRange {
        start: Some(Bytes::from(start)),
        end: prefix_end(&prefix).map(Bytes::from),
    };
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let decoder = KeyDecoder::load(state.store.as_ref()).await;
    let mut iterator = Kv::scan(state.store.as_ref(), range)
        .await
        .map_err(AdminError::internal)?;
    let mut entries = Vec::new();
    let mut last_key = None;
    let mut truncated = false;
    while let Some(entry) = iterator.next().await.map_err(AdminError::internal)? {
        if entries.len() >= limit {
            truncated = true;
            break;
        }
        last_key = Some(entry.key.clone());
        entries.push(KvEntry {
            key: STANDARD.encode(&entry.key),
            key_display: decoder.key(&entry.key),
            value_size: entry.value.len(),
            value_display: clip(&decoder.value(&entry.key, &entry.value), 160),
        });
    }
    let next_after = truncated
        .then(|| last_key.as_ref().map(|key| STANDARD.encode(key)))
        .flatten();
    Ok(axum::Json(ScanResponse {
        entries,
        truncated,
        next_after,
    }))
}

async fn get_value(
    State(state): State<AdminState>,
    RawQuery(raw): RawQuery,
) -> Result<axum::Json<GetResponse>, AdminError> {
    let query: GetQuery = parse_query(raw.as_deref())?;
    let key = STANDARD
        .decode(query.key)
        .map_err(|error| AdminError::bad_request(format!("bad key: {error}")))?;
    let value = Kv::get(state.store.as_ref(), &key)
        .await
        .map_err(AdminError::internal)?
        .ok_or_else(|| AdminError::not_found("key not found"))?;
    let decoder = KeyDecoder::load(state.store.as_ref()).await;
    let value_json = serde_json::from_slice(&value).ok();
    Ok(axum::Json(GetResponse {
        key: STANDARD.encode(&key),
        key_display: decoder.key(&key),
        key_hex: hex_dump(&key),
        value_size: value.len(),
        value_display: decoder.value(&key, &value),
        value_hex: hex_dump(&value),
        value_json,
    }))
}

fn parse_query<T: serde::de::DeserializeOwned>(raw: Option<&str>) -> Result<T, AdminError> {
    serde_urlencoded::from_str(raw.unwrap_or_default())
        .map_err(|error| AdminError::bad_request(format!("bad query: {error}")))
}

fn clip(value: &str, length: usize) -> String {
    let mut characters = value.chars();
    let clipped = characters.by_ref().take(length).collect::<String>();
    if characters.next().is_some() {
        format!("{clipped}…")
    } else {
        clipped
    }
}

struct AdminError {
    status: StatusCode,
    detail: String,
}

impl AdminError {
    fn bad_request(detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            detail: detail.into(),
        }
    }

    fn not_found(detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            detail: detail.into(),
        }
    }

    fn internal(error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            detail: error.to_string(),
        }
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct Body {
            error: String,
        }
        (self.status, axum::Json(Body { error: self.detail })).into_response()
    }
}
