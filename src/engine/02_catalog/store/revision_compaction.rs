use bytes::Bytes;

use crate::engine::catalog::identity::CatalogVersion;
use crate::engine::catalog::{Error, ErrorKind, Result};
use crate::engine::kv::KvView;

use super::revisions::{current_revision, revision_key};
use super::{map_kv, parse_u64};

const COMPACTED_THROUGH_KEY: &[u8] = b"/rad/catalog/meta/schema_revision_compacted_through";

pub async fn revision_compacted_through<V: KvView + ?Sized>(
    view: &mut V,
) -> Result<CatalogVersion> {
    let Some(raw) = view.get(COMPACTED_THROUGH_KEY).await.map_err(map_kv)? else {
        return Ok(CatalogVersion::ZERO);
    };
    parse_u64("compacted revision horizon", None, &raw).map(Into::into)
}

pub async fn revision_compaction_needed<V: KvView + ?Sized>(
    view: &mut V,
    retain_recent: u64,
) -> Result<bool> {
    if retain_recent == 0 {
        return Ok(false);
    }
    let current = current_revision(view).await?;
    if current.version.get() <= retain_recent {
        return Ok(false);
    }
    let compacted = revision_compacted_through(view).await?;
    Ok(compacted.get() < current.version.get() - retain_recent)
}

pub async fn compact_revision_history_batch<V: KvView + ?Sized>(
    view: &mut V,
    retain_recent: u64,
    batch_size: usize,
) -> Result<(usize, bool)> {
    if retain_recent == 0 {
        return Err(Error::message(
            ErrorKind::InvalidInput,
            "catalog: revision retention must be positive",
        ));
    }
    if batch_size == 0 {
        return Err(Error::message(
            ErrorKind::InvalidInput,
            "catalog: revision compaction batch size must be positive",
        ));
    }
    let current = current_revision(view).await?;
    if current.version.get() <= retain_recent {
        return Ok((0, false));
    }
    let target = current.version.get() - retain_recent;
    let mut compacted = revision_compacted_through(view).await?.get();
    let mut deleted = 0;
    while compacted < target && deleted < batch_size {
        compacted += 1;
        view.delete(&revision_key(compacted.into()))
            .await
            .map_err(map_kv)?;
        deleted += 1;
    }
    if deleted != 0 {
        view.put(
            Bytes::from_static(COMPACTED_THROUGH_KEY),
            Bytes::from(compacted.to_string()),
        )
        .await
        .map_err(map_kv)?;
    }
    Ok((deleted, compacted < target))
}

#[cfg(test)]
mod tests {
    use crate::engine::catalog::identity::{
        DefinitionGeneration, ExistenceGeneration, SchemaId, TableId, WriteProtocolGeneration,
    };
    use crate::engine::catalog::model::Table;
    use crate::engine::catalog::store::{bump_revision, revisions, save_table};
    use crate::engine::kv::TransactionalKv;
    use crate::engine::kv::slatedb;

    use super::*;

    #[tokio::test]
    async fn compaction_advances_in_bounded_batches_and_keeps_current() {
        let mut database = slatedb::Store::memory("catalog-revision-compaction")
            .await
            .unwrap();
        let mut table = Table {
            id: TableId::from("t1"),
            schema_id: SchemaId::new(1).unwrap(),
            name: "users".into(),
            definition_generation: DefinitionGeneration::ZERO,
            existence_generation: ExistenceGeneration::from(1),
            write_protocol_generation: WriteProtocolGeneration::from(1),
            columns: Vec::new(),
            primary_key: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            constraints: Vec::new(),
        };
        for version in 1..=5 {
            table.name = format!("users-v{version}");
            table.definition_generation = DefinitionGeneration::from(version);
            save_table(&mut database, &mut table).await.unwrap();
            assert_eq!(
                bump_revision(
                    &mut database,
                    crate::engine::catalog::model::Timestamp::test_value()
                )
                .await
                .unwrap()
                .version
                .get(),
                version
            );
        }
        assert!(revision_compaction_needed(&mut database, 2).await.unwrap());
        assert_eq!(
            compact_revision_history_batch(&mut database, 2, 2)
                .await
                .unwrap(),
            (2, true)
        );
        assert_eq!(
            compact_revision_history_batch(&mut database, 2, 2)
                .await
                .unwrap(),
            (1, false)
        );
        assert_eq!(
            revisions(&mut database)
                .await
                .unwrap()
                .into_iter()
                .map(|revision| revision.version.get())
                .collect::<Vec<_>>(),
            vec![4, 5]
        );
        database.close().await.unwrap();
    }
}
