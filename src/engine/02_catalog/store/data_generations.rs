//! Durable MVCC content generations for relation cache dependencies.
//!
//! One key covers all logical rows in one physical table identity. Table
//! identities are never reused. A generation can therefore remain after table
//! deletion without making a new table observe the old value.

use bytes::Bytes;

use crate::engine::catalog::Result;
use crate::engine::catalog::identity::{DataGeneration, TableId};
use crate::engine::kv::{KvView, keys};

use super::{map_kv, parse_u64, physical_table_number};

pub fn table_data_generation_key(table_id: &TableId) -> Result<Vec<u8>> {
    Ok(keys::catalog_table_data_generation_key(
        physical_table_number(table_id)?,
    ))
}

pub async fn read_table_data_generation<V: KvView + ?Sized>(
    view: &V,
    table_id: &TableId,
) -> Result<DataGeneration> {
    let key = table_data_generation_key(table_id)?;
    // Generation zero is the canonical value for an absent key. This rule
    // avoids eager key initialization and a table scan.
    let generation = match view.get(&key).await.map_err(map_kv)? {
        Some(raw) => parse_u64("table data generation", Some(table_id.as_str()), &raw)?,
        None => 0,
    };
    Ok(DataGeneration::from(generation))
}

pub async fn advance_table_data_generation<V: KvView + ?Sized>(
    view: &mut V,
    table_id: &TableId,
) -> Result<DataGeneration> {
    // Keep this read and write in the transaction that changes the rows. The
    // tracked read-write key prevents two mutations of one table from both
    // committing the same next generation. This table-wide key can cause
    // write contention. Do not untrack it or move the update after commit.
    let next = read_table_data_generation(view, table_id).await?.next();
    view.put(
        Bytes::from(table_data_generation_key(table_id)?),
        Bytes::from(next.to_string()),
    )
    .await
    .map_err(map_kv)?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use crate::engine::kv::slatedb;
    use crate::engine::kv::{IsolationLevel, TransactionView, TransactionalKv};

    use super::*;

    #[tokio::test]
    async fn generation_is_transactional_and_visible_at_the_pinned_snapshot() {
        let database = slatedb::Store::memory("table-data-generation")
            .await
            .unwrap();
        let table_id = TableId::from("t1");
        assert_eq!(
            read_table_data_generation(&database, &table_id)
                .await
                .unwrap(),
            DataGeneration::ZERO
        );

        let rolled_back = database
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        {
            let mut view = TransactionView(&*rolled_back);
            assert_eq!(
                advance_table_data_generation(&mut view, &table_id)
                    .await
                    .unwrap(),
                DataGeneration::from(1)
            );
        }
        rolled_back.rollback();
        assert_eq!(
            read_table_data_generation(&database, &table_id)
                .await
                .unwrap(),
            DataGeneration::ZERO
        );

        let first = database
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        {
            let mut view = TransactionView(&*first);
            advance_table_data_generation(&mut view, &table_id)
                .await
                .unwrap();
        }
        first.commit().await.unwrap();

        let pinned = database.begin(IsolationLevel::Snapshot).await.unwrap();
        let second = database
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        {
            let mut view = TransactionView(&*second);
            advance_table_data_generation(&mut view, &table_id)
                .await
                .unwrap();
        }
        second.commit().await.unwrap();

        let pinned_view = TransactionView(&*pinned);
        assert_eq!(
            read_table_data_generation(&pinned_view, &table_id)
                .await
                .unwrap(),
            DataGeneration::from(1)
        );
        assert_eq!(
            read_table_data_generation(&database, &table_id)
                .await
                .unwrap(),
            DataGeneration::from(2)
        );
        pinned.rollback();
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_advances_conflict() {
        let database = slatedb::Store::memory("table-data-generation-conflict")
            .await
            .unwrap();
        let table_id = TableId::from("t1");
        let first = database
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        let second = database
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        for transaction in [&*first, &*second] {
            let mut view = TransactionView(transaction);
            advance_table_data_generation(&mut view, &table_id)
                .await
                .unwrap();
        }
        first.commit().await.unwrap();
        assert!(second.commit().await.is_err());
        database.close().await.unwrap();
    }
}
