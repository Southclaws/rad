use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::engine::catalog::{Error, ErrorKind, Result};
use crate::engine::kv::{KvView, keys, manifest};

use super::durable_json::{decode, encode};
use super::map_kv;

/// What this database requires of binaries that open it. Distinct from the
/// specification revision: a binary must be able to read the existing
/// storage world before it may enable or write a new storage feature.
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StorageCompatibility {
    pub min_reader: u32,
    pub min_writer: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
}

impl StorageCompatibility {
    pub fn current() -> Self {
        Self {
            min_reader: manifest::STORAGE_FORMAT_VERSION,
            min_writer: manifest::STORAGE_FORMAT_VERSION,
            features: Vec::new(),
        }
    }
}

/// Validates the persisted compatibility manifest before the process serves
/// requests. A writing process initializes an absent manifest and must
/// satisfy both capability floors; a read-only process accepts an absent
/// manifest (the writer has not initialized the store yet) and must satisfy
/// only the reader floor. Fails closed on requirements this binary does not
/// satisfy and on features it does not know.
pub async fn admit_storage_compatibility<V: KvView + ?Sized>(
    view: &mut V,
    writes: bool,
) -> Result<()> {
    let key = keys::storage_manifest_key();
    let Some(raw) = view.get(&key).await.map_err(map_kv)? else {
        if !writes {
            return Ok(());
        }
        let raw = encode(
            "storage compatibility",
            "",
            &StorageCompatibility::current(),
        )?;
        return view
            .put(Bytes::from(key), Bytes::from(raw))
            .await
            .map_err(map_kv);
    };
    let stored: StorageCompatibility = decode("storage compatibility", "", &raw)?;
    let supported = manifest::STORAGE_FORMAT_VERSION;
    if stored.min_reader > supported {
        return Err(Error::message(
            ErrorKind::CatalogCorrupt,
            format!(
                "storage: this database requires reader capability {} but this binary supports {supported}",
                stored.min_reader
            ),
        ));
    }
    if writes && stored.min_writer > supported {
        return Err(Error::message(
            ErrorKind::CatalogCorrupt,
            format!(
                "storage: this database requires writer capability {} but this binary supports {supported}",
                stored.min_writer
            ),
        ));
    }
    if let Some(feature) = stored.features.first() {
        return Err(Error::message(
            ErrorKind::CatalogCorrupt,
            format!("storage: this database enables unknown storage feature {feature:?}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::engine::kv::TransactionalKv;
    use crate::engine::kv::slatedb;

    use super::*;

    #[tokio::test]
    async fn first_boot_writes_the_manifest_and_reopen_accepts_it() {
        let mut database = slatedb::Store::memory("storage-compat").await.unwrap();
        admit_storage_compatibility(&mut database, false)
            .await
            .unwrap();
        admit_storage_compatibility(&mut database, true)
            .await
            .unwrap();
        admit_storage_compatibility(&mut database, true)
            .await
            .unwrap();
        let raw = crate::engine::kv::Kv::get(&database, &keys::storage_manifest_key())
            .await
            .unwrap()
            .unwrap();
        let stored: StorageCompatibility = decode("storage compatibility", "", &raw).unwrap();
        assert_eq!(stored, StorageCompatibility::current());
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn future_requirements_and_unknown_features_fail_closed() {
        for record in [
            StorageCompatibility {
                min_reader: manifest::STORAGE_FORMAT_VERSION + 1,
                min_writer: manifest::STORAGE_FORMAT_VERSION,
                features: Vec::new(),
            },
            StorageCompatibility {
                min_reader: manifest::STORAGE_FORMAT_VERSION,
                min_writer: manifest::STORAGE_FORMAT_VERSION + 1,
                features: Vec::new(),
            },
            StorageCompatibility {
                min_reader: manifest::STORAGE_FORMAT_VERSION,
                min_writer: manifest::STORAGE_FORMAT_VERSION,
                features: vec!["sharding".into()],
            },
        ] {
            let mut database = slatedb::Store::memory("storage-compat-closed")
                .await
                .unwrap();
            let raw = encode("storage compatibility", "", &record).unwrap();
            crate::engine::kv::Kv::put(
                &database,
                Bytes::from(keys::storage_manifest_key()),
                Bytes::from(raw),
            )
            .await
            .unwrap();
            let error = admit_storage_compatibility(&mut database, true)
                .await
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::CatalogCorrupt, "{record:?}");
            database.close().await.unwrap();
        }
    }
}
