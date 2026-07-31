use std::collections::HashSet;

use bytes::Bytes;

use crate::engine::catalog::identity::{CatalogVersion, DefinitionGeneration, SchemaId};
use crate::engine::catalog::model::{Reclamation, ReclamationKind, Schema, Table, Timestamp};
use crate::engine::catalog::{Error, ErrorKind, Result};
use crate::engine::kv::KvView;

use super::durable_json::encode;
use super::{map_kv, parse_u64, prefix_bounds, queue_reclamation, table_definition_reclamation_id};

const TABLE_DEFINITION_PREFIX: &str = "/rad/catalog/object/table/";
const TABLE_HEAD_PREFIX: &str = "/rad/catalog/head/table/";

pub fn table_definition_key(id: SchemaId, generation: DefinitionGeneration) -> Vec<u8> {
    format!(
        "{TABLE_DEFINITION_PREFIX}{:010}/definition/{:020}",
        id.get(),
        generation.get()
    )
    .into_bytes()
}

pub fn table_definition_range(id: SchemaId) -> (Vec<u8>, Vec<u8>) {
    let start = format!("{TABLE_DEFINITION_PREFIX}{:010}/definition/", id.get()).into_bytes();
    prefix_bounds(start)
}

pub fn table_head_key(id: SchemaId) -> Vec<u8> {
    format!("{TABLE_HEAD_PREFIX}{:010}", id.get()).into_bytes()
}

pub async fn publish_definitions<V: KvView + ?Sized>(
    view: &mut V,
    version: CatalogVersion,
    tables: &[Table],
    previous: &Schema,
    now: Timestamp,
) -> Result<()> {
    let mut live = HashSet::with_capacity(tables.len());
    for table in tables {
        live.insert(table.schema_id);
        let previous_head = definition_head(view, table.schema_id).await?;
        let raw = encode("table definition", &table.schema_id.to_string(), table)?;
        let definition_key = table_definition_key(table.schema_id, table.definition_generation);
        if let Some(existing) = view.get(&definition_key).await.map_err(map_kv)? {
            if existing.as_ref() != raw.as_slice() {
                return Err(Error::message(
                    ErrorKind::Conflict,
                    format!(
                        "catalog: immutable table definition {} generation {} already has different contents",
                        table.schema_id, table.definition_generation
                    ),
                ));
            }
        } else {
            view.put(Bytes::from(definition_key), Bytes::from(raw))
                .await
                .map_err(map_kv)?;
        }
        view.put(
            Bytes::from(table_head_key(table.schema_id)),
            Bytes::from(format!("{version}:{}", table.definition_generation)),
        )
        .await
        .map_err(map_kv)?;

        if let Some((_, previous_generation)) = previous_head
            && previous_generation != table.definition_generation
        {
            let mut reclamation = Reclamation::pending(
                table_definition_reclamation_id(table.schema_id, previous_generation),
                ReclamationKind::TableDefinition,
                version,
                now,
            );
            reclamation.table_schema_id = Some(table.schema_id);
            reclamation.definition_generation = previous_generation;
            queue_reclamation(view, reclamation, now).await?;
        }
    }

    for table in &previous.tables {
        if live.contains(&table.id) {
            continue;
        }
        if let Some((_, previous_generation)) = definition_head(view, table.id).await? {
            let mut reclamation = Reclamation::pending(
                table_definition_reclamation_id(table.id, previous_generation),
                ReclamationKind::TableDefinition,
                version,
                now,
            );
            reclamation.table_schema_id = Some(table.id);
            reclamation.definition_generation = previous_generation;
            queue_reclamation(view, reclamation, now).await?;
        }
        view.delete(&table_head_key(table.id))
            .await
            .map_err(map_kv)?;
    }
    Ok(())
}

pub async fn definition_head<V: KvView + ?Sized>(
    view: &mut V,
    id: SchemaId,
) -> Result<Option<(CatalogVersion, DefinitionGeneration)>> {
    let Some(raw) = view.get(&table_head_key(id)).await.map_err(map_kv)? else {
        return Ok(None);
    };
    let value = std::str::from_utf8(&raw).map_err(|error| {
        Error::source(
            ErrorKind::CatalogCorrupt,
            format!("catalog: corrupt table definition head {raw:?}"),
            error,
        )
    })?;
    let Some((version, generation)) = value.split_once(':') else {
        return Err(Error::message(
            ErrorKind::CatalogCorrupt,
            format!("catalog: corrupt table definition head {raw:?}"),
        ));
    };
    let version = parse_u64("definition version", None, version.as_bytes())?;
    let generation = parse_u64("definition generation", None, generation.as_bytes())?;
    Ok(Some((version.into(), generation.into())))
}

#[cfg(test)]
mod tests {
    use crate::engine::catalog::identity::{ExistenceGeneration, TableId, WriteProtocolGeneration};
    use crate::engine::catalog::store::list_reclamations;
    use crate::engine::kv::TransactionalKv;
    use crate::engine::kv::slatedb;

    use super::*;

    fn table(generation: u64) -> Table {
        Table {
            id: TableId::from("t1"),
            schema_id: SchemaId::new(1).unwrap(),
            name: "users".into(),
            definition_generation: generation.into(),
            existence_generation: ExistenceGeneration::from(1),
            write_protocol_generation: WriteProtocolGeneration::from(1),
            columns: Vec::new(),
            primary_key: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            constraints: Vec::new(),
        }
    }

    #[tokio::test]
    async fn definitions_are_immutable_and_replaced_heads_queue_reclamation() {
        let mut database = slatedb::Store::memory("catalog-definitions").await.unwrap();
        publish_definitions(
            &mut database,
            1.into(),
            &[table(1)],
            &Schema::default(),
            Timestamp::test_value(),
        )
        .await
        .unwrap();
        let previous = Schema::from_physical(&[table(1)]).unwrap();
        publish_definitions(
            &mut database,
            2.into(),
            &[table(2)],
            &previous,
            Timestamp::test_value(),
        )
        .await
        .unwrap();
        assert_eq!(
            definition_head(&mut database, SchemaId::new(1).unwrap())
                .await
                .unwrap(),
            Some((2.into(), 2.into()))
        );
        assert_eq!(list_reclamations(&mut database).await.unwrap().len(), 1);
        database.close().await.unwrap();
    }
}
