//! Canonical binary identifier formats and their application-facing porcelain.

use std::fmt;
use std::str::FromStr as _;

use base64::Engine as _;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Format {
    Uuid,
    Ulid,
    Xid,
}

impl Format {
    pub fn recognize(value: &str) -> Option<Self> {
        match value {
            "uuid" => Some(Self::Uuid),
            "ulid" => Some(Self::Ulid),
            "xid" => Some(Self::Xid),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Uuid => "uuid",
            Self::Ulid => "ulid",
            Self::Xid => "xid",
        }
    }

    pub const fn byte_len(self) -> usize {
        match self {
            Self::Uuid | Self::Ulid => 16,
            Self::Xid => 12,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Generator {
    UuidV4,
    UuidV7,
    Ulid,
    Xid,
}

impl Generator {
    pub const fn format(self) -> Format {
        match self {
            Self::UuidV4 | Self::UuidV7 => Format::Uuid,
            Self::Ulid => Format::Ulid,
            Self::Xid => Format::Xid,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid {format} value: {message}")]
pub struct Error {
    format: &'static str,
    message: String,
}

impl Error {
    fn new(format: Format, message: impl Into<String>) -> Self {
        Self {
            format: format.name(),
            message: message.into(),
        }
    }
}

pub fn validate(format: Format, bytes: &[u8]) -> Result<(), Error> {
    if bytes.len() != format.byte_len() {
        return Err(Error::new(
            format,
            format!(
                "expected {} canonical bytes, got {}",
                format.byte_len(),
                bytes.len()
            ),
        ));
    }
    Ok(())
}

pub fn parse(format: Format, text: &str) -> Result<Vec<u8>, Error> {
    let bytes = match format {
        Format::Uuid => {
            let value = uuid::Uuid::parse_str(text)
                .map_err(|error| Error::new(format, error.to_string()))?;
            if value.hyphenated().to_string() != text {
                return Err(Error::new(
                    format,
                    "text is not canonical lowercase UUID form",
                ));
            }
            value.into_bytes().to_vec()
        }
        Format::Ulid => {
            let value = ulid::Ulid::from_string(text)
                .map_err(|error| Error::new(format, error.to_string()))?;
            if value.to_string() != text {
                return Err(Error::new(
                    format,
                    "text is not canonical uppercase ULID form",
                ));
            }
            value.to_bytes().to_vec()
        }
        Format::Xid => {
            let value =
                xid::Id::from_str(text).map_err(|error| Error::new(format, error.to_string()))?;
            if value.to_string() != text {
                return Err(Error::new(
                    format,
                    "text is not canonical lowercase XID form",
                ));
            }
            value.as_bytes().to_vec()
        }
    };
    Ok(bytes)
}

pub fn render(format: Format, bytes: &[u8]) -> Result<String, Error> {
    validate(format, bytes)?;
    Ok(match format {
        Format::Uuid => uuid::Uuid::from_slice(bytes)
            .expect("UUID byte length checked")
            .hyphenated()
            .to_string(),
        Format::Ulid => {
            ulid::Ulid::from_bytes(bytes.try_into().expect("ULID byte length checked")).to_string()
        }
        Format::Xid => xid::Id::from_bytes(bytes)
            .expect("XID byte length checked")
            .to_string(),
    })
}

pub fn encode_base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn decode_base64(text: &str) -> Result<Vec<u8>, Base64Error> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(Base64Error::Decode)?;
    if encode_base64(&bytes) != text {
        return Err(Base64Error::NonCanonical);
    }
    Ok(bytes)
}

#[derive(Debug)]
pub enum Base64Error {
    Decode(base64::DecodeError),
    NonCanonical,
}

impl fmt::Display for Base64Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(error) => error.fmt(formatter),
            Self::NonCanonical => formatter.write_str("bytes are not canonical padded base64"),
        }
    }
}

impl std::error::Error for Base64Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Decode(error) => Some(error),
            Self::NonCanonical => None,
        }
    }
}

pub fn generate_system(generator: Generator) -> Vec<u8> {
    match generator {
        Generator::UuidV4 => uuid::Uuid::new_v4().into_bytes().to_vec(),
        Generator::UuidV7 => uuid::Uuid::now_v7().into_bytes().to_vec(),
        Generator::Ulid => ulid::Ulid::new().to_bytes().to_vec(),
        Generator::Xid => xid::new().as_bytes().to_vec(),
    }
}

/// Builds a standards-shaped identifier from caller-owned time and entropy.
/// Deterministic simulation uses this instead of process-global clocks or RNGs.
pub fn generate_deterministic(
    generator: Generator,
    unix_millis: u64,
    entropy: [u8; 16],
) -> Vec<u8> {
    match generator {
        Generator::UuidV4 => uuid::Builder::from_random_bytes(entropy)
            .into_uuid()
            .into_bytes()
            .to_vec(),
        Generator::UuidV7 => uuid::Builder::from_unix_timestamp_millis(
            unix_millis,
            &entropy[6..].try_into().expect("ten-byte entropy slice"),
        )
        .into_uuid()
        .into_bytes()
        .to_vec(),
        Generator::Ulid => ulid::Ulid::from_parts(unix_millis, u128::from_be_bytes(entropy))
            .to_bytes()
            .to_vec(),
        Generator::Xid => {
            let mut bytes = [0_u8; 12];
            bytes[..4].copy_from_slice(&((unix_millis / 1_000) as u32).to_be_bytes());
            bytes[4..].copy_from_slice(&entropy[8..]);
            bytes.to_vec()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_porcelain_is_canonical_and_accepts_zero_values() {
        let cases = [
            (Format::Uuid, "00000000-0000-0000-0000-000000000000"),
            (Format::Ulid, "00000000000000000000000000"),
            (Format::Xid, "00000000000000000000"),
        ];
        for (format, text) in cases {
            let bytes = parse(format, text).unwrap();
            assert_eq!(bytes, vec![0; format.byte_len()]);
            assert_eq!(render(format, &bytes).unwrap(), text);
        }
    }

    #[test]
    fn identifier_porcelain_rejects_malformed_and_noncanonical_text() {
        for (format, text) in [
            (Format::Uuid, "not-a-uuid"),
            (Format::Uuid, "00000000000000000000000000000000"),
            (Format::Uuid, "00000000-0000-0000-0000-00000000000A"),
            (Format::Ulid, "80000000000000000000000000"),
            (Format::Ulid, "0000000000000000000000000o"),
            (Format::Xid, "0000000000000000000w"),
            (Format::Xid, "0000000000000000000A"),
        ] {
            assert!(parse(format, text).is_err(), "accepted {format:?} {text:?}");
        }
    }

    #[test]
    fn raw_bytes_use_canonical_padded_base64() {
        for bytes in [vec![], vec![0], vec![0, 0xff, 1, 0x80]] {
            let text = encode_base64(&bytes);
            assert_eq!(decode_base64(&text).unwrap(), bytes);
        }
        assert!(decode_base64("_w==").is_err());
        assert!(decode_base64("/w").is_err());
    }

    #[test]
    fn deterministic_generators_set_versions_and_preserve_time_order() {
        let entropy = [0x55; 16];
        let v4 = generate_deterministic(Generator::UuidV4, 1_700_000_000_000, entropy);
        let v7a = generate_deterministic(Generator::UuidV7, 1_700_000_000_000, entropy);
        let v7b = generate_deterministic(Generator::UuidV7, 1_700_000_000_001, entropy);
        assert_eq!(uuid::Uuid::from_slice(&v4).unwrap().get_version_num(), 4);
        assert_eq!(uuid::Uuid::from_slice(&v7a).unwrap().get_version_num(), 7);
        assert!(v7a < v7b);

        for generator in [Generator::Ulid, Generator::Xid] {
            let first = generate_deterministic(generator, 1_700_000_000_000, entropy);
            let second = generate_deterministic(generator, 1_700_000_001_000, entropy);
            assert!(
                first < second,
                "{generator:?} canonical bytes lost time order"
            );
        }
    }

    #[test]
    fn system_generators_emit_valid_canonical_payloads() {
        for generator in [
            Generator::UuidV4,
            Generator::UuidV7,
            Generator::Ulid,
            Generator::Xid,
        ] {
            let bytes = generate_system(generator);
            let format = generator.format();
            assert_eq!(bytes.len(), format.byte_len());
            let text = render(format, &bytes).unwrap();
            assert_eq!(parse(format, &text).unwrap(), bytes);
        }
        assert_eq!(
            uuid::Uuid::from_slice(&generate_system(Generator::UuidV4))
                .unwrap()
                .get_version_num(),
            4
        );
        assert_eq!(
            uuid::Uuid::from_slice(&generate_system(Generator::UuidV7))
                .unwrap()
                .get_version_num(),
            7
        );
    }
}
