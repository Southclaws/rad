use bytes::Bytes;

use crate::engine::catalog::identity::TransitionId;
use crate::engine::catalog::{Error, ErrorKind, Result};
use crate::engine::kv::KvView;

use super::{map_kv, prefix_bounds};

const TRANSITION_VIOLATION_PREFIX: &str = "/rad/catalog/transition_violation/";

fn transition_violation_key(id: &TransitionId, row_identity: &[u8]) -> Vec<u8> {
    format!(
        "{TRANSITION_VIOLATION_PREFIX}{id}/{}",
        encode_hex(row_identity)
    )
    .into_bytes()
}

pub fn transition_violation_range(id: &TransitionId) -> (Vec<u8>, Vec<u8>) {
    let start = format!("{TRANSITION_VIOLATION_PREFIX}{id}/").into_bytes();
    prefix_bounds(start)
}

pub async fn put_transition_violation<V: KvView + ?Sized>(
    view: &mut V,
    id: &TransitionId,
    row_identity: &[u8],
    cause: &str,
) -> Result<()> {
    view.put(
        Bytes::from(transition_violation_key(id, row_identity)),
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
    view.delete(&transition_violation_key(id, row_identity))
        .await
        .map_err(map_kv)
}

pub async fn first_transition_violation<V: KvView + ?Sized>(
    view: &mut V,
    id: &TransitionId,
) -> Result<Option<(Vec<u8>, String)>> {
    let (start, end) = transition_violation_range(id);
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
    let encoded = entry.key.strip_prefix(start.as_slice()).ok_or_else(|| {
        Error::message(
            ErrorKind::CatalogCorrupt,
            "catalog: malformed transition violation key",
        )
    })?;
    let row_identity = decode_hex(encoded)?;
    let cause = String::from_utf8(entry.value.to_vec()).map_err(|error| {
        Error::source(
            ErrorKind::CatalogCorrupt,
            "catalog: transition violation cause is not UTF-8",
            error,
        )
    })?;
    Ok(Some((row_identity, cause)))
}

fn encode_hex(value: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn decode_hex(value: &[u8]) -> Result<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return Err(Error::message(
            ErrorKind::CatalogCorrupt,
            "catalog: malformed hex row identity",
        ));
    }
    value
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_digit(pair[0])?;
            let low = hex_digit(pair[1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_digit(value: u8) -> Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(Error::message(
            ErrorKind::CatalogCorrupt,
            "catalog: malformed hex row identity",
        )),
    }
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
