// HTTP JSON API + web UI + SSE (hand-rolled HTTP/1.1). Pure std.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use crate::events::Event;
use crate::http::{parse_range, BuiltResponse, Headers, Request, Response};
use crate::json::Json;
use crate::mime::{decode_encoded_words, Part};
use crate::server::Server;
use crate::store::{StoreError, Summary};
use crate::stream::Stream;
use crate::url::parse_query;

/// Uniform handler signature shared with smtp::serve.
pub fn serve(srv: &Server, stream: Stream, peer: Option<SocketAddr>) -> io::Result<()> {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(60)));

    // The reader owns the original transport; `sock` is a second handle for writing.
    // A TLS stream cannot be cloned — HTTP-over-TLS is out of scope (§10), so drop it.
    let mut sock = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };

    let req = match crate::http::read_request(stream) {
        Ok(r) => r,
        Err(e) => {
            // A clean EOF (client opened + closed) is silent; a real parse error is a 400.
            if e.kind() != io::ErrorKind::UnexpectedEof {
                let _ = error_response(&mut sock, 400, "bad_request", "malformed request");
            }
            return Ok(());
        }
    };

    eprintln!("[req] {:?} {} {}", peer, req.method, req.raw_path);

    // A dispatch error is a socket write failure (all logical errors are written as
    // responses and return Ok); the response may be half-written, so do not append a 500.
    if let Err(e) = dispatch(srv, req, &mut sock) {
        eprintln!("[handler] {}", e);
    }
    let _ = sock.flush();
    Ok(())
}

pub fn dispatch<R: io::BufRead>(
    srv: &Server,
    req: Request<R>,
    sock: &mut Stream,
) -> io::Result<()> {
    let method = req.method.as_str();
    let path = req.path.as_str();
    let query = parse_query(&req.query_raw);

    // /healthz is always public (no auth), even when require_auth is on.
    if path == "/healthz" {
        return if method == "GET" {
            write_text(sock, 200, "text/plain; charset=utf-8", b"ok")
        } else {
            error_response(sock, 405, "method_not_allowed", "method not allowed")
        };
    }

    if srv.require_auth && !check_auth(srv, &req.headers) {
        return unauthorized(sock);
    }

    if let Some(rest) = path.strip_prefix("/api/v1/") {
        let segs: Vec<&str> = rest.split('/').collect();
        return match segs.as_slice() {
            ["info"] => {
                if method == "GET" {
                    build_info(srv).write_to(sock)
                } else {
                    error_response(sock, 405, "method_not_allowed", "method not allowed")
                }
            }
            ["events"] => {
                if method == "GET" {
                    serve_events(srv, sock)
                } else {
                    error_response(sock, 405, "method_not_allowed", "method not allowed")
                }
            }
            ["messages"] => match method {
                "GET" => {
                    let limit = qget(&query, "limit")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(50)
                        .min(500);
                    let offset = qget(&query, "offset")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(0);
                    let q = qget(&query, "q");
                    build_list(srv, limit, offset, q).write_to(sock)
                }
                "DELETE" => {
                    let n = srv.store.delete_all().unwrap_or(0);
                    srv.hub.publish(Event::Clear);
                    let body = Json::Obj(vec![("deleted".into(), Json::Int(n as i64))]).to_string();
                    BuiltResponse::new(200).json(body).write_to(sock)
                }
                _ => error_response(sock, 405, "method_not_allowed", "method not allowed"),
            },
            ["messages", id] => match method {
                "GET" => build_message(srv, id).write_to(sock),
                "DELETE" => match srv.store.delete(id) {
                    Ok(()) => {
                        srv.hub.publish(Event::Delete((*id).to_string()));
                        BuiltResponse::new(204).write_to(sock)
                    }
                    Err(StoreError::NotFound | StoreError::InvalidId) => {
                        error_response(sock, 404, "not_found", "no such message")
                    }
                    Err(_) => error_response(sock, 500, "internal", "delete failed"),
                },
                _ => error_response(sock, 405, "method_not_allowed", "method not allowed"),
            },
            ["messages", id, "raw"] => match method {
                "GET" => get_raw(srv, sock, id, false, &req.headers),
                "HEAD" => get_raw(srv, sock, id, true, &req.headers),
                _ => error_response(sock, 405, "method_not_allowed", "method not allowed"),
            },
            ["messages", id, "html"] => {
                if method == "GET" {
                    get_html(srv, sock, id)
                } else {
                    error_response(sock, 405, "method_not_allowed", "method not allowed")
                }
            }
            ["messages", id, "parts", part_id] => {
                if method == "GET" {
                    let download = qget(&query, "download").is_some_and(|v| v != "0");
                    get_part(srv, sock, id, part_id, &req.headers, download)
                } else {
                    error_response(sock, 405, "method_not_allowed", "method not allowed")
                }
            }
            _ => error_response(sock, 404, "not_found", "not found"),
        };
    }

    // Embedded UI assets.
    if let Some(asset) = crate::assets::get(path) {
        return match method {
            "GET" => serve_asset(sock, asset, false),
            "HEAD" => serve_asset(sock, asset, true),
            _ => error_response(sock, 405, "method_not_allowed", "method not allowed"),
        };
    }

    error_response(sock, 404, "not_found", "not found")
}

// ---- testable builders (BuiltResponse, no socket) ----

pub fn build_list(srv: &Server, limit: usize, offset: usize, q: Option<&str>) -> BuiltResponse {
    let mut all = match srv.store.list() {
        Ok(v) => v,
        Err(_) => return build_error(500, "internal", "list failed"),
    };
    all.reverse(); // list() is oldest-first; the API is newest-first
    let filtered: Vec<&Summary> = match q {
        Some(qq) if !qq.is_empty() => {
            let needle = qq.to_lowercase();
            all.iter().filter(|s| summary_matches(s, &needle)).collect()
        }
        _ => all.iter().collect(),
    };
    let total = filtered.len();
    let page: Vec<&Summary> = filtered.into_iter().skip(offset).take(limit).collect();

    let mut body = String::from("{\"total\":");
    body.push_str(&total.to_string());
    body.push_str(",\"count\":");
    body.push_str(&page.len().to_string());
    body.push_str(",\"offset\":");
    body.push_str(&offset.to_string());
    body.push_str(",\"limit\":");
    body.push_str(&limit.to_string());
    body.push_str(",\"messages\":[");
    for (i, s) in page.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        // Summary::to_json is the single source of truth for the schema (§3/§5 store F14).
        body.push_str(&s.to_json());
    }
    body.push_str("]}");
    BuiltResponse::new(200).json(body)
}

pub fn build_message(srv: &Server, id: &str) -> BuiltResponse {
    let summary = match srv.store.get_summary(id) {
        Ok(s) => s,
        Err(StoreError::NotFound | StoreError::InvalidId) => {
            return build_error(404, "not_found", "no such message")
        }
        Err(_) => return build_error(500, "internal", "read failed"),
    };
    let raw = match srv.store.get_raw(id) {
        Ok(r) => r,
        Err(_) => return build_error(404, "not_found", "no such message"),
    };
    let msg = crate::mime::parse(&raw);

    let headers = Json::Arr(
        msg.headers
            .iter()
            .map(|(k, v)| {
                Json::Arr(vec![
                    Json::Str(k.clone()),
                    Json::Str(decode_encoded_words(v)),
                ])
            })
            .collect(),
    );
    let text = match msg.text_body() {
        Some(t) => Json::Str(t),
        None => Json::Null,
    };
    let html = match msg.html_body() {
        Some(h) => Json::Str(h),
        None => Json::Null,
    };
    let parts = Json::Arr(msg.flat_parts().into_iter().map(part_json).collect());

    let mut body = String::from("{\"summary\":");
    body.push_str(&summary.to_json());
    body.push_str(",\"headers\":");
    body.push_str(&headers.to_string());
    body.push_str(",\"text\":");
    body.push_str(&text.to_string());
    body.push_str(",\"html\":");
    body.push_str(&html.to_string());
    body.push_str(",\"parts\":");
    body.push_str(&parts.to_string());
    body.push('}');
    BuiltResponse::new(200).json(body)
}

pub fn build_info(srv: &Server) -> BuiltResponse {
    let count = srv.store.count().unwrap_or(0);
    #[cfg(feature = "tls")]
    let tls = srv.tls.is_some();
    #[cfg(not(feature = "tls"))]
    let tls = false;
    let mm = match srv.max_messages {
        Some(n) => Json::Int(n as i64),
        None => Json::Null,
    };
    let obj = Json::Obj(vec![
        ("name".into(), Json::Str("minimail".to_string())),
        ("version".into(), Json::Str(srv.version.to_string())),
        ("hostname".into(), Json::Str(srv.hostname.clone())),
        ("smtp".into(), Json::Str(srv.smtp_bind.clone())),
        ("http".into(), Json::Str(srv.http_bind.clone())),
        ("count".into(), Json::Int(count as i64)),
        ("max_messages".into(), mm),
        ("max_size".into(), Json::Int(srv.max_size as i64)),
        ("anonymous".into(), Json::Bool(!srv.require_auth)),
        ("tls".into(), Json::Bool(tls)),
    ]);
    BuiltResponse::new(200).json(obj.to_string())
}

pub fn build_error(status: u16, code: &str, message: &str) -> BuiltResponse {
    BuiltResponse::new(status).json(error_body(code, message))
}

// ---- streaming helpers (write directly to the pinned transport) ----

fn get_raw(
    srv: &Server,
    sock: &mut Stream,
    id: &str,
    head_only: bool,
    headers: &Headers,
) -> io::Result<()> {
    if !crate::store::valid_id(id) {
        return error_response(sock, 404, "not_found", "no such message");
    }
    let path = srv.store.eml_path(id);
    let mut file = match File::open(&path) {
        Ok(f) => f,
        Err(_) => return error_response(sock, 404, "not_found", "no such message"),
    };
    let size = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return error_response(sock, 500, "internal", "stat failed"),
    };

    let range_hdr = headers.get("range");
    let (status, start, length) = match range_hdr.and_then(|v| parse_range(v, size)) {
        Some((s, e)) => (206u16, s, e - s + 1),
        None => {
            if range_hdr.is_some() {
                return range_not_satisfiable(sock, size);
            }
            (200, 0, size)
        }
    };

    let cd = format!("attachment; filename=\"{id}.eml\"");
    let mut resp = Response::new(status)
        .header("Content-Type", "message/rfc822")
        .header("Content-Disposition", &cd)
        .header("Accept-Ranges", "bytes");
    if status == 206 {
        resp = resp.header(
            "Content-Range",
            &format!("bytes {}-{}/{}", start, start + length - 1, size),
        );
    }
    resp.write_headers(sock, Some(length))?;
    if head_only {
        return Ok(());
    }
    stream_file(&mut file, sock, start, length)
}

fn get_html(srv: &Server, sock: &mut Stream, id: &str) -> io::Result<()> {
    let raw = match srv.store.get_raw(id) {
        Ok(r) => r,
        Err(_) => return error_response(sock, 404, "not_found", "no such message"),
    };
    let msg = crate::mime::parse(&raw);
    match msg.html_body() {
        Some(html) => {
            let bytes = html.into_bytes();
            // Captured mail is attacker-controlled. The UI renders it in a sandboxed
            // iframe, but this endpoint can also be navigated to directly, so sandbox
            // it at the HTTP layer too: an opaque origin with scripting disabled.
            Response::new(200)
                .header("Content-Type", "text/html; charset=utf-8")
                .header("Content-Security-Policy", "sandbox")
                .header("X-Content-Type-Options", "nosniff")
                .write_headers(sock, Some(bytes.len() as u64))?;
            sock.write_all(&bytes)
        }
        None => error_response(sock, 404, "not_found", "no html part"),
    }
}

fn get_part(
    srv: &Server,
    sock: &mut Stream,
    id: &str,
    part_id: &str,
    headers: &Headers,
    download: bool,
) -> io::Result<()> {
    let raw = match srv.store.get_raw(id) {
        Ok(r) => r,
        Err(_) => return error_response(sock, 404, "not_found", "no such message"),
    };
    let msg = crate::mime::parse(&raw);
    let part = match msg.part_by_id(part_id) {
        Some(p) => p,
        None => return error_response(sock, 404, "not_found", "no such part"),
    };

    let bytes = &part.body;
    let size = bytes.len() as u64;
    let attach = download || part.is_attachment;
    let fname = sanitize_filename(
        &part
            .filename
            .clone()
            .unwrap_or_else(|| format!("part-{part_id}")),
    );
    let disp = if attach {
        format!("attachment; filename=\"{fname}\"")
    } else {
        format!("inline; filename=\"{fname}\"")
    };

    let range_hdr = headers.get("range");
    let (status, start, length) = match range_hdr.and_then(|v| parse_range(v, size)) {
        Some((s, e)) => (206u16, s, e - s + 1),
        None => {
            if range_hdr.is_some() {
                return range_not_satisfiable(sock, size);
            }
            (200, 0, size)
        }
    };

    // The part's Content-Type is attacker-controlled; an inline text/html or SVG part
    // would otherwise execute on this origin when opened directly. Sandbox + nosniff.
    let mut resp = Response::new(status)
        .header("Content-Type", part.content_type.as_str())
        .header("Content-Disposition", &disp)
        .header("Content-Security-Policy", "sandbox")
        .header("X-Content-Type-Options", "nosniff")
        .header("Accept-Ranges", "bytes");
    if status == 206 {
        resp = resp.header(
            "Content-Range",
            &format!("bytes {}-{}/{}", start, start + length - 1, size),
        );
    }
    resp.write_headers(sock, Some(length))?;
    sock.write_all(&bytes[start as usize..(start + length) as usize])
}

fn serve_asset(sock: &mut Stream, asset: crate::assets::Asset, head_only: bool) -> io::Result<()> {
    Response::new(200)
        .header("Content-Type", asset.content_type)
        .write_headers(sock, Some(asset.bytes.len() as u64))?;
    if head_only {
        return Ok(());
    }
    sock.write_all(asset.bytes)
}

// SSE: headers written by hand (no Content-Length, close-delimited) and every frame
// flushed immediately. Routing this through BuiltResponse::write_to would buffer and
// break live updates (§8).
fn serve_events(srv: &Server, sock: &mut Stream) -> io::Result<()> {
    // Long-lived: disable the read timeout; the write timeout still bounds a stuck consumer.
    let _ = sock.set_read_timeout(None);

    let mut head = String::new();
    head.push_str("HTTP/1.1 200 OK\r\n");
    head.push_str("Content-Type: text/event-stream\r\n");
    head.push_str("Cache-Control: no-cache\r\n");
    head.push_str("Connection: keep-alive\r\n");
    head.push_str(&format!("Date: {}\r\n", crate::util::http_date_now()));
    head.push_str(&format!("Server: minimail/{}\r\n", srv.version));
    head.push_str("\r\n");
    sock.write_all(head.as_bytes())?;
    sock.flush()?;

    let rx = srv.hub.subscribe();
    loop {
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(frame) => {
                sock.write_all(frame.as_bytes())?;
                sock.flush()?;
            }
            // Heartbeat comment; a write error here means a dead client -> end the thread,
            // which drops the receiver and prunes the subscriber on the next publish.
            Err(RecvTimeoutError::Timeout) => {
                sock.write_all(b": ping\n\n")?;
                sock.flush()?;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

fn range_not_satisfiable(sock: &mut Stream, size: u64) -> io::Result<()> {
    BuiltResponse::new(416)
        .header("Content-Range", &format!("bytes */{size}"))
        .json(error_body("range_not_satisfiable", "range not satisfiable"))
        .write_to(sock)
}

fn error_response(sock: &mut Stream, status: u16, code: &str, message: &str) -> io::Result<()> {
    build_error(status, code, message).write_to(sock)
}

/// HTTP Basic auth: base64-decode `Authorization: Basic`, look up
/// `creds.secret_for(user)`, compare with `constant_time_eq`. SPEC §8.
fn check_auth(srv: &Server, headers: &Headers) -> bool {
    let auth = match headers.get("authorization") {
        Some(a) => a,
        None => return false,
    };
    // Scheme token is case-insensitive per RFC 7617.
    let rest = match auth.split_once(' ') {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("basic") => rest.trim(),
        _ => return false,
    };
    let decoded = match crate::base64::decode(rest) {
        Some(d) => d,
        None => return false,
    };
    let s = match std::str::from_utf8(&decoded) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let (user, pass) = match s.split_once(':') {
        Some(up) => up,
        None => return false,
    };
    match srv.creds.secret_for(user) {
        Some(secret) => crate::creds::constant_time_eq(secret.as_bytes(), pass.as_bytes()),
        None => {
            // Compare against a dummy to keep timing roughly uniform for unknown users.
            let _ = crate::creds::constant_time_eq(pass.as_bytes(), pass.as_bytes());
            false
        }
    }
}

fn unauthorized(sock: &mut Stream) -> io::Result<()> {
    BuiltResponse::new(401)
        .header("WWW-Authenticate", "Basic realm=\"minimail\"")
        .json(error_body("unauthorized", "authentication required"))
        .write_to(sock)
}

fn write_text(sock: &mut Stream, status: u16, ct: &str, body: &[u8]) -> io::Result<()> {
    Response::new(status)
        .header("Content-Type", ct)
        .write_headers(sock, Some(body.len() as u64))?;
    sock.write_all(body)
}

fn stream_file(file: &mut File, sock: &mut Stream, start: u64, length: u64) -> io::Result<()> {
    if start > 0 {
        file.seek(SeekFrom::Start(start))?;
    }
    let mut buf = vec![0u8; 64 * 1024];
    let mut remaining = length;
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = file.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        sock.write_all(&buf[..n])?;
        remaining -= n as u64;
    }
    Ok(())
}

// ---- small helpers ----

fn error_body(code: &str, message: &str) -> String {
    Json::Obj(vec![(
        "error".into(),
        Json::Obj(vec![
            ("code".into(), Json::Str(code.to_string())),
            ("message".into(), Json::Str(message.to_string())),
        ]),
    )])
    .to_string()
}

fn part_json(p: &Part) -> Json {
    let children: Vec<Json> = p.children.iter().map(|c| Json::Str(c.id.clone())).collect();
    Json::Obj(vec![
        ("id".into(), Json::Str(p.id.clone())),
        ("content_type".into(), Json::Str(p.content_type.clone())),
        ("filename".into(), opt_str(&p.filename)),
        ("disposition".into(), opt_str(&p.disposition)),
        ("content_id".into(), opt_str(&p.content_id)),
        ("size".into(), Json::Int(p.body.len() as i64)),
        ("is_attachment".into(), Json::Bool(p.is_attachment)),
        ("children".into(), Json::Arr(children)),
    ])
}

fn opt_str(o: &Option<String>) -> Json {
    match o {
        Some(s) => Json::Str(s.clone()),
        None => Json::Null,
    }
}

fn summary_matches(s: &Summary, needle: &str) -> bool {
    let hit = |h: &str| h.to_lowercase().contains(needle);
    hit(&s.subject)
        || hit(&s.from_header)
        || hit(&s.to_header)
        || hit(&s.from)
        || s.to.iter().any(|t| hit(t))
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .filter(|&c| !matches!(c, '"' | '\\' | '\r' | '\n'))
        .collect()
}

fn qget<'a>(q: &'a [(String, String)], name: &str) -> Option<&'a str> {
    q.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::creds::Credentials;
    use crate::events::Hub;
    use crate::store::{Envelope, Store};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct ScopedRoot(PathBuf);
    impl Drop for ScopedRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn tmp_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!("minimail_api_test_{}_{}", label, nanos));
        p
    }

    fn test_server(store: Store) -> Server {
        Server {
            store,
            creds: Credentials::new(),
            require_auth: false,
            hostname: "mail.local".to_string(),
            version: "0.1.0",
            max_size: 26214400,
            max_messages: None,
            smtp_bind: "127.0.0.1:1025".to_string(),
            http_bind: "127.0.0.1:8025".to_string(),
            hub: Hub::new(),
            #[cfg(feature = "tls")]
            tls: None,
        }
    }

    fn env() -> Envelope {
        Envelope {
            mail_from: "alice@example.com".into(),
            rcpt_to: vec!["bob@example.com".into()],
            helo: "client".into(),
            remote: "127.0.0.1:5".into(),
            auth_user: None,
        }
    }

    fn body_of(resp: BuiltResponse) -> (u16, String) {
        let status = resp.status;
        let bytes = resp.body.into_bytes().unwrap();
        (status, String::from_utf8(bytes).unwrap())
    }

    #[test]
    fn build_info_reports_config() {
        let p = tmp_root("info");
        let _g = ScopedRoot(p.clone());
        let srv = test_server(Store::new(p).unwrap());
        let resp = build_info(&srv);
        assert_eq!(resp.status, 200);
        assert_eq!(
            resp.header_value("content-type"),
            Some("application/json; charset=utf-8")
        );
        let (_, b) = body_of(resp);
        assert!(b.contains("\"name\":\"minimail\""));
        assert!(b.contains("\"hostname\":\"mail.local\""));
        assert!(b.contains("\"anonymous\":true"));
        assert!(b.contains("\"tls\":false"));
        assert!(b.contains("\"max_messages\":null"));
        assert!(b.contains("\"count\":0"));
    }

    #[test]
    fn build_list_empty() {
        let p = tmp_root("list_empty");
        let _g = ScopedRoot(p.clone());
        let srv = test_server(Store::new(p).unwrap());
        let (status, b) = body_of(build_list(&srv, 50, 0, None));
        assert_eq!(status, 200);
        assert!(b.contains("\"total\":0"));
        assert!(b.contains("\"count\":0"));
        assert!(b.contains("\"messages\":[]"));
    }

    #[test]
    fn build_list_lists_and_filters() {
        let p = tmp_root("list");
        let _g = ScopedRoot(p.clone());
        let store = Store::new(p).unwrap();
        store
            .put(
                b"Subject: Apple\r\nFrom: a@x.com\r\n\r\nbody\r\n",
                &env(),
                None,
            )
            .unwrap();
        store
            .put(
                b"Subject: Banana\r\nFrom: b@x.com\r\n\r\nbody\r\n",
                &env(),
                None,
            )
            .unwrap();
        let srv = test_server(store);

        let (_, b) = body_of(build_list(&srv, 50, 0, None));
        assert!(b.contains("\"total\":2"));
        assert!(b.contains("\"count\":2"));
        assert!(b.contains("Apple"));
        assert!(b.contains("Banana"));

        // Case-insensitive substring filter over the summary fields.
        let (_, f) = body_of(build_list(&srv, 50, 0, Some("apple")));
        assert!(f.contains("\"total\":1"));
        assert!(f.contains("Apple"));
        assert!(!f.contains("Banana"));
    }

    #[test]
    fn build_list_paginates() {
        let p = tmp_root("paginate");
        let _g = ScopedRoot(p.clone());
        let store = Store::new(p).unwrap();
        for i in 0..3 {
            let raw = format!("Subject: m{}\r\n\r\nbody\r\n", i);
            store.put(raw.as_bytes(), &env(), None).unwrap();
        }
        let srv = test_server(store);
        let (_, b) = body_of(build_list(&srv, 1, 0, None));
        assert!(b.contains("\"total\":3"));
        assert!(b.contains("\"count\":1"));
        assert!(b.contains("\"limit\":1"));
    }

    #[test]
    fn build_message_detail_and_parts() {
        let p = tmp_root("msg");
        let _g = ScopedRoot(p.clone());
        let store = Store::new(p).unwrap();
        let raw = b"From: Alice <alice@example.com>\r\nTo: bob@example.com\r\nSubject: Hi\r\nContent-Type: multipart/mixed; boundary=BOUND\r\n\r\n--BOUND\r\nContent-Type: text/plain\r\n\r\nhello body\r\n--BOUND\r\nContent-Type: image/png\r\nContent-Disposition: attachment; filename=\"cat.png\"\r\nContent-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n--BOUND--\r\n";
        let sum = store.put(raw, &env(), None).unwrap();
        let srv = test_server(store);
        let (status, b) = body_of(build_message(&srv, &sum.id));
        assert_eq!(status, 200);
        assert!(b.contains("\"summary\""));
        assert!(b.contains("\"headers\""));
        assert!(b.contains("\"parts\""));
        assert!(b.contains("cat.png"));
        assert!(b.contains("\"is_attachment\":true"));
        assert!(b.contains("hello body")); // text body decoded
        assert!(b.contains("\"content_type\":\"multipart/mixed\""));
    }

    #[test]
    fn build_message_missing_is_404() {
        let p = tmp_root("msg404");
        let _g = ScopedRoot(p.clone());
        let srv = test_server(Store::new(p).unwrap());
        let (status, b) = body_of(build_message(&srv, "nope-nope-nope"));
        assert_eq!(status, 404);
        assert!(b.contains("\"code\":\"not_found\""));
        assert!(b.contains("no such message"));
    }

    #[test]
    fn build_error_envelope_shape() {
        let (status, b) = body_of(build_error(404, "not_found", "no such message"));
        assert_eq!(status, 404);
        assert_eq!(
            b,
            "{\"error\":{\"code\":\"not_found\",\"message\":\"no such message\"}}"
        );
    }
}
