//! Transactional high-water marks for increment-generated columns.

use bytes::Bytes;

use crate::engine::catalog::identity::{SchemaId, TableId};
use crate::engine::catalog::{Error, ErrorKind, Result};
use crate::engine::kv::{KvView, keys};

use super::{map_kv, parse_u64, physical_table_number};

pub fn column_increment_key(table_id: &TableId, column_id: SchemaId) -> Result<Vec<u8>> {
    Ok(keys::catalog_column_increment_key(
        physical_table_number(table_id)?,
        column_id.get(),
    ))
}

pub async fn read_column_increment<V: KvView + ?Sized>(
    view: &V,
    table_id: &TableId,
    column_id: SchemaId,
) -> Result<i64> {
    let key = column_increment_key(table_id, column_id)?;
    let Some(raw) = view.get(&key).await.map_err(map_kv)? else {
        return Ok(0);
    };
    let value = parse_u64("column increment", Some(table_id.as_str()), &raw)?;
    i64::try_from(value).map_err(|_| {
        Error::message(
            ErrorKind::CatalogCorrupt,
            format!(
                "catalog: column increment for table {table_id:?}, column {column_id} exceeds int64"
            ),
        )
    })
}

pub async fn save_column_increment<V: KvView + ?Sized>(
    view: &V,
    table_id: &TableId,
    column_id: SchemaId,
    value: i64,
) -> Result<()> {
    if value < 0 {
        return Err(Error::message(
            ErrorKind::CatalogCorrupt,
            format!(
                "catalog: negative column increment for table {table_id:?}, column {column_id}"
            ),
        ));
    }
    view.put(
        Bytes::from(column_increment_key(table_id, column_id)?),
        Bytes::from(value.to_string()),
    )
    .await
    .map_err(map_kv)
}

pub async fn delete_column_increment<V: KvView + ?Sized>(
    view: &V,
    table_id: &TableId,
    column_id: SchemaId,
) -> Result<()> {
    view.delete(&column_increment_key(table_id, column_id)?)
        .await
        .map_err(map_kv)
}

#[cfg(test)]
mod tests {
    use crate::engine::kv::slatedb::Store;
    use crate::engine::kv::{Kv, TransactionView, TransactionalKv};

    use super::*;

    #[tokio::test]
    async fn increment_state_rolls_back_and_rejects_malformed_values() {
        let store = Store::memory("catalog-column-increment").await.unwrap();
        let table_id = TableId::from("t42");
        let column_id = SchemaId::new(7).unwrap();

        let transaction = store
            .begin(crate::engine::kv::IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        {
            let view = TransactionView(transaction.as_ref());
            save_column_increment(&view, &table_id, column_id, 12)
                .await
                .unwrap();
        }
        transaction.rollback();
        assert_eq!(
            read_column_increment(&store, &table_id, column_id)
                .await
                .unwrap(),
            0
        );

        let key = column_increment_key(&table_id, column_id).unwrap();
        Kv::put(
            &store,
            Bytes::from(key),
            Bytes::from_static(b"9223372036854775808"),
        )
        .await
        .unwrap();
        assert_eq!(
            read_column_increment(&store, &table_id, column_id)
                .await
                .unwrap_err()
                .kind(),
            ErrorKind::CatalogCorrupt
        );
        store.close().await.unwrap();
    }
}
