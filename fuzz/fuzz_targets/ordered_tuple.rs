#![no_main]

use libfuzzer_sys::fuzz_target;
use rad::engine::exec::codec::{decode_tuple, encode_tuple};

fuzz_target!(|input: &[u8]| {
    if input.len() > 64 * 1024 {
        return;
    }
    let decoded;
    let input = if let Some(hex) = input.strip_prefix(b"hex:") {
        let Some(bytes) = decode_hex(hex) else {
            return;
        };
        decoded = bytes;
        decoded.as_slice()
    } else {
        input
    };
    let Ok(values) = decode_tuple(input) else {
        return;
    };
    if let Ok(encoded) = encode_tuple(&values) {
        assert_eq!(encoded, input);
        assert_eq!(decode_tuple(&encoded).expect("encoded tuple decodes"), values);
    }
});

fn decode_hex(input: &[u8]) -> Option<Vec<u8>> {
    let input = input.strip_suffix(b"\n").unwrap_or(input);
    let input = input.strip_suffix(b"\r").unwrap_or(input);
    if input.len() % 2 != 0 {
        return None;
    }
    input
        .chunks_exact(2)
        .map(|pair| Some((digit(pair[0])? << 4) | digit(pair[1])?))
        .collect()
}

fn digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}
