use bytes::Bytes;

use crate::engine::catalog::identity::{
    AccessGeneration, ColumnId, ExistenceGeneration, IndexId, TableId, ValueGeneration,
};
use crate::engine::catalog::model::{CatalogDependencies, Column, Index, Table};
use crate::engine::catalog::{Error, ErrorKind, Result};
use crate::engine::kv::{KvView, keys};

use super::{map_kv, parse_u64, write_protocol_key};

pub async fn admit_catalog_dependencies<V: KvView + ?Sized>(
    view: &V,
    dependencies: &CatalogDependencies,
) -> Result<()> {
    for dependency in &dependencies.table_existence {
        read_generation_fence(
            view,
            &table_existence_fence_key(&dependency.table_id)?,
            dependency.generation.get(),
            &format!("existence fence for table {:?}", dependency.table_name),
        )
        .await?;
    }
    for dependency in &dependencies.column_values {
        read_generation_fence(
            view,
            &column_value_fence_key(&dependency.table_id, &dependency.column_id)?,
            dependency.generation.get(),
            &format!(
                "value fence for column {:?}.{:?}",
                dependency.table_name, dependency.column_name
            ),
        )
        .await?;
    }
    for dependency in &dependencies.index_access {
        read_generation_fence(
            view,
            &index_access_fence_key(&dependency.table_id, &dependency.index_id)?,
            dependency.generation.get(),
            &format!(
                "access fence for index {:?} on table {:?}",
                dependency.index_name, dependency.table_name
            ),
        )
        .await?;
    }
    for dependency in &dependencies.write_protocols {
        read_generation_fence(
            view,
            &write_protocol_key(&dependency.table_id)?,
            dependency.generation.get(),
            &format!("write protocol fence for table {:?}", dependency.table_name),
        )
        .await?;
    }
    Ok(())
}

async fn read_generation_fence<V: KvView + ?Sized>(
    view: &V,
    key: &[u8],
    expected: u64,
    label: &str,
) -> Result<()> {
    let actual = match view.get(key).await.map_err(map_kv)? {
        Some(raw) => parse_u64(label, None, &raw)?,
        None => 0,
    };
    if actual != expected {
        return Err(Error::message(
            ErrorKind::Conflict,
            format!("catalog: {label} changed from generation {expected} to {actual}"),
        ));
    }
    Ok(())
}

pub fn table_existence_fence_key(table_id: &TableId) -> Result<Vec<u8>> {
    Ok(keys::catalog_fence_table_existence_key(
        super::physical_table_number(table_id)?,
    ))
}

pub async fn read_table_existence_fence<V: KvView + ?Sized>(
    view: &mut V,
    table: &Table,
) -> Result<()> {
    read_generation_fence(
        view,
        &table_existence_fence_key(&table.id)?,
        table.existence_generation.get(),
        &format!("existence fence for table {:?}", table.name),
    )
    .await
}

pub async fn advance_table_existence_fence<V: KvView + ?Sized>(
    view: &mut V,
    table: &Table,
) -> Result<ExistenceGeneration> {
    read_table_existence_fence(view, table).await?;
    let next = table.existence_generation.next();
    write_generation(view, table_existence_fence_key(&table.id)?, next.get()).await?;
    Ok(next)
}

pub fn column_value_fence_key(table_id: &TableId, column_id: &ColumnId) -> Result<Vec<u8>> {
    let column = column_id.physical_number("c").ok_or_else(|| {
        Error::message(
            ErrorKind::CatalogCorrupt,
            format!("catalog: malformed column identity {column_id:?}"),
        )
    })?;
    Ok(keys::catalog_fence_column_value_key(
        super::physical_table_number(table_id)?,
        column,
    ))
}

pub async fn read_column_value_fence<V: KvView + ?Sized>(
    view: &mut V,
    table: &Table,
    column: &Column,
) -> Result<()> {
    read_generation_fence(
        view,
        &column_value_fence_key(&table.id, &column.id)?,
        column.value_generation.get(),
        &format!("value fence for column {:?}.{:?}", table.name, column.name),
    )
    .await
}

pub async fn advance_column_value_fence<V: KvView + ?Sized>(
    view: &mut V,
    table: &Table,
    column: &Column,
) -> Result<ValueGeneration> {
    read_column_value_fence(view, table, column).await?;
    let next = column.value_generation.next();
    write_generation(
        view,
        column_value_fence_key(&table.id, &column.id)?,
        next.get(),
    )
    .await?;
    Ok(next)
}

pub fn index_access_fence_key(table_id: &TableId, index_id: &IndexId) -> Result<Vec<u8>> {
    let index = index_id.physical_number("i").ok_or_else(|| {
        Error::message(
            ErrorKind::CatalogCorrupt,
            format!("catalog: malformed index identity {index_id:?}"),
        )
    })?;
    Ok(keys::catalog_fence_index_access_key(
        super::physical_table_number(table_id)?,
        index,
    ))
}

pub async fn read_index_access_fence<V: KvView + ?Sized>(
    view: &mut V,
    table: &Table,
    index: &Index,
) -> Result<()> {
    read_generation_fence(
        view,
        &index_access_fence_key(&table.id, &index.id)?,
        index.access_generation.get(),
        &format!(
            "access fence for index {:?} on table {:?}",
            index.name, table.name
        ),
    )
    .await
}

pub async fn advance_index_access_fence<V: KvView + ?Sized>(
    view: &mut V,
    table: &Table,
    index: &Index,
) -> Result<AccessGeneration> {
    read_index_access_fence(view, table, index).await?;
    let next = index.access_generation.next();
    write_generation(
        view,
        index_access_fence_key(&table.id, &index.id)?,
        next.get(),
    )
    .await?;
    Ok(next)
}

async fn write_generation<V: KvView + ?Sized>(
    view: &mut V,
    key: Vec<u8>,
    value: u64,
) -> Result<()> {
    view.put(Bytes::from(key), Bytes::from(value.to_string()))
        .await
        .map_err(map_kv)
}

#[cfg(test)]
mod tests {
    use crate::engine::catalog::identity::{
        DefinitionGeneration, SchemaId, WriteProtocolGeneration,
    };
    use crate::engine::catalog::model::ScalarType;
    use crate::engine::kv::TransactionalKv;
    use crate::engine::kv::slatedb;

    use super::*;

    fn fixture() -> Table {
        Table {
            id: "t1".into(),
            schema_id: SchemaId::new(1).unwrap(),
            name: "users".into(),
            definition_generation: DefinitionGeneration::from(1),
            existence_generation: ExistenceGeneration::ZERO,
            write_protocol_generation: WriteProtocolGeneration::ZERO,
            storage_generation: crate::engine::catalog::identity::StorageGeneration::INITIAL,
            columns: vec![Column {
                id: "c1".into(),
                schema_id: SchemaId::new(1).unwrap(),
                name: "id".into(),
                value_generation: ValueGeneration::ZERO,
                scalar_type: ScalarType::Text,
                nullable: false,
                format: String::new(),
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
    async fn advancing_a_fence_rejects_stale_dependencies() {
        let mut database = slatedb::Store::memory("catalog-fences").await.unwrap();
        let table = fixture();
        let mut dependencies = CatalogDependencies::default();
        dependencies.add_table_read(&table, &table.columns);
        admit_catalog_dependencies(&database, &dependencies)
            .await
            .unwrap();
        assert_eq!(
            advance_table_existence_fence(&mut database, &table)
                .await
                .unwrap(),
            1.into()
        );
        assert_eq!(
            admit_catalog_dependencies(&database, &dependencies)
                .await
                .unwrap_err()
                .kind(),
            ErrorKind::Conflict
        );
        database.close().await.unwrap();
    }
}
