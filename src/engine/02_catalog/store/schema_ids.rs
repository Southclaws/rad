use bytes::Bytes;

use crate::engine::catalog::identity::SchemaId;
use crate::engine::catalog::{Error, ErrorKind, Result};
use crate::engine::kv::{KvView, keys};

use super::{map_kv, parse_u64};

pub async fn schema_table_id_high_water<V: KvView + ?Sized>(
    view: &mut V,
) -> Result<Option<SchemaId>> {
    read_high_water(
        view,
        keys::catalog_schema_table_id_high_water_key(),
        "table schema ID high-water",
    )
    .await
}

pub async fn advance_schema_table_id_high_water<V: KvView + ?Sized>(
    view: &mut V,
    candidate: SchemaId,
) -> Result<SchemaId> {
    advance_high_water(
        view,
        keys::catalog_schema_table_id_high_water_key(),
        "table schema ID high-water",
        candidate,
    )
    .await
}

pub async fn schema_column_id_high_water<V: KvView + ?Sized>(
    view: &mut V,
    table: SchemaId,
) -> Result<Option<SchemaId>> {
    read_high_water(
        view,
        keys::catalog_schema_column_id_high_water_key(table.get()),
        "column schema ID high-water",
    )
    .await
}

pub async fn advance_schema_column_id_high_water<V: KvView + ?Sized>(
    view: &mut V,
    table: SchemaId,
    candidate: SchemaId,
) -> Result<SchemaId> {
    advance_high_water(
        view,
        keys::catalog_schema_column_id_high_water_key(table.get()),
        "column schema ID high-water",
        candidate,
    )
    .await
}

async fn advance_high_water<V: KvView + ?Sized>(
    view: &mut V,
    key: Vec<u8>,
    kind: &str,
    candidate: SchemaId,
) -> Result<SchemaId> {
    let current = read_high_water(view, key.clone(), kind).await?;
    let high_water = current.map_or(candidate, |current| current.max(candidate));
    if current != Some(high_water) {
        view.put(Bytes::from(key), Bytes::from(high_water.get().to_string()))
            .await
            .map_err(map_kv)?;
    }
    Ok(high_water)
}

async fn read_high_water<V: KvView + ?Sized>(
    view: &mut V,
    key: Vec<u8>,
    kind: &str,
) -> Result<Option<SchemaId>> {
    view.get(&key)
        .await
        .map_err(map_kv)?
        .map(|raw| parse_high_water(kind, &raw))
        .transpose()
}

fn parse_high_water(kind: &str, raw: &[u8]) -> Result<SchemaId> {
    let value = parse_u64(kind, None, raw)?;
    let value = u32::try_from(value).map_err(|_| {
        Error::message(
            ErrorKind::CatalogCorrupt,
            format!("catalog: {kind} is outside the schema ID range"),
        )
    })?;
    SchemaId::new(value).map_err(|_| {
        Error::message(
            ErrorKind::CatalogCorrupt,
            format!("catalog: {kind} is outside the schema ID range"),
        )
    })
}
