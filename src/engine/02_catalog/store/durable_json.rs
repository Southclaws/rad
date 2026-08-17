use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::engine::catalog::{Error, ErrorKind, Result};

/// Every structured durable value is stored inside this envelope. The
/// `format` discriminator identifies the payload serialization explicitly,
/// so historical formats are never inferred from the presence or absence of
/// fields. Format 1 is JSON with the record under `record`.
const PAYLOAD_FORMAT: u32 = 1;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Envelope<T> {
    format: u32,
    record: T,
}

pub(crate) fn decode<T: DeserializeOwned>(kind: &str, id: &str, raw: &[u8]) -> Result<T> {
    let identity = |id: &str| {
        if id.is_empty() {
            String::new()
        } else {
            format!(" {id:?}")
        }
    };
    let envelope: Envelope<T> = serde_json::from_slice(raw).map_err(|error| {
        Error::source(
            ErrorKind::CatalogCorrupt,
            format!("catalog: corrupt {kind}{}: {error}", identity(id)),
            error,
        )
    })?;
    if envelope.format != PAYLOAD_FORMAT {
        return Err(Error::message(
            ErrorKind::CatalogCorrupt,
            format!(
                "catalog: {kind}{} has unsupported payload format {}",
                identity(id),
                envelope.format
            ),
        ));
    }
    Ok(envelope.record)
}

pub(crate) fn encode<T: Serialize>(kind: &str, id: &str, value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(&Envelope {
        format: PAYLOAD_FORMAT,
        record: value,
    })
    .map_err(|error| {
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

    #[derive(Debug, Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Record {
        value: u64,
    }

    #[test]
    fn envelope_round_trips_and_carries_the_format_discriminator() {
        let raw = encode("record", "one", &Record { value: 1 }).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&raw).unwrap(),
            serde_json::json!({ "format": 1, "record": { "value": 1 } })
        );
        assert_eq!(decode::<Record>("record", "one", &raw).unwrap().value, 1);
    }

    #[test]
    fn durable_decoder_rejects_unknown_trailing_and_foreign_formats() {
        for malformed in [
            br#"{"format":1,"record":{"value":1,"future":2}}"#.as_slice(),
            br#"{"format":1,"record":{"value":1}} {"format":1}"#.as_slice(),
            br#"{"format":1,"record":{"value":1},"extra":2}"#.as_slice(),
            br#"{"value":1}"#.as_slice(),
        ] {
            let error = decode::<Record>("record", "one", malformed).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::CatalogCorrupt, "{malformed:?}");
        }
        let error =
            decode::<Record>("record", "one", br#"{"format":2,"record":{"value":1}}"#).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::CatalogCorrupt);
        assert!(error.to_string().contains("unsupported payload format"));
    }
}
