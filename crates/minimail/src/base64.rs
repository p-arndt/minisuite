// Base64 encode/decode (RFC 4648). Pure std, no external deps.

#[allow(dead_code)] // used only by frozen encode/encode_wrapped, unused in this binary
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with '=' padding.
#[allow(dead_code)] // frozen SPEC §5 base64 API; minimail only ever decodes
pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// MIME base64: encode wrapped at `line_width` cols with CRLF (line_width e.g. 76).
#[allow(dead_code)] // frozen SPEC §5 base64 API; minimail only ever decodes
pub fn encode_wrapped(data: &[u8], line_width: usize) -> String {
    let b = encode(data);
    if line_width == 0 || b.len() <= line_width {
        return b;
    }
    // Encoded output is pure ASCII, so byte-boundary slicing is safe.
    let mut out = String::with_capacity(b.len() + (b.len() / line_width) * 2);
    let mut i = 0;
    while i < b.len() {
        if i > 0 {
            out.push_str("\r\n");
        }
        let end = (i + line_width).min(b.len());
        out.push_str(&b[i..end]);
        i = end;
    }
    out
}

/// Lenient decode: skips ASCII whitespace/newlines, returns None on an invalid
/// alphabet byte. Missing trailing '=' padding is tolerated (SMTP AUTH clients
/// vary); present padding must not be followed by more data.
pub fn decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3 + 3);
    let mut acc: u32 = 0;
    let mut nbits: u32 = 0;
    let mut nsig: usize = 0; // count of significant (non-pad, non-space) chars
    let mut seen_pad = false;
    for &b in s.as_bytes() {
        match b {
            b' ' | b'\t' | b'\r' | b'\n' | 0x0b | 0x0c => continue,
            b'=' => {
                seen_pad = true;
                continue;
            }
            _ => {}
        }
        if seen_pad {
            // Alphabet data after padding is malformed.
            return None;
        }
        let v = val(b)? as u32;
        acc = (acc << 6) | v;
        nbits += 6;
        nsig += 1;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }
    // A lone trailing 6-bit group cannot encode a byte, with or without padding.
    if nsig % 4 == 1 {
        return None;
    }
    Some(out)
}

fn val(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 4648 §10 test vectors.
    const VECTORS: &[(&[u8], &str)] = &[
        (b"", ""),
        (b"f", "Zg=="),
        (b"fo", "Zm8="),
        (b"foo", "Zm9v"),
        (b"foob", "Zm9vYg=="),
        (b"fooba", "Zm9vYmE="),
        (b"foobar", "Zm9vYmFy"),
    ];

    #[test]
    fn rfc4648_encode() {
        for (raw, enc) in VECTORS {
            assert_eq!(&encode(raw), enc, "encode {raw:?}");
        }
    }

    #[test]
    fn rfc4648_decode() {
        for (raw, enc) in VECTORS {
            assert_eq!(decode(enc).as_deref(), Some(*raw), "decode {enc}");
        }
    }

    #[test]
    fn round_trip_all_padding_lengths() {
        // Lengths mod 3 = 0,1,2 exercise the three padding cases.
        for len in 0..=64usize {
            let data: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let enc = encode(&data);
            assert_eq!(decode(&enc).as_deref(), Some(data.as_slice()), "len {len}");
        }
    }

    #[test]
    fn decode_skips_embedded_whitespace() {
        // CRLF-wrapped MIME body plus stray spaces/tabs.
        assert_eq!(decode("Zm9v\r\nYmFy").as_deref(), Some(&b"foobar"[..]));
        assert_eq!(decode("Z m 8 =").as_deref(), Some(&b"fo"[..]));
        assert_eq!(decode("\tZm9v Yg==\n").as_deref(), Some(&b"foob"[..]));
    }

    #[test]
    fn decode_missing_padding_lenient() {
        assert_eq!(decode("Zg").as_deref(), Some(&b"f"[..]));
        assert_eq!(decode("Zm8").as_deref(), Some(&b"fo"[..]));
        assert_eq!(decode("Zm9vYg").as_deref(), Some(&b"foob"[..]));
    }

    #[test]
    fn decode_rejects_invalid_alphabet_byte() {
        assert_eq!(decode("Zm9v!"), None);
        assert_eq!(decode("****"), None);
        assert_eq!(decode("Zg-="), None);
    }

    #[test]
    fn decode_rejects_data_after_padding() {
        assert_eq!(decode("Zg==Zg=="), None);
    }

    #[test]
    fn decode_rejects_lone_trailing_char() {
        // One significant char (6 bits) cannot form a byte.
        assert_eq!(decode("Zm9vZ"), None);
    }

    #[test]
    fn encode_wrapped_inserts_crlf() {
        let data = vec![0u8; 60]; // encodes to 80 chars
        let w = encode_wrapped(&data, 76);
        let lines: Vec<&str> = w.split("\r\n").collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), 76);
        assert_eq!(lines[1].len(), 4);
        // Still decodes back to the original despite the wrapping.
        assert_eq!(decode(&w).as_deref(), Some(data.as_slice()));
    }

    #[test]
    fn encode_wrapped_no_wrap_when_short() {
        assert_eq!(encode_wrapped(b"foobar", 76), "Zm9vYmFy");
        assert_eq!(encode_wrapped(b"foobar", 0), "Zm9vYmFy");
    }
}
