use bytes::Bytes;

use crate::engine::catalog::identity::CatalogVersion;
use crate::engine::catalog::model::{Revision, Schema, Timestamp};
use crate::engine::catalog::{Error, ErrorKind, Result};
use crate::engine::kv::KvView;

use super::durable_json::{decode, encode};
use super::{list_tables, map_kv, parse_u64, prefix_range, publish_definitions};

const SCHEMA_VERSION_KEY: &[u8] = b"/rad/catalog/meta/schema_version";
const CATALOG_GENERATION_KEY: &[u8] = b"/rad/catalog/meta/catalog_generation";
const SCHEMA_REVISION_PREFIX: &str = "/rad/catalog/meta/schema_revision/";

pub(crate) async fn bump_catalog_generation<V: KvView + ?Sized>(view: &mut V) -> Result<u64> {
    let current = match view.get(CATALOG_GENERATION_KEY).await.map_err(map_kv)? {
        Some(raw) => parse_u64("catalog_generation", None, &raw)?,
        None => 0,
    };
    let next = current.checked_add(1).ok_or_else(|| {
        Error::message(
            ErrorKind::CatalogCorrupt,
            "catalog: catalog_generation exhausted",
        )
    })?;
    view.put(
        Bytes::from_static(CATALOG_GENERATION_KEY),
        Bytes::from(next.to_string()),
    )
    .await
    .map_err(map_kv)?;
    Ok(next)
}

pub async fn current_revision<V: KvView + ?Sized>(view: &mut V) -> Result<Revision> {
    let Some(raw) = view.get(SCHEMA_VERSION_KEY).await.map_err(map_kv)? else {
        let schema = Schema::default();
        return Ok(Revision {
            version: CatalogVersion::ZERO,
            created_at: Timestamp::default(),
            hash: schema.hash()?,
            schema,
        });
    };
    let version = parse_u64("schema_version", None, &raw)?;
    read_revision(view, version.into()).await?.ok_or_else(|| {
        Error::message(
            ErrorKind::CatalogCorrupt,
            format!("catalog: schema_version {version} has no revision record"),
        )
    })
}

pub async fn bump_revision<V: KvView + ?Sized>(view: &mut V, now: Timestamp) -> Result<Revision> {
    let current = current_revision(view).await?;
    let tables = list_tables(view).await?;
    let schema = Schema::from_physical(&tables)?;
    let next_version = current.version.next();
    let next = Revision {
        version: next_version,
        created_at: now,
        hash: schema.hash()?,
        schema,
    };
    let raw = encode("schema revision", &next.version.to_string(), &next)?;
    view.put(Bytes::from(revision_key(next.version)), Bytes::from(raw))
        .await
        .map_err(map_kv)?;
    view.put(
        Bytes::from_static(SCHEMA_VERSION_KEY),
        Bytes::from(next.version.to_string()),
    )
    .await
    .map_err(map_kv)?;
    publish_definitions(view, next.version, &tables, &current.schema, now).await?;
    Ok(next)
}

pub async fn revisions<V: KvView + ?Sized>(view: &mut V) -> Result<Vec<Revision>> {
    let prefix = SCHEMA_REVISION_PREFIX.as_bytes();
    let mut iterator = view.scan(prefix_range(prefix)).await.map_err(map_kv)?;
    let mut values = Vec::new();
    while let Some(entry) = iterator.next().await.map_err(map_kv)? {
        let key_version = entry
            .key
            .strip_prefix(prefix)
            .and_then(|value| std::str::from_utf8(value).ok())
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| {
                Error::message(ErrorKind::CatalogCorrupt, "catalog: malformed revision key")
            })?;
        let revision: Revision = decode("schema revision", &key_version.to_string(), &entry.value)?;
        if revision.version.get() != key_version {
            return Err(Error::message(
                ErrorKind::CatalogCorrupt,
                format!(
                    "catalog: schema revision key {key_version} contains version {}",
                    revision.version
                ),
            ));
        }
        validate_revision(&revision)?;
        values.push(revision);
    }
    Ok(values)
}

pub(crate) async fn read_revision<V: KvView + ?Sized>(
    view: &mut V,
    version: CatalogVersion,
) -> Result<Option<Revision>> {
    let Some(raw) = view.get(&revision_key(version)).await.map_err(map_kv)? else {
        return Ok(None);
    };
    let revision: Revision = decode("schema revision", &version.to_string(), &raw)?;
    if revision.version != version {
        return Err(Error::message(
            ErrorKind::CatalogCorrupt,
            format!(
                "catalog: schema revision key {version} contains version {}",
                revision.version
            ),
        ));
    }
    validate_revision(&revision)?;
    Ok(Some(revision))
}

fn validate_revision(revision: &Revision) -> Result<()> {
    let expected = revision
        .schema
        .hash()
        .map_err(|error| Error::message(ErrorKind::CatalogCorrupt, error.to_string()))?;
    if revision.hash != expected {
        return Err(Error::message(
            ErrorKind::CatalogCorrupt,
            format!(
                "catalog: schema revision {} hash is {:?}, want {expected:?}",
                revision.version, revision.hash
            ),
        ));
    }
    Ok(())
}

pub(crate) fn revision_key(version: CatalogVersion) -> Vec<u8> {
    format!("{SCHEMA_REVISION_PREFIX}{:020}", version.get()).into_bytes()
}

#[cfg(test)]
mod tests {
    use crate::engine::catalog::identity::{
        DefinitionGeneration, ExistenceGeneration, SchemaId, TableId, WriteProtocolGeneration,
    };
    use crate::engine::catalog::model::Table;
    use crate::engine::kv::slatedb;
    use crate::engine::kv::{Kv, TransactionalKv};

    use super::*;
    use crate::engine::catalog::store::save_table;

    #[tokio::test]
    async fn revision_history_starts_at_zero_and_corruption_is_typed() {
        let mut database = slatedb::Store::memory("catalog-revision-zero")
            .await
            .unwrap();
        let revision = current_revision(&mut database).await.unwrap();
        assert_eq!(revision.version, CatalogVersion::ZERO);
        assert!(revision.created_at.is_zero());
        assert_eq!(revision.schema.canonical_json().unwrap(), b"{}");
        assert!(revisions(&mut database).await.unwrap().is_empty());

        Kv::put(
            &database,
            Bytes::from_static(SCHEMA_VERSION_KEY),
            Bytes::from_static(b"not-a-version"),
        )
        .await
        .unwrap();
        assert_eq!(
            current_revision(&mut database).await.unwrap_err().kind(),
            ErrorKind::CatalogCorrupt
        );
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn revision_persists_canonical_schema_and_immutable_definition() {
        let mut database = slatedb::Store::memory("catalog-revision").await.unwrap();
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
        save_table(&mut database, &mut table).await.unwrap();
        let revision = bump_revision(&mut database, Timestamp::test_value())
            .await
            .unwrap();
        assert_eq!(revision.version, CatalogVersion::from(1));
        assert_eq!(revision.schema.tables.len(), 1);
        assert_eq!(current_revision(&mut database).await.unwrap(), revision);
        assert_eq!(revisions(&mut database).await.unwrap(), vec![revision]);
        database.close().await.unwrap();
    }
}
