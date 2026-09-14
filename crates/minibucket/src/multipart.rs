// S3 multipart upload.
//
// Layout under <root>/buckets/<bucket>/uploads/<upload-id>/:
//   .info                # one line: <unix-ts>\n<content-type>\n<key>\n
//   parts/<NNNNN>        # part data
//   parts/<NNNNN>.meta   # one line: <md5-hex>\n<size>\n

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::http::{AwsChunkedReader, FixedReader, Headers, Request, Response};
use crate::md5::Md5;
use crate::s3::{error_response, read_body_all, write_xml, Server};
use crate::sha256::hex;
use crate::util::{iso8601, xml_escape};

/// An upload id is exactly 32 ASCII hex digits, as produced by `new_upload_id`.
/// Both cases are accepted (the generator emits upper-case; clients may
/// normalise). Anything else -- including path separators, `..`, or an empty
/// string -- is rejected so the id can never escape the uploads directory.
pub(crate) fn valid_upload_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Why a request could not be mapped to an upload directory. Each variant
/// carries the S3 error the handlers report.
#[derive(Debug, PartialEq, Eq)]
enum UploadDirError {
    /// Bucket name is malformed or the bucket does not exist.
    NoSuchBucket,
    /// uploadId is not a well-formed id (see `valid_upload_id`).
    InvalidUploadId,
    /// Well-formed, but no such upload directory on disk.
    NoSuchUpload,
}

impl UploadDirError {
    fn respond(
        &self,
        sock: &mut std::net::TcpStream,
        rid: &str,
        resource: &str,
    ) -> std::io::Result<()> {
        match self {
            UploadDirError::NoSuchBucket => {
                error_response(sock, 404, "NoSuchBucket", "no such bucket", rid, resource)
            }
            UploadDirError::InvalidUploadId => {
                error_response(sock, 400, "InvalidArgument", "bad uploadId", rid, resource)
            }
            UploadDirError::NoSuchUpload => {
                error_response(sock, 404, "NoSuchUpload", "unknown uploadId", rid, resource)
            }
        }
    }
}

/// Builds `<root>/buckets/<bucket>/uploads/<upload-id>` from untrusted input.
///
/// Both components are validated before joining, and the result is asserted
/// to stay under `<root>/buckets`, so an attacker-supplied `uploadId=/data`
/// or `../..` can never resolve to a path outside the store. Does not check
/// that the directory exists; see `existing_upload_dir`.
fn upload_dir(srv: &Server, bucket: &str, upload_id: &str) -> Result<PathBuf, UploadDirError> {
    if !crate::storage::valid_bucket(bucket) || !srv.storage.bucket_exists(bucket) {
        return Err(UploadDirError::NoSuchBucket);
    }
    if !valid_upload_id(upload_id) {
        return Err(UploadDirError::InvalidUploadId);
    }
    let base = srv.storage.root.join("buckets");
    let dir = base.join(bucket).join("uploads").join(upload_id);
    // Defence in depth: the validators above already make this unreachable.
    if !dir.starts_with(&base) {
        return Err(UploadDirError::InvalidUploadId);
    }
    Ok(dir)
}

/// Like `upload_dir`, but additionally requires the upload to exist on disk.
fn existing_upload_dir(
    srv: &Server,
    bucket: &str,
    upload_id: &str,
) -> Result<PathBuf, UploadDirError> {
    let dir = upload_dir(srv, bucket, upload_id)?;
    if !dir.is_dir() {
        return Err(UploadDirError::NoSuchUpload);
    }
    Ok(dir)
}

fn part_path(dir: &Path, n: u32) -> PathBuf {
    dir.join("parts").join(format!("{:05}", n))
}

fn part_meta(dir: &Path, n: u32) -> PathBuf {
    dir.join("parts").join(format!("{:05}.meta", n))
}

fn new_upload_id() -> String {
    // 32 hex chars derived from time + a counter.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let c = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mix1 = n
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let mix2 = (n ^ c).wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(c);
    format!("{:016X}{:016X}", mix1, mix2)
}

pub fn create_multipart(
    srv: &Server,
    sock: &mut std::net::TcpStream,
    bucket: &str,
    key: &str,
    headers: &Headers,
    rid: &str,
) -> std::io::Result<()> {
    if !srv.storage.bucket_exists(bucket) {
        return error_response(sock, 404, "NoSuchBucket", "no such bucket", rid, bucket);
    }
    if !crate::storage::valid_key(key) {
        return error_response(sock, 400, "InvalidArgument", "bad key", rid, key);
    }
    let upload_id = new_upload_id();
    let dir = match upload_dir(srv, bucket, &upload_id) {
        Ok(d) => d,
        Err(e) => return e.respond(sock, rid, bucket),
    };
    fs::create_dir_all(dir.join("parts"))?;
    let ct = headers
        .get("content-type")
        .unwrap_or("application/octet-stream");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut info = File::create(dir.join(".info"))?;
    writeln!(info, "{}", now)?;
    writeln!(info, "{}", ct)?;
    writeln!(info, "{}", key)?;

    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><InitiateMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Bucket>{}</Bucket><Key>{}</Key><UploadId>{}</UploadId></InitiateMultipartUploadResult>"#,
        xml_escape(bucket),
        xml_escape(key),
        xml_escape(&upload_id),
    );
    write_xml(sock, 200, &body, rid)
}

// Nine parameters, but they are all the pieces S3 puts in one UploadPart
// request; bundling them into a struct would only move the noise.
#[allow(clippy::too_many_arguments)]
pub fn upload_part<R: std::io::BufRead>(
    srv: &Server,
    req: &mut Request<R>,
    sock: &mut std::net::TcpStream,
    bucket: &str,
    _key: &str,
    upload_id: &str,
    part_number: u32,
    rid: &str,
) -> std::io::Result<()> {
    if !(1..=10_000).contains(&part_number) {
        return error_response(
            sock,
            400,
            "InvalidArgument",
            "partNumber out of range",
            rid,
            upload_id,
        );
    }
    let dir = match existing_upload_dir(srv, bucket, upload_id) {
        Ok(d) => d,
        Err(e) => return e.respond(sock, rid, upload_id),
    };
    let data_path = part_path(&dir, part_number);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&data_path)?;
    let mut md5 = Md5::new();
    let mut size: u64 = 0;

    let is_chunked = req
        .headers
        .get("content-encoding")
        .map(|v| v.contains("aws-chunked"))
        .unwrap_or(false);
    let content_sha = req.headers.get("x-amz-content-sha256").unwrap_or("");
    let streaming = is_chunked
        || content_sha == "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"
        || content_sha == "STREAMING-UNSIGNED-PAYLOAD-TRAILER";

    let mut buf = vec![0u8; 64 * 1024];
    // Copy the body into the part file. A read error includes body
    // verification failures (chunk-signature / x-amz-content-sha256
    // mismatch): drop the half-written part so a rejected upload leaves
    // nothing behind, then let handle() map the error to an S3 response.
    let copied: std::io::Result<()> = (|| {
        if streaming {
            let ctx = req.chunk_ctx.take();
            let mut r = AwsChunkedReader::new(&mut req.reader).with_chunk_ctx(ctx);
            loop {
                let n = r.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n])?;
                md5.update(&buf[..n]);
                size += n as u64;
            }
        } else {
            let remaining = req
                .headers
                .get("content-length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0u64);
            let mut r = FixedReader {
                r: &mut req.reader,
                remaining,
            };
            loop {
                let n = r.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n])?;
                md5.update(&buf[..n]);
                size += n as u64;
            }
        }
        file.flush()
    })();
    drop(file);
    if let Err(e) = copied {
        let _ = std::fs::remove_file(&data_path);
        return Err(e);
    }

    let digest = md5.finalize();
    let etag_hex = hex(&digest);
    let mut mf = File::create(part_meta(&dir, part_number))?;
    writeln!(mf, "{}", etag_hex)?;
    writeln!(mf, "{}", size)?;

    let resp = Response::new(200)
        .header("ETag", &format!("\"{}\"", etag_hex))
        .header("x-amz-request-id", rid);
    resp.write_headers(sock, Some(0))?;
    Ok(())
}

pub fn complete_multipart<R: std::io::BufRead>(
    srv: &Server,
    req: &mut Request<R>,
    sock: &mut std::net::TcpStream,
    bucket: &str,
    key: &str,
    upload_id: &str,
    rid: &str,
) -> std::io::Result<()> {
    let dir = match existing_upload_dir(srv, bucket, upload_id) {
        Ok(d) => d,
        Err(e) => return e.respond(sock, rid, upload_id),
    };
    // Read content-type from .info.
    let info_text = fs::read_to_string(dir.join(".info")).unwrap_or_default();
    let mut lines = info_text.lines();
    let _ = lines.next();
    let content_type = lines
        .next()
        .unwrap_or("application/octet-stream")
        .to_string();

    // Parse request body to learn the part order requested by the client.
    let body = read_body_all(req)?;
    let xml = String::from_utf8_lossy(&body);
    let mut requested: Vec<u32> = Vec::new();
    let mut idx = 0;
    while let Some(s) = xml[idx..].find("<Part>") {
        let from = idx + s + 6;
        let to = match xml[from..].find("</Part>") {
            Some(e) => from + e,
            None => break,
        };
        let blk = &xml[from..to];
        if let Some(n_str) = extract_inner(blk, "PartNumber") {
            if let Ok(n) = n_str.parse::<u32>() {
                requested.push(n);
            }
        }
        idx = to + 7;
    }

    // Verify each requested part exists, then concatenate into final object.
    let mut writer = match srv.storage.put_object_writer(bucket, key) {
        Ok(w) => w,
        Err(e) => return error_response(sock, 500, "InternalError", &format!("{:?}", e), rid, key),
    };
    let mut part_md5s: Vec<u8> = Vec::with_capacity(requested.len() * 16);
    let mut buf = vec![0u8; 64 * 1024];
    for n in &requested {
        let p = part_path(&dir, *n);
        if !p.exists() {
            writer.abort();
            return error_response(
                sock,
                400,
                "InvalidPart",
                &format!("missing part {}", n),
                rid,
                key,
            );
        }
        let mut f = File::open(&p)?;
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            writer.write(&buf[..n])?;
        }
        // Pull the part's md5 hex from the meta file and decode to bytes.
        let meta_text = fs::read_to_string(part_meta(&dir, *n)).unwrap_or_default();
        let first_line = meta_text.lines().next().unwrap_or("");
        if let Some(bytes) = decode_hex16(first_line) {
            part_md5s.extend_from_slice(&bytes);
        }
    }
    let (_etag, _size, _vid) = writer.finish(&content_type)?;

    // S3 multipart ETag: md5(concat(part_md5_bytes)) + "-" + count, hex.
    let final_digest = crate::md5::md5(&part_md5s);
    let final_etag = format!("{}-{}", hex(&final_digest), requested.len());

    // Rewrite the meta sidecar's etag line so list/head returns the multipart ETag.
    rewrite_meta_etag(srv, bucket, key, &final_etag).ok();

    // Cleanup upload directory.
    let _ = fs::remove_dir_all(&dir);

    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><CompleteMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Location>/{}/{}</Location><Bucket>{}</Bucket><Key>{}</Key><ETag>&quot;{}&quot;</ETag></CompleteMultipartUploadResult>"#,
        xml_escape(bucket),
        xml_escape(key),
        xml_escape(bucket),
        xml_escape(key),
        final_etag,
    );
    write_xml(sock, 200, &body, rid)
}

pub fn abort_multipart(
    srv: &Server,
    sock: &mut std::net::TcpStream,
    bucket: &str,
    _key: &str,
    upload_id: &str,
    rid: &str,
) -> std::io::Result<()> {
    let dir = match existing_upload_dir(srv, bucket, upload_id) {
        Ok(d) => d,
        Err(e) => return e.respond(sock, rid, upload_id),
    };
    fs::remove_dir_all(&dir)?;
    let resp = Response::new(204).header("x-amz-request-id", rid);
    resp.write_headers(sock, Some(0))?;
    Ok(())
}

pub fn list_parts(
    srv: &Server,
    sock: &mut std::net::TcpStream,
    bucket: &str,
    key: &str,
    upload_id: &str,
    rid: &str,
) -> std::io::Result<()> {
    let dir = match existing_upload_dir(srv, bucket, upload_id) {
        Ok(d) => d,
        Err(e) => return e.respond(sock, rid, upload_id),
    };
    let mut parts: Vec<(u32, String, u64)> = Vec::new();
    if let Ok(rd) = fs::read_dir(dir.join("parts")) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.ends_with(".meta") {
                continue;
            }
            if let Ok(n) = name.parse::<u32>() {
                let meta = fs::read_to_string(part_meta(&dir, n)).unwrap_or_default();
                let mut it = meta.lines();
                let etag = it.next().unwrap_or("").to_string();
                let size: u64 = it.next().unwrap_or("0").parse().unwrap_or(0);
                parts.push((n, etag, size));
            }
        }
    }
    parts.sort_by_key(|p| p.0);

    let mut body = String::new();
    body.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    body.push_str(r#"<ListPartsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#);
    body.push_str(&format!("<Bucket>{}</Bucket>", xml_escape(bucket)));
    body.push_str(&format!("<Key>{}</Key>", xml_escape(key)));
    body.push_str(&format!("<UploadId>{}</UploadId>", xml_escape(upload_id)));
    body.push_str("<IsTruncated>false</IsTruncated>");
    for (n, etag, size) in &parts {
        body.push_str("<Part>");
        body.push_str(&format!("<PartNumber>{}</PartNumber>", n));
        body.push_str(&format!("<ETag>&quot;{}&quot;</ETag>", etag));
        body.push_str(&format!("<Size>{}</Size>", size));
        body.push_str("</Part>");
    }
    body.push_str("</ListPartsResult>");
    write_xml(sock, 200, &body, rid)
}

pub fn list_multipart_uploads(
    srv: &Server,
    sock: &mut std::net::TcpStream,
    bucket: &str,
    rid: &str,
) -> std::io::Result<()> {
    if !crate::storage::valid_bucket(bucket) || !srv.storage.bucket_exists(bucket) {
        return error_response(sock, 404, "NoSuchBucket", "no such bucket", rid, bucket);
    }
    let uploads_dir = srv
        .storage
        .root
        .join("buckets")
        .join(bucket)
        .join("uploads");
    let mut entries: Vec<(String, String, u64)> = Vec::new();
    if let Ok(rd) = fs::read_dir(&uploads_dir) {
        for e in rd.flatten() {
            let upload_id = e.file_name().to_string_lossy().to_string();
            let info = fs::read_to_string(e.path().join(".info")).unwrap_or_default();
            let mut it = info.lines();
            let ts: u64 = it.next().unwrap_or("0").parse().unwrap_or(0);
            let _ct = it.next().unwrap_or("").to_string();
            let key = it.next().unwrap_or("").to_string();
            entries.push((upload_id, key, ts));
        }
    }
    entries.sort_by(|a, b| a.1.cmp(&b.1));
    let mut body = String::new();
    body.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    body.push_str(
        r#"<ListMultipartUploadsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#,
    );
    body.push_str(&format!("<Bucket>{}</Bucket>", xml_escape(bucket)));
    body.push_str("<IsTruncated>false</IsTruncated>");
    for (id, key, ts) in &entries {
        body.push_str("<Upload>");
        body.push_str(&format!("<Key>{}</Key>", xml_escape(key)));
        body.push_str(&format!("<UploadId>{}</UploadId>", xml_escape(id)));
        body.push_str(&format!("<Initiated>{}</Initiated>", iso8601(*ts)));
        body.push_str("</Upload>");
    }
    body.push_str("</ListMultipartUploadsResult>");
    write_xml(sock, 200, &body, rid)
}

fn extract_inner(s: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let i = s.find(&open)? + open.len();
    let j = s[i..].find(&close)?;
    Some(s[i..i + j].to_string())
}

fn decode_hex16(s: &str) -> Option<[u8; 16]> {
    let s = s.trim();
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        let h = hex_nibble(s.as_bytes()[i * 2])?;
        let l = hex_nibble(s.as_bytes()[i * 2 + 1])?;
        *byte = (h << 4) | l;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn rewrite_meta_etag(srv: &Server, bucket: &str, key: &str, new_etag: &str) -> std::io::Result<()> {
    // The meta file path mirrors what storage::put_object_writer uses.
    let path = srv
        .storage
        .root
        .join("buckets")
        .join(bucket)
        .join("meta")
        .join(format!("{}.meta", key));
    if !path.exists() {
        return Ok(());
    }
    let text = fs::read_to_string(&path)?;
    let mut out = String::new();
    let mut wrote = false;
    let mut map: HashMap<&str, String> = HashMap::new();
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("etag: ") {
            map.insert("etag", v.to_string());
            wrote = true;
            continue;
        }
    }
    drop(map);
    for line in text.lines() {
        if line.starts_with("etag: ") {
            out.push_str(&format!("etag: {}\n", new_etag));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !wrote {
        out.push_str(&format!("etag: {}\n", new_etag));
    }
    fs::write(&path, out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Storage;
    use std::io::{BufReader, Cursor};
    use std::net::{Shutdown, TcpListener, TcpStream};

    fn tmp_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!("minibucket_multipart_{}_{}", label, nanos));
        p
    }

    struct ScopedRoot(PathBuf);
    impl Drop for ScopedRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A server whose store lives at `<tmp>/store`, plus a sibling
    /// `<tmp>/sentinel` directory that must never be touched by any handler.
    fn make_server(label: &str) -> (Server, PathBuf, ScopedRoot) {
        let outer = tmp_root(label);
        let sentinel = outer.join("sentinel");
        fs::create_dir_all(&sentinel).unwrap();
        fs::write(sentinel.join("keep"), b"do not delete").unwrap();
        let storage = Storage::new(outer.join("store")).unwrap();
        storage.create_bucket("bkt").unwrap();
        let srv = Server {
            storage,
            credentials: crate::creds::Credentials::new(),
            require_auth: false,
            region: "us-east-1".into(),
            domain: None,
        };
        (srv, sentinel, ScopedRoot(outer))
    }

    /// Loopback socket pair: handlers write into `.0`, tests read from `.1`.
    fn socket_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    /// Runs `f` against a fresh socket and returns the raw HTTP response.
    fn run<F>(f: F) -> String
    where
        F: FnOnce(&mut TcpStream) -> std::io::Result<()>,
    {
        let (mut sock, mut peer) = socket_pair();
        f(&mut sock).unwrap();
        sock.shutdown(Shutdown::Write).unwrap();
        let mut out = String::new();
        peer.read_to_string(&mut out).unwrap();
        out
    }

    fn status_of(resp: &str) -> u16 {
        resp.split_whitespace().nth(1).unwrap().parse().unwrap()
    }

    fn request(body: &[u8]) -> Request<BufReader<Cursor<Vec<u8>>>> {
        let mut headers = Headers::default();
        headers.insert("content-length", &body.len().to_string());
        Request {
            method: "PUT".into(),
            raw_path: "/bkt/k".into(),
            path: "/bkt/k".into(),
            query_raw: String::new(),
            headers,
            reader: BufReader::new(Cursor::new(body.to_vec())),
            chunk_ctx: None,
        }
    }

    fn initiate(srv: &Server) -> String {
        let resp = run(|s| create_multipart(srv, s, "bkt", "k", &Headers::default(), "rid"));
        assert_eq!(status_of(&resp), 200, "{}", resp);
        extract_inner(&resp, "UploadId").expect("UploadId in response")
    }

    // Path components an attacker might smuggle in via `?uploadId=`.
    const TRAVERSALS: &[&str] = &["/data", "../..", "../../sentinel", "", "..", "."];

    #[test]
    fn valid_upload_id_rules() {
        assert!(valid_upload_id(&new_upload_id()));
        assert!(valid_upload_id(&"A".repeat(32)));
        assert!(valid_upload_id(&"a".repeat(32))); // lower-case hex accepted
        assert!(valid_upload_id("0123456789abcdefABCDEF0123456789"));
        assert!(!valid_upload_id("/data"));
        assert!(!valid_upload_id("../.."));
        assert!(!valid_upload_id(""));
        assert!(!valid_upload_id(&"A".repeat(31))); // too short
        assert!(!valid_upload_id(&"A".repeat(33))); // too long
        assert!(!valid_upload_id(&"G".repeat(32))); // non-hex
        assert!(!valid_upload_id(&format!("{}/", "A".repeat(31)))); // separator
        assert!(!valid_upload_id(&format!("{}\u{e9}", "A".repeat(30)))); // non-ASCII, 32 bytes
    }

    #[test]
    fn upload_dir_rejects_bad_input_and_stays_under_root() {
        let (srv, _sentinel, _g) = make_server("dir");
        let id = new_upload_id();
        let base = srv.storage.root.join("buckets");
        assert!(upload_dir(&srv, "bkt", &id).unwrap().starts_with(&base));
        for bad in TRAVERSALS {
            assert_eq!(
                upload_dir(&srv, "bkt", bad),
                Err(UploadDirError::InvalidUploadId),
                "uploadId {:?}",
                bad
            );
            assert_eq!(
                upload_dir(&srv, bad, &id),
                Err(UploadDirError::NoSuchBucket),
                "bucket {:?}",
                bad
            );
        }
        assert_eq!(
            upload_dir(&srv, "missing", &id),
            Err(UploadDirError::NoSuchBucket)
        );
        assert_eq!(
            existing_upload_dir(&srv, "bkt", &id),
            Err(UploadDirError::NoSuchUpload)
        );
    }

    #[test]
    fn abort_with_traversal_id_is_rejected_and_leaves_sentinel() {
        let (srv, sentinel, _g) = make_server("abort");
        for bad in TRAVERSALS {
            let resp = run(|s| abort_multipart(&srv, s, "bkt", "k", bad, "rid"));
            assert_eq!(status_of(&resp), 400, "uploadId {:?}: {}", bad, resp);
            assert!(resp.contains("InvalidArgument"));
            assert!(
                sentinel.join("keep").is_file(),
                "sentinel deleted by {:?}",
                bad
            );
        }
        // The store root itself must survive as well.
        assert!(srv.storage.root.join("buckets").join("bkt").is_dir());
    }

    #[test]
    fn complete_with_traversal_id_is_rejected_and_leaves_sentinel() {
        let (srv, sentinel, _g) = make_server("complete");
        for bad in TRAVERSALS {
            let mut req = request(b"<CompleteMultipartUpload></CompleteMultipartUpload>");
            let resp = run(|s| complete_multipart(&srv, &mut req, s, "bkt", "k", bad, "rid"));
            assert_eq!(status_of(&resp), 400, "uploadId {:?}: {}", bad, resp);
            assert!(resp.contains("InvalidArgument"));
            assert!(
                sentinel.join("keep").is_file(),
                "sentinel deleted by {:?}",
                bad
            );
        }
    }

    #[test]
    fn upload_part_with_traversal_id_is_rejected_and_writes_nothing_outside() {
        let (srv, sentinel, _g) = make_server("part");
        for bad in TRAVERSALS {
            let mut req = request(b"hello");
            let resp = run(|s| upload_part(&srv, &mut req, s, "bkt", "k", bad, 1, "rid"));
            assert_eq!(status_of(&resp), 400, "uploadId {:?}: {}", bad, resp);
            assert!(resp.contains("InvalidArgument"));
        }
        assert!(sentinel.join("keep").is_file());
        assert!(
            !sentinel.join("parts").exists(),
            "part written outside root"
        );
        assert!(!srv.storage.root.join("parts").exists());
        assert!(!srv.storage.root.join("buckets").join("parts").exists());
    }

    #[test]
    fn list_parts_with_traversal_id_is_rejected() {
        let (srv, _sentinel, _g) = make_server("list");
        for bad in TRAVERSALS {
            let resp = run(|s| list_parts(&srv, s, "bkt", "k", bad, "rid"));
            assert_eq!(status_of(&resp), 400, "uploadId {:?}: {}", bad, resp);
        }
    }

    #[test]
    fn handlers_require_existing_bucket() {
        let (srv, _sentinel, _g) = make_server("nobucket");
        let id = new_upload_id();
        for bucket in ["nope", "../../sentinel", "/", "BAD"] {
            let resp = run(|s| abort_multipart(&srv, s, bucket, "k", &id, "rid"));
            assert_eq!(status_of(&resp), 404, "bucket {:?}: {}", bucket, resp);
            assert!(resp.contains("NoSuchBucket"), "{}", resp);
            let resp = run(|s| list_multipart_uploads(&srv, s, bucket, "rid"));
            assert_eq!(status_of(&resp), 404, "bucket {:?}: {}", bucket, resp);
        }
    }

    #[test]
    fn well_formed_unknown_id_is_no_such_upload() {
        let (srv, _sentinel, _g) = make_server("unknown");
        let id = new_upload_id();
        let resp = run(|s| abort_multipart(&srv, s, "bkt", "k", &id, "rid"));
        assert_eq!(status_of(&resp), 404, "{}", resp);
        assert!(resp.contains("NoSuchUpload"), "{}", resp);
    }

    #[test]
    fn valid_flow_initiate_upload_complete() {
        let (srv, _sentinel, _g) = make_server("flow");
        let id = initiate(&srv);
        assert!(valid_upload_id(&id));

        let mut req = request(b"hello ");
        let resp = run(|s| upload_part(&srv, &mut req, s, "bkt", "k", &id, 1, "rid"));
        assert_eq!(status_of(&resp), 200, "{}", resp);
        let mut req = request(b"world");
        let resp = run(|s| upload_part(&srv, &mut req, s, "bkt", "k", &id, 2, "rid"));
        assert_eq!(status_of(&resp), 200, "{}", resp);

        let resp = run(|s| list_parts(&srv, s, "bkt", "k", &id, "rid"));
        assert_eq!(status_of(&resp), 200, "{}", resp);
        assert!(resp.contains("<PartNumber>1</PartNumber>"));
        assert!(resp.contains("<PartNumber>2</PartNumber>"));

        let xml = "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber></Part><Part><PartNumber>2</PartNumber></Part></CompleteMultipartUpload>";
        let mut req = request(xml.as_bytes());
        let resp = run(|s| complete_multipart(&srv, &mut req, s, "bkt", "k", &id, "rid"));
        assert_eq!(status_of(&resp), 200, "{}", resp);
        assert!(resp.contains("<ETag>&quot;"), "{}", resp);
        assert!(resp.contains("-2&quot;</ETag>"), "{}", resp);

        let (_meta, mut f) = srv.storage.get_object("bkt", "k").unwrap();
        let mut got = String::new();
        f.read_to_string(&mut got).unwrap();
        assert_eq!(got, "hello world");

        // Upload directory is cleaned up; a second complete is NoSuchUpload.
        assert!(!srv
            .storage
            .root
            .join("buckets/bkt/uploads")
            .join(&id)
            .exists());
        let mut req = request(xml.as_bytes());
        let resp = run(|s| complete_multipart(&srv, &mut req, s, "bkt", "k", &id, "rid"));
        assert_eq!(status_of(&resp), 404, "{}", resp);
    }

    #[test]
    fn valid_flow_abort() {
        let (srv, _sentinel, _g) = make_server("abortok");
        let id = initiate(&srv);
        let resp = run(|s| abort_multipart(&srv, s, "bkt", "k", &id, "rid"));
        assert_eq!(status_of(&resp), 204, "{}", resp);
        assert!(!srv
            .storage
            .root
            .join("buckets/bkt/uploads")
            .join(&id)
            .exists());
    }
}
