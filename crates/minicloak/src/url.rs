// Percent encoding/decoding for URLs and form bodies. Pure std, no deps.

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn unreserved(b: u8) -> bool {
    matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~')
}

// Decode %XX escapes. When `plus_as_space` is set, a bare '+' becomes a space
// (application/x-www-form-urlencoded); otherwise '+' is left untouched.
fn decode(s: &str, plus_as_space: bool) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        if plus_as_space && b == b'+' {
            out.push(b' ');
        } else {
            out.push(b);
        }
        i += 1;
    }
    out
}

// Decode %XX escapes only. '+' is preserved literally.
pub fn percent_decode_str(s: &str) -> String {
    String::from_utf8_lossy(&decode(s, false)).into_owned()
}

// Percent-encode every byte except the unreserved set (A-Za-z0-9-._~).
// Escapes use uppercase hex.
pub fn percent_encode(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if unreserved(b) {
            o.push(b as char);
        } else {
            o.push('%');
            o.push_str(&format!("{:02X}", b));
        }
    }
    o
}

// Like percent_decode_str, but '+' decodes to a space (form encoding).
pub fn form_decode(s: &str) -> String {
    String::from_utf8_lossy(&decode(s, true)).into_owned()
}

// Parse a query/form string into key/value pairs. Splits on '&', then on the
// first '='; both sides are form-decoded. A bare "k" yields ("k", "").
pub fn parse_query(q: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if q.is_empty() {
        return out;
    }
    for part in q.split('&') {
        if part.is_empty() {
            continue;
        }
        if let Some(eq) = part.find('=') {
            let k = form_decode(&part[..eq]);
            let v = form_decode(&part[eq + 1..]);
            out.push((k, v));
        } else {
            out.push((form_decode(part), String::new()));
        }
    }
    out
}

// First value matching `name`, if any.
pub fn qget<'a>(params: &'a [(String, String)], name: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trip() {
        let s = "a b/c?d=e&f~g.h_i-j";
        let enc = percent_encode(s);
        assert_eq!(percent_decode_str(&enc), s);
    }

    #[test]
    fn percent_encode_reserved() {
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("a/b"), "a%2Fb");
        assert_eq!(percent_encode("-._~"), "-._~");
    }

    #[test]
    fn percent_decode_no_plus() {
        // percent_decode_str must NOT turn '+' into a space.
        assert_eq!(percent_decode_str("a+b"), "a+b");
        assert_eq!(percent_decode_str("hello%20world"), "hello world");
        assert_eq!(percent_decode_str("%2F%2f"), "//");
    }

    #[test]
    fn percent_decode_keeps_invalid_escape() {
        assert_eq!(percent_decode_str("a%zz"), "a%zz");
        assert_eq!(percent_decode_str("ab%"), "ab%");
    }

    #[test]
    fn form_decode_plus_is_space() {
        assert_eq!(form_decode("a+b"), "a b");
        assert_eq!(form_decode("a%2Fb+c"), "a/b c");
    }

    #[test]
    fn decode_handles_non_utf8() {
        assert!(percent_decode_str("%FF").contains('\u{FFFD}'));
    }

    #[test]
    fn parse_query_simple() {
        let q = parse_query("a=1&b=2");
        assert_eq!(q, vec![("a".into(), "1".into()), ("b".into(), "2".into())]);
    }

    #[test]
    fn parse_query_empty_and_leading_amp() {
        assert!(parse_query("").is_empty());
        let q = parse_query("&&a=1");
        assert_eq!(q, vec![("a".into(), "1".into())]);
    }

    #[test]
    fn parse_query_bare_key_and_empty_value() {
        let q = parse_query("flag&k=");
        assert_eq!(q, vec![("flag".into(), "".into()), ("k".into(), "".into())]);
    }

    #[test]
    fn parse_query_form_decodes() {
        let q = parse_query("k+ey=val%2Fue");
        assert_eq!(q, vec![("k ey".into(), "val/ue".into())]);
    }

    #[test]
    fn parse_query_duplicate_keys_and_qget() {
        let q = parse_query("k=1&k=2");
        assert_eq!(q, vec![("k".into(), "1".into()), ("k".into(), "2".into())]);
        // qget returns the first match.
        assert_eq!(qget(&q, "k"), Some("1"));
        assert_eq!(qget(&q, "missing"), None);
    }
}
