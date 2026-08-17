use bytes::Bytes;

use crate::engine::catalog::identity::TransitionId;
use crate::engine::catalog::{Error, ErrorKind, Result};
use crate::engine::kv::{KvView, keys};

use super::{map_kv, prefix_bounds};

fn transition_violation_key(id: &TransitionId, row_identity: &[u8]) -> Result<Vec<u8>> {
    Ok(keys::catalog_transition_violation_key(
        super::physical_transition_number(id)?,
        row_identity,
    ))
}

pub fn transition_violation_range(id: &TransitionId) -> Result<(Vec<u8>, Vec<u8>)> {
    Ok(prefix_bounds(
        keys::catalog_transition_violation_prefix_transition(super::physical_transition_number(
            id,
        )?),
    ))
}

pub async fn put_transition_violation<V: KvView + ?Sized>(
    view: &mut V,
    id: &TransitionId,
    row_identity: &[u8],
    cause: &str,
) -> Result<()> {
    view.put(
        Bytes::from(transition_violation_key(id, row_identity)?),
        Bytes::copy_from_slice(cause.as_bytes()),
    )
    .await
    .map_err(map_kv)
}

pub async fn delete_transition_violation<V: KvView + ?Sized>(
    view: &mut V,
    id: &TransitionId,
    row_identity: &[u8],
) -> Result<()> {
    view.delete(&transition_violation_key(id, row_identity)?)
        .await
        .map_err(map_kv)
}

pub async fn first_transition_violation<V: KvView + ?Sized>(
    view: &mut V,
    id: &TransitionId,
) -> Result<Option<(Vec<u8>, String)>> {
    let (start, end) = transition_violation_range(id)?;
    let mut iterator = view
        .scan(crate::engine::kv::KeyRange::new(
            Bytes::copy_from_slice(&start),
            Bytes::from(end),
        ))
        .await
        .map_err(map_kv)?;
    let Some(entry) = iterator.next().await.map_err(map_kv)? else {
        return Ok(None);
    };
    let row_identity = keys::decode_catalog_transition_violation_key(&entry.key)
        .ok_or_else(|| {
            Error::message(
                ErrorKind::CatalogCorrupt,
                "catalog: malformed transition violation key",
            )
        })?
        .row_identity;
    let cause = String::from_utf8(entry.value.to_vec()).map_err(|error| {
        Error::source(
            ErrorKind::CatalogCorrupt,
            "catalog: transition violation cause is not UTF-8",
            error,
        )
    })?;
    Ok(Some((row_identity, cause)))
}

#[cfg(test)]
mod tests {
    use crate::engine::kv::TransactionalKv;
    use crate::engine::kv::slatedb;

    use super::*;

    #[tokio::test]
    async fn violations_are_ordered_by_encoded_row_identity() {
        let mut database = slatedb::Store::memory("catalog-transition-violations")
            .await
            .unwrap();
        let id = TransitionId::from("tr1");
        put_transition_violation(&mut database, &id, &[2], "second")
            .await
            .unwrap();
        put_transition_violation(&mut database, &id, &[1], "first")
            .await
            .unwrap();
        assert_eq!(
            first_transition_violation(&mut database, &id)
                .await
                .unwrap(),
            Some((vec![1], "first".into()))
        );
        delete_transition_violation(&mut database, &id, &[1])
            .await
            .unwrap();
        assert_eq!(
            first_transition_violation(&mut database, &id)
                .await
                .unwrap(),
            Some((vec![2], "second".into()))
        );
        database.close().await.unwrap();
    }
}
