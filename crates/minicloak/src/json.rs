// Minimal JSON (RFC 8259): a compact writer, and a parser just large enough to
// read back the token payloads we mint ourselves. Pure std.

/// Escape a string for inclusion in a JSON string literal (without surrounding quotes).
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// A JSON value that can be serialized compactly.
pub enum J {
    S(String),
    N(u64),
    B(bool),
    A(Vec<J>),
    O(Vec<(String, J)>),
}

impl J {
    /// Convenience: owned string value.
    pub fn s(v: &str) -> J {
        J::S(v.to_string())
    }

    /// Convenience: array of string values.
    pub fn arr_s(v: &[&str]) -> J {
        J::A(v.iter().map(|x| J::S(x.to_string())).collect())
    }

    /// Convenience: object from borrowed keys, preserving insertion order.
    pub fn obj(pairs: Vec<(&str, J)>) -> J {
        J::O(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    fn write(&self, out: &mut String) {
        match self {
            J::S(v) => {
                out.push('"');
                out.push_str(&escape(v));
                out.push('"');
            }
            J::N(v) => out.push_str(&v.to_string()),
            J::B(v) => out.push_str(if *v { "true" } else { "false" }),
            J::A(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            J::O(pairs) => {
                out.push('{');
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push('"');
                    out.push_str(&escape(k));
                    out.push_str("\":");
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

/// Serializes compactly, with no insignificant whitespace.
impl std::fmt::Display for J {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let mut out = String::new();
        self.write(&mut out);
        f.write_str(&out)
    }
}

// --- Parser ---
//
// Only ever fed JWT payloads we minted ourselves, so this stays small: integers
// (no floats, no exponents) and no \u escape decoding beyond the ASCII range.

#[derive(Clone, Debug, PartialEq)]
pub enum V {
    Str(String),
    Num(u64),
    Bool(bool),
    Null,
    Arr(Vec<V>),
    Obj(Vec<(String, V)>),
}

impl V {
    pub fn get(&self, key: &str) -> Option<&V> {
        match self {
            V::Obj(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            V::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            V::Num(n) => Some(*n),
            _ => None,
        }
    }
    /// Convenience for the common `payload.str("sub")` shape.
    pub fn str_field(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(|v| v.as_str())
    }
    pub fn u64_field(&self, key: &str) -> Option<u64> {
        self.get(key).and_then(|v| v.as_u64())
    }
}

pub fn parse(s: &str) -> Option<V> {
    let b = s.as_bytes();
    let mut p = 0usize;
    let v = parse_value(b, &mut p)?;
    skip_ws(b, &mut p);
    if p == b.len() {
        Some(v)
    } else {
        None
    }
}

fn skip_ws(b: &[u8], p: &mut usize) {
    while *p < b.len() && matches!(b[*p], b' ' | b'\t' | b'\n' | b'\r') {
        *p += 1;
    }
}

fn eat(b: &[u8], p: &mut usize, lit: &[u8]) -> bool {
    if b.len() - *p >= lit.len() && &b[*p..*p + lit.len()] == lit {
        *p += lit.len();
        true
    } else {
        false
    }
}

fn parse_value(b: &[u8], p: &mut usize) -> Option<V> {
    skip_ws(b, p);
    match *b.get(*p)? {
        b'"' => parse_string(b, p).map(V::Str),
        b'{' => parse_obj(b, p),
        b'[' => parse_arr(b, p),
        b't' => eat(b, p, b"true").then_some(V::Bool(true)),
        b'f' => eat(b, p, b"false").then_some(V::Bool(false)),
        b'n' => eat(b, p, b"null").then_some(V::Null),
        c if c.is_ascii_digit() => parse_num(b, p),
        _ => None,
    }
}

fn parse_num(b: &[u8], p: &mut usize) -> Option<V> {
    let start = *p;
    while *p < b.len() && b[*p].is_ascii_digit() {
        *p += 1;
    }
    if *p == start {
        return None;
    }
    std::str::from_utf8(&b[start..*p])
        .ok()?
        .parse()
        .ok()
        .map(V::Num)
}

fn parse_string(b: &[u8], p: &mut usize) -> Option<String> {
    if *b.get(*p)? != b'"' {
        return None;
    }
    *p += 1;
    let mut out = String::new();
    loop {
        match *b.get(*p)? {
            b'"' => {
                *p += 1;
                // The slice was valid UTF-8 to begin with, so pushed chars are too.
                return Some(out);
            }
            b'\\' => {
                *p += 1;
                let c = *b.get(*p)?;
                *p += 1;
                match c {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'b' => out.push('\u{08}'),
                    b'f' => out.push('\u{0c}'),
                    b'u' => {
                        if b.len() - *p < 4 {
                            return None;
                        }
                        let hex = std::str::from_utf8(&b[*p..*p + 4]).ok()?;
                        let cp = u32::from_str_radix(hex, 16).ok()?;
                        out.push(char::from_u32(cp)?);
                        *p += 4;
                    }
                    _ => return None,
                }
            }
            _ => {
                // Copy one whole UTF-8 scalar.
                let rest = std::str::from_utf8(&b[*p..]).ok()?;
                let c = rest.chars().next()?;
                out.push(c);
                *p += c.len_utf8();
            }
        }
    }
}

fn parse_arr(b: &[u8], p: &mut usize) -> Option<V> {
    *p += 1; // '['
    let mut items = Vec::new();
    skip_ws(b, p);
    if *b.get(*p)? == b']' {
        *p += 1;
        return Some(V::Arr(items));
    }
    loop {
        items.push(parse_value(b, p)?);
        skip_ws(b, p);
        match *b.get(*p)? {
            b',' => *p += 1,
            b']' => {
                *p += 1;
                return Some(V::Arr(items));
            }
            _ => return None,
        }
    }
}

fn parse_obj(b: &[u8], p: &mut usize) -> Option<V> {
    *p += 1; // '{'
    let mut pairs = Vec::new();
    skip_ws(b, p);
    if *b.get(*p)? == b'}' {
        *p += 1;
        return Some(V::Obj(pairs));
    }
    loop {
        skip_ws(b, p);
        let k = parse_string(b, p)?;
        skip_ws(b, p);
        if *b.get(*p)? != b':' {
            return None;
        }
        *p += 1;
        pairs.push((k, parse_value(b, p)?));
        skip_ws(b, p);
        match *b.get(*p)? {
            b',' => *p += 1,
            b'}' => {
                *p += 1;
                return Some(V::Obj(pairs));
            }
            _ => return None,
        }
    }
}

#[cfg(test)]
mod parser_tests {
    use super::*;

    #[test]
    fn roundtrips_a_token_payload() {
        let j = J::obj(vec![
            ("iss", J::s("http://x/realms/dev")),
            ("exp", J::N(1_700_000_300)),
            ("roles", J::arr_s(&["admin", "staff"])),
            ("email_verified", J::B(true)),
        ]);
        let v = parse(&j.to_string()).expect("parse");
        assert_eq!(v.str_field("iss"), Some("http://x/realms/dev"));
        assert_eq!(v.u64_field("exp"), Some(1_700_000_300));
        assert_eq!(v.get("email_verified"), Some(&V::Bool(true)));
        match v.get("roles") {
            Some(V::Arr(a)) => assert_eq!(a[1], V::Str("staff".into())),
            other => panic!("roles: {:?}", other),
        }
    }

    #[test]
    fn null_parses_even_though_we_never_emit_it() {
        let v = parse(r#"{"a":null}"#).expect("parse");
        assert_eq!(v.get("a"), Some(&V::Null));
    }

    #[test]
    fn escapes_survive_the_round_trip() {
        let raw = "a\"b\\c\nd\te\u{00e4}f";
        let s = format!("\"{}\"", escape(raw));
        assert_eq!(parse(&s), Some(V::Str(raw.to_string())));
    }

    #[test]
    fn unicode_escape_is_decoded() {
        assert_eq!(parse(r#""ä""#), Some(V::Str("ä".into())));
    }

    #[test]
    fn empty_containers() {
        assert_eq!(parse("{}"), Some(V::Obj(vec![])));
        assert_eq!(parse("[]"), Some(V::Arr(vec![])));
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\"}",
            "{\"a\":}",
            "tru",
            "1 2",
            "\"unterminated",
        ] {
            assert_eq!(parse(bad), None, "should reject {:?}", bad);
        }
    }

    #[test]
    fn trailing_garbage_is_rejected() {
        assert_eq!(parse("{} x"), None);
        assert_eq!(parse("  {\"a\":1}  "), parse("{\"a\":1}"));
    }

    #[test]
    fn accessors_are_type_safe() {
        let v = parse(r#"{"a":"s","b":7}"#).unwrap();
        assert_eq!(v.u64_field("a"), None);
        assert_eq!(v.str_field("b"), None);
        assert_eq!(v.str_field("missing"), None);
        assert_eq!(V::Num(1).get("x"), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping() {
        assert_eq!(escape("a\"b"), "a\\\"b");
        assert_eq!(escape("a\\b"), "a\\\\b");
        assert_eq!(escape("a\nb"), "a\\nb");
        assert_eq!(escape("\r\t\u{08}\u{0c}"), "\\r\\t\\b\\f");
        assert_eq!(escape("\u{01}"), "\\u0001");
    }

    #[test]
    fn nested_output() {
        let v = J::obj(vec![
            ("name", J::s("foo")),
            ("n", J::N(42)),
            ("ok", J::B(true)),
            ("list", J::arr_s(&["a", "b"])),
        ]);
        assert_eq!(
            v.to_string(),
            r#"{"name":"foo","n":42,"ok":true,"list":["a","b"]}"#
        );
    }

    #[test]
    fn key_order_preserved() {
        let v = J::obj(vec![("z", J::N(1)), ("a", J::N(2)), ("m", J::N(3))]);
        assert_eq!(v.to_string(), r#"{"z":1,"a":2,"m":3}"#);
    }

    #[test]
    fn array_of_objects() {
        let v = J::A(vec![
            J::obj(vec![("k", J::s("v"))]),
            J::obj(vec![("k", J::s("w"))]),
        ]);
        assert_eq!(v.to_string(), r#"[{"k":"v"},{"k":"w"}]"#);
    }
}
