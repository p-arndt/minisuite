// Minimal HTTP/1.1 server primitives. Just enough to serve the OIDC endpoints.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;

pub const MAX_HEADER_BYTES: usize = 64 * 1024;
pub const MAX_LINE_BYTES: usize = 16 * 1024;

pub struct Request<R: BufRead = BufReader<TcpStream>> {
    pub method: String,
    pub raw_path: String,  // /authorize (still percent-encoded)
    pub path: String,      // percent-decoded path
    pub query_raw: String, // a=1&b=2 (still encoded)
    pub headers: Headers,
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

pub fn read_request(stream: TcpStream) -> io::Result<Request<BufReader<TcpStream>>> {
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

pub fn read_line_limited<R: BufRead>(
    r: &mut R,
    out: &mut String,
    limit: usize,
) -> io::Result<usize> {
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

// Read exactly Content-Length bytes (0 if the header is absent). Errors if the
// declared length exceeds `max`.
pub fn read_body<R: BufRead>(req: &mut Request<R>, max: usize) -> io::Result<Vec<u8>> {
    let len = match req.headers.get("content-length") {
        Some(v) => v
            .trim()
            .parse::<usize>()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad content-length"))?,
        None => 0,
    };
    if len > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
    }
    let mut out = vec![0u8; len];
    if len > 0 {
        req.reader.read_exact(&mut out)?;
    }
    Ok(out)
}

// --- Response writer ---

// A response body. Bytes are buffered (all OIDC responses are small).
pub enum Body {
    Empty,
    Bytes(Vec<u8>),
}

impl Body {
    pub fn len(&self) -> usize {
        match self {
            Body::Empty => 0,
            Body::Bytes(v) => v.len(),
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
    fn set_content_type(&mut self, ct: &str) {
        let has_ct = self
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-type"));
        if !has_ct {
            self.headers.push(("Content-Type".into(), ct.into()));
        }
    }
    pub fn json(mut self, body: String) -> Self {
        self.set_content_type("application/json");
        self.body = Body::Bytes(body.into_bytes());
        self
    }
    pub fn html(mut self, body: String) -> Self {
        self.set_content_type("text/html; charset=utf-8");
        self.body = Body::Bytes(body.into_bytes());
        self
    }
    pub fn redirect(location: &str) -> Self {
        BuiltResponse::new(302).header("Location", location)
    }
    pub fn write_to<W: Write>(self, w: &mut W) -> io::Result<()> {
        let body_len = self.body.len();
        write!(
            w,
            "HTTP/1.1 {} {}\r\n",
            self.status,
            status_text(self.status)
        )?;
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
        // No default Content-Type: it is set only via json/html/text.
        if !have_len {
            write!(w, "Content-Length: {}\r\n", body_len)?;
        }
        if !have_conn {
            write!(w, "Connection: close\r\n")?;
        }
        if !have_date {
            write!(w, "Date: {}\r\n", crate::util::http_date_now())?;
        }
        if !have_server {
            // Taken from Cargo.toml, so `just release` cannot leave it stale.
            write!(w, "Server: minicloak/{}\r\n", env!("CARGO_PKG_VERSION"))?;
        }
        write!(w, "\r\n")?;
        match self.body {
            Body::Empty => {}
            Body::Bytes(v) => w.write_all(&v)?,
        }
        Ok(())
    }

    #[cfg(test)]
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
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "OK",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

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
        h.insert("X-Trace", "abc");
        h.insert("host", "other"); // same key, different case: should overwrite
        assert_eq!(h.get("host"), Some("other"));
        assert_eq!(h.order, vec!["host", "x-trace"]);
    }

    fn cursor_request(headers: &[(&str, &str)], body: &[u8]) -> Request<Cursor<Vec<u8>>> {
        let mut h = Headers::default();
        for (k, v) in headers {
            h.insert(k, v);
        }
        Request {
            method: "POST".into(),
            raw_path: "/token".into(),
            path: "/token".into(),
            query_raw: String::new(),
            headers: h,
            reader: Cursor::new(body.to_vec()),
        }
    }

    #[test]
    fn read_body_reads_content_length() {
        let mut req = cursor_request(&[("Content-Length", "5")], b"hello world");
        let body = read_body(&mut req, 1024).unwrap();
        assert_eq!(body, b"hello");
    }

    #[test]
    fn read_body_absent_is_empty() {
        let mut req = cursor_request(&[], b"ignored");
        let body = read_body(&mut req, 1024).unwrap();
        assert!(body.is_empty());
    }

    #[test]
    fn read_body_rejects_over_max() {
        let mut req = cursor_request(&[("Content-Length", "100")], b"");
        assert!(read_body(&mut req, 10).is_err());
    }

    #[test]
    fn redirect_sets_location_and_status() {
        let r = BuiltResponse::redirect("/login");
        assert_eq!(r.status, 302);
        assert_eq!(r.header_value("Location"), Some("/login"));
        assert_eq!(r.body.len(), 0);
    }

    #[test]
    fn write_to_emits_content_length_zero_for_empty_body() {
        let mut buf = Vec::new();
        BuiltResponse::new(204).write_to(&mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.starts_with("HTTP/1.1 204 No Content\r\n"));
        assert!(out.contains("Content-Length: 0\r\n"));
        assert!(out.contains("Connection: close\r\n"));
        assert!(out.contains(&format!(
            "Server: minicloak/{}\r\n",
            env!("CARGO_PKG_VERSION")
        )));
        // No Content-Type default.
        assert!(!out.to_ascii_lowercase().contains("content-type:"));
    }

    #[test]
    fn write_to_json_sets_content_type() {
        let mut buf = Vec::new();
        BuiltResponse::new(200)
            .json("{}".into())
            .write_to(&mut buf)
            .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("Content-Type: application/json\r\n"));
        assert!(out.contains("Content-Length: 2\r\n"));
        assert!(out.ends_with("\r\n\r\n{}"));
    }
}
