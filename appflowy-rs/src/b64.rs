//! Shared base64 decoding (no `base64` crate dependency).

/// Standard base64 decoder. `base64` is intentionally not a dependency; this
/// mirrors Python `base64.b64decode(value)` with `validate=False`
/// (non-alphabet bytes discarded, padding required to a multiple of 4).
pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut sextets: Vec<u8> = Vec::new();
    let mut padding = 0usize;
    for &b in input.as_bytes() {
        match b {
            b'A'..=b'Z' => sextets.push(b - b'A'),
            b'a'..=b'z' => sextets.push(b - b'a' + 26),
            b'0'..=b'9' => sextets.push(b - b'0' + 52),
            b'+' => sextets.push(62),
            b'/' => sextets.push(63),
            b'=' => padding += 1,
            _ => {}
        }
    }
    if sextets.len() % 4 == 1 || (!sextets.len().is_multiple_of(4) && padding == 0) {
        return None;
    }
    let mut out = Vec::with_capacity(sextets.len() * 3 / 4 + 3);
    let mut chunks = sextets.chunks_exact(4);
    for chunk in &mut chunks {
        out.push((chunk[0] << 2) | (chunk[1] >> 4));
        out.push((chunk[1] << 4) | (chunk[2] >> 2));
        out.push((chunk[2] << 6) | chunk[3]);
    }
    match chunks.remainder() {
        [a, b] => out.push((a << 2) | (b >> 4)),
        [a, b, c] => {
            out.push((a << 2) | (b >> 4));
            out.push((b << 4) | (c >> 2));
        }
        _ => {}
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_standard_and_lenient_inputs() {
        assert_eq!(base64_decode("SGVsbG8="), Some(b"Hello".to_vec()));
        assert_eq!(base64_decode("aGVsbG8=").as_deref(), Some(b"hello".as_slice()));
        assert_eq!(base64_decode("aGk=").as_deref(), Some(b"hi".as_slice()));
        assert_eq!(base64_decode("PQ=="), Some(vec![b'=']));
        assert_eq!(base64_decode(""), Some(Vec::new()));
        // validate=False discards non-alphabet bytes.
        assert_eq!(base64_decode("SGVs bG8=\n"), Some(b"Hello".to_vec()));
        assert_eq!(base64_decode("aG VsbG8!@#="), Some(b"hello".to_vec()));
        assert_eq!(base64_decode("QQ==="), Some(b"A".to_vec())); // excess padding ignored
        // Python raises Invalid padding for both of these (bad length without
        // padding, and a lone data char): decode fails.
        assert_eq!(base64_decode("SGVsbG8"), None); // missing padding
        assert_eq!(base64_decode("aGVsbG8"), None);
        assert_eq!(base64_decode("a"), None);
    }
}
