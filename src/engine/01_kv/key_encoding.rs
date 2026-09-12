pub const TAG_NULL: u8 = 0x01;
pub const TAG_BOOL: u8 = 0x02;
pub const TAG_I64: u8 = 0x03;
pub const TAG_F64: u8 = 0x04;
pub const TAG_TEXT: u8 = 0x05;
pub const TAG_BYTES: u8 = 0x06;

const ESCAPE: u8 = 0x00;
const ESCAPED_FF: u8 = 0xff;
const TERMINATOR: u8 = 0x01;

/// Encodes text so that byte order matches UTF-8 lexicographic order.
pub fn encode_text(value: &str) -> Vec<u8> {
    encode_escaped(TAG_TEXT, value.as_bytes())
}

/// Encodes arbitrary bytes so byte order remains lexicographic and tuples are
/// self-delimiting even when a value contains zero bytes.
pub fn encode_bytes(value: &[u8]) -> Vec<u8> {
    encode_escaped(TAG_BYTES, value)
}

fn encode_escaped(tag: u8, value: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(value.len() + 3);
    encoded.push(tag);
    for byte in value.iter().copied() {
        if byte == ESCAPE {
            encoded.extend_from_slice(&[ESCAPE, ESCAPED_FF]);
        } else {
            encoded.push(byte);
        }
    }
    encoded.extend_from_slice(&[ESCAPE, TERMINATOR]);
    encoded
}

/// Encodes an integer so that byte order matches numeric order.
pub fn encode_i64(value: i64) -> [u8; 9] {
    encode_ordered_u64(TAG_I64, (value as u64) ^ (1_u64 << 63))
}

/// Encodes a non-NaN float so that byte order matches numeric order.
pub fn encode_f64(value: f64) -> Option<[u8; 9]> {
    if value.is_nan() {
        return None;
    }

    let raw = value.to_bits();
    let ordered = if raw & (1_u64 << 63) != 0 {
        !raw
    } else {
        raw | (1_u64 << 63)
    };
    Some(encode_ordered_u64(TAG_F64, ordered))
}

fn encode_ordered_u64(tag: u8, value: u64) -> [u8; 9] {
    let mut encoded = [0; 9];
    encoded[0] = tag;
    encoded[1..].copy_from_slice(&value.to_be_bytes());
    encoded
}

pub fn encode_null() -> [u8; 1] {
    [TAG_NULL]
}

pub fn encode_bool(value: bool) -> [u8; 2] {
    [TAG_BOOL, u8::from(value)]
}

/// Appends the canonical (minimal-length) varint encoding of `value`.
pub fn append_uvarint(output: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        output.push((value as u8) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

/// Reads one canonical varint at `position`, advancing it past the encoding.
/// Returns `None` for truncated, non-canonical, or overflowing input.
pub fn read_uvarint(input: &[u8], position: &mut usize) -> Option<u64> {
    let mut value = 0_u64;
    for shift in (0..70).step_by(7) {
        let byte = *input.get(*position)?;
        *position += 1;
        if shift == 63 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            if shift != 0 && byte == 0 {
                return None;
            }
            return Some(value);
        }
    }
    None
}

/// Returns the exclusive upper bound for all keys beginning with `prefix`.
pub fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    for index in (0..end.len()).rev() {
        if end[index] != 0xff {
            end[index] += 1;
            end.truncate(index + 1);
            return Some(end);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn next_u64(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn primitive_encodings_preserve_order() {
        assert!(encode_null().as_slice() < encode_bool(false).as_slice());
        assert!(encode_bool(false) < encode_bool(true));
        assert!(encode_i64(i64::MIN) < encode_i64(-1));
        assert!(encode_i64(-1) < encode_i64(0));
        assert!(encode_i64(0) < encode_i64(i64::MAX));
        assert!(encode_f64(-10.0) < encode_f64(-0.0));
        assert!(encode_f64(-0.0) < encode_f64(0.0));
        assert!(encode_f64(0.0) < encode_f64(10.0));
        assert_eq!(encode_f64(f64::NAN), None);
        assert!(encode_text("app") < encode_text("apple"));
        assert!(encode_text("a\0") < encode_text("a\0b"));
        assert!(encode_bytes(b"a\0") < encode_bytes(b"a\0b"));
    }

    #[test]
    fn primitive_encodings_preserve_order_for_seeded_samples() {
        let mut state = 0x6b65_7965_6e63_0001;
        for _ in 0..4_096 {
            let left = next_u64(&mut state) as i64;
            let right = next_u64(&mut state) as i64;
            assert_eq!(
                encode_i64(left).cmp(&encode_i64(right)),
                left.cmp(&right),
                "int64 ordering mismatch for {left} and {right}"
            );

            let left = f64::from_bits(next_u64(&mut state));
            let right = f64::from_bits(next_u64(&mut state));
            if !left.is_nan() && !right.is_nan() {
                assert_eq!(
                    encode_f64(left).unwrap().cmp(&encode_f64(right).unwrap()),
                    left.total_cmp(&right),
                    "float64 ordering mismatch for {left:?} and {right:?}"
                );
            }
        }
    }

    #[test]
    fn text_encoding_is_self_delimiting_and_nul_safe() {
        let encoded_a = encode_text("a");
        let encoded_a_nul = encode_text("a\0");
        assert!(!encoded_a_nul.starts_with(&encoded_a));

        let ordered = [
            "", "a", "app", "apple", "b", "ba\0", "ba\0\0", "ba\u{1}", "bb",
        ];
        for pair in ordered.windows(2) {
            assert!(
                encode_text(pair[0]) < encode_text(pair[1]),
                "text ordering mismatch for {:?} and {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn bytes_encoding_is_self_delimiting_and_preserves_binary_order() {
        let ordered: [&[u8]; 8] = [
            b"", b"\0", b"\0\0", b"\0\xff", b"a", b"a\0", b"a\xff", b"\xff",
        ];
        for pair in ordered.windows(2) {
            assert!(encode_bytes(pair[0]) < encode_bytes(pair[1]));
        }
        assert!(!encode_bytes(b"a\0b").starts_with(&encode_bytes(b"a")));
    }

    #[test]
    fn type_tags_define_one_stable_cross_type_order() {
        let encoded = [
            encode_null().to_vec(),
            encode_bool(false).to_vec(),
            encode_bool(true).to_vec(),
            encode_i64(i64::MAX).to_vec(),
            encode_f64(f64::MAX).unwrap().to_vec(),
            encode_text(""),
        ];
        for pair in encoded.windows(2) {
            assert!(pair[0] < pair[1]);
        }
    }

    #[test]
    fn prefix_end_returns_the_smallest_exclusive_bound() {
        assert_eq!(prefix_end(&[0x01]), Some(vec![0x02]));
        assert_eq!(prefix_end(b"ab\xff"), Some(b"ac".to_vec()));
        assert_eq!(prefix_end(&[0xab, 0xcd]), Some(vec![0xab, 0xce]));
        assert_eq!(prefix_end(&[0xff]), None);
        assert_eq!(prefix_end(&[]), None);

        let prefix = [0x03, 0xff];
        let end = prefix_end(&prefix).unwrap();
        for suffix in [vec![], vec![0x00], vec![0xff], vec![0xff, 0xff]] {
            let mut key = prefix.to_vec();
            key.extend_from_slice(&suffix);
            assert!(prefix.as_slice() <= key.as_slice());
            assert!(key < end);
        }
    }
}
