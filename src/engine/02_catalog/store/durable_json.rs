use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::engine::catalog::{Error, ErrorKind, Result};

pub(crate) fn decode<T: DeserializeOwned>(kind: &str, id: &str, raw: &[u8]) -> Result<T> {
    serde_json::from_slice(raw).map_err(|error| {
        let identity = if id.is_empty() {
            String::new()
        } else {
            format!(" {id:?}")
        };
        Error::source(
            ErrorKind::CatalogCorrupt,
            format!("catalog: corrupt {kind}{identity}: {error}"),
            error,
        )
    })
}

pub(crate) fn encode<T: Serialize>(kind: &str, id: &str, value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|error| {
        let identity = if id.is_empty() {
            String::new()
        } else {
            format!(" {id:?}")
        };
        Error::source(
            ErrorKind::CatalogDrift,
            format!("catalog: encode {kind}{identity}: {error}"),
            error,
        )
    })
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Record {
        value: u64,
    }

    #[test]
    fn durable_decoder_rejects_unknown_and_trailing_values() {
        assert_eq!(
            decode::<Record>("record", "one", br#"{"value":1}"#)
                .unwrap()
                .value,
            1
        );
        for malformed in [
            br#"{"value":1,"future":2}"#.as_slice(),
            br#"{"value":1} {"value":2}"#.as_slice(),
        ] {
            let error = decode::<Record>("record", "one", malformed).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::CatalogCorrupt);
        }
    }
}
