use bytes::Bytes;

use crate::engine::catalog::identity::{DefinitionGeneration, SchemaId, TableId};
use crate::engine::catalog::model::{Schema, Table};
use crate::engine::catalog::{Error, ErrorKind, Result};
use crate::engine::kv::KvView;

use super::durable_json::{decode, encode};
use super::{map_kv, parse_u64, prefix_range};

const NEXT_ID_KEY: &[u8] = b"/rad/catalog/meta/next_id";
const TABLE_PREFIX: &str = "/rad/catalog/table/";
const TABLE_NAME_PREFIX: &str = "/rad/catalog/table_name/";

pub fn table_key(id: &TableId) -> Vec<u8> {
    format!("{TABLE_PREFIX}{id}").into_bytes()
}

pub fn table_name_key(name: &str) -> Vec<u8> {
    format!("{TABLE_NAME_PREFIX}{name}").into_bytes()
}

/// Publish a mutable logical-name lookup without making it a compatibility
/// fence. Pinned work resolves names once and then relies on stable physical
/// identities plus the generation fences in `fences.rs`.
pub async fn save_table_name<V: KvView + ?Sized>(view: &V, name: &str, id: &TableId) -> Result<()> {
    let key = table_name_key(name);
    view.untrack_write(&key).map_err(map_kv)?;
    view.put(
        Bytes::from(key),
        Bytes::copy_from_slice(id.as_str().as_bytes()),
    )
    .await
    .map_err(map_kv)
}

pub async fn delete_table_name<V: KvView + ?Sized>(view: &V, name: &str) -> Result<()> {
    let key = table_name_key(name);
    view.untrack_write(&key).map_err(map_kv)?;
    view.delete(&key).await.map_err(map_kv)
}

pub async fn delete_table_metadata<V: KvView + ?Sized>(view: &V, id: &TableId) -> Result<()> {
    let key = table_key(id);
    view.untrack_write(&key).map_err(map_kv)?;
    view.delete(&key).await.map_err(map_kv)
}

pub async fn get_table<V: KvView + ?Sized>(view: &V, name: &str) -> Result<Option<Table>> {
    let Some(id) = view.get(&table_name_key(name)).await.map_err(map_kv)? else {
        return Ok(None);
    };
    let id = std::str::from_utf8(&id).map_err(|error| {
        Error::source(
            ErrorKind::CatalogCorrupt,
            format!("catalog: table name {name:?} contains a non-UTF-8 physical ID"),
            error,
        )
    })?;
    get_table_by_id(view, &TableId::from(id)).await
}

pub async fn get_table_by_id<V: KvView + ?Sized>(view: &V, id: &TableId) -> Result<Option<Table>> {
    let Some(raw) = view.get(&table_key(id)).await.map_err(map_kv)? else {
        return Ok(None);
    };
    let table: Table = decode("table entry", id.as_str(), &raw)?;
    if &table.id != id {
        return Err(Error::message(
            ErrorKind::CatalogCorrupt,
            format!(
                "catalog: table key {id:?} contains physical identity {:?}",
                table.id
            ),
        ));
    }
    Ok(Some(table))
}

pub async fn get_table_by_schema_id<V: KvView + ?Sized>(
    view: &mut V,
    id: SchemaId,
) -> Result<Option<Table>> {
    let mut found: Option<Table> = None;
    for table in list_tables(view).await? {
        if table.schema_id != id {
            continue;
        }
        if let Some(previous) = found.as_ref() {
            return Err(Error::message(
                ErrorKind::CatalogDrift,
                format!(
                    "catalog: tables {:?} and {:?} share schema ID {id}",
                    previous.name, table.name
                ),
            ));
        }
        found = Some(table);
    }
    Ok(found)
}

pub async fn list_tables<V: KvView + ?Sized>(view: &mut V) -> Result<Vec<Table>> {
    let prefix = TABLE_PREFIX.as_bytes();
    let mut iterator = view.scan(prefix_range(prefix)).await.map_err(map_kv)?;
    let mut tables = Vec::new();
    while let Some(entry) = iterator.next().await.map_err(map_kv)? {
        let key_id = entry
            .key
            .strip_prefix(prefix)
            .and_then(|value| std::str::from_utf8(value).ok())
            .ok_or_else(|| {
                Error::message(
                    ErrorKind::CatalogCorrupt,
                    format!("catalog: malformed table key {:?}", entry.key),
                )
            })?;
        let table: Table = decode("table entry", key_id, &entry.value)?;
        if table.id.as_str() != key_id {
            return Err(Error::message(
                ErrorKind::CatalogCorrupt,
                format!(
                    "catalog: table key {key_id:?} contains physical identity {:?}",
                    table.id
                ),
            ));
        }
        tables.push(table);
    }
    tables.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(tables)
}

pub async fn read_schema<V: KvView + ?Sized>(view: &mut V) -> Result<Schema> {
    Schema::from_physical(&list_tables(view).await?)
}

pub async fn save_table<V: KvView + ?Sized>(view: &mut V, table: &mut Table) -> Result<()> {
    let key = table_key(&table.id);
    if let Some(raw) = view.get(&key).await.map_err(map_kv)? {
        let current: Table = decode("table entry", table.id.as_str(), &raw)?;
        if table.definition_generation <= current.definition_generation {
            table.definition_generation = current.definition_generation.next();
        }
    } else if table.definition_generation == DefinitionGeneration::ZERO {
        table.definition_generation = DefinitionGeneration::from(1);
    }
    let raw = encode("table entry", table.id.as_str(), table)?;
    view.untrack_write(&key).map_err(map_kv)?;
    view.put(Bytes::from(key), Bytes::from(raw))
        .await
        .map_err(map_kv)
}

pub async fn next_physical_id<V: KvView + ?Sized>(view: &mut V, kind: &str) -> Result<String> {
    let next = match view.get(NEXT_ID_KEY).await.map_err(map_kv)? {
        Some(raw) => parse_u64("next_id", None, &raw)?
            .checked_add(1)
            .ok_or_else(|| {
                Error::message(ErrorKind::CatalogCorrupt, "catalog: next_id exhausted")
            })?,
        None => 1,
    };
    view.put(
        Bytes::from_static(NEXT_ID_KEY),
        Bytes::from(next.to_string()),
    )
    .await
    .map_err(map_kv)?;
    Ok(format!("{kind}{next}"))
}

#[cfg(test)]
mod tests {
    use crate::engine::catalog::identity::{
        ExistenceGeneration, ValueGeneration, WriteProtocolGeneration,
    };
    use crate::engine::catalog::model::{Column, ScalarType};
    use crate::engine::kv::slatedb;
    use crate::engine::kv::{Kv, TransactionalKv};

    use super::*;

    fn fixture() -> Table {
        Table {
            id: "t1".into(),
            schema_id: SchemaId::new(1).unwrap(),
            name: "users".into(),
            definition_generation: DefinitionGeneration::ZERO,
            existence_generation: ExistenceGeneration::from(1),
            write_protocol_generation: WriteProtocolGeneration::from(1),
            columns: vec![Column {
                id: "c1".into(),
                schema_id: SchemaId::new(1).unwrap(),
                name: "id".into(),
                value_generation: ValueGeneration::from(1),
                scalar_type: ScalarType::Text,
                nullable: false,
                format: "uuid".into(),
                insert_default: None,
                missing_value: None,
            }],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            constraints: Vec::new(),
        }
    }

    #[tokio::test]
    async fn table_metadata_round_trips_and_generation_advances() {
        let mut database = slatedb::Store::memory("catalog-table-roundtrip")
            .await
            .unwrap();
        let mut table = fixture();
        save_table(&mut database, &mut table).await.unwrap();
        Kv::put(
            &database,
            Bytes::from(table_name_key(&table.name)),
            Bytes::copy_from_slice(table.id.as_str().as_bytes()),
        )
        .await
        .unwrap();
        assert_eq!(table.definition_generation, DefinitionGeneration::from(1));
        let loaded = get_table(&database, "users").await.unwrap().unwrap();
        assert_eq!(loaded, table);

        save_table(&mut database, &mut table).await.unwrap();
        assert_eq!(table.definition_generation, DefinitionGeneration::from(2));
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn list_rejects_key_payload_identity_drift() {
        let mut database = slatedb::Store::memory("catalog-table-drift").await.unwrap();
        let table = fixture();
        let raw = encode("table", table.id.as_str(), &table).unwrap();
        Kv::put(
            &database,
            Bytes::from(table_key(&TableId::from("alias"))),
            Bytes::from(raw),
        )
        .await
        .unwrap();
        assert_eq!(
            list_tables(&mut database).await.unwrap_err().kind(),
            ErrorKind::CatalogCorrupt
        );
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn physical_ids_are_durable_and_monotonic() {
        let mut database = slatedb::Store::memory("catalog-ids").await.unwrap();
        assert_eq!(next_physical_id(&mut database, "t").await.unwrap(), "t1");
        assert_eq!(next_physical_id(&mut database, "c").await.unwrap(), "c2");
        database.close().await.unwrap();
    }
}
