// Minimal TOML (v1.0.0 subset): table headers, strings, bools, integers and
// arrays -- enough to describe users and clients, and nothing more. Pure std.
//
// Anything outside the subset (inline tables, arrays of tables, floats, dates,
// multi-line strings) is a hard error with a line number rather than a silent
// skip: a config file that half-parses is worse than one that refuses to load.

use std::fmt;
use std::io;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Str(String),
    Bool(bool),
    Int(i64),
    Arr(Vec<Value>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The name used in "expected X, found Y" diagnostics.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Str(_) => "a string",
            Value::Bool(_) => "a boolean",
            Value::Int(_) => "an integer",
            Value::Arr(_) => "an array",
        }
    }
}

/// One `[a.b]` table and the key/value pairs beneath it, in file order.
///
/// Deliberately a flat list rather than a tree: every caller here wants to walk
/// tables in the order they were written (so `Users::order` stays meaningful)
/// and match on a two-segment path.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    pub path: Vec<String>,
    pub line: usize,
    pub entries: Vec<Entry>,
}

impl Table {
    /// The header as it would be written, for error messages.
    pub fn name(&self) -> String {
        self.path.join(".")
    }

    pub fn get(&self, key: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.key == key)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub key: String,
    pub value: Value,
    pub line: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Error {
    pub line: usize,
    pub msg: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.msg)
    }
}

impl std::error::Error for Error {}

impl From<Error> for io::Error {
    fn from(e: Error) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, e.to_string())
    }
}

type Result<T> = std::result::Result<T, Error>;

pub fn parse(text: &str) -> Result<Vec<Table>> {
    let mut p = P {
        c: text.chars().collect(),
        i: 0,
        line: 1,
    };
    let mut tables: Vec<Table> = Vec::new();
    loop {
        p.trivia(true);
        match p.peek() {
            None => break,
            Some('[') => {
                if p.peek_at(1) == Some('[') {
                    return p.err("arrays of tables ('[[x]]') are not supported");
                }
                let line = p.line;
                p.bump();
                let path = p.key_path()?;
                p.trivia(false);
                // peek, then bump: bumping a '\n' first would blame the next line.
                if p.peek() != Some(']') {
                    return p.err("expected ']' to close the table header");
                }
                p.bump();
                p.end_of_line()?;
                if let Some(prev) = tables.iter().find(|t| t.path == path) {
                    return Err(Error {
                        line,
                        msg: format!(
                            "table [{}] was already defined on line {}",
                            path.join("."),
                            prev.line
                        ),
                    });
                }
                tables.push(Table {
                    path,
                    line,
                    entries: Vec::new(),
                });
            }
            Some(_) => {
                let line = p.line;
                let key = p.key()?;
                p.trivia(false);
                if p.peek() == Some('.') {
                    return p.err(format!(
                        "dotted keys are not supported; write a [{}] table instead",
                        key
                    ));
                }
                if p.peek() != Some('=') {
                    return p.err(format!("expected '=' after the key '{}'", key));
                }
                p.bump();
                p.trivia(false);
                let value = p.value()?;
                p.end_of_line()?;
                let table = match tables.last_mut() {
                    Some(t) => t,
                    // Every key in this format belongs to a user or a client, so a
                    // key before the first header is always a mistake.
                    None => {
                        return Err(Error {
                            line,
                            msg: format!(
                                "'{}' sits above any table header; put it under [users.<name>] or [clients.<id>]",
                                key
                            ),
                        })
                    }
                };
                if let Some(prev) = table.get(&key) {
                    return Err(Error {
                        line,
                        msg: format!(
                            "key '{}' was already set in [{}] on line {}",
                            key,
                            table.name(),
                            prev.line
                        ),
                    });
                }
                table.entries.push(Entry { key, value, line });
            }
        }
    }
    Ok(tables)
}

struct P {
    c: Vec<char>,
    i: usize,
    line: usize,
}

impl P {
    fn peek(&self) -> Option<char> {
        self.c.get(self.i).copied()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.c.get(self.i + n).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.i += 1;
        if ch == '\n' {
            self.line += 1;
        }
        Some(ch)
    }

    fn err<T>(&self, msg: impl Into<String>) -> Result<T> {
        Err(Error {
            line: self.line,
            msg: msg.into(),
        })
    }

    /// Spaces, tabs, carriage returns and comments. With `newlines`, also line
    /// breaks -- true between tables and inside arrays, false within one line.
    fn trivia(&mut self, newlines: bool) {
        loop {
            match self.peek() {
                Some(' ') | Some('\t') | Some('\r') => {
                    self.bump();
                }
                Some('\n') if newlines => {
                    self.bump();
                }
                Some('#') => {
                    while !matches!(self.peek(), None | Some('\n')) {
                        self.bump();
                    }
                }
                _ => return,
            }
        }
    }

    /// Consume the rest of the line, allowing only a trailing comment.
    fn end_of_line(&mut self) -> Result<()> {
        self.trivia(false);
        match self.peek() {
            None => Ok(()),
            Some('\n') => {
                self.bump();
                Ok(())
            }
            Some(c) => self.err(format!("unexpected '{}' at the end of the line", c)),
        }
    }

    fn key_path(&mut self) -> Result<Vec<String>> {
        let mut path = Vec::new();
        loop {
            self.trivia(false);
            path.push(self.key()?);
            self.trivia(false);
            if self.peek() == Some('.') {
                self.bump();
            } else {
                return Ok(path);
            }
        }
    }

    /// A bare key, or a quoted one -- `[users."ada@example.com"]` is how a name
    /// containing a '.' or a space gets written.
    fn key(&mut self) -> Result<String> {
        match self.peek() {
            Some('"') => self.basic_string(),
            Some('\'') => self.literal_string(),
            _ => {
                let start = self.i;
                while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    self.bump();
                }
                if self.i == start {
                    return match self.peek() {
                        None => self.err("expected a key, found the end of the file"),
                        Some(c) => self.err(format!(
                            "expected a key, found '{}'; quote it if it is part of a name",
                            c
                        )),
                    };
                }
                Ok(self.c[start..self.i].iter().collect())
            }
        }
    }

    fn value(&mut self) -> Result<Value> {
        match self.peek() {
            Some('"') => self.basic_string().map(Value::Str),
            Some('\'') => self.literal_string().map(Value::Str),
            Some('[') => self.array(),
            Some('{') => self.err("inline tables ('{ ... }') are not supported"),
            None => self.err("expected a value, found the end of the file"),
            _ => self.bare_value(),
        }
    }

    fn array(&mut self) -> Result<Value> {
        self.bump(); // '['
        let mut items = Vec::new();
        loop {
            self.trivia(true);
            match self.peek() {
                None => return self.err("unterminated array: expected ']'"),
                Some(']') => {
                    self.bump();
                    return Ok(Value::Arr(items));
                }
                _ => {}
            }
            items.push(self.value()?);
            self.trivia(true);
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                // A trailing comma is legal, so the ']' arm above also handles it.
                Some(']') => {
                    self.bump();
                    return Ok(Value::Arr(items));
                }
                None => return self.err("unterminated array: expected ']'"),
                Some(c) => return self.err(format!("expected ',' or ']' in array, found '{}'", c)),
            }
        }
    }

    /// `true`, `false`, or an integer. Everything else in this position is a
    /// TOML type we do not implement, so name it rather than say "invalid".
    fn bare_value(&mut self) -> Result<Value> {
        let line = self.line;
        let start = self.i;
        while !matches!(
            self.peek(),
            None | Some(',') | Some(']') | Some('\n') | Some('#')
        ) {
            self.bump();
        }
        let raw: String = self.c[start..self.i].iter().collect();
        let raw = raw.trim();
        let fail = |msg: String| Err(Error { line, msg });

        match raw {
            "true" => return Ok(Value::Bool(true)),
            "false" => return Ok(Value::Bool(false)),
            "" => return fail("expected a value".to_string()),
            _ => {}
        }
        let generic = || {
            fail(format!(
                "{:?}: expected a quoted string, true, false, or an integer",
                raw
            ))
        };
        // Only a token that opens like a number gets a number-shaped diagnosis;
        // otherwise `bare` would be read as an exponent because it contains an 'e'.
        if !raw.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '+') {
            return generic();
        }
        if raw.contains('.') || raw.contains('e') || raw.contains('E') {
            return fail(format!("{:?}: floats are not supported", raw));
        }
        if raw.starts_with("0x") || raw.starts_with("0o") || raw.starts_with("0b") {
            return fail(format!("{:?}: only decimal integers are supported", raw));
        }
        // TOML allows '_' as a digit separator and a leading '+'.
        let digits = raw.replace('_', "");
        let digits = digits.strip_prefix('+').unwrap_or(&digits);
        match digits.parse::<i64>() {
            Ok(n) => Ok(Value::Int(n)),
            Err(_) => generic(),
        }
    }

    fn basic_string(&mut self) -> Result<String> {
        // The opening quote's line, not wherever we ran off the end: an
        // unterminated string is a mistake on the line it starts on.
        let line = self.line;
        self.bump(); // opening '"'
        if self.peek() == Some('"') && self.peek_at(1) == Some('"') {
            return self.err("multi-line strings (\"\"\") are not supported");
        }
        let mut out = String::new();
        loop {
            match self.bump() {
                None | Some('\n') => return Err(unterminated(line, '"')),
                Some('"') => return Ok(out),
                Some('\\') => out.push(self.escape()?),
                Some(c) => out.push(c),
            }
        }
    }

    fn escape(&mut self) -> Result<char> {
        match self.bump() {
            Some('"') => Ok('"'),
            Some('\\') => Ok('\\'),
            Some('n') => Ok('\n'),
            Some('r') => Ok('\r'),
            Some('t') => Ok('\t'),
            Some('b') => Ok('\u{08}'),
            Some('f') => Ok('\u{0c}'),
            Some('u') => self.unicode_escape(4),
            Some('U') => self.unicode_escape(8),
            None => Err(unterminated(self.line, '"')),
            Some(c) => self.err(format!("unknown escape '\\{}'", c)),
        }
    }

    fn unicode_escape(&mut self, digits: usize) -> Result<char> {
        let start = self.i;
        for _ in 0..digits {
            match self.peek() {
                Some(c) if c.is_ascii_hexdigit() => {
                    self.bump();
                }
                _ => return self.err(format!("\\u escape needs {} hex digits", digits)),
            }
        }
        let hex: String = self.c[start..self.i].iter().collect();
        // The digit count is fixed, so this only overflows for \U above 0x10FFFF,
        // which char::from_u32 rejects anyway.
        let code = u32::from_str_radix(&hex, 16).unwrap_or(0x11_0000);
        char::from_u32(code).ok_or_else(|| Error {
            line: self.line,
            msg: format!("\\u{} is not a character", hex),
        })
    }

    /// `'...'`: no escapes, so a Windows path or a password full of backslashes
    /// can be written literally.
    fn literal_string(&mut self) -> Result<String> {
        let line = self.line;
        self.bump(); // opening '\''
        if self.peek() == Some('\'') && self.peek_at(1) == Some('\'') {
            return self.err("multi-line strings (''') are not supported");
        }
        let mut out = String::new();
        loop {
            match self.bump() {
                None | Some('\n') => return Err(unterminated(line, '\'')),
                Some('\'') => return Ok(out),
                Some(c) => out.push(c),
            }
        }
    }
}

fn unterminated(line: usize, quote: char) -> Error {
    Error {
        line,
        msg: format!("unterminated string: expected a closing {}", quote),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(text: &str) -> Table {
        let mut t = parse(text).unwrap();
        assert_eq!(t.len(), 1);
        t.pop().unwrap()
    }

    fn err(text: &str) -> Error {
        parse(text).unwrap_err()
    }

    fn arr(items: &[&str]) -> Value {
        Value::Arr(items.iter().map(|s| Value::Str((*s).into())).collect())
    }

    #[test]
    fn tables_keep_file_order() {
        let ts = parse("[users.bob]\n[users.alice]\n[clients.spa]\n").unwrap();
        let names: Vec<String> = ts.iter().map(Table::name).collect();
        assert_eq!(names, ["users.bob", "users.alice", "clients.spa"]);
        assert_eq!(ts[0].path, ["users", "bob"]);
    }

    #[test]
    fn values_of_every_supported_type() {
        let t = one("[x]\ns = \"hi\"\nb = true\nn = 42\na = [\"p\", \"q\"]\n");
        assert_eq!(t.get("s").unwrap().value, Value::Str("hi".into()));
        assert_eq!(t.get("b").unwrap().value, Value::Bool(true));
        assert_eq!(t.get("n").unwrap().value, Value::Int(42));
        assert_eq!(t.get("a").unwrap().value, arr(&["p", "q"]));
        assert_eq!(t.get("nope"), None);
    }

    #[test]
    fn arrays_span_lines_and_allow_trailing_commas_and_comments() {
        let t = one("[x]\nuris = [\n  \"a\", # first\n  \"b\",\n]\n");
        assert_eq!(t.get("uris").unwrap().value, arr(&["a", "b"]));
        assert_eq!(
            one("[x]\na = []\n").get("a").unwrap().value,
            Value::Arr(vec![])
        );
        // Mixed element types parse; rejecting them is the schema's job, not the parser's.
        assert_eq!(
            one("[x]\na = [\"p\", 3]\n").get("a").unwrap().value,
            Value::Arr(vec![Value::Str("p".into()), Value::Int(3)])
        );
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let t = one("# top\n\n[x] # header\n\n  k = \"v\"  # trailing\n\n");
        assert_eq!(t.get("k").unwrap().value.as_str(), Some("v"));
    }

    #[test]
    fn hash_inside_a_string_is_not_a_comment() {
        let t = one("[x]\npw = \"pass#word\"\n");
        assert_eq!(t.get("pw").unwrap().value.as_str(), Some("pass#word"));
    }

    #[test]
    fn quoted_keys_carry_dots_and_spaces() {
        let ts = parse("[users.\"ada@example.com\"]\nk = \"v\"\n[users.'a.b']\n").unwrap();
        assert_eq!(ts[0].path, ["users", "ada@example.com"]);
        assert_eq!(ts[1].path, ["users", "a.b"]);
    }

    #[test]
    fn basic_string_escapes() {
        let t = one("[x]\na = \"q\\\"\\\\\\n\\t\\u0041\\U0001F600\"\n");
        assert_eq!(
            t.get("a").unwrap().value.as_str(),
            Some("q\"\\\n\tA\u{1F600}")
        );
    }

    #[test]
    fn literal_strings_take_backslashes_verbatim() {
        let t = one("[x]\np = 'C:\\Users\\n'\n");
        assert_eq!(t.get("p").unwrap().value.as_str(), Some("C:\\Users\\n"));
    }

    #[test]
    fn integers_take_signs_and_separators() {
        let t = one("[x]\na = -7\nb = +1_000\n");
        assert_eq!(t.get("a").unwrap().value, Value::Int(-7));
        assert_eq!(t.get("b").unwrap().value, Value::Int(1000));
    }

    #[test]
    fn entries_carry_their_own_line_numbers() {
        let t = one("[x]\n\n\nk = \"v\"\n");
        assert_eq!(t.line, 1);
        assert_eq!(t.get("k").unwrap().line, 4);
    }

    #[test]
    fn duplicate_table_and_duplicate_key_are_refused() {
        assert!(err("[a]\n[a]\n").msg.contains("already defined on line 1"));
        assert!(err("[a]\nk = \"1\"\nk = \"2\"\n")
            .msg
            .contains("already set"));
        // Same leaf name under different parents is fine.
        assert!(parse("[users.a]\n[clients.a]\n").is_ok());
    }

    #[test]
    fn a_key_above_the_first_header_is_refused() {
        let e = err("k = \"v\"\n[x]\n");
        assert_eq!(e.line, 1);
        assert!(e.msg.contains("above any table header"));
    }

    #[test]
    fn unsupported_toml_is_named_not_just_rejected() {
        assert!(err("[[a]]\n").msg.contains("arrays of tables"));
        assert!(err("[x]\na = { b = 1 }\n").msg.contains("inline tables"));
        assert!(err("[x]\na = 1.5\n").msg.contains("floats"));
        assert!(err("[x]\na = 0xff\n").msg.contains("decimal integers"));
        assert!(err("[x]\na = \"\"\"hi\"\"\"\n").msg.contains("multi-line"));
        assert!(err("[x]\na = '''hi'''\n").msg.contains("multi-line"));
        assert!(err("[x]\na.b = 1\n").msg.contains("dotted keys"));
    }

    #[test]
    fn malformed_input_reports_the_offending_line() {
        assert_eq!(err("[a]\n\nk = \n").line, 3);
        assert_eq!(err("[a]\nk \"v\"\n").line, 2);
        assert_eq!(err("[a\n").line, 1);
        assert_eq!(err("[a]\nk = \"oops\n").line, 2);
        assert_eq!(err("[a]\nk = 'oops\n").line, 2);
        assert_eq!(err("[a]\nk = [\"a\"\n").line, 3);
        assert_eq!(err("[a]\nk = \"v\" junk\n").line, 2);
        assert!(err("[a]\nk = bare\n")
            .msg
            .contains("expected a quoted string"));
        assert!(err("[a]\nk = \"\\q\"\n").msg.contains("unknown escape"));
        assert!(err("[a]\nk = \"\\u00\"\n").msg.contains("4 hex digits"));
    }

    #[test]
    fn a_bare_word_is_not_diagnosed_as_a_float() {
        // "bare" contains an 'e', which a naive exponent check reads as a float.
        for word in ["bare", "enabled", "None", "yes"] {
            let msg = err(&format!("[a]\nk = {}\n", word)).msg;
            assert!(
                msg.contains("expected a quoted string"),
                "{}: {}",
                word,
                msg
            );
        }
        assert!(err("[a]\nk = 1e3\n").msg.contains("floats"));
    }

    #[test]
    fn an_unterminated_construct_blames_the_line_it_opened_on() {
        // Not the line the scanner happened to stop on.
        assert_eq!(err("[a\n").line, 1);
        assert_eq!(err("[a]\nk = \"oops\nj = \"v\"\n").line, 2);
        assert_eq!(err("[a]\nk = 'oops\nj = \"v\"\n").line, 2);
        assert_eq!(err("[a]\nk\n= \"v\"\n").line, 2);
    }

    #[test]
    fn empty_input_yields_no_tables() {
        assert!(parse("").unwrap().is_empty());
        assert!(parse("\n# just a comment\n\n").unwrap().is_empty());
    }

    #[test]
    fn error_renders_with_its_line() {
        let e = Error {
            line: 7,
            msg: "boom".into(),
        };
        assert_eq!(e.to_string(), "line 7: boom");
        let io: io::Error = e.into();
        assert_eq!(io.kind(), io::ErrorKind::InvalidData);
    }
}
