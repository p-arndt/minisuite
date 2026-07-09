// Base64 (RFC 4648): standard + URL-safe alphabets. Pure std, no external deps.

const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn encode_inner(data: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(alphabet[((n >> 18) & 63) as usize] as char);
        out.push(alphabet[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(alphabet[((n >> 6) & 63) as usize] as char);
        } else if pad {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(alphabet[(n & 63) as usize] as char);
        } else if pad {
            out.push('=');
        }
    }
    out
}

fn val(c: u8, alphabet: &[u8; 64]) -> Option<u32> {
    alphabet.iter().position(|&a| a == c).map(|p| p as u32)
}

fn decode_inner(s: &str, alphabet: &[u8; 64]) -> Option<Vec<u8>> {
    // Collect significant symbols, skipping whitespace and padding.
    let mut syms: Vec<u32> = Vec::with_capacity(s.len());
    for &c in s.as_bytes() {
        match c {
            b'\r' | b'\n' | b'\t' | b' ' | b'=' => continue,
            _ => syms.push(val(c, alphabet)?),
        }
    }
    // A remainder of exactly 1 symbol is impossible in valid base64.
    if syms.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(syms.len() / 4 * 3 + 2);
    for group in syms.chunks(4) {
        let mut n = 0u32;
        for i in 0..4 {
            n |= group.get(i).copied().unwrap_or(0) << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if group.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if group.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// Standard alphabet, '=' padded.
pub fn encode(data: &[u8]) -> String {
    encode_inner(data, STD, true)
}

/// Standard alphabet; tolerates missing padding and embedded \r \n \t ' '.
pub fn decode(s: &str) -> Option<Vec<u8>> {
    decode_inner(s, STD)
}

/// URL-safe alphabet (-_), no padding.
pub fn encode_url(data: &[u8]) -> String {
    encode_inner(data, URL, false)
}

/// URL-safe alphabet; padding optional.
pub fn decode_url(s: &str) -> Option<Vec<u8>> {
    decode_inner(s, URL)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        for &(bytes, txt) in VECTORS {
            assert_eq!(encode(bytes), txt);
        }
    }

    #[test]
    fn rfc4648_decode() {
        for &(bytes, txt) in VECTORS {
            assert_eq!(decode(txt).as_deref(), Some(bytes));
        }
    }

    #[test]
    fn roundtrip_all_bytes() {
        let data: Vec<u8> = (0..=255u8).collect();
        assert_eq!(decode(&encode(&data)).unwrap(), data);
        assert_eq!(decode_url(&encode_url(&data)).unwrap(), data);
    }

    #[test]
    fn url_safe_charset() {
        let data: Vec<u8> = (0..=255u8).collect();
        let e = encode_url(&data);
        assert!(!e.contains('='));
        assert!(!e.contains('+'));
        assert!(!e.contains('/'));
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(decode("a"), None); // remainder of 1
        assert_eq!(decode("!!!!"), None); // invalid chars
        assert_eq!(decode_url("a"), None);
    }

    #[test]
    fn tolerates_pem_newlines() {
        let data: Vec<u8> = (0..48u8).collect();
        let e = encode(&data);
        let mut pem = String::new();
        for (i, c) in e.chars().enumerate() {
            if i > 0 && i % 16 == 0 {
                pem.push_str("\r\n");
            }
            pem.push(c);
        }
        pem.push('\n');
        assert_eq!(decode(&pem).unwrap(), data);
    }

    #[test]
    fn missing_padding_ok() {
        assert_eq!(decode("Zg").unwrap(), b"f");
        assert_eq!(decode("Zm8").unwrap(), b"fo");
    }
}
