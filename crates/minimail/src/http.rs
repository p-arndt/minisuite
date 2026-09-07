// Minimal HTTP/1.1 primitives. Just enough to serve the JSON API + web UI.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;

pub const MAX_HEADER_BYTES: usize = 64 * 1024;
pub const MAX_LINE_BYTES: usize = 16 * 1024;

pub struct Request<R: BufRead = BufReader<TcpStream>> {
    pub method: String,
    pub raw_path: String,  // /messages/{id} (still percent-encoded)
    pub path: String,      // percent-decoded path
    pub query_raw: String, // a=1&b=2 (still encoded)
    pub headers: Headers,
    #[allow(dead_code)] // frozen SPEC §5 field; API requests carry no body, so never re-read
    pub reader: R,
}

#[derive(Default, Clone)]
pub struct Headers {
    // canonical lowercase name -> original-cased name + value
    pub map: HashMap<String, (String, String)>,
    pub order: Vec<String>,
}

impl Headers {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.map
            .get(&name.to_ascii_lowercase())
            .map(|(_, v)| v.as_str())
    }
    pub fn insert(&mut self, name: &str, value: &str) {
        let lc = name.to_ascii_lowercase();
        if !self.map.contains_key(&lc) {
            self.order.push(lc.clone());
        }
        self.map.insert(lc, (name.to_string(), value.to_string()));
    }
}

/// Generalized over any Read (so a TLS Stream parses via the same path).
pub fn read_request<S: Read>(stream: S) -> io::Result<Request<BufReader<S>>> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let n = read_line_limited(&mut reader, &mut line, MAX_LINE_BYTES)?;
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "empty"));
    }
    let line = line.trim_end_matches(['\r', '\n']).to_string();
    let mut it = line.splitn(3, ' ');
    let method = it.next().unwrap_or("").to_string();
    let target = it.next().unwrap_or("").to_string();
    let _version = it.next().unwrap_or("HTTP/1.1").to_string();

    let (raw_path, query_raw) = match target.find('?') {
        Some(i) => (target[..i].to_string(), target[i + 1..].to_string()),
        None => (target.clone(), String::new()),
    };
    let path = crate::url::percent_decode_str(&raw_path);

    let mut headers = Headers::default();
    let mut total = 0usize;
    loop {
        let mut hl = String::new();
        let nr = read_line_limited(&mut reader, &mut hl, MAX_LINE_BYTES)?;
        if nr == 0 {
            break;
        }
        total += nr;
        if total > MAX_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "headers too large",
            ));
        }
        let trimmed = hl.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(c) = trimmed.find(':') {
            let name = trimmed[..c].trim();
            let value = trimmed[c + 1..].trim();
            headers.insert(name, value);
        }
    }

    Ok(Request {
        method,
        raw_path,
        path,
        query_raw,
        headers,
        reader,
    })
}

fn read_line_limited<R: BufRead>(r: &mut R, out: &mut String, limit: usize) -> io::Result<usize> {
    let mut total = 0;
    loop {
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            return Ok(total);
        }
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let s = std::str::from_utf8(&buf[..=pos])
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 header"))?;
            out.push_str(s);
            total += pos + 1;
            r.consume(pos + 1);
            return Ok(total);
        } else {
            let s = std::str::from_utf8(buf)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 header"))?;
            out.push_str(s);
            total += buf.len();
            let consumed = buf.len();
            r.consume(consumed);
            if total > limit {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
            }
        }
    }
}

// --- Body readers ---

// Standard fixed-length body
#[allow(dead_code)] // frozen SPEC §5 type; no request-body path is wired in this binary
pub struct FixedReader<'a, R: BufRead> {
    pub r: &'a mut R,
    pub remaining: u64,
}
impl<'a, R: BufRead> Read for FixedReader<'a, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let cap = (self.remaining.min(out.len() as u64)) as usize;
        let n = self.r.read(&mut out[..cap])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

// --- Response writer ---

pub struct Response {
    pub status: u16,
    pub status_text: &'static str,
    pub headers: Vec<(String, String)>,
}

impl Response {
    pub fn new(status: u16) -> Self {
        let text = status_text(status);
        Self {
            status,
            status_text: text,
            headers: Vec::new(),
        }
    }
    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }
    /// NO Content-Type default (handlers set it). Defaults Connection: close,
    /// Date, Server: minimail/<ver>. Content-Length from the body_len hint.
    pub fn write_headers<W: Write>(&self, w: &mut W, body_len: Option<u64>) -> io::Result<()> {
        write!(w, "HTTP/1.1 {} {}\r\n", self.status, self.status_text)?;
        let mut have_len = false;
        let mut have_conn = false;
        let mut have_date = false;
        let mut have_server = false;
        for (k, v) in &self.headers {
            let lk = k.to_ascii_lowercase();
            if lk == "content-length" {
                have_len = true;
            }
            if lk == "connection" {
                have_conn = true;
            }
            if lk == "date" {
                have_date = true;
            }
            if lk == "server" {
                have_server = true;
            }
            write!(w, "{}: {}\r\n", k, v)?;
        }
        if !have_len {
            if let Some(l) = body_len {
                write!(w, "Content-Length: {}\r\n", l)?;
            }
        }
        if !have_conn {
            write!(w, "Connection: close\r\n")?;
        }
        if !have_date {
            write!(w, "Date: {}\r\n", crate::util::http_date_now())?;
        }
        if !have_server {
            write!(w, "Server: minimail/{}\r\n", env!("CARGO_PKG_VERSION"))?;
        }
        write!(w, "\r\n")?;
        Ok(())
    }
}

// A response body. Bytes are buffered (small JSON); Stream defers reading until
// write_to so handlers can return a file/network reader without slurping it.
#[allow(dead_code)] // Stream/Empty are for handlers that stream or send no body
pub enum Body {
    Empty,
    Bytes(Vec<u8>),
    Stream(Box<dyn Read + Send>),
}

impl Body {
    pub fn len_hint(&self) -> Option<u64> {
        match self {
            Body::Empty => Some(0),
            Body::Bytes(v) => Some(v.len() as u64),
            Body::Stream(_) => None,
        }
    }
    #[allow(dead_code)] // frozen SPEC §5 Body API; responses stream, never buffered here
    pub fn into_bytes(self) -> io::Result<Vec<u8>> {
        match self {
            Body::Empty => Ok(Vec::new()),
            Body::Bytes(v) => Ok(v),
            Body::Stream(mut r) => {
                let mut out = Vec::new();
                r.read_to_end(&mut out)?;
                Ok(out)
            }
        }
    }
}

// A value-form HTTP response. Handlers build one of these and can either
// hand it to write_to (production) or assert on its parts (tests).
pub struct BuiltResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

impl BuiltResponse {
    pub fn new(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Body::Empty,
        }
    }
    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }
    #[allow(dead_code)] // public API for handlers that want Stream/Empty bodies directly
    pub fn body(mut self, body: Body) -> Self {
        self.body = body;
        self
    }
    pub fn json(mut self, body: String) -> Self {
        // Mark as JSON if not already set.
        let has_ct = self
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-type"));
        if !has_ct {
            self.headers.push((
                "Content-Type".into(),
                "application/json; charset=utf-8".into(),
            ));
        }
        self.body = Body::Bytes(body.into_bytes());
        self
    }
    pub fn write_to<W: Write>(self, w: &mut W) -> io::Result<()> {
        let len_hint = self.body.len_hint();
        let resp = Response {
            status: self.status,
            status_text: status_text(self.status),
            headers: self.headers,
        };
        resp.write_headers(w, len_hint)?;
        match self.body {
            Body::Empty => {}
            Body::Bytes(v) => w.write_all(&v)?,
            Body::Stream(mut r) => {
                let mut buf = [0u8; 64 * 1024];
                loop {
                    let n = r.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    w.write_all(&buf[..n])?;
                }
            }
        }
        Ok(())
    }

    #[allow(dead_code)] // frozen SPEC §5 BuiltResponse API; exercised only by unit tests
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        412 => "Precondition Failed",
        416 => "Range Not Satisfiable",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

/// Inclusive (start, end) clamped to `size`. Accepts "bytes=S-E", "bytes=S-",
/// and suffix "bytes=-N". None if unsatisfiable.
pub fn parse_range(v: &str, size: u64) -> Option<(u64, u64)> {
    let spec = v.trim().strip_prefix("bytes=")?.trim();
    // Only a single range is supported; a comma-list is treated as unsatisfiable.
    if spec.contains(',') {
        return None;
    }
    let (s, e) = spec.split_once('-')?;
    let (s, e) = (s.trim(), e.trim());
    if size == 0 {
        return None;
    }
    if s.is_empty() {
        // Suffix form: the last N bytes.
        let n: u64 = e.parse().ok()?;
        if n == 0 {
            return None;
        }
        let n = n.min(size);
        return Some((size - n, size - 1));
    }
    let start: u64 = s.parse().ok()?;
    if start >= size {
        return None; // start past EOF -> unsatisfiable
    }
    let end = if e.is_empty() {
        size - 1
    } else {
        e.parse::<u64>().ok()?.min(size - 1)
    };
    if end < start {
        return None;
    }
    Some((start, end))
}

/// Extension -> Content-Type map; default application/octet-stream.
#[allow(dead_code)] // frozen SPEC §5 helper; assets serve their own stored content_type
pub fn mime_type_for(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    let ext = lower.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "txt" | "text" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "eml" => "message/rfc822",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "zip" => "application/zip",
        "csv" => "text/csv; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_lookup_is_case_insensitive() {
        let mut h = Headers::default();
        h.insert("Content-Type", "text/plain");
        assert_eq!(h.get("content-type"), Some("text/plain"));
        assert_eq!(h.get("CONTENT-TYPE"), Some("text/plain"));
        assert_eq!(h.get("Content-Type"), Some("text/plain"));
        assert_eq!(h.get("missing"), None);
    }

    #[test]
    fn headers_insert_preserves_first_order_and_overwrites_value() {
        let mut h = Headers::default();
        h.insert("Host", "example");
        h.insert("X-Amz-Date", "20240101T000000Z");
        h.insert("host", "other"); // same key, different case: should overwrite
        assert_eq!(h.get("host"), Some("other"));
        assert_eq!(h.order, vec!["host", "x-amz-date"]);
    }

    #[test]
    fn parse_range_start_end() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=100-199", 1000), Some((100, 199)));
        // end past EOF clamps to size-1
        assert_eq!(parse_range("bytes=990-2000", 1000), Some((990, 999)));
    }

    #[test]
    fn parse_range_open_ended() {
        assert_eq!(parse_range("bytes=500-", 1000), Some((500, 999)));
        assert_eq!(parse_range("bytes=0-", 1000), Some((0, 999)));
    }

    #[test]
    fn parse_range_suffix() {
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        // suffix larger than size clamps to whole
        assert_eq!(parse_range("bytes=-5000", 1000), Some((0, 999)));
    }

    #[test]
    fn parse_range_unsatisfiable() {
        assert_eq!(parse_range("bytes=1000-1100", 1000), None); // start past EOF
        assert_eq!(parse_range("bytes=-0", 1000), None); // zero-length suffix
        assert_eq!(parse_range("bytes=0-99", 0), None); // empty resource
        assert_eq!(parse_range("items=0-99", 1000), None); // wrong unit
        assert_eq!(parse_range("bytes=abc", 1000), None); // garbage
        assert_eq!(parse_range("bytes=0-99,200-299", 1000), None); // multi-range
    }

    #[test]
    fn status_text_mapping() {
        assert_eq!(status_text(200), "OK");
        assert_eq!(status_text(204), "No Content");
        assert_eq!(status_text(206), "Partial Content");
        assert_eq!(status_text(304), "Not Modified");
        assert_eq!(status_text(400), "Bad Request");
        assert_eq!(status_text(401), "Unauthorized");
        assert_eq!(status_text(404), "Not Found");
        assert_eq!(status_text(405), "Method Not Allowed");
        assert_eq!(status_text(416), "Range Not Satisfiable");
        assert_eq!(status_text(500), "Internal Server Error");
        assert_eq!(status_text(999), "OK"); // unknown -> OK
    }

    #[test]
    fn mime_type_for_extensions() {
        assert_eq!(mime_type_for("/app.css"), "text/css; charset=utf-8");
        assert_eq!(mime_type_for("index.html"), "text/html; charset=utf-8");
        assert_eq!(mime_type_for("app.JS"), "text/javascript; charset=utf-8");
        assert_eq!(mime_type_for("favicon.svg"), "image/svg+xml");
        assert_eq!(mime_type_for("noext"), "application/octet-stream");
        assert_eq!(mime_type_for("cat.png"), "image/png");
    }
}
