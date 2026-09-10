//! Durable MVCC content generations for relation cache dependencies.
//!
//! A table identity is not reused. Generation stripes can remain after table
//! deletion without affecting a different table.

use bytes::Bytes;

use crate::engine::catalog::identity::{DataGeneration, TableId};
use crate::engine::catalog::{Error, ErrorKind, Result};
use crate::engine::kv::{KvView, keys};

use super::{map_kv, parse_u64, physical_table_number, prefix_range};

pub const TABLE_DATA_GENERATION_STRIPES: usize = 16;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TableDataGeneration {
    stripes: [DataGeneration; TABLE_DATA_GENERATION_STRIPES],
}

impl TableDataGeneration {
    pub const ZERO: Self = Self {
        stripes: [DataGeneration::ZERO; TABLE_DATA_GENERATION_STRIPES],
    };

    pub fn stripes(&self) -> &[DataGeneration; TABLE_DATA_GENERATION_STRIPES] {
        &self.stripes
    }

    #[cfg(test)]
    pub(crate) fn test_value(value: u64) -> Self {
        let mut generation = Self::ZERO;
        generation.stripes[0] = DataGeneration::from(value);
        generation
    }
}

impl Default for TableDataGeneration {
    fn default() -> Self {
        Self::ZERO
    }
}

#[cfg(test)]
impl From<u64> for TableDataGeneration {
    fn from(value: u64) -> Self {
        Self::test_value(value)
    }
}

pub fn table_data_generation_stripe_key(table_id: &TableId, stripe: usize) -> Result<Vec<u8>> {
    debug_assert!(stripe < TABLE_DATA_GENERATION_STRIPES);
    Ok(keys::catalog_table_data_generation_stripe_key(
        physical_table_number(table_id)?,
        stripe as u64,
    ))
}

pub async fn read_table_data_generation<V: KvView + ?Sized>(
    view: &V,
    table_id: &TableId,
) -> Result<TableDataGeneration> {
    let table_number = physical_table_number(table_id)?;
    let prefix = keys::catalog_table_data_generation_stripe_prefix_table(table_number);
    // A serializable cache hit must read the complete range. The range read
    // conflicts with every generation stripe that a concurrent table
    // mutation can write, including a stripe that is absent at this snapshot.
    let mut iterator = view.scan(prefix_range(&prefix)).await.map_err(map_kv)?;
    let mut generation = TableDataGeneration::ZERO;
    while let Some(entry) = iterator.next().await.map_err(map_kv)? {
        let parts =
            keys::decode_catalog_table_data_generation_stripe_key(&entry.key).ok_or_else(|| {
                Error::message(
                    ErrorKind::CatalogCorrupt,
                    format!(
                        "catalog: malformed table data generation key {:?}",
                        entry.key
                    ),
                )
            })?;
        let stripe = usize::try_from(parts.stripe).map_err(|_| {
            Error::message(
                ErrorKind::CatalogCorrupt,
                format!(
                    "catalog: table data generation stripe {} is out of range for {:?}",
                    parts.stripe, table_id
                ),
            )
        })?;
        let slot = generation.stripes.get_mut(stripe).ok_or_else(|| {
            Error::message(
                ErrorKind::CatalogCorrupt,
                format!(
                    "catalog: table data generation stripe {} is out of range for {:?}",
                    parts.stripe, table_id
                ),
            )
        })?;
        *slot = DataGeneration::from(parse_u64(
            "table data generation",
            Some(table_id.as_str()),
            &entry.value,
        )?);
    }
    Ok(generation)
}

pub async fn advance_table_data_generation<'a, V, I>(
    view: &mut V,
    table_id: &TableId,
    primary_keys: I,
) -> Result<()>
where
    V: KvView + ?Sized,
    I: IntoIterator<Item = &'a [u8]>,
{
    let mut selected = [false; TABLE_DATA_GENERATION_STRIPES];
    for primary_key in primary_keys {
        selected[data_generation_stripe(primary_key)] = true;
    }
    for (stripe, selected) in selected.into_iter().enumerate() {
        if !selected {
            continue;
        }
        // Keep each read and write in the transaction that changes the rows.
        // Writers for one stripe must conflict so two commits cannot publish
        // the same next value. Writers for different stripes can commit
        // independently because a complete table identity reads all stripes.
        let key = table_data_generation_stripe_key(table_id, stripe)?;
        let current = match view.get(&key).await.map_err(map_kv)? {
            Some(raw) => DataGeneration::from(parse_u64(
                "table data generation",
                Some(table_id.as_str()),
                &raw,
            )?),
            None => DataGeneration::ZERO,
        };
        view.put(Bytes::from(key), Bytes::from(current.next().to_string()))
            .await
            .map_err(map_kv)?;
    }
    Ok(())
}

fn data_generation_stripe(primary_key: &[u8]) -> usize {
    // FNV-1a has fixed output for the same storage key on every target. Cache
    // correctness does not depend on hash quality because every query reads
    // all stripes. Hash quality affects only write-conflict distribution.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in primary_key {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (hash % TABLE_DATA_GENERATION_STRIPES as u64) as usize
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
        let primary_key = [b'a'];
        let stripe = data_generation_stripe(&primary_key);
        assert_eq!(
            read_table_data_generation(&database, &table_id)
                .await
                .unwrap(),
            TableDataGeneration::ZERO
        );

        let rolled_back = database
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        {
            let mut view = TransactionView(&*rolled_back);
            advance_table_data_generation(&mut view, &table_id, [&primary_key[..]])
                .await
                .unwrap();
        }
        rolled_back.rollback();
        assert_eq!(
            read_table_data_generation(&database, &table_id)
                .await
                .unwrap(),
            TableDataGeneration::ZERO
        );

        let first = database
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        {
            let mut view = TransactionView(&*first);
            advance_table_data_generation(&mut view, &table_id, [&primary_key[..]])
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
            advance_table_data_generation(&mut view, &table_id, [&primary_key[..]])
                .await
                .unwrap();
        }
        second.commit().await.unwrap();

        let pinned_view = TransactionView(&*pinned);
        assert_eq!(
            read_table_data_generation(&pinned_view, &table_id)
                .await
                .unwrap()
                .stripes()[stripe],
            DataGeneration::from(1)
        );
        assert_eq!(
            read_table_data_generation(&database, &table_id)
                .await
                .unwrap()
                .stripes()[stripe],
            DataGeneration::from(2)
        );
        pinned.rollback();
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_advances_on_one_stripe_conflict() {
        let database = slatedb::Store::memory("table-data-generation-conflict")
            .await
            .unwrap();
        let table_id = TableId::from("t1");
        let primary_key = [b'a'];
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
            advance_table_data_generation(&mut view, &table_id, [&primary_key[..]])
                .await
                .unwrap();
        }
        first.commit().await.unwrap();
        assert!(second.commit().await.is_err());
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_advances_on_different_stripes_commit() {
        let database = slatedb::Store::memory("table-data-generation-striped")
            .await
            .unwrap();
        let table_id = TableId::from("t1");
        let first_key = [b'a'];
        let second_key = (0_u8..=u8::MAX)
            .map(|value| [value])
            .find(|key| data_generation_stripe(key) != data_generation_stripe(&first_key))
            .unwrap();
        let first = database
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        let second = database
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        {
            let mut view = TransactionView(&*first);
            advance_table_data_generation(&mut view, &table_id, [&first_key[..]])
                .await
                .unwrap();
        }
        {
            let mut view = TransactionView(&*second);
            advance_table_data_generation(&mut view, &table_id, [&second_key[..]])
                .await
                .unwrap();
        }
        first.commit().await.unwrap();
        second.commit().await.unwrap();
        assert_eq!(
            read_table_data_generation(&database, &table_id)
                .await
                .unwrap()
                .stripes()
                .iter()
                .map(|generation| generation.get())
                .sum::<u64>(),
            2
        );
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn complete_generation_read_conflicts_with_a_later_stripe_write() {
        let database = slatedb::Store::memory("table-data-generation-read-fence")
            .await
            .unwrap();
        let table_id = TableId::from("t1");
        let reader = database
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        {
            let view = TransactionView(&*reader);
            assert_eq!(
                read_table_data_generation(&view, &table_id).await.unwrap(),
                TableDataGeneration::ZERO
            );
        }

        let writer = database
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        {
            let mut view = TransactionView(&*writer);
            advance_table_data_generation(&mut view, &table_id, [&b"row"[..]])
                .await
                .unwrap();
        }
        writer.commit().await.unwrap();
        reader
            .put(Bytes::from_static(b"other"), Bytes::from_static(b"value"))
            .unwrap();
        assert!(reader.commit().await.is_err());
        database.close().await.unwrap();
    }

    #[test]
    fn primary_key_hash_can_select_every_stripe() {
        let selected = (0_u16..=u16::MAX)
            .map(|value| data_generation_stripe(&value.to_be_bytes()))
            .fold(
                [false; TABLE_DATA_GENERATION_STRIPES],
                |mut found, stripe| {
                    found[stripe] = true;
                    found
                },
            );
        assert!(selected.into_iter().all(|found| found));
    }
}
