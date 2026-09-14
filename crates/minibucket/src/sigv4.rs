// AWS SigV4 verification (HTTP header style). Sufficient for AWS SDKs
// talking to S3, including STREAMING-AWS4-HMAC-SHA256-PAYLOAD requests
// (seed signature verified here, per-chunk signatures via ChunkContext).
//
// Besides the signature itself this module also enforces the two things a
// valid signature does not cover on its own:
//   * the request time (x-amz-date) must be within MAX_CLOCK_SKEW_SECS of
//     the server clock, so a captured request cannot be replayed later;
//   * when x-amz-content-sha256 carries a real digest, the body that actually
//     arrives must hash to it (PayloadHashReader), so a captured signature
//     cannot be reused with a different body.

use std::io::{self, BufRead, Read};

use crate::hmac::hmac_sha256;
use crate::http::Headers;
use crate::sha256::{hex, sha256, Sha256};
use crate::url::{encode_component, encode_path_sigv4, parse_query};

// Maximum accepted difference between x-amz-date and the server clock, in
// either direction. Same window S3 uses.
pub const MAX_CLOCK_SKEW_SECS: u64 = 15 * 60;

#[derive(Debug)]
pub struct AuthInfo {
    pub access_key: String,
    pub date: String,
    pub region: String,
    pub service: String,
    pub signed_headers: Vec<String>,
    pub signature: String,
    pub amz_date: String,
    pub payload_hash: String,
}

#[derive(Debug)]
pub enum AuthError {
    Missing,
    Malformed,
    BadSignature,
    // x-amz-date is absent or not "YYYYMMDDTHHMMSSZ".
    BadDate,
    // x-amz-date is further than MAX_CLOCK_SKEW_SECS from the server clock.
    RequestTimeTooSkewed,
}

// Clock-skew check for header-authenticated requests. `now` is seconds since
// the Unix epoch (passed in so tests can pin it).
pub fn check_request_time(amz_date: &str, now: u64) -> Result<(), AuthError> {
    let signed_at = crate::util::parse_amz_date(amz_date).ok_or(AuthError::BadDate)?;
    if signed_at.abs_diff(now) > MAX_CLOCK_SKEW_SECS {
        return Err(AuthError::RequestTimeTooSkewed);
    }
    Ok(())
}

pub fn parse_authorization(headers: &Headers) -> Result<AuthInfo, AuthError> {
    let auth = headers.get("authorization").ok_or(AuthError::Missing)?;
    if !auth.starts_with("AWS4-HMAC-SHA256") {
        return Err(AuthError::Malformed);
    }
    let rest = auth["AWS4-HMAC-SHA256".len()..].trim_start();
    let mut credential = "";
    let mut signed_headers = "";
    let mut signature = "";
    for part in rest.split(',') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("Credential=") {
            credential = v;
        } else if let Some(v) = part.strip_prefix("SignedHeaders=") {
            signed_headers = v;
        } else if let Some(v) = part.strip_prefix("Signature=") {
            signature = v;
        }
    }
    if credential.is_empty() || signed_headers.is_empty() || signature.is_empty() {
        return Err(AuthError::Malformed);
    }
    let cparts: Vec<&str> = credential.split('/').collect();
    if cparts.len() != 5 || cparts[4] != "aws4_request" {
        return Err(AuthError::Malformed);
    }
    let amz_date = headers.get("x-amz-date").unwrap_or("").to_string();
    let payload_hash = headers
        .get("x-amz-content-sha256")
        .unwrap_or("UNSIGNED-PAYLOAD")
        .to_string();
    Ok(AuthInfo {
        access_key: cparts[0].to_string(),
        date: cparts[1].to_string(),
        region: cparts[2].to_string(),
        service: cparts[3].to_string(),
        signed_headers: signed_headers.split(';').map(|s| s.to_string()).collect(),
        signature: signature.to_string(),
        amz_date,
        payload_hash,
    })
}

pub fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> [u8; 32] {
    let mut k = Vec::with_capacity(4 + secret.len());
    k.extend_from_slice(b"AWS4");
    k.extend_from_slice(secret.as_bytes());
    let kd = hmac_sha256(&k, date.as_bytes());
    let kr = hmac_sha256(&kd, region.as_bytes());
    let ks = hmac_sha256(&kr, service.as_bytes());
    hmac_sha256(&ks, b"aws4_request")
}

pub fn canonical_request(
    method: &str,
    raw_path: &str,
    query_raw: &str,
    headers: &Headers,
    signed_headers: &[String],
    payload_hash: &str,
) -> String {
    let parts = parse_query(query_raw);
    canonical_request_pairs(
        method,
        raw_path,
        &parts,
        headers,
        signed_headers,
        payload_hash,
    )
}

// Same as canonical_request but takes already-parsed (decoded) query pairs.
// Presigned-URL verification uses this to drop the X-Amz-Signature parameter
// from the canonical query before signing.
pub fn canonical_request_pairs(
    method: &str,
    raw_path: &str,
    query_pairs: &[(String, String)],
    headers: &Headers,
    signed_headers: &[String],
    payload_hash: &str,
) -> String {
    // Canonical URI: the path, percent-encoded per SigV4 (S3: single-encoded).
    // We re-encode from the decoded form to ensure canonical output regardless
    // of how the client presented it on the wire.
    let decoded_path = crate::url::percent_decode_str(raw_path);
    let canonical_uri = encode_path_sigv4(&decoded_path);

    // Canonical query string: encode each key/value, sort by encoded key.
    let mut encoded: Vec<(String, String)> = query_pairs
        .iter()
        .map(|(k, v)| (encode_component(k), encode_component(v)))
        .collect();
    encoded.sort_by(|a, b| match a.0.cmp(&b.0) {
        std::cmp::Ordering::Equal => a.1.cmp(&b.1),
        o => o,
    });
    let canonical_query = encoded
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&");

    // Canonical headers.
    let mut canonical_headers = String::new();
    for name in signed_headers {
        let v = headers.get(name).unwrap_or("");
        let collapsed = collapse_ws(v);
        canonical_headers.push_str(&name.to_ascii_lowercase());
        canonical_headers.push(':');
        canonical_headers.push_str(&collapsed);
        canonical_headers.push('\n');
    }
    let signed_str = signed_headers.join(";");

    format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method, canonical_uri, canonical_query, canonical_headers, signed_str, payload_hash
    )
}

fn collapse_ws(s: &str) -> String {
    let s = s.trim();
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c == ' ' || c == '\t' {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out
}

pub fn string_to_sign(amz_date: &str, scope: &str, canonical_req: &str) -> String {
    let h = hex(&sha256(canonical_req.as_bytes()));
    format!("AWS4-HMAC-SHA256\n{}\n{}\n{}", amz_date, scope, h)
}

pub fn verify(
    method: &str,
    raw_path: &str,
    query_raw: &str,
    headers: &Headers,
    secret: &str,
    info: &AuthInfo,
) -> Result<(), AuthError> {
    let canon = canonical_request(
        method,
        raw_path,
        query_raw,
        headers,
        &info.signed_headers,
        &info.payload_hash,
    );
    let scope = format!(
        "{}/{}/{}/aws4_request",
        info.date, info.region, info.service
    );
    let sts = string_to_sign(&info.amz_date, &scope, &canon);
    let key = signing_key(secret, &info.date, &info.region, &info.service);
    let sig = hex(&hmac_sha256(&key, sts.as_bytes()));
    if constant_time_eq(sig.as_bytes(), info.signature.as_bytes()) {
        Ok(())
    } else {
        // Deliberately no canonical request / signature values in the log.
        eprintln!(
            "[sigv4] signature mismatch for access key {} ({} {})",
            info.access_key, method, raw_path
        );
        Err(AuthError::BadSignature)
    }
}

// ---- Presigned URLs (query-string authentication) ----
//
// A presigned request carries all SigV4 parameters in the query string instead
// of the Authorization header:
//   X-Amz-Algorithm=AWS4-HMAC-SHA256
//   X-Amz-Credential=<access>/<date>/<region>/<service>/aws4_request
//   X-Amz-Date=<amz-date>
//   X-Amz-Expires=<seconds>
//   X-Amz-SignedHeaders=<h1;h2;...>
//   X-Amz-Signature=<hex>
// The payload is always UNSIGNED-PAYLOAD, and the string-to-sign is built from
// a canonical query that excludes X-Amz-Signature itself.

pub struct PresignedInfo {
    pub info: AuthInfo,
    // Validity window in seconds, from X-Amz-Expires.
    pub expires: u64,
}

// True if the query string looks like a presigned SigV4 request.
pub fn is_presigned(query_raw: &str) -> bool {
    parse_query(query_raw)
        .iter()
        .any(|(k, _)| k == "X-Amz-Signature")
}

pub fn parse_presigned(query_raw: &str) -> Result<PresignedInfo, AuthError> {
    let pairs = parse_query(query_raw);
    let mut algorithm = "";
    let mut credential = "";
    let mut amz_date = "";
    let mut expires = "";
    let mut signed_headers = "";
    let mut signature = "";
    for (k, v) in &pairs {
        match k.as_str() {
            "X-Amz-Algorithm" => algorithm = v,
            "X-Amz-Credential" => credential = v,
            "X-Amz-Date" => amz_date = v,
            "X-Amz-Expires" => expires = v,
            "X-Amz-SignedHeaders" => signed_headers = v,
            "X-Amz-Signature" => signature = v,
            _ => {}
        }
    }
    if algorithm != "AWS4-HMAC-SHA256" {
        return Err(AuthError::Malformed);
    }
    if credential.is_empty()
        || amz_date.is_empty()
        || signed_headers.is_empty()
        || signature.is_empty()
    {
        return Err(AuthError::Malformed);
    }
    let expires: u64 = expires.parse().map_err(|_| AuthError::Malformed)?;
    let cparts: Vec<&str> = credential.split('/').collect();
    if cparts.len() != 5 || cparts[4] != "aws4_request" {
        return Err(AuthError::Malformed);
    }
    Ok(PresignedInfo {
        info: AuthInfo {
            access_key: cparts[0].to_string(),
            date: cparts[1].to_string(),
            region: cparts[2].to_string(),
            service: cparts[3].to_string(),
            signed_headers: signed_headers.split(';').map(|s| s.to_string()).collect(),
            signature: signature.to_string(),
            amz_date: amz_date.to_string(),
            payload_hash: "UNSIGNED-PAYLOAD".to_string(),
        },
        expires,
    })
}

pub fn verify_presigned(
    method: &str,
    raw_path: &str,
    query_raw: &str,
    headers: &Headers,
    secret: &str,
    info: &AuthInfo,
) -> Result<(), AuthError> {
    // Canonical query excludes X-Amz-Signature; everything else is signed.
    let pairs: Vec<(String, String)> = parse_query(query_raw)
        .into_iter()
        .filter(|(k, _)| k != "X-Amz-Signature")
        .collect();
    let canon = canonical_request_pairs(
        method,
        raw_path,
        &pairs,
        headers,
        &info.signed_headers,
        &info.payload_hash,
    );
    let scope = format!(
        "{}/{}/{}/aws4_request",
        info.date, info.region, info.service
    );
    let sts = string_to_sign(&info.amz_date, &scope, &canon);
    let key = signing_key(secret, &info.date, &info.region, &info.service);
    let sig = hex(&hmac_sha256(&key, sts.as_bytes()));
    if constant_time_eq(sig.as_bytes(), info.signature.as_bytes()) {
        Ok(())
    } else {
        // Deliberately no canonical request / signature values in the log.
        eprintln!(
            "[sigv4-presign] signature mismatch for access key {} ({} {})",
            info.access_key, method, raw_path
        );
        Err(AuthError::BadSignature)
    }
}

// Per-chunk signing context for STREAMING-AWS4-HMAC-SHA256-PAYLOAD.
// Each chunk: string_to_sign = "AWS4-HMAC-SHA256-PAYLOAD\n<amzdate>\n<scope>\n<prev-sig>\n<sha256-of-empty>\n<sha256-of-chunk>"
//             chunk_signature = hex(HMAC(signing_key, string_to_sign))
// The seed signature is from the Authorization header; each verified chunk's
// computed signature becomes prev-sig for the next.
pub struct ChunkContext {
    pub signing_key: [u8; 32],
    pub amz_date: String,
    pub scope: String,
    pub prev_signature: String,
    pub empty_hash: String,
}

impl ChunkContext {
    pub fn new(secret: &str, info: &AuthInfo) -> Self {
        let key = signing_key(secret, &info.date, &info.region, &info.service);
        let scope = format!(
            "{}/{}/{}/aws4_request",
            info.date, info.region, info.service
        );
        Self {
            signing_key: key,
            amz_date: info.amz_date.clone(),
            scope,
            prev_signature: info.signature.clone(),
            empty_hash: hex(&sha256(b"")),
        }
    }

    pub fn expected_signature(&self, chunk_data: &[u8]) -> String {
        let chunk_hash = hex(&sha256(chunk_data));
        let sts = format!(
            "AWS4-HMAC-SHA256-PAYLOAD\n{}\n{}\n{}\n{}\n{}",
            self.amz_date, self.scope, self.prev_signature, self.empty_hash, chunk_hash
        );
        hex(&hmac_sha256(&self.signing_key, sts.as_bytes()))
    }

    pub fn verify_and_advance(&mut self, chunk_data: &[u8], got: &str) -> Result<(), AuthError> {
        let expected = self.expected_signature(chunk_data);
        if constant_time_eq(expected.as_bytes(), got.as_bytes()) {
            self.prev_signature = expected;
            Ok(())
        } else {
            eprintln!("[sigv4-chunk] chunk signature mismatch");
            Err(AuthError::BadSignature)
        }
    }
}

// ---- Body hash verification (x-amz-content-sha256) ----

// True if `s` is a lowercase/uppercase hex SHA-256 digest (as opposed to one
// of the UNSIGNED-PAYLOAD / STREAMING-* markers).
pub fn is_hex_digest(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

// io::Error payload: the body hashed to something other than the declared
// x-amz-content-sha256. Surfaced to clients as 400 XAmzContentSHA256Mismatch.
#[derive(Debug)]
pub struct ContentSha256Mismatch;

impl std::fmt::Display for ContentSha256Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("body does not match x-amz-content-sha256")
    }
}
impl std::error::Error for ContentSha256Mismatch {}

// io::Error payload: the connection ended before Content-Length bytes of a
// hash-verified body arrived. Surfaced as 400 IncompleteBody.
#[derive(Debug)]
pub struct IncompleteBody;

impl std::fmt::Display for IncompleteBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("body ended before Content-Length bytes")
    }
}
impl std::error::Error for IncompleteBody {}

pub fn is_content_sha256_mismatch(e: &io::Error) -> bool {
    e.get_ref()
        .map(|inner| inner.is::<ContentSha256Mismatch>())
        .unwrap_or(false)
}

pub fn is_incomplete_body(e: &io::Error) -> bool {
    e.get_ref()
        .map(|inner| inner.is::<IncompleteBody>())
        .unwrap_or(false)
}

struct PayloadCheck {
    hasher: Sha256,
    expected: String,
    remaining: u64,
}

// BufRead adapter that sits between the socket and the S3 handlers. In
// pass-through mode it is transparent. In verifying mode it hashes the first
// `content_length` bytes that flow through it and, once the last of them has
// been handed out, compares the digest with the declared one: on mismatch the
// read that would have delivered the final bytes fails with
// ContentSha256Mismatch instead. Handlers propagate that error before they
// finalize anything (the PUT-object writer aborts, so its temp file is never
// renamed into place). Whatever the handler does, no code path sees a
// complete body that did not hash correctly.
//
// Only fixed-length bodies are verified this way; aws-chunked bodies carry
// per-chunk signatures (ChunkContext) instead.
pub struct PayloadHashReader<R> {
    inner: R,
    check: Option<PayloadCheck>,
    // Errors detected inside `consume` (which cannot fail) are surfaced on the
    // next read / fill_buf.
    pending: Option<io::Error>,
}

impl<R: BufRead> PayloadHashReader<R> {
    // Pass-through: nothing is checked.
    pub fn passthrough(inner: R) -> Self {
        Self {
            inner,
            check: None,
            pending: None,
        }
    }

    // Verify that the next `content_length` bytes hash to `expected_hex`. A
    // zero-length body is checked right here, since no read will ever happen.
    pub fn verifying(inner: R, expected_hex: &str, content_length: u64) -> io::Result<Self> {
        let mut r = Self {
            inner,
            check: Some(PayloadCheck {
                hasher: Sha256::new(),
                expected: expected_hex.to_ascii_lowercase(),
                remaining: content_length,
            }),
            pending: None,
        };
        if content_length == 0 {
            r.observe(&[])?;
        }
        Ok(r)
    }

    fn finish(check: PayloadCheck) -> io::Result<()> {
        let got = hex(&check.hasher.finalize());
        if constant_time_eq(got.as_bytes(), check.expected.as_bytes()) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                ContentSha256Mismatch,
            ))
        }
    }

    // Feed bytes that were handed to the caller. Bytes past `remaining` belong
    // to something else (a pipelined request) and are not part of the body.
    fn observe(&mut self, data: &[u8]) -> io::Result<()> {
        let Some(check) = self.check.as_mut() else {
            return Ok(());
        };
        let n = (data.len() as u64).min(check.remaining) as usize;
        check.hasher.update(&data[..n]);
        check.remaining -= n as u64;
        if check.remaining == 0 {
            let check = self.check.take().expect("check present");
            Self::finish(check)?;
        }
        Ok(())
    }

    fn take_pending(&mut self) -> io::Result<()> {
        match self.pending.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn eof(&self) -> io::Result<()> {
        if self.check.is_some() {
            Err(io::Error::new(io::ErrorKind::UnexpectedEof, IncompleteBody))
        } else {
            Ok(())
        }
    }
}

impl<R: BufRead> Read for PayloadHashReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.take_pending()?;
        let n = self.inner.read(out)?;
        if n == 0 {
            self.eof()?;
            return Ok(0);
        }
        self.observe(&out[..n])?;
        Ok(n)
    }
}

impl<R: BufRead> BufRead for PayloadHashReader<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.take_pending()?;
        let at_eof = self.inner.fill_buf()?.is_empty();
        if at_eof {
            self.eof()?;
        }
        self.inner.fill_buf()
    }

    fn consume(&mut self, amt: usize) {
        if self.check.is_some() {
            // The bytes being consumed are still in the inner buffer; hash
            // them before they go away.
            let observed: Vec<u8> = match self.inner.fill_buf() {
                Ok(b) => b[..amt.min(b.len())].to_vec(),
                Err(_) => Vec::new(),
            };
            if let Err(e) = self.observe(&observed) {
                self.pending = Some(e);
            }
        }
        self.inner.consume(amt);
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    // AWS official SigV4 test: get-vanilla
    // https://docs.aws.amazon.com/general/latest/gr/sigv4-signed-request-examples.html
    #[test]
    fn signing_key_official() {
        let key = signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "iam",
        );
        assert_eq!(
            hex(&key),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
    }

    fn headers_from(pairs: &[(&str, &str)]) -> Headers {
        let mut h = Headers::default();
        for (k, v) in pairs {
            h.insert(k, v);
        }
        h
    }

    #[test]
    fn parse_authorization_basic() {
        let h = headers_from(&[
            (
                "Authorization",
                "AWS4-HMAC-SHA256 Credential=AKIA/20240101/us-east-1/s3/aws4_request, \
                 SignedHeaders=host;x-amz-date, Signature=deadbeef",
            ),
            ("x-amz-date", "20240101T000000Z"),
            ("x-amz-content-sha256", "abc123"),
        ]);
        let info = parse_authorization(&h).unwrap();
        assert_eq!(info.access_key, "AKIA");
        assert_eq!(info.date, "20240101");
        assert_eq!(info.region, "us-east-1");
        assert_eq!(info.service, "s3");
        assert_eq!(info.signed_headers, vec!["host", "x-amz-date"]);
        assert_eq!(info.signature, "deadbeef");
        assert_eq!(info.amz_date, "20240101T000000Z");
        assert_eq!(info.payload_hash, "abc123");
    }

    #[test]
    fn parse_authorization_missing_header() {
        let h = Headers::default();
        assert!(matches!(parse_authorization(&h), Err(AuthError::Missing)));
    }

    #[test]
    fn parse_authorization_malformed() {
        let h = headers_from(&[("Authorization", "AWS2-FOO Credential=x")]);
        assert!(matches!(parse_authorization(&h), Err(AuthError::Malformed)));

        // Right algorithm, missing pieces.
        let h = headers_from(&[(
            "Authorization",
            "AWS4-HMAC-SHA256 Credential=a/b/c/d/aws4_request",
        )]);
        assert!(matches!(parse_authorization(&h), Err(AuthError::Malformed)));
    }

    #[test]
    fn parse_authorization_default_payload_hash() {
        let h = headers_from(&[(
            "Authorization",
            "AWS4-HMAC-SHA256 Credential=K/20240101/us-east-1/s3/aws4_request, \
             SignedHeaders=host, Signature=abc",
        )]);
        let info = parse_authorization(&h).unwrap();
        assert_eq!(info.payload_hash, "UNSIGNED-PAYLOAD");
    }

    #[test]
    fn canonical_request_orders_query_and_lowercases_headers() {
        let h = headers_from(&[("Host", "example.com"), ("X-Amz-Date", "20240101T000000Z")]);
        let canon = canonical_request(
            "GET",
            "/bucket/key",
            "b=2&a=1",
            &h,
            &["host".to_string(), "x-amz-date".to_string()],
            "UNSIGNED-PAYLOAD",
        );
        let expected = "GET\n/bucket/key\na=1&b=2\nhost:example.com\nx-amz-date:20240101T000000Z\n\nhost;x-amz-date\nUNSIGNED-PAYLOAD";
        assert_eq!(canon, expected);
    }

    #[test]
    fn canonical_request_collapses_header_whitespace() {
        let h = headers_from(&[("Host", "  ex  ample  ")]);
        let canon = canonical_request(
            "GET",
            "/",
            "",
            &h,
            &["host".to_string()],
            "UNSIGNED-PAYLOAD",
        );
        assert!(canon.contains("host:ex ample\n"));
    }

    // End-to-end SigV4 verify: build a signed request, then verify accepts the
    // correct signature and rejects a tampered one.
    #[test]
    fn verify_roundtrip_accepts_and_rejects() {
        let secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
        let date = "20240101";
        let region = "us-east-1";
        let service = "s3";
        let amz_date = "20240101T000000Z";
        let payload_hash = "UNSIGNED-PAYLOAD";
        let signed_headers = vec!["host".to_string(), "x-amz-date".to_string()];

        let h = headers_from(&[
            ("Host", "example.com"),
            ("X-Amz-Date", amz_date),
            ("x-amz-content-sha256", payload_hash),
        ]);

        let canon = canonical_request("GET", "/b/k", "", &h, &signed_headers, payload_hash);
        let scope = format!("{}/{}/{}/aws4_request", date, region, service);
        let sts = string_to_sign(amz_date, &scope, &canon);
        let key = signing_key(secret, date, region, service);
        let sig = hex(&hmac_sha256(&key, sts.as_bytes()));

        let info = AuthInfo {
            access_key: "AKIA".into(),
            date: date.into(),
            region: region.into(),
            service: service.into(),
            signed_headers: signed_headers.clone(),
            signature: sig.clone(),
            amz_date: amz_date.into(),
            payload_hash: payload_hash.into(),
        };
        assert!(verify("GET", "/b/k", "", &h, secret, &info).is_ok());

        // Tampered signature: must be rejected.
        let bad = AuthInfo {
            signature: "0".repeat(sig.len()),
            ..info
        };
        assert!(matches!(
            verify("GET", "/b/k", "", &h, secret, &bad),
            Err(AuthError::BadSignature)
        ));
    }

    #[test]
    fn is_presigned_detects_query_signature() {
        assert!(is_presigned("X-Amz-Signature=abc&foo=bar"));
        assert!(!is_presigned("foo=bar"));
        assert!(!is_presigned(""));
    }

    #[test]
    fn parse_presigned_malformed() {
        // Wrong algorithm.
        assert!(matches!(
            parse_presigned("X-Amz-Algorithm=AWS2&X-Amz-Signature=x&X-Amz-Credential=a/b/c/d/aws4_request&X-Amz-Date=d&X-Amz-SignedHeaders=host"),
            Err(AuthError::Malformed)
        ));
        // Non-numeric expires.
        assert!(matches!(
            parse_presigned("X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Signature=x&X-Amz-Credential=a/20240101/us-east-1/s3/aws4_request&X-Amz-Date=d&X-Amz-Expires=soon&X-Amz-SignedHeaders=host"),
            Err(AuthError::Malformed)
        ));
    }

    // End-to-end presigned verify: build a signed query, verify accepts the
    // correct signature and rejects a tampered one.
    #[test]
    fn presigned_roundtrip_accepts_and_rejects() {
        let secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
        let base = "X-Amz-Algorithm=AWS4-HMAC-SHA256\
            &X-Amz-Credential=AKIA/20240101/us-east-1/s3/aws4_request\
            &X-Amz-Date=20240101T000000Z&X-Amz-Expires=3600&X-Amz-SignedHeaders=host";
        let h = headers_from(&[("Host", "example.com")]);

        // Compute the expected signature exactly as verify_presigned does.
        let pairs: Vec<(String, String)> = parse_query(base);
        let canon = canonical_request_pairs(
            "GET",
            "/b/k",
            &pairs,
            &h,
            &["host".to_string()],
            "UNSIGNED-PAYLOAD",
        );
        let scope = "20240101/us-east-1/s3/aws4_request";
        let sts = string_to_sign("20240101T000000Z", scope, &canon);
        let key = signing_key(secret, "20240101", "us-east-1", "s3");
        let sig = hex(&hmac_sha256(&key, sts.as_bytes()));

        let good = format!("{}&X-Amz-Signature={}", base, sig);
        let pre = parse_presigned(&good).unwrap();
        assert_eq!(pre.expires, 3600);
        assert_eq!(pre.info.access_key, "AKIA");
        assert_eq!(pre.info.payload_hash, "UNSIGNED-PAYLOAD");
        assert!(verify_presigned("GET", "/b/k", &good, &h, secret, &pre.info).is_ok());

        // Tampered signature: must be rejected.
        let bad = format!("{}&X-Amz-Signature={}", base, "0".repeat(sig.len()));
        let badpre = parse_presigned(&bad).unwrap();
        assert!(matches!(
            verify_presigned("GET", "/b/k", &bad, &h, secret, &badpre.info),
            Err(AuthError::BadSignature)
        ));
    }

    #[test]
    fn constant_time_eq_lengths_and_content() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn chunk_context_signs_and_advances() {
        let secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
        let info = AuthInfo {
            access_key: "AKIA".into(),
            date: "20240101".into(),
            region: "us-east-1".into(),
            service: "s3".into(),
            signed_headers: vec!["host".into()],
            signature: "seed".into(),
            amz_date: "20240101T000000Z".into(),
            payload_hash: "STREAMING-AWS4-HMAC-SHA256-PAYLOAD".into(),
        };
        let mut ctx = ChunkContext::new(secret, &info);
        assert_eq!(ctx.prev_signature, "seed");
        let exp = ctx.expected_signature(b"hello");
        assert!(ctx.verify_and_advance(b"hello", &exp).is_ok());
        // prev_signature must advance to the just-verified chunk signature.
        assert_eq!(ctx.prev_signature, exp);
        // A mismatched signature must fail.
        assert!(ctx.verify_and_advance(b"world", "00").is_err());
    }

    // "YYYYMMDDTHHMMSSZ" for a Unix timestamp (the inverse of parse_amz_date).
    fn amz_date_at(secs: u64) -> String {
        crate::util::iso8601(secs)
            .replace(['-', ':'], "")
            .replace(".000Z", "Z")
    }

    #[test]
    fn request_time_within_window_is_accepted() {
        let now = 1_700_000_000;
        // One minute old, one minute ahead: both fine.
        assert!(check_request_time(&amz_date_at(now - 60), now).is_ok());
        assert!(check_request_time(&amz_date_at(now + 60), now).is_ok());
        // Exactly at the edge is still fine.
        assert!(check_request_time(&amz_date_at(now - MAX_CLOCK_SKEW_SECS), now).is_ok());
    }

    #[test]
    fn request_time_outside_window_is_rejected() {
        let now = 1_700_000_000;
        assert!(matches!(
            check_request_time(&amz_date_at(now - 20 * 60), now),
            Err(AuthError::RequestTimeTooSkewed)
        ));
        assert!(matches!(
            check_request_time(&amz_date_at(now + 20 * 60), now),
            Err(AuthError::RequestTimeTooSkewed)
        ));
    }

    #[test]
    fn request_time_requires_a_parsable_date() {
        assert!(matches!(check_request_time("", 0), Err(AuthError::BadDate)));
        assert!(matches!(
            check_request_time("2024-01-01T00:00:00Z", 0),
            Err(AuthError::BadDate)
        ));
    }

    #[test]
    fn is_hex_digest_only_matches_sha256_hex() {
        assert!(is_hex_digest(&hex(&sha256(b""))));
        assert!(is_hex_digest(&hex(&sha256(b"")).to_ascii_uppercase()));
        assert!(!is_hex_digest("UNSIGNED-PAYLOAD"));
        assert!(!is_hex_digest("STREAMING-AWS4-HMAC-SHA256-PAYLOAD"));
        assert!(!is_hex_digest("abc"));
        assert!(!is_hex_digest(&"g".repeat(64)));
    }

    fn read_all<R: BufRead>(r: &mut PayloadHashReader<R>, len: usize) -> io::Result<Vec<u8>> {
        // Mirror how FixedReader drives the transport: bounded reads, never
        // past Content-Length.
        let mut out = Vec::new();
        let mut buf = [0u8; 4];
        while out.len() < len {
            let cap = buf.len().min(len - out.len());
            let n = r.read(&mut buf[..cap])?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        Ok(out)
    }

    #[test]
    fn payload_hash_reader_accepts_matching_body() {
        let body = b"hello world";
        let digest = hex(&sha256(body));
        let inner = io::BufReader::new(io::Cursor::new(body.to_vec()));
        let mut r = PayloadHashReader::verifying(inner, &digest, body.len() as u64).unwrap();
        assert_eq!(read_all(&mut r, body.len()).unwrap(), body);
        // Uppercase digests are accepted too.
        let inner = io::BufReader::new(io::Cursor::new(body.to_vec()));
        let mut r = PayloadHashReader::verifying(inner, &digest.to_ascii_uppercase(), 11).unwrap();
        assert_eq!(read_all(&mut r, body.len()).unwrap(), body);
    }

    #[test]
    fn payload_hash_reader_rejects_replayed_signature_with_other_body() {
        // The digest was computed for one body, a different one arrives.
        let digest = hex(&sha256(b"hello world"));
        let body = b"hello w0rld";
        let inner = io::BufReader::new(io::Cursor::new(body.to_vec()));
        let mut r = PayloadHashReader::verifying(inner, &digest, body.len() as u64).unwrap();
        let e = read_all(&mut r, body.len()).unwrap_err();
        assert!(is_content_sha256_mismatch(&e));
        assert!(!is_incomplete_body(&e));
    }

    #[test]
    fn payload_hash_reader_checks_empty_body_up_front() {
        let inner = io::BufReader::new(io::Cursor::new(Vec::new()));
        assert!(PayloadHashReader::verifying(inner, &hex(&sha256(b"")), 0).is_ok());
        let inner = io::BufReader::new(io::Cursor::new(Vec::new()));
        let e = match PayloadHashReader::verifying(inner, &hex(&sha256(b"x")), 0) {
            Ok(_) => panic!("empty body with wrong digest must be rejected"),
            Err(e) => e,
        };
        assert!(is_content_sha256_mismatch(&e));
    }

    #[test]
    fn payload_hash_reader_rejects_truncated_body() {
        let body = b"hello world";
        let digest = hex(&sha256(body));
        // Declared 11 bytes, only 5 on the wire.
        let inner = io::BufReader::new(io::Cursor::new(body[..5].to_vec()));
        let mut r = PayloadHashReader::verifying(inner, &digest, body.len() as u64).unwrap();
        let e = read_all(&mut r, body.len()).unwrap_err();
        assert!(is_incomplete_body(&e));
    }

    #[test]
    fn payload_hash_reader_passthrough_is_transparent() {
        let inner = io::BufReader::new(io::Cursor::new(b"abc".to_vec()));
        let mut r = PayloadHashReader::passthrough(inner);
        let mut out = String::new();
        r.read_to_string(&mut out).unwrap();
        assert_eq!(out, "abc");
        // EOF on a pass-through reader is just EOF.
        assert_eq!(r.read(&mut [0u8; 1]).unwrap(), 0);
    }

    #[test]
    fn payload_hash_reader_hashes_via_bufread_consume() {
        let body = b"line1\nline2\n";
        let digest = hex(&sha256(body));
        let inner = io::BufReader::new(io::Cursor::new(body.to_vec()));
        let mut r = PayloadHashReader::verifying(inner, &digest, body.len() as u64).unwrap();
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        r.read_line(&mut line).unwrap();
        assert_eq!(line, "line1\nline2\n");
        // Wrong digest: consume() cannot fail, so the mismatch detected while
        // consuming the last body byte surfaces on the following call.
        let inner = io::BufReader::new(io::Cursor::new(body.to_vec()));
        let mut r = PayloadHashReader::verifying(inner, &hex(&sha256(b"other")), 12).unwrap();
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        r.read_line(&mut line).unwrap();
        let e = r.fill_buf().map(|_| ()).unwrap_err();
        assert!(is_content_sha256_mismatch(&e));
    }

    // Build an aws-chunked body whose chunk signatures are valid for `ctx`.
    fn signed_chunked_body(ctx: &ChunkContext, chunks: &[&[u8]]) -> Vec<u8> {
        let mut prev = ctx.prev_signature.clone();
        let mut out = Vec::new();
        for data in chunks.iter().chain(std::iter::once(&&b""[..])) {
            let tmp = ChunkContext {
                signing_key: ctx.signing_key,
                amz_date: ctx.amz_date.clone(),
                scope: ctx.scope.clone(),
                prev_signature: prev.clone(),
                empty_hash: ctx.empty_hash.clone(),
            };
            let sig = tmp.expected_signature(data);
            out.extend_from_slice(
                format!("{:x};chunk-signature={}\r\n", data.len(), sig).as_bytes(),
            );
            out.extend_from_slice(data);
            out.extend_from_slice(b"\r\n");
            prev = sig;
        }
        out
    }

    fn streaming_info() -> AuthInfo {
        AuthInfo {
            access_key: "AKIA".into(),
            date: "20240101".into(),
            region: "us-east-1".into(),
            service: "s3".into(),
            signed_headers: vec!["host".into()],
            signature: "seed".into(),
            amz_date: "20240101T000000Z".into(),
            payload_hash: "STREAMING-AWS4-HMAC-SHA256-PAYLOAD".into(),
        }
    }

    fn chunked_request(body: Vec<u8>) -> crate::http::Request<io::BufReader<io::Cursor<Vec<u8>>>> {
        let mut headers = Headers::default();
        headers.insert("Content-Encoding", "aws-chunked");
        crate::http::Request {
            method: "POST".into(),
            raw_path: "/b".into(),
            path: "/b".into(),
            query_raw: "delete".into(),
            headers,
            reader: io::BufReader::new(io::Cursor::new(body)),
            chunk_ctx: None,
        }
    }

    // read_body_all (used by the POST routes) must pick up the chunk context
    // carried on the request and verify every chunk signature.
    #[test]
    fn read_body_all_verifies_chunks_from_request_context() {
        let secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
        let info = streaming_info();
        let body = signed_chunked_body(&ChunkContext::new(secret, &info), &[b"hello", b"world"]);

        // Correct signatures: accepted, context consumed.
        let mut req = chunked_request(body.clone());
        req.chunk_ctx = Some(ChunkContext::new(secret, &info));
        assert_eq!(crate::s3::read_body_all(&mut req).unwrap(), b"helloworld");
        assert!(req.chunk_ctx.is_none());

        // Same bytes but signed under a different seed: rejected.
        let other = AuthInfo {
            signature: "other-seed".into(),
            ..streaming_info()
        };
        let mut req = chunked_request(body);
        req.chunk_ctx = Some(ChunkContext::new(secret, &other));
        assert!(crate::s3::read_body_all(&mut req).is_err());
    }
}
