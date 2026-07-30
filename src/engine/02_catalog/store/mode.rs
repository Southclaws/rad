use std::str::FromStr;

use bytes::Bytes;

use crate::engine::catalog::model::Mode;
use crate::engine::catalog::{Error, ErrorKind, Result};
use crate::engine::kv::KvView;

use super::map_kv;

const MODE_KEY: &[u8] = b"/rad/catalog/meta/mode";

pub async fn read_mode<V: KvView + ?Sized>(view: &mut V) -> Result<Mode> {
    Ok(read_stored_mode(view).await?.unwrap_or(Mode::Direct))
}

pub async fn read_stored_mode<V: KvView + ?Sized>(view: &mut V) -> Result<Option<Mode>> {
    let Some(raw) = view.get(MODE_KEY).await.map_err(map_kv)? else {
        return Ok(None);
    };
    let value = std::str::from_utf8(&raw).map_err(|error| {
        Error::source(
            ErrorKind::CatalogCorrupt,
            format!("catalog: corrupt stored mode {raw:?}"),
            error,
        )
    })?;
    Mode::from_str(value).map(Some).map_err(|_| {
        Error::message(
            ErrorKind::CatalogCorrupt,
            format!("catalog: corrupt stored mode {raw:?}"),
        )
    })
}

pub async fn set_mode<V: KvView + ?Sized>(view: &mut V, mode: Mode) -> Result<()> {
    let value = match mode {
        Mode::Direct => "direct",
        Mode::Schema => "schema",
    };
    view.put(
        Bytes::from_static(MODE_KEY),
        Bytes::from_static(value.as_bytes()),
    )
    .await
    .map_err(map_kv)
}

#[cfg(test)]
mod tests {
    use crate::engine::kv::TransactionalKv;
    use crate::engine::kv::slatedb;

    use super::*;

    #[tokio::test]
    async fn missing_mode_defaults_to_direct_and_stored_mode_round_trips() {
        let mut database = slatedb::Store::memory("catalog-mode").await.unwrap();
        assert_eq!(read_mode(&mut database).await.unwrap(), Mode::Direct);
        assert_eq!(read_stored_mode(&mut database).await.unwrap(), None);
        set_mode(&mut database, Mode::Schema).await.unwrap();
        assert_eq!(read_mode(&mut database).await.unwrap(), Mode::Schema);
        database.close().await.unwrap();
    }
}
