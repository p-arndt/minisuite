// Minimal JSON (RFC 8259 subset): value + serializer + parser + escaper. Pure std.

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64), // exact integers for sizes/timestamps (no f64 rounding)
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    // Frozen public name per SPEC §5; callers use `.to_string()`. Not a Display shim.
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    pub fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Int(i) => out.push_str(&i.to_string()),
            Json::Num(n) => {
                // NaN/Infinity are not valid JSON; degrade to null.
                if n.is_finite() {
                    out.push_str(&n.to_string());
                } else {
                    out.push_str("null");
                }
            }
            Json::Str(s) => {
                out.push('"');
                escape(s, out);
                out.push('"');
            }
            Json::Arr(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write(out);
                }
                out.push(']');
            }
            Json::Obj(o) => {
                out.push('{');
                for (i, (k, v)) in o.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push('"');
                    escape(k, out);
                    out.push('"');
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(o) => o.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    #[allow(dead_code)] // frozen SPEC §5 json accessor; no consumer in this binary build
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Int(i) => Some(*i),
            Json::Num(n) => Some(*n as i64),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Int(i) if *i >= 0 => Some(*i as u64),
            Json::Num(n) if *n >= 0.0 => Some(*n as u64),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }
}

/// Append the JSON-escaped form of `s` (no surrounding quotes) to `out`:
/// \" \\ \n \r \t \b \f and control chars < 0x20 -> \u00XX. Non-ASCII is
/// emitted verbatim, so the result is valid UTF-8 for any Rust String.
pub fn escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
}

/// Permissive enough to round-trip our own sidecars. NOT a hardened parser for
/// untrusted large input.
pub fn parse(s: &str) -> Option<Json> {
    let mut p = Parser {
        chars: s.chars().collect(),
        pos: 0,
    };
    p.skip_ws();
    let v = p.value()?;
    p.skip_ws();
    // Only trailing whitespace may follow the top-level value.
    if p.pos == p.chars.len() {
        Some(v)
    } else {
        None
    }
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.chars.get(self.pos).copied();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
            self.pos += 1;
        }
    }

    fn value(&mut self) -> Option<Json> {
        match self.peek()? {
            '"' => self.string().map(Json::Str),
            '{' => self.object(),
            '[' => self.array(),
            't' | 'f' => self.boolean(),
            'n' => self.null(),
            _ => self.number(),
        }
    }

    fn literal(&mut self, word: &str) -> bool {
        if self.chars[self.pos..].starts_with(&word.chars().collect::<Vec<_>>()[..]) {
            self.pos += word.chars().count();
            true
        } else {
            false
        }
    }

    fn null(&mut self) -> Option<Json> {
        self.literal("null").then_some(Json::Null)
    }

    fn boolean(&mut self) -> Option<Json> {
        if self.literal("true") {
            Some(Json::Bool(true))
        } else if self.literal("false") {
            Some(Json::Bool(false))
        } else {
            None
        }
    }

    fn string(&mut self) -> Option<String> {
        if self.next()? != '"' {
            return None;
        }
        let mut out = String::new();
        loop {
            match self.next()? {
                '"' => return Some(out),
                '\\' => match self.next()? {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    '/' => out.push('/'),
                    'b' => out.push('\u{08}'),
                    'f' => out.push('\u{0c}'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'u' => self.unicode_escape(&mut out)?,
                    _ => return None,
                },
                c => out.push(c),
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..4 {
            v = v * 16 + self.next()?.to_digit(16)?;
        }
        Some(v)
    }

    fn unicode_escape(&mut self, out: &mut String) -> Option<()> {
        let hi = self.hex4()?;
        if (0xD800..=0xDBFF).contains(&hi) {
            // High surrogate: pair it with a following \uXXXX low surrogate.
            if self.peek() == Some('\\') && self.chars.get(self.pos + 1) == Some(&'u') {
                self.pos += 2;
                let lo = self.hex4()?;
                if (0xDC00..=0xDFFF).contains(&lo) {
                    let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                    out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                } else {
                    out.push('\u{FFFD}');
                    out.push(char::from_u32(lo).unwrap_or('\u{FFFD}'));
                }
            } else {
                out.push('\u{FFFD}');
            }
        } else if (0xDC00..=0xDFFF).contains(&hi) {
            out.push('\u{FFFD}'); // lone low surrogate
        } else {
            out.push(char::from_u32(hi).unwrap_or('\u{FFFD}'));
        }
        Some(())
    }

    fn number(&mut self) -> Option<Json> {
        let start = self.pos;
        while matches!(self.peek(), Some('0'..='9' | '-' | '+' | '.' | 'e' | 'E')) {
            self.pos += 1;
        }
        if self.pos == start {
            return None;
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        // Prefer an exact integer; fall back to f64 for fractional/exponent/huge.
        if !text.contains(['.', 'e', 'E']) {
            if let Ok(i) = text.parse::<i64>() {
                return Some(Json::Int(i));
            }
        }
        text.parse::<f64>().ok().map(Json::Num)
    }

    fn array(&mut self) -> Option<Json> {
        self.next(); // '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(']') {
            self.pos += 1;
            return Some(Json::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value()?);
            self.skip_ws();
            match self.next()? {
                ',' => continue,
                ']' => return Some(Json::Arr(items)),
                _ => return None,
            }
        }
    }

    fn object(&mut self) -> Option<Json> {
        self.next(); // '{'
        let mut fields = Vec::new();
        self.skip_ws();
        if self.peek() == Some('}') {
            self.pos += 1;
            return Some(Json::Obj(fields));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            if self.next()? != ':' {
                return None;
            }
            self.skip_ws();
            let val = self.value()?;
            fields.push((key, val));
            self.skip_ws();
            match self.next()? {
                ',' => continue,
                '}' => return Some(Json::Obj(fields)),
                _ => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_control_quote_backslash() {
        let mut out = String::new();
        escape("a\"b\\c\nd\re\tf\u{08}g\u{0c}h\u{01}i", &mut out);
        assert_eq!(out, "a\\\"b\\\\c\\nd\\re\\tf\\bg\\fh\\u0001i");
    }

    #[test]
    fn escape_all_low_control_chars_use_u00xx() {
        // Every C0 control except the named ones renders as \u00XX, lowercase.
        for cp in 0u32..0x20 {
            if matches!(cp, 0x08 | 0x09 | 0x0a | 0x0c | 0x0d) {
                continue; // named: \b \t \n \f \r
            }
            let s: String = char::from_u32(cp).unwrap().to_string();
            let mut out = String::new();
            escape(&s, &mut out);
            assert_eq!(out, format!("\\u{:04x}", cp));
        }
    }

    #[test]
    fn escape_preserves_unicode_verbatim() {
        // Non-ASCII passes through as valid UTF-8, not \u-escaped.
        let mut out = String::new();
        escape("Hello ☀ — café 🦀", &mut out);
        assert_eq!(out, "Hello ☀ — café 🦀");
        let s = Json::Str("☀🦀".into()).to_string();
        assert_eq!(s, "\"☀🦀\"");
        assert!(s.is_char_boundary(s.len()));
    }

    #[test]
    fn number_formatting() {
        assert_eq!(Json::Int(0).to_string(), "0");
        assert_eq!(Json::Int(-7).to_string(), "-7");
        assert_eq!(Json::Int(1_720_000_000).to_string(), "1720000000");
        assert_eq!(Json::Int(i64::MAX).to_string(), "9223372036854775807");
        assert_eq!(Json::Num(1.5).to_string(), "1.5");
        assert_eq!(Json::Num(-0.25).to_string(), "-0.25");
        // Non-finite is not valid JSON -> null.
        assert_eq!(Json::Num(f64::NAN).to_string(), "null");
        assert_eq!(Json::Num(f64::INFINITY).to_string(), "null");
    }

    #[test]
    fn nested_structure_serialization() {
        let v = Json::Obj(vec![
            ("id".into(), Json::Str("a\"b".into())),
            ("size".into(), Json::Int(20480)),
            ("ok".into(), Json::Bool(true)),
            ("nil".into(), Json::Null),
            (
                "to".into(),
                Json::Arr(vec![
                    Json::Str("bob@example.com".into()),
                    Json::Str("eve@example.com".into()),
                ]),
            ),
            (
                "att".into(),
                Json::Arr(vec![Json::Obj(vec![
                    ("part_id".into(), Json::Str("2".into())),
                    ("size".into(), Json::Int(10)),
                ])]),
            ),
        ]);
        assert_eq!(
            v.to_string(),
            r#"{"id":"a\"b","size":20480,"ok":true,"nil":null,"to":["bob@example.com","eve@example.com"],"att":[{"part_id":"2","size":10}]}"#
        );
    }

    #[test]
    fn parse_scalars_and_accessors() {
        assert_eq!(parse("null"), Some(Json::Null));
        assert_eq!(parse("true").and_then(|j| j.as_bool()), Some(true));
        assert_eq!(parse("false").and_then(|j| j.as_bool()), Some(false));
        assert_eq!(parse("42").and_then(|j| j.as_i64()), Some(42));
        assert_eq!(parse("-5").and_then(|j| j.as_i64()), Some(-5));
        assert_eq!(parse("42").and_then(|j| j.as_u64()), Some(42));
        assert_eq!(parse("-5").and_then(|j| j.as_u64()), None);
        assert_eq!(parse("3.5").and_then(|j| j.as_i64()), Some(3));
        assert_eq!(
            parse(r#""hi""#).as_ref().and_then(|j| j.as_str()),
            Some("hi")
        );
    }

    #[test]
    fn parse_string_escapes_and_surrogates() {
        assert_eq!(
            parse(r#""a\"b\\c\n\t\/\b\f\r""#).unwrap(),
            Json::Str("a\"b\\c\n\t/\u{08}\u{0c}\r".into())
        );
        // BMP \u escape.
        assert_eq!(parse(r#""☀""#).unwrap(), Json::Str("☀".into()));
        // Surrogate pair for U+1F980 (crab).
        assert_eq!(parse(r#""🦀""#).unwrap(), Json::Str("🦀".into()));
        // Lone high surrogate degrades to the replacement char.
        assert_eq!(parse(r#""\ud83e""#).unwrap(), Json::Str("\u{FFFD}".into()));
    }

    #[test]
    fn parse_nested_and_whitespace() {
        let j = parse(" { \"a\" : [ 1 , 2 , { \"b\" : true } ] , \"c\" : null } ").unwrap();
        assert_eq!(
            j.get("a").and_then(|a| a.as_arr()).map(|a| a.len()),
            Some(3)
        );
        assert_eq!(
            j.get("a").unwrap().as_arr().unwrap()[2]
                .get("b")
                .and_then(|b| b.as_bool()),
            Some(true)
        );
        assert_eq!(parse("[]").unwrap(), Json::Arr(vec![]));
        assert_eq!(parse("{}").unwrap(), Json::Obj(vec![]));
    }

    #[test]
    fn parse_rejects_garbage() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("{"), None);
        assert_eq!(parse("[1,]"), None);
        assert_eq!(parse("truex"), None);
        assert_eq!(parse("1 2"), None); // trailing non-whitespace
        assert_eq!(parse(r#"{"a" 1}"#), None); // missing colon
    }

    #[test]
    fn parse_round_trip_preserves_serialization() {
        // A shape mirroring the sidecar Summary: control chars, unicode, nesting.
        let original = Json::Obj(vec![
            (
                "id".into(),
                Json::Str("0001720000000-000000042-0000001a".into()),
            ),
            ("received_unix".into(), Json::Int(1_720_000_000)),
            ("size".into(), Json::Int(20480)),
            ("from".into(), Json::Str("alice@example.com".into())),
            (
                "to".into(),
                Json::Arr(vec![Json::Str("bob@example.com".into())]),
            ),
            ("auth_user".into(), Json::Null),
            (
                "subject".into(),
                Json::Str("Hi ☀\t\"weird\"\u{01}\\end".into()),
            ),
            ("has_text".into(), Json::Bool(true)),
            ("has_html".into(), Json::Bool(false)),
            (
                "attachments".into(),
                Json::Arr(vec![Json::Obj(vec![
                    ("part_id".into(), Json::Str("2".into())),
                    ("filename".into(), Json::Str("cat 🐱.png".into())),
                    ("content_type".into(), Json::Str("image/png".into())),
                    ("size".into(), Json::Int(20480)),
                ])]),
            ),
        ]);
        let s1 = original.to_string();
        let reparsed = parse(&s1).expect("round-trip parse");
        // Structural equality survives the round trip...
        assert_eq!(reparsed, original);
        // ...and re-serializing is byte-identical.
        assert_eq!(reparsed.to_string(), s1);
    }
}
