# minimail — Implementation Specification

Definitive build spec. Independent agents implement each module in parallel, seeing only this
document. Every public signature here is the contract; copy it verbatim. If prose and a signature
disagree, the signature wins.

House rules (inherited from minibucket, non-negotiable): hand-rolled on `std`, no tokio/serde/hyper.
Thread-per-connection blocking IO. `#[cfg(test)] mod tests` at the bottom of each module. One-line
`//` file-header comment naming the spec + scope. `eprintln!` with bracketed `[tag]` logging. Fatal
setup = `.expect("terse lowercase")`; bad user input = `eprintln!` + `std::process::exit(2)`.

---

## 1. Overview & non-goals

minimail is a MailHog/Mailpit-style development mail sink. It speaks SMTP on one port, accepts every
message, and **never delivers onward**. Captured mail is retrieved through an embedded web UI and a
JSON API over HTTP on a second port. Storage is plain files under `--root`: one byte-exact `.eml`
per message plus a `.json` sidecar (envelope + parsed summary). `ls`/`cat`/`grep`/`backup` work with
standard tools.

**In scope:** full MIME parsing (header unfolding, RFC 2047 encoded-words, multipart trees,
quoted-printable + base64, attachment extraction/download, text/plain + text/html views); SMTP AUTH
PLAIN + LOGIN against a `KEY=SECRET` credentials file (or `--anonymous`); HTTP Basic auth for UI/API;
optional TLS (STARTTLS + implicit SMTPS) behind a default-OFF `tls` Cargo feature (rustls +
rustls-pemfile, the only two direct deps and only when the feature is on); SSE live updates.

**Non-goals:** POP3, IMAP, onward relay/delivery, spam/virus filtering, cert generation, HTTPS for
the web UI (dev tool; TLS is SMTP-only), multi-mailbox/multi-tenant layout, a database, any external
crate in the default build, graceful shutdown, a thread pool or connection cap.

Target: ~5.0–5.5k lines of Rust, readable in an afternoon. Small beats clever.

---

## 2. CLI surface

Hand-rolled `parse_args()` in `main.rs` (minibucket shape: default struct literal, then
`while let Some(a) = args.next()` matching `a.as_str()`). Value flags fall back to the existing
default via `unwrap_or`/`unwrap_or_default` (never a "missing value" hard error). Unknown flag →
`eprintln!("unknown arg: {}", a); std::process::exit(2)`. `--help`/`-h` prints usage then `exit(0)`.

### Flags

| Flag | Metavar | Default | Meaning |
|------|---------|---------|---------|
| `--smtp-bind` | ADDR | `127.0.0.1:1025` | SMTP listener address |
| `--http-bind` | ADDR | `127.0.0.1:8025` | HTTP UI/API listener address |
| `--root` | DIR | `./mail` | storage root (messages under `<root>/messages/`) |
| `--credentials` | FILE | (none) | `KEY=SECRET` file; loaded via `Credentials::load_file`, `exit(2)` on error |
| `--user` | KEY | (none) | inline credential key; paired with `--password` |
| `--password` | SECRET | (none) | inline credential secret; paired with `--user` |
| `--anonymous` | — | off | disable all auth (SMTP AUTH not required; HTTP Basic bypassed) |
| `--hostname` | NAME | system hostname or `minimail` | SMTP greeting / EHLO name |
| `--max-size` | BYTES | `26214400` (25 MiB) | ESMTP SIZE limit; oversize DATA → 552 |
| `--max-messages` | N | (unlimited) | retention cap; oldest pruned after each save |
| `--tls-cert` | FILE | (none) | PEM cert chain (**`tls` feature only**; otherwise unknown arg → exit 2) |
| `--tls-key` | FILE | (none) | PEM private key (**`tls` feature only**) |
| `--smtps-bind` | ADDR | (none) | implicit-TLS SMTP listener (**`tls` feature only**) |
| `--help`, `-h` | — | — | print usage, `exit(0)` |

`--user`/`--password` are buffered in `pending_user`/`pending_password` locals and flushed together
at the **end of each loop iteration** (`if let (Some(u), Some(p)) = ...`). If exactly one is set after
the loop: `eprintln!("--user and --password must be provided together"); std::process::exit(2)`.

Credential resolution after the loop (minibucket parity):
`if !cfg.anonymous && cfg.creds.is_empty() { cfg.creds.add("minimail", "minimail"); }`

`--tls-cert`/`--tls-key`/`--smtps-bind` match arms and their `Config` fields are entirely
`#[cfg(feature = "tls")]`. On the default build they are **unrecognized args → exit(2)** (documented
cost of a zero-unused-warning build). If `--smtps-bind` is set but cert/key are missing:
`eprintln!("--smtps-bind requires --tls-cert and --tls-key"); std::process::exit(2)`.

### `--help` text (byte for byte)

```
minimail — a tiny dev SMTP sink with a web UI + JSON API

Usage: minimail [options]

  --smtp-bind ADDR         default 127.0.0.1:1025
  --http-bind ADDR         default 127.0.0.1:8025
  --root DIR               default ./mail
  --credentials FILE       KEY=SECRET file for SMTP AUTH and HTTP Basic
  --user KEY               inline credential key (with --password)
  --password SECRET        inline credential secret (with --user)
  --anonymous              disable all auth (SMTP AUTH + HTTP Basic)
  --hostname NAME          SMTP greeting name (default: system hostname)
  --max-size BYTES         max message size, default 26214400 (25 MiB)
  --max-messages N         keep only the newest N messages
  --tls-cert FILE          PEM certificate chain (requires --features tls)
  --tls-key FILE           PEM private key (requires --features tls)
  --smtps-bind ADDR        implicit-TLS SMTP listener (requires --features tls)
  -h, --help               show this help and exit
```

On a `--features tls` build the three TLS lines are always shown. On the default build they are still
shown in `--help` (so users learn the feature exists) but the flags error at parse time — this is
stated in the README.

### Startup banner (stderr, byte for byte modulo interpolation)

```
minimail 0.1.0
  SMTP  smtp://127.0.0.1:1025
  HTTP  http://127.0.0.1:8025
  root  ./mail
  auth  1 credential(s)          # or: anonymous (no auth)
```

Additional conditional lines, each two-space indented, in this order when present:
- `  hostname  mail.local`
- `  max-size  26214400 bytes`
- `  max-messages  500`
- `  SMTPS  smtps://127.0.0.1:465` (`#[cfg(feature="tls")]`, only if `--smtps-bind` set)
- `  tls  enabled (STARTTLS)` (`#[cfg(feature="tls")]`, only if cert/key loaded)

When `--anonymous`, the `auth` line reads `  auth  anonymous (no auth)`.

---

## 3. On-disk layout

### Directory tree

```
<root>/
  messages/
    <id>.eml          raw, dot-unstuffed DATA bytes, byte-exact (CRLF preserved)
    <id>.json         sidecar: envelope + parsed summary (UTF-8 JSON)
    <id>.eml.tmp      transient; skipped by listing; renamed into place atomically
```

Flat — no mailbox/bucket layer. `messages/` is created on `Store::new`. The `.eml` is the exact
post-transparency DATA payload (dot-unstuffed, terminating `.` line removed, CRLF preserved). **No
`Received:` header is prepended** — the `.eml` stays pristine; all envelope/transport metadata lives
only in the sidecar (documented in README so `grep`ers of `.eml` know the envelope sender is not
there). Attachments are **not** stored separately — they are re-extracted from the `.eml` on demand,
so the `.eml` is the single source of truth.

### Message-ID scheme

Ported verbatim from minibucket `storage.rs::new_version_id`:

```rust
pub fn new_message_id() -> String {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap();
    let secs = now.as_secs();
    let nanos = now.subsec_nanos();
    let c = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{:013}-{:09}-{:08x}", secs, nanos, c)
}
```

13-digit seconds + 9-digit nanos + 8-hex process-global atomic counter ⇒ globally unique and
**lexicographically time-sortable**, so `read_dir` + plain `str` sort is chronological (oldest first;
newest first = reverse). The counter guarantees uniqueness even under a repeated/backward clock or
same-nanosecond concurrent saves across the unbounded thread-per-connection model.

`valid_id(id)` rejects empty, `len > 128`, or containing NUL / `/` / `\` / a `.` or `..` path
segment — defense-in-depth because the API takes `{id}` from the URL path.

### Sidecar JSON schema (`Summary`)

Built as `json::Json::Obj(..).to_string()` so **every string is escaped** — never
`writeln!("k: {v}")`. Read back with `json::parse` + typed accessors, **lenient**: missing fields
fall back to defaults so a partial/older sidecar still yields a usable `Summary`. Field types:

```jsonc
{
  "id": "0001720000000-000000042-0000001a",   // string
  "received_at": "2026-07-10T12:00:00.000Z",  // string, iso8601
  "received_unix": 1720000000,                // integer (Json::Int)
  "size": 20480,                              // integer (bytes of .eml)
  "from": "alice@example.com",                // string, envelope MAIL FROM ("" for <>)
  "to": ["bob@example.com"],                  // array<string>, envelope RCPT TO
  "remote": "127.0.0.1:54321",                // string, peer addr
  "auth_user": "alice",                       // string or null
  "helo": "client.example",                   // string
  "subject": "Hello ☀",                  // string, RFC 2047-decoded
  "from_header": "Alice <alice@example.com>", // string, decoded From:
  "to_header": "bob@example.com",             // string, decoded To:
  "cc_header": "",                            // string, decoded Cc:
  "date_header": "Tue, 02 Jan 2024 03:04:05 +0000", // string, raw Date:
  "message_id": "<abc123@host>",              // string, raw Message-ID:
  "content_type": "multipart/mixed",          // string, lowercased top-level type/subtype
  "has_text": true,                           // bool
  "has_html": true,                           // bool
  "attachments": [                            // array<object>
    { "part_id": "2", "filename": "cat.png",
      "content_type": "image/png", "size": 20480 }
  ]
}
```

### Atomic-write protocol (`Store::put`)

Mirrors minibucket `ObjectWriter.finish` — payload durable first, sidecar after:

1. Write raw bytes to `messages/<id>.eml.tmp` (64 KiB copy loop), `flush()`, `drop(file)`,
   `fs::rename(tmp, "<id>.eml")` — payload is now durable.
2. `mime::parse(raw)` → build `Summary` → `File::create("<id>.json")` +
   `write_all(summary.to_json().as_bytes())` (non-atomic, exactly like minibucket meta).
3. Retention prune (below).

Listing skips any `*.tmp`. `get_summary` tolerates a missing/partial `.json` (crash between steps) by
**re-deriving a minimal `Summary` from the `.eml`** (file len for `size`, mtime for `received`,
`mime::parse` for subject/headers) — the minibucket `unwrap_or_else`/read_meta tolerance.

### Retention (`--max-messages N`)

After each successful `put`, if `N` is set: `read_dir(messages/)`, collect `<id>` stems (skip
`*.tmp`), sort ascending, and while `count > N` remove the oldest `.eml` + `.json` pair. Because IDs
sort by time, "oldest" is a prefix of the sorted list. **Lock-free** — no `Mutex`. rename gives
payload atomicity, the `AtomicU32` counter gives id uniqueness; the prune-vs-intake race is bounded
(whole-file unlink is atomic; a double-delete is swallowed with `let _ =`) and is accepted exactly as
minibucket accepts its analogous races. This is the deliberate house posture; do **not** add a lock.

---

## 4. Module table (acyclic, layered)

`L0` = leaf (std only, no intra-crate deps). Each higher layer may only depend on strictly lower
layers. `[tls]` marks a dependency present only under `--features tls`.

| Layer | File | Purpose | Deps | LOC |
|-------|------|---------|------|-----|
| L0 | `src/util.rs` | date engine + RFC 5322/ISO-8601 format+parse, ids | — | 260 |
| L0 | `src/url.rs` | percent decode/encode, query parse | — | 160 |
| L0 | `src/base64.rs` | RFC 4648 encode/decode | — | 120 |
| L0 | `src/qp.rs` | quoted-printable + RFC 2047 Q decode | — | 110 |
| L0 | `src/json.rs` | JSON value + serializer + parser + escaper | — | 320 |
| L0 | `src/creds.rs` | credential store + `constant_time_eq` | — | 150 |
| L0 | `src/assets.rs` | `include_str!` web UI assets + content-type map | — | 60 |
| L0 | `src/events.rs` | SSE Hub + typed `Event` frame builder | — | 110 |
| L0 | `src/stream.rs` | `enum Stream { Plain, [Tls] }`, timeouts | — | 150 |
| L1 | `src/mime.rs` | full MIME parse (headers, 2047, multipart, CTE, charset) | base64, qp, util | 800 |
| L1 | `src/http.rs` | HTTP/1.1 primitives (ported) | util, url | 430 |
| L1 | `src/tls.rs` | rustls wrapper (**whole file `#[cfg(feature="tls")]`**) | stream | 100 |
| L2 | `src/store.rs` | on-disk spool, ids, atomic write, retention, `Summary` | mime, json, util | 420 |
| L3 | `src/server.rs` | owns `Server` + re-exports `Hub`; shared state | store, creds, events, [tls] | 90 |
| L4 | `src/smtp.rs` | pure `Session` state machine + thin `serve()` driver | server, store, creds, base64, util, events, stream, [tls] | 640 |
| L4 | `src/api.rs` | HTTP JSON API + UI + SSE | server, http, store, mime, json, base64, creds, events, url, util, assets, stream | 660 |
| L5 | `src/main.rs` | CLI, `Config`, two/three accept loops, banner | server, store, creds, smtp, api, util, stream, http, [tls] | 300 |

**Total budget: ~5,160 LOC** (default build; `tls.rs` adds ~100 only with the feature). Inside a
layer, `server` is ordered after `store`. The graph is a strict DAG: `L0 → L1 → L2 → L3 → L4 → L5`.

Shared-type ownership (each type defined **once**):
- `Config` → `main.rs`
- `Server` → `server.rs`
- `Hub`, `Event` → `events.rs` (`server.rs` re-exports `Hub`)
- `Store`, `Summary`, `Envelope`, `Attachment`, `StoreError` → `store.rs`
- `Part`, `ParsedMessage` → `mime.rs`
- `Stream` → `stream.rs`
- `Reply`, `Step`, `Cmd`, `Session` → `smtp.rs`
- `Request`, `Headers`, `Response`, `BuiltResponse`, `Body`, `FixedReader` → `http.rs`
- `Credentials` → `creds.rs`
- `Json` → `json.rs`

---

## 5. Per-module contracts

### `src/util.rs` — ported from minibucket `util.rs`

```rust
// Small utilities: date formatting/parsing (RFC 5322 + ISO-8601), ids. Pure std.

pub fn now_secs() -> u64;
pub fn http_date_now() -> String;                 // RFC 7231
pub fn http_date(secs: u64) -> String;            // "Thu, 01 Jan 1970 00:00:00 GMT"
pub fn iso8601(secs: u64) -> String;              // "1970-01-01T00:00:00.000Z"
pub fn rfc5322_date(secs: u64) -> String;         // "Thu, 01 Jan 1970 00:00:00 +0000"

/// Parse an RFC 5322 Date: header to unix seconds. Lenient: optional "Wed, "
/// day-of-week, named month via MONTH_NAMES.position, numeric (+0000) or named
/// (GMT/UT/UTC/EST/EDT/CST/CDT/MST/MDT/PST/PDT) zone. Returns None on garbage.
pub fn parse_rfc5322_date(s: &str) -> Option<u64>;

pub fn request_id() -> String;                    // 16 upper-hex, non-crypto LCG (verbatim)

// Made pub for mime/store reuse; ported verbatim from minibucket:
pub fn civil_from_days(days: i64) -> (i32, u32, u32, u32); // (year, m 1-12, d 1-31, wd 0=Sun)
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64;
```

### `src/url.rs` — ported verbatim from minibucket `url.rs`

```rust
// Percent encoding/decoding for URLs and query strings. Pure std.
pub fn percent_decode(s: &str) -> Vec<u8>;
pub fn percent_decode_str(s: &str) -> String;
pub fn encode_component(s: &str) -> String;
pub fn parse_query(q: &str) -> Vec<(String, String)>;
```

### `src/base64.rs` — new (minibucket has none)

```rust
// Base64 encode/decode (RFC 4648). Pure std, no external deps.

/// Standard base64 with '=' padding.
pub fn encode(data: &[u8]) -> String;
/// MIME base64: encode wrapped at `line_width` cols with CRLF (line_width e.g. 76).
pub fn encode_wrapped(data: &[u8], line_width: usize) -> String;
/// Lenient decode: skips ASCII whitespace/newlines, requires valid padding,
/// returns None on an invalid alphabet byte.
pub fn decode(s: &str) -> Option<Vec<u8>>;
// const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
// fn val(b: u8) -> Option<u8>;
```

### `src/qp.rs` — new

```rust
// Quoted-printable (RFC 2045 §6.7) + RFC 2047 'Q' decode. Pure std.

/// Standard QP: "=XX" hex escapes; soft line breaks "=\r\n" and "=\n" dropped.
/// Lenient: a lone '=' not followed by valid hex is passed through literally.
pub fn decode(input: &[u8]) -> Vec<u8>;
/// RFC 2047 Q-encoding: '_' -> 0x20, then "=XX"; NO soft line breaks.
pub fn decode_q(input: &[u8]) -> Vec<u8>;
// pub(crate) fn hex_val(b: u8) -> Option<u8>;
```

### `src/json.rs` — new

```rust
// Minimal JSON (RFC 8259 subset): value + serializer + parser. Pure std.

pub enum Json {
    Null,
    Bool(bool),
    Int(i64),          // exact integers for sizes/timestamps (no f64 rounding)
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn to_string(&self) -> String;
    pub fn write(&self, out: &mut String);
    pub fn get(&self, key: &str) -> Option<&Json>;   // Obj lookup
    pub fn as_str(&self) -> Option<&str>;
    pub fn as_i64(&self) -> Option<i64>;             // Int, or Num truncated
    pub fn as_u64(&self) -> Option<u64>;
    pub fn as_bool(&self) -> Option<bool>;
    pub fn as_arr(&self) -> Option<&[Json]>;
}

/// Append the JSON-escaped form of `s` (no surrounding quotes) to `out`:
/// \" \\ \n \r \t \b \f and control chars < 0x20 -> \u00XX. Mirrors util::xml_escape.
pub fn escape(s: &str, out: &mut String);
/// Permissive enough to round-trip our own sidecars. NOT a hardened parser for
/// untrusted large input.
pub fn parse(s: &str) -> Option<Json>;
```

### `src/creds.rs` — ported verbatim from minibucket `creds.rs` + `constant_time_eq`

```rust
// Credential store: user -> password. Same KEY=SECRET file format as minibucket.

#[derive(Clone, Default)]
pub struct Credentials { pub map: std::collections::HashMap<String, String> }

impl Credentials {
    pub fn new() -> Self;
    pub fn add(&mut self, access: &str, secret: &str);
    pub fn secret_for(&self, access: &str) -> Option<&str>;
    pub fn is_empty(&self) -> bool;
    /// UTF-8 KEY=SECRET lines; '#' starts a comment ANYWHERE on the line; both
    /// sides trimmed; first '=' splits (secrets may contain '='). Errors
    /// InvalidData "path:line: expected KEY=SECRET" / "blank key or secret".
    pub fn load_file(path: &std::path::Path) -> std::io::Result<Self>;
}

/// Length-check then XOR-accumulate. Route EVERY password comparison through this.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool;
```

Documented caveats (README + `creds.example`): a password containing `#` is truncated; leading/
trailing whitespace is stripped and cannot be represented.

### `src/assets.rs` — new

```rust
// Embedded web UI assets (compiled in via include_str!/include_bytes!).

pub struct Asset { pub content_type: &'static str, pub bytes: &'static [u8] }

// const INDEX_HTML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ui/index.html"));
// const APP_CSS:   &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ui/app.css"));
// const APP_JS:    &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ui/app.js"));
// const FAVICON:   &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ui/favicon.svg"));

/// "/" and "/index.html" -> index; "/app.css","/app.js","/favicon.svg". None otherwise.
pub fn get(path: &str) -> Option<Asset>;
```

### `src/events.rs` — new (single owner of the SSE wire schema)

```rust
// In-process pub/sub hub for SSE + the typed event schema. Pure std.

/// The ONLY producer of SSE frame strings. UI and every publisher share this.
pub enum Event {
    Message(String),  // full Summary JSON (already serialized)
    Delete(String),   // message id
    Clear,
}

impl Event {
    /// Render the exact SSE frame, e.g. "event: message\ndata: {..}\n\n".
    /// Message -> event: message, data: <summary json>
    /// Delete  -> event: delete,  data: {"id":"<id>"}
    /// Clear   -> event: clear,   data: {}
    pub fn frame(&self) -> String;
}

pub struct Hub { /* subs: Mutex<Vec<std::sync::mpsc::Sender<String>>> */ }

impl Hub {
    pub fn new() -> Self;
    pub fn subscribe(&self) -> std::sync::mpsc::Receiver<String>;
    /// Render `ev.frame()` and send to all subscribers; drop senders that Err (closed).
    pub fn publish(&self, ev: Event);
    pub fn subscriber_count(&self) -> usize;
}

impl Default for Hub { fn default() -> Self; }
```

### `src/stream.rs` — new (transport seam; keeps default build rustls-free)

```rust
// Socket transport: plain TCP, or (feature=tls) a rustls stream. Pure std by default.

pub enum Stream {
    Plain(std::net::TcpStream),
    #[cfg(feature = "tls")]
    Tls(Box<rustls::StreamOwned<rustls::ServerConnection, std::net::TcpStream>>),
}

impl Stream {
    pub fn peer_addr(&self) -> std::io::Result<std::net::SocketAddr>;
    pub fn set_nodelay(&self, on: bool) -> std::io::Result<()>;
    pub fn set_read_timeout(&self, dur: Option<std::time::Duration>) -> std::io::Result<()>;
    pub fn set_write_timeout(&self, dur: Option<std::time::Duration>) -> std::io::Result<()>;
    /// Plain: clone into a second Stream::Plain. Tls: Err(Unsupported).
    pub fn try_clone(&self) -> std::io::Result<Stream>;
    pub fn is_tls(&self) -> bool;
    /// Recover the concrete TcpStream (for the STARTTLS handshake). Only valid on Plain.
    pub fn into_tcp(self) -> std::io::Result<std::net::TcpStream>;
}

impl std::io::Read for Stream { fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize>; }
impl std::io::Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize>;
    fn flush(&mut self) -> std::io::Result<()>;
}
```

In the default build this is a **single-variant enum** (Rust emits no dead-code/unused-variant
warning because `Plain` is constructed and matched everywhere). Because it holds a concrete
`TcpStream`, `serve()` can recover it via `into_tcp()` for the rustls handshake — this is why we use
an enum, not `Box<dyn ReadWrite>`.

### `src/tls.rs` — new; **entire file `#[cfg(feature = "tls")]`**

Declared `#[cfg(feature = "tls")] mod tls;` in `main.rs`, so it does not compile in the default build
and no rustls symbol is referenced. This is the only file that names rustls / rustls-pemfile.

```rust
// TLS support (feature = "tls"): rustls + rustls-pemfile only. No cert generation.

pub fn load_server_config(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> std::io::Result<std::sync::Arc<rustls::ServerConfig>>;

/// Complete a server handshake over an already-accepted TcpStream and return a
/// Stream::Tls. Used for STARTTLS upgrade and implicit SMTPS.
pub fn accept(
    tcp: std::net::TcpStream,
    cfg: &std::sync::Arc<rustls::ServerConfig>,
) -> std::io::Result<crate::stream::Stream>;
```

### `src/mime.rs` — new (largest correctness surface)

```rust
// MIME message parsing (RFC 5322 headers, RFC 2045-2047-2231 bodies). Pure std.
// Never panics; parses leniently and returns partial results on malformed input.

pub struct Part {
    pub headers: Vec<(String, String)>,   // unfolded, original case, raw values
    pub content_type: String,             // lowercased "type/subtype"; default "text/plain"
    pub charset: Option<String>,          // lowercased; from Content-Type charset=
    pub encoding: String,                 // lowercased CTE; default "7bit"
    pub disposition: Option<String>,      // "inline" | "attachment"
    pub filename: Option<String>,         // RFC 2231/2047-decoded; disposition filename= or type name=
    pub content_id: Option<String>,       // Content-ID without <> (for cid: resolution)
    pub is_attachment: bool,              // see classification rules
    pub id: String,                       // dotted path: root "0", children "1","1.1","2"...
    pub body: Vec<u8>,                    // transfer-decoded bytes (leaf only; empty for multipart)
    pub children: Vec<Part>,
}

pub struct ParsedMessage {
    pub headers: Vec<(String, String)>,   // top-level, unfolded, original case
    pub root: Part,
    pub subject: String,                  // RFC 2047-decoded
    pub from: String,                     // decoded From:
    pub to: String,                       // decoded To:
    pub cc: String,                       // decoded Cc:
    pub date: String,                     // raw Date: header value
    pub message_id: String,               // raw Message-ID: header value
}

impl ParsedMessage {
    pub fn text_body(&self) -> Option<String>;     // first text/plain leaf, charset->UTF-8
    pub fn html_body(&self) -> Option<String>;     // first text/html leaf, charset->UTF-8
    pub fn part_by_id(&self, id: &str) -> Option<&Part>;
    pub fn attachments(&self) -> Vec<&Part>;       // pre-order, is_attachment == true
    pub fn flat_parts(&self) -> Vec<&Part>;        // pre-order, root first
}

pub fn parse(raw: &[u8]) -> ParsedMessage;

/// Split raw bytes into unfolded (name, value) headers + the byte offset of the body.
/// Accepts CRLF or bare-LF line endings; header block ends at the first empty line.
pub fn unfold_headers(raw: &[u8]) -> (Vec<(String, String)>, usize);

pub fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str>;

/// RFC 2047. Decodes B and Q encoded-words; concatenates the raw BYTES of
/// consecutive same-charset words BEFORE charset->UTF-8 (so a multibyte char
/// split across two words survives); elides linear whitespace BETWEEN two
/// adjacent encoded-words; preserves whitespace between an encoded-word and
/// plain text.
pub fn decode_encoded_words(s: &str) -> String;

/// Returns (lowercased "type/subtype", params) with lowercased param keys.
/// Handles RFC 2231 continued (name*0*=,name*1*=) and extended
/// (name*=charset'lang'pct-encoded) parameters, reassembled and percent-decoded.
pub fn parse_content_type(v: &str) -> (String, Vec<(String, String)>);

pub fn decode_body(encoding: &str, raw: &[u8]) -> Vec<u8>;  // 7bit/8bit/binary passthru; qp; base64

/// Transcode to UTF-8. Explicit: us-ascii, utf-8, iso-8859-1, iso-8859-15,
/// windows-1252 (incl. the 0x80-0x9F cp1252 punctuation block). Unknown ->
/// String::from_utf8_lossy (documented).
pub fn charset_to_utf8(bytes: &[u8], charset: &str) -> String;
```

**Part-id is a dotted-path string**, frozen here and reused unchanged by `store::Attachment.part_id`
and `api` `/parts/{part_id}` + `part_by_id`.

### `src/http.rs` — ported from minibucket `http.rs` with the noted edits

```rust
// Minimal HTTP/1.1 primitives. Just enough to serve the JSON API + web UI.

pub const MAX_HEADER_BYTES: usize = 64 * 1024;
pub const MAX_LINE_BYTES: usize = 16 * 1024;

pub struct Request<R: std::io::BufRead = std::io::BufReader<std::net::TcpStream>> {
    pub method: String, pub raw_path: String, pub path: String,
    pub query_raw: String, pub headers: Headers, pub reader: R,
}

#[derive(Default, Clone)]
pub struct Headers {
    pub map: std::collections::HashMap<String, (String, String)>, // key=lowercase -> (orig, val)
    pub order: Vec<String>,
}
impl Headers {
    pub fn get(&self, name: &str) -> Option<&str>;   // copied verbatim (+ its 2 unit tests)
    pub fn insert(&mut self, name: &str, value: &str);
}

/// Generalized over any Read (so a TLS Stream parses via the same path).
pub fn read_request<S: std::io::Read>(stream: S)
    -> std::io::Result<Request<std::io::BufReader<S>>>;

pub struct FixedReader<'a, R: std::io::BufRead> { pub r: &'a mut R, pub remaining: u64 }
impl<'a, R: std::io::BufRead> std::io::Read for FixedReader<'a, R> {}

pub struct Response { pub status: u16, pub status_text: &'static str, pub headers: Vec<(String, String)> }
impl Response {
    pub fn new(status: u16) -> Self;
    pub fn header(self, k: &str, v: &str) -> Self;
    /// NO Content-Type default (handlers set it). Defaults Connection: close,
    /// Date, Server: minimail/<ver>. Content-Length from body_len hint.
    pub fn write_headers<W: std::io::Write>(&self, w: &mut W, body_len: Option<u64>) -> std::io::Result<()>;
}

pub enum Body { Empty, Bytes(Vec<u8>), Stream(Box<dyn std::io::Read + Send>) }
impl Body { pub fn len_hint(&self) -> Option<u64>; pub fn into_bytes(self) -> std::io::Result<Vec<u8>>; }

pub struct BuiltResponse { pub status: u16, pub headers: Vec<(String, String)>, pub body: Body }
impl BuiltResponse {
    pub fn new(status: u16) -> Self;
    pub fn header(self, k: &str, v: &str) -> Self;
    pub fn body(self, body: Body) -> Self;
    pub fn json(self, body: String) -> Self;   // sets Content-Type: application/json; charset=utf-8 if unset
    pub fn write_to<W: std::io::Write>(self, w: &mut W) -> std::io::Result<()>; // 64KiB stream loop
    pub fn header_value(&self, name: &str) -> Option<&str>;
}

pub fn status_text(code: u16) -> &'static str;   // superset incl. 204/206/304/400/401/404/405/416/500

/// Inclusive (start,end) clamped to `size`. Accepts "bytes=S-E", "bytes=S-",
/// and suffix "bytes=-N". None if unsatisfiable.
pub fn parse_range(v: &str, size: u64) -> Option<(u64, u64)>;

pub fn mime_type_for(path: &str) -> &'static str;  // extension map; default application/octet-stream
```

### `src/store.rs` — ported from minibucket `storage.rs` (lock-free)

```rust
// File-backed mail store: <root>/messages/<id>.eml + <id>.json. Pure std.

pub struct Envelope {
    pub mail_from: String,            // reverse-path ("" for <>)
    pub rcpt_to: Vec<String>,         // forward-paths
    pub helo: String,
    pub remote: String,               // peer ip:port
    pub auth_user: Option<String>,
}

pub struct Attachment {
    pub part_id: String,              // mime dotted-path id
    pub filename: String,
    pub content_type: String,
    pub size: usize,
}

pub struct Summary {
    pub id: String,
    pub received_at: String,          // iso8601
    pub received_unix: u64,
    pub size: usize,
    pub from: String,
    pub to: Vec<String>,
    pub remote: String,
    pub auth_user: Option<String>,
    pub helo: String,
    pub subject: String,
    pub from_header: String,
    pub to_header: String,
    pub cc_header: String,
    pub date_header: String,
    pub message_id: String,
    pub content_type: String,
    pub has_text: bool,
    pub has_html: bool,
    pub attachments: Vec<Attachment>,
}

impl Summary {
    /// The SINGLE source of truth for summary serialization (sidecar AND API list/detail
    /// project from this). Built via json::Json::Obj so every string is escaped.
    pub fn to_json(&self) -> String;
    /// Lenient: missing fields default. Used to read sidecars back.
    pub fn from_json(s: &str) -> Option<Summary>;
}

#[derive(Debug)]
pub enum StoreError { Io, NotFound, InvalidId, TooLarge }
impl From<std::io::Error> for StoreError { fn from(_: std::io::Error) -> Self; } // discards inner -> Io

#[derive(Clone)]
pub struct Store { pub root: std::path::PathBuf }  // lock-free, Clone-over-PathBuf, NO Mutex

impl Store {
    pub fn new(root: std::path::PathBuf) -> std::io::Result<Self>;   // mkdir -p <root>/messages
    /// .eml tmp+rename, then .json, then prune if max_messages set. Parses via mime for Summary.
    pub fn put(&self, raw: &[u8], env: &Envelope, max_messages: Option<usize>)
        -> Result<Summary, StoreError>;
    pub fn list(&self) -> Result<Vec<Summary>, StoreError>;          // ascending by id (oldest first)
    pub fn get_summary(&self, id: &str) -> Result<Summary, StoreError>; // re-derives if .json missing
    pub fn get_raw(&self, id: &str) -> Result<Vec<u8>, StoreError>;
    pub fn eml_path(&self, id: &str) -> std::path::PathBuf;          // for ranged streaming
    pub fn delete(&self, id: &str) -> Result<(), StoreError>;
    pub fn delete_all(&self) -> Result<usize, StoreError>;
    pub fn count(&self) -> std::io::Result<usize>;
}

pub fn new_message_id() -> String;   // "{:013}-{:09}-{:08x}", static AtomicU32
pub fn valid_id(id: &str) -> bool;   // reject empty/>128/NUL/'/'/'\\'/'.'/'..'
```

### `src/server.rs` — new (owns shared state; frozen before smtp/api)

```rust
// Shared server state. Owns Server; re-exports the events Hub.
pub use crate::events::Hub;

pub struct Server {
    pub store: crate::store::Store,
    pub creds: crate::creds::Credentials,
    pub require_auth: bool,        // gates BOTH SMTP AUTH-required and HTTP Basic (== !anonymous)
    pub hostname: String,
    pub version: &'static str,     // env!("CARGO_PKG_VERSION")
    pub max_size: usize,           // ESMTP SIZE limit, bytes
    pub max_messages: Option<usize>,
    pub smtp_bind: String,         // for /api/v1/info
    pub http_bind: String,
    pub hub: Hub,
    #[cfg(feature = "tls")]
    pub tls: Option<std::sync::Arc<rustls::ServerConfig>>,
}
```

### `src/smtp.rs` — new (pure `Session` + thin `serve()`)

```rust
// SMTP sink server (RFC 5321/4954/3207). Never relays. Pure std (+ optional tls).

/// Uniform handler signature shared with api::serve.
pub fn serve(srv: &crate::server::Server, stream: crate::stream::Stream,
             peer: Option<std::net::SocketAddr>) -> std::io::Result<()>;

pub struct Reply { pub code: u16, pub lines: Vec<String> }   // rendered "250-" ... "250 " multiline

pub enum Step { Reply(Reply), NeedData(Reply), StartTls(Reply), Quit(Reply) }

pub enum Cmd {
    Helo(String), Ehlo(String), Mail(String), Rcpt(String), Data,
    Rset, Noop, Quit, Vrfy(String), Expn(String), Help, Auth(String),
    StartTls, AuthContinue(String), Unknown,
}

enum AuthState { None, Plain, LoginUser, LoginPass(String) }
enum State { Start, Greeted, Mail, Rcpt }

pub struct Session<'a> {
    /* srv: &'a Server, peer: String, helo: Option<String>, esmtp: bool,
       state: State, tls: bool, authed: Option<String>, auth_state: AuthState,
       mail_from: Option<String>, rcpt_to: Vec<String>, errors: u32 */
    _p: std::marker::PhantomData<&'a ()>,
}

impl<'a> Session<'a> {
    pub fn new(srv: &'a crate::server::Server, peer: Option<std::net::SocketAddr>, tls: bool) -> Self;
    pub fn greeting(&self) -> Reply;                     // "220 <hostname> ESMTP minimail ready"
    /// PURE: no IO. Advances state, returns the wire action. AUTH continuations
    /// arrive here too (auth_state routes them).
    pub fn handle_line(&mut self, line: &str) -> Step;
    /// PURE: consume the dot-unstuffed DATA payload, store, publish, reset txn.
    pub fn on_data(&mut self, raw: &[u8]) -> Reply;
    pub fn reset_after_starttls(&mut self);              // forget helo + auth (RFC 3207)
}

// ---- pure, unit-tested helpers ----
pub fn parse_command(line: &str) -> Cmd;                 // ASCII-case-insensitive verb
pub fn parse_addr(param: &str) -> Option<String>;        // "FROM:<a@b> SIZE=1" -> "a@b"; "<>" -> ""
pub fn parse_size(param: &str) -> Option<usize>;         // SIZE=nnnn; ignores all other params
pub fn decode_auth_plain(b64: &str) -> Option<(String, String)>; // authzid\0authcid\0pass -> (authcid,pass)

/// Strip one leading '.' from each line; used by serve() before on_data. PURE, testable
/// against RFC 5321 5.2 vectors (leading-dot, '..' at start, '.' mid-line).
pub fn dot_unstuff(raw: &[u8]) -> Vec<u8>;

/// Scan a DATA accumulation buffer for the end-of-data terminator, accepting BOTH
/// "\r\n.\r\n" and bare "\n.\n". Returns the byte index at which the payload ends
/// (exclusive of the terminator) if present.
pub fn find_data_terminator(buf: &[u8]) -> Option<usize>;
```

### `src/api.rs` — new (HTTP JSON API + UI + SSE)

```rust
// HTTP JSON API + web UI + SSE (hand-rolled HTTP/1.1). Pure std.

/// Uniform handler signature shared with smtp::serve.
pub fn serve(srv: &crate::server::Server, stream: crate::stream::Stream,
             peer: Option<std::net::SocketAddr>) -> std::io::Result<()>;

pub fn dispatch<R: std::io::BufRead>(srv: &crate::server::Server,
    req: crate::http::Request<R>, sock: &mut crate::stream::Stream) -> std::io::Result<()>;

// build/write split — unit-tested via BuiltResponse, no socket:
pub fn build_list(srv: &crate::server::Server, limit: usize, offset: usize, q: Option<&str>)
    -> crate::http::BuiltResponse;
pub fn build_message(srv: &crate::server::Server, id: &str) -> crate::http::BuiltResponse;
pub fn build_info(srv: &crate::server::Server) -> crate::http::BuiltResponse;
pub fn build_error(status: u16, code: &str, message: &str) -> crate::http::BuiltResponse;

// private streaming helpers — write directly to the (possibly TLS) socket:
// fn get_raw(srv, sock: &mut Stream, id, head_only) -> io::Result<()>;
// fn get_html(srv, sock: &mut Stream, id) -> io::Result<()>;
// fn get_part(srv, sock: &mut Stream, id, part_id, headers, download) -> io::Result<()>; // ranged
// fn serve_asset(sock: &mut Stream, path) -> io::Result<()>;
// fn serve_events(srv, sock: &mut Stream) -> io::Result<()>;   // manual SSE writer + heartbeats
// fn error_response(sock: &mut Stream, status, code, message) -> io::Result<()>;
// fn check_auth(srv, headers: &Headers) -> bool;               // Basic + constant_time_eq
// fn has_q(q, name) -> bool;  fn qget<'a>(q, name) -> Option<&'a str>;  // copied from s3.rs
```

All private streaming helpers take `sock: &mut crate::stream::Stream` (pinned) so ranged/raw streaming
compiles against the accept-loop transport with TLS on or off.

### `src/main.rs` — CLI + wiring (owns `Config`)

```rust
// minimail: a tiny (default) dependency-free dev SMTP sink + web UI/JSON API.
// mod util; mod url; mod base64; mod qp; mod json; mod creds; mod assets;
// mod events; mod stream; mod mime; mod http; mod store; mod server;
// mod smtp; mod api; #[cfg(feature = "tls")] mod tls;

struct Config {
    smtp_bind: String,
    http_bind: String,
    root: std::path::PathBuf,
    creds: crate::creds::Credentials,
    anonymous: bool,
    hostname: String,
    max_size: usize,
    max_messages: Option<usize>,
    #[cfg(feature = "tls")] tls_cert: Option<std::path::PathBuf>,
    #[cfg(feature = "tls")] tls_key: Option<std::path::PathBuf>,
    #[cfg(feature = "tls")] smtps_bind: Option<String>,
}

fn parse_args() -> Config;

/// One accept loop, thread-per-connection, Arc::clone per connection.
/// `implicit_tls` wraps the accepted TcpStream via tls::accept before handing off.
fn serve_loop(
    listener: std::net::TcpListener,
    srv: std::sync::Arc<crate::server::Server>,
    handler: fn(&crate::server::Server, crate::stream::Stream, Option<std::net::SocketAddr>) -> std::io::Result<()>,
    implicit_tls: bool,
);

fn main();  // bind both (three) listeners; SMTP loops on spawned threads; HTTP loop on main thread
```

---

## 6. SMTP state machine

Transport: thread-per-connection. `serve()` wraps the accepted `Stream` in
`BufReader<Stream>` for command reading and writes replies via `get_mut()` (no `try_clone` needed —
one socket, request/response interleave). `set_read_timeout(Some(300s))` and `set_write_timeout`.
On read timeout → `421 4.4.2 <hostname> timeout, closing connection`, close. On EOF → close.

**Command reading:** read one line up to `\n`, byte-transparent (raw bytes → `String::from_utf8_lossy`
so a UTF-8 reverse-path is not mangled — consistent with advertising SMTPUTF8). Accept **CRLF or bare
LF** line endings (a strict-CRLF reader would hang a telnet session). Command lines are read with a
generous cap of **4096 octets** (not the 512 minimum — the sink accepts everything); overflow →
`500 5.5.2 Command line too long`.

**DATA reading:** a **separate byte-clean reader** — never reuse `http::read_line_limited` (rejects
non-UTF-8) or the command reader. Accumulate raw bytes; detect the terminator with
`find_data_terminator` (accepts `\r\n.\r\n` **and** bare `\n.\n`). Then `dot_unstuff` the payload
(pure). No per-line length cap on DATA (real HTML mail has long lines). Size is enforced against
`max_size` on the accumulated byte count; on overflow, **keep draining to the terminator** (do not
desync the connection) then reply 552 and reset the transaction.

### States and allowed verbs

| State | Entered by | Verbs accepted (others → 503) |
|-------|-----------|-------------------------------|
| `Start` | connect (after 220) | EHLO, HELO, AUTH, STARTTLS, NOOP, RSET, QUIT, VRFY, HELP |
| `Greeted` | EHLO/HELO ok | + MAIL (AUTH/STARTTLS/EHLO/HELO still ok) |
| `Mail` | MAIL ok | RCPT, DATA(→554 if no rcpt), RSET, NOOP, QUIT, plus above |
| `Rcpt` | ≥1 RCPT ok | RCPT, DATA, RSET, NOOP, QUIT, plus above |

RSET → `Greeted` (clears mail_from/rcpt). Successful EHLO/HELO reset the transaction → `Greeted`.
STARTTLS success → `Start` (client must re-EHLO). DATA completion → `Greeted`. An error counter
increments on every 5xx; after **>10** hard errors → `421 4.7.0 <hostname> too many errors, closing`.

### EHLO capability list (each a `250-` line, last is `250 `)

```
250-<hostname> greets <domain>
250-PIPELINING
250-8BITMIME
250-SMTPUTF8
250-SIZE <max_size>
250-ENHANCEDSTATUSCODES
250-AUTH PLAIN LOGIN
250-STARTTLS            (only #[cfg(feature="tls")] && srv.tls.is_some() && !session.tls)
250 HELP
```

HELO advertises nothing: single `250 <hostname>`. When EHLO was used, every reply text carries the
enhanced status triple (e.g. `250 2.1.0 Ok`); after HELO the enhanced triple is omitted.

### Exact reply strings

```
220 <hostname> ESMTP minimail ready               (greeting)
220 2.0.0 Ready to start TLS                       (STARTTLS accepted)
221 2.0.0 <hostname> closing connection            (QUIT)
235 2.7.0 Authentication successful                (AUTH ok)
250 2.1.0 Ok                                       (MAIL FROM accepted)
250 2.1.5 Ok                                       (RCPT TO accepted)
250 2.0.0 Ok                                       (RSET / NOOP)
250 2.0.0 Ok: queued as <id>                       (DATA stored)
250 <hostname>                                     (HELO)
334 <base64-challenge>                             (AUTH continuation; empty string allowed)
354 End data with <CR><LF>.<CR><LF>                (DATA go-ahead)
421 4.4.2 <hostname> timeout, closing connection
421 4.7.0 <hostname> too many errors, closing
451 4.3.0 Requested action aborted: local error in processing   (store IO error)
452 4.5.3 Too many recipients                      (>100 rcpts)
500 5.5.2 Command unrecognized                     (unknown verb; errors += 1)
500 5.5.2 Command line too long                    (>4096 octets)
501 5.5.2 Syntax error in parameters               (bad addr / bad AUTH blob)
502 5.5.1 Command not implemented                  (STARTTLS when unavailable, EXPN)
503 5.5.1 Bad sequence of commands
504 5.5.4 Unrecognized authentication mechanism
530 5.7.0 Authentication required                  (MAIL when require_auth && !authed)
535 5.7.8 Authentication credentials invalid       (unknown user OR bad password — same code)
550 5.1.1 Mailbox unavailable                      (reserved; not emitted by the sink normally)
552 5.3.4 Message size exceeds fixed limit         (SIZE= over max, or DATA overflow)
554 5.5.1 No valid recipients                      (DATA with zero rcpts)
252 2.1.5 Cannot VRFY user, but will accept message (VRFY)
214 2.0.0 Commands: HELO EHLO MAIL RCPT DATA RSET NOOP VRFY QUIT AUTH STARTTLS HELP  (HELP)
```

### MAIL / RCPT parsing (lenient sink)

`parse_addr` extracts the `<addr>` between angle brackets; `<>` → `""` (null reverse-path). `parse_size`
reads `SIZE=nnnn`. **All other MAIL/RCPT parameters — `BODY=7BIT`, `BODY=8BITMIME`, `SMTPUTF8`, `AUTH=`,
anything — are silently ignored. Never emit 555.** (A sink that advertises an extension must accept its
param.) `SIZE=n` with `n > max_size` → 552 before DATA. MAIL when `require_auth && authed.is_none()` →
530. MAIL when already in Mail/Rcpt → 503 (nested MAIL). RCPT before MAIL → 503. RCPT accepts every
address (no relay/recipient validation); `> 100` rcpts → 452.

### AUTH wire flows (RFC 4954)

Passwords compared with `constant_time_eq` against `creds.secret_for(user)`. Unknown user and bad
password both → `535 5.7.8` (do not leak which). AUTH when already authed → 503. When `require_auth`
is false, AUTH is still accepted and records the user (clients that insist on AUTH work).

**AUTH PLAIN** (with initial response): `AUTH PLAIN <b64>` → `decode_auth_plain` splits
`authzid\0authcid\0passwd`, verify → 235 / 535. Bad base64 → 501.

**AUTH PLAIN** (no initial): `AUTH PLAIN` → `334 ` (empty challenge); next line is the base64 blob,
decoded identically.

**AUTH LOGIN**: `AUTH LOGIN` → `334 VXNlcm5hbWU6` (base64 of `Username:`) → client sends base64
username → `334 UGFzc3dvcmQ6` (base64 of `Password:`) → client sends base64 password → verify.

Unknown mechanism → 504.

### STARTTLS (`#[cfg(feature = "tls")]`)

Only in `Start`/`Greeted`, feature on, `srv.tls.is_some()`, not already TLS. `handle_line` returns
`Step::StartTls(Reply 220)`. `serve()` writes 220, then **asserts the BufReader buffer is empty**
(RFC 3207 anti-injection): if non-empty → `501 5.5.2` and drop the connection. Otherwise
`reader.into_inner()` → `Stream::into_tcp()` → `tls::accept()` → new `BufReader<Stream::Tls>`,
`session.reset_after_starttls()`, continue at `Start`. Handshake failure → `454 4.7.0 TLS not
available` and close. When the feature is OFF, the STARTTLS verb hits an arm that returns `502` and
references no TLS symbol.

---

## 7. MIME parsing contract

**Header unfolding** (`unfold_headers`): a header continues onto the next line when that line begins
with SP or TAB; join by replacing the CRLF/LF + leading whitespace with a single space. Split each
header on the first `:`, trim the name; the value keeps internal structure. The header block ends at
the first empty line (CRLF or bare LF). Return `(headers, body_offset)`.

**RFC 2047 encoded-words** (`decode_encoded_words`): match `=?charset?B?...?=` and `=?charset?Q?...?=`
case-insensitively. `B` → `base64::decode`; `Q` → `qp::decode_q`. Two mandatory rules:
1. **Byte accumulation before charset decode:** consecutive encoded-words with the **same charset and
   encoding** have their decoded *bytes* concatenated, then `charset_to_utf8` runs once — so a
   multibyte character split across two words (common from the Ruby Mail gem) is not turned into two
   `U+FFFD`.
2. **Adjacent-word whitespace elision:** linear whitespace occurring **between two adjacent
   encoded-words** is dropped; whitespace between an encoded-word and ordinary text is preserved.

**Content-Type / Content-Disposition params** (`parse_content_type`): split on `;`, key lowercased,
value unquoted (strip surrounding `"`). RFC 2231 support:
- Continuations: `name*0=`, `name*1=`, … reassembled in index order.
- Extended: `name*=charset'lang'pct-encoded` and `name*0*=`/`name*1*=` — percent-decode, then
  `charset_to_utf8`. Used for non-ASCII `filename*=` (ActionMailer/Nodemailer).
`filename` resolution order: Content-Disposition `filename` (RFC 2231/2047-decoded) → Content-Type
`name` → `None`.

**Multipart tree walking:** when `content_type` is `multipart/*`, read the `boundary` param. A part
boundary is a line `--<boundary>`; the closing boundary is `--<boundary>--`. **Boundary CRLF
ownership:** the CRLF (or bare LF) immediately preceding a `--boundary` delimiter belongs to the
delimiter, **not** to the preceding part body — strip exactly one trailing line ending from each
part's raw bytes before transfer-decoding, so binary attachments (PNG/PDF) verify byte-exact.
Preamble (before the first boundary) and epilogue (after the closing boundary) are discarded. Missing
terminal boundary → treat EOF as the close (lenient, never panic). Nested multiparts recurse. Assign
`id`: root `"0"`, its children `"1"`, `"2"`, …, grandchildren `"1.1"`, `"1.2"`, ….

**Transfer decoding** (`decode_body`, keyed on lowercased CTE): `base64` → `base64::decode`;
`quoted-printable` → `qp::decode`; `7bit`/`8bit`/`binary`/unknown → passthrough. Only leaf parts get a
decoded `body`; multipart parts have empty `body`.

**Attachment vs inline classification** (`is_attachment`): true if Content-Disposition is
`attachment`; OR the part has a `filename`; OR the leaf is neither `text/plain` nor `text/html` and is
not referenced by a `cid:` from an HTML part (Content-ID present ⇒ treated as inline related resource,
`is_attachment = false`). `text/plain` and `text/html` leaves shown in the body views are not
attachments unless explicitly `Content-Disposition: attachment`.

**Charset → UTF-8** (`charset_to_utf8`): explicit support for `us-ascii`, `utf-8`, `iso-8859-1`
(latin-1), `iso-8859-15` (latin-9), `windows-1252` (including the 0x80–0x9F cp1252 punctuation block —
a ~30-line table). Charset name matching is case-insensitive with common aliases (`latin1`, `cp1252`,
`ansi_x3.4`). Anything else → `String::from_utf8_lossy` (documented lossy fallback).

`text_body()`/`html_body()` return the first matching leaf, charset-decoded. `part_by_id` walks by
dotted id. `parse` never panics — malformed input yields partial results.

---

## 8. HTTP / JSON API

Base: HTTP/1.1, thread-per-connection, `Connection: close` (except SSE). Default response
`Content-Type: application/json; charset=utf-8` (handlers override for HTML/assets/raw). Malformed
request line → best-effort `try_clone` the Plain stream and write `400` JSON; on a TLS stream where
clone is unsupported, drop silently (minibucket parity).

**Auth:** HTTP Basic guards everything under `/` and `/api` **except `/healthz`** when
`srv.require_auth`. `check_auth` base64-decodes the `Authorization: Basic` header, looks up
`creds.secret_for(user)`, compares with `constant_time_eq`. Failure → `401` with
`WWW-Authenticate: Basic realm="minimail"` and a JSON error body. `--anonymous` sets
`require_auth = false` and bypasses the check entirely.

**Error envelope** (all non-2xx JSON): `{"error":{"code":"not_found","message":"..."}}` via
`build_error`. Status codes used: 200, 204, 206, 304, 400, 401, 404, 405, 416, 500.

Routing is a most-specific-first cascade on method + path; catch-all → 404. Testable handlers
(`build_list`/`build_message`/`build_info`/`build_error`) return `BuiltResponse` and are unit-tested
with a temp-dir `Store` + `Cursor` readers.

### Routes

**`GET /healthz`** (no auth) → `200 text/plain` `ok`

**`GET /`, `GET /index.html`** → `200 text/html` (embedded UI). **`GET /app.css`** → `text/css`.
**`GET /app.js`** → `application/javascript`. **`GET /favicon.svg`** → `image/svg+xml`. Unknown static
path → `404` JSON.

**`GET /api/v1/info`** → `200`
```json
{"name":"minimail","version":"0.1.0","hostname":"mail.local",
 "smtp":"127.0.0.1:1025","http":"127.0.0.1:8025","count":12,
 "max_messages":null,"max_size":26214400,"anonymous":false,"tls":false}
```

**`GET /api/v1/messages?limit=&offset=&q=`** → `200`. `limit` default 50 (cap 500), `offset` default 0,
`q` = case-insensitive substring over subject/from_header/to_header/from/to. Newest first.
```json
{"total":12,"count":2,"offset":0,"limit":50,
 "messages":[ { /* Summary object, verbatim §3 schema */ } ]}
```

**`GET /api/v1/messages/{id}`** → `200` | `404`
```json
{"summary": { /* Summary */ },
 "headers": [["From","Alice <a@b>"],["To","b@c"],["Subject","Hi"]],
 "text": "plain body" ,
 "html": "<html>..." ,
 "parts": [
   {"id":"0","content_type":"multipart/mixed","filename":null,"disposition":null,
    "content_id":null,"size":0,"is_attachment":false,"children":["1","2"]},
   {"id":"1","content_type":"text/plain","filename":null,"disposition":null,
    "content_id":null,"size":120,"is_attachment":false,"children":[]},
   {"id":"2","content_type":"image/png","filename":"cat.png","disposition":"attachment",
    "content_id":null,"size":20480,"is_attachment":true,"children":[]}
 ]}
```
`text`/`html` are `null` when absent. `headers` are unfolded, original order/case, RFC 2047-decoded.
404 → `{"error":{"code":"not_found","message":"no such message"}}`.

**`GET`/`HEAD /api/v1/messages/{id}/raw`** → `200 message/rfc822` (raw `.eml`, streamed 64 KiB;
`Content-Disposition: attachment; filename="{id}.eml"`; `Accept-Ranges: bytes`, `206`/`416` on Range)
| `404`.

**`GET /api/v1/messages/{id}/html`** → `200 text/html; charset=utf-8` (the decoded text/html part, for
`<iframe srcdoc>`) | `404` if no HTML part.

**`GET /api/v1/messages/{id}/parts/{part_id}?download=1`** → `200` | `206` | `404` | `416`. Serves one
MIME part's decoded bytes with its own Content-Type; `Content-Disposition: attachment; filename="…"`
when `download=1` or the part is an attachment, else `inline`. Range-capable (`http::parse_range`
including suffix; `Accept-Ranges: bytes`). Body streamed.

**`DELETE /api/v1/messages/{id}`** → `204` | `404`. Publishes `Event::Delete(id)`.

**`DELETE /api/v1/messages`** → `200` `{"deleted":12}`. Publishes `Event::Clear`.

**`GET /api/v1/events`** → `200 text/event-stream` (SSE, long-lived). Headers written by hand:
`Content-Type: text/event-stream`, `Cache-Control: no-cache`, `Connection: keep-alive`, **no
Content-Length** (close-delimited). Subscribes to `srv.hub`. Loops on `Receiver::recv_timeout(20s)`:
on an event write the frame verbatim (from `Event::frame()`) and **flush immediately**; on timeout
write a `: ping\n\n` heartbeat and flush (detects dead clients via write error, which ends the thread
and prunes the subscriber). This connection disables the read timeout; a write timeout bounds a stuck
consumer. **Do not** route SSE through `write_headers`/`BuiltResponse` — that breaks live updates.

Unknown `/api` route → `404` JSON. Unsupported method on a known path → `405`.

---

## 9. Web UI

Files under **`src/ui/`** (so `assets.rs` can `include_str!` from `CARGO_MANIFEST_DIR/src/ui`):
`index.html`, `app.css`, `app.js`, `favicon.svg`. Embedded at compile time — the scratch runtime image
ships only the binary. **Hard constraint: no CDN, no external fetch, no external fonts** (system font
stack), inline SVG favicon — strict-CSP friendly and required for the `FROM scratch` image. The
Dockerfile COPYs `src/ui/` into the builder (it is inside `src/`, so `COPY src ./src` already covers
it); `.dockerignore` must not exclude it.

**Layout:** two-pane mail client. Left: live message list (newest on top) showing from, subject, a
relative timestamp, and a paperclip when attachments exist; a top bar with a search box (drives `?q=`),
a "Clear all" button, a connection dot (green when SSE is live), and the SMTP/HTTP addresses from
`/api/v1/info`. Right: the selected message with tabs — **Text**, **HTML** (rendered in a sandboxed
`<iframe sandbox srcdoc=...>` / `src=/api/v1/messages/{id}/html` so message scripts cannot run and
cannot touch the app), **Headers** (raw table), **Raw** (link to `/raw`), and an Attachments strip
linking to `/parts/{id}?download=1`. Theme-aware via `prefers-color-scheme`.

**XSS safety:** all dynamic text inserted via `textContent` (never `innerHTML` for message-derived
data); HTML bodies only ever rendered inside the sandboxed iframe.

**Data flow:** on load `GET /api/v1/info` then `GET /api/v1/messages`; selecting a row
`GET /api/v1/messages/{id}`. Search re-queries with `?q=`. Delete/Clear call the DELETE routes then
update the DOM.

**Live update:** `new EventSource('/api/v1/events')`. `message` → prepend the summary; `delete` →
remove that row; `clear` → empty the list. The green dot reflects `EventSource.readyState`.
`EventSource` auto-reconnects; on (re)connect `app.js` does one reconciling `GET /api/v1/messages` so
no event is missed across a gap. No steady-state polling. Browser reuses origin Basic credentials on
the EventSource request. ~700 lines total of hand-written HTML/CSS/vanilla JS, no framework, no build
step.

---

## 10. TLS feature gating

**Goal:** default `cargo build --release` is 100% std-only, zero external deps, **zero warnings**;
`--features tls` adds exactly `rustls` + `rustls-pemfile`.

`Cargo.toml`:
```toml
[features]
default = []
tls = ["dep:rustls", "dep:rustls-pemfile"]

[dependencies]
rustls = { version = "0.23", optional = true }
rustls-pemfile = { version = "2", optional = true }
```
The `dep:` syntax means the crates are pulled in **only** by the feature. The Dockerfile and CI both
run bare `cargo build --release` with **no `--features`**, so `tls` stays off in the static-musl
scratch build (no native TLS, no link breakage). **Never** add `--all-features` anywhere.

Items that are `#[cfg(feature = "tls")]` (and produce no warning when absent):
- **Whole file** `src/tls.rs`, declared `#[cfg(feature = "tls")] mod tls;` — not compiled by default.
- `stream::Stream::Tls` variant + its arms in the `Read`/`Write`/method matches. Default build is a
  single-variant enum (no dead-code/unused-variant warning; `Plain` is constructed and matched).
- `server::Server::tls` field.
- `main::Config::{tls_cert, tls_key, smtps_bind}` fields, their `parse_args` arms, their banner lines,
  and the implicit-SMTPS listener spawn.
- `smtp`: the STARTTLS EHLO advertisement line, `Session::cmd_starttls` path, and the `serve()`
  handshake block. The `STARTTLS` command match arm exists in both builds — when the feature is off it
  returns `502` from an arm that references no TLS symbol.

No rustls type appears in any non-feature-gated signature: transport is `Stream`, and the shared
handler signature is `fn(&Server, Stream, Option<SocketAddr>)`.

**STARTTLS flow:** §6. **Implicit SMTPS:** with `--smtps-bind`, a third accept loop runs
`serve_loop(..., implicit_tls = true)`, which `tls::accept`s each `TcpStream` **before** handing the
`Stream::Tls` to `smtp::serve`. HTTPS for the UI is out of scope. No cert generation.

`tls::accept` recovers the concrete `TcpStream` from `Stream::into_tcp()` for the handshake — feasible
precisely because `Stream` is an enum over `TcpStream`, not a `dyn` trait object.

---

## 11. Testing contract

Unit tests live in `#[cfg(test)] mod tests { use super::*; }` at the bottom of each module (no
`tests/` dir). Temp files via a nanos-timestamp helper prefixed `minimail_<module>_`; RAII
`struct ScopedRoot(PathBuf)` with a `Drop` that `remove_dir_all`s. All test code must be
`rustfmt`-clean and `clippy -D warnings`-clean.

Required coverage per module:

- **base64**: RFC 4648 vectors `""→""`, `"f"→"Zg=="`, `"fo"→"Zm8="`, `"foo"→"Zm9v"`,
  `"foob"→"Zm9vYg=="`, `"fooba"→"Zm9vYmE="`, `"foobar"→"Zm9vYmFy"`; round-trip incl. all padding
  lengths; decode skips embedded whitespace/newlines; decode returns `None` on bad alphabet byte.
- **qp**: `=XX` decode; soft line break `=\r\n` and `=\n` dropped; lone `=` passed through; `decode_q`
  maps `_`→space and does NOT drop soft breaks.
- **json**: escape of `"` `\` `\n` `\r` `\t` control→`\u00XX` and a non-ASCII char; `Int` round-trips
  exactly (no f64); parse→to_string idempotence on a sidecar; lenient parse of a partial object.
- **util**: `parse_rfc5322_date` with and without day-of-week, numeric and named zones, and garbage
  (→`None`); `rfc5322_date`/`iso8601` fixed-epoch vectors; civil-date round-trip.
- **mime**: RFC 2047 **adjacent same-charset words concatenate before decode** (split multibyte →
  single char) and **whitespace between adjacent words elided**; B and Q words; nested multipart tree +
  dotted ids; **boundary preceding-CRLF stripped** (binary attachment byte-exact); qp + base64 CTE;
  RFC 2231 `filename*0*=`/`filename*=utf-8''...` decode; windows-1252 0x80–0x9F and iso-8859-1
  transcoding; malformed/missing-terminal-boundary does not panic.
- **store**: `new_message_id` monotonic & sortable & **unique across threads** (spawn N threads, mint
  ids, assert set size == count and sorted order); `valid_id` **rejects path traversal** (`../`, `/`,
  `.`, `..`, NUL, over-length); `put`→`get_summary` round-trip; `get_summary` re-derives when `.json`
  is deleted; retention drops oldest to `N`; `Summary::to_json`→`from_json` round-trip with a subject
  containing a quote/newline/emoji.
- **smtp** (pure `Session`): feed exact wire lines, assert `Reply`/`Step` — multiline EHLO caps list;
  MAIL with `BODY=8BITMIME`/`SMTPUTF8` params **accepted** (250, never 555); AUTH PLAIN (initial +
  challenge) and AUTH LOGIN full exchanges (235/535, unknown-user==bad-pass code); 503 sequencing;
  `dot_unstuff` RFC 5321 5.2 vectors (leading dot, `..` at start, `.` mid-line, lone-dot terminator);
  `find_data_terminator` for `\r\n.\r\n` **and** bare `\n.\n`; SIZE over limit → 552; error counter →
  421 after 10.
- **http**: the two verbatim `Headers` tests (case-insensitive get, insertion order); `parse_range`
  for `bytes=S-E`, `bytes=S-`, suffix `bytes=-N`, and unsatisfiable → `None`; `status_text` mapping.
- **creds**: verbatim minibucket tests (parse lines, reject malformed, reject blank secret, `#`
  comment/whitespace); `constant_time_eq` equal/unequal/length-mismatch.
- **api**: `build_list`/`build_message`/`build_info` against a temp-dir `Store` asserting `.status`,
  `header_value`, and body substrings; `build_error` envelope; 404 for missing id.

`smoketest.py` (stdlib only — `smtplib` + `urllib`, **no pip deps**): assumes an already-running
server (does not start/stop it; the `just smoke` recipe documents starting one). Port the minibucket
`t(label, fn)` runner verbatim (global `passed`/`failed`, `OK   `/`FAIL ` prints truncated `[:200]`,
final `f"\n{passed} passed, {failed} failed"`, `sys.exit(0 if failed==0 else 1)`). Config as inline
constants: `SMTP_HOST/SMTP_PORT`, `API_BASE`, creds `minimail`/`minimail`. Lifecycle narrative that
cleans up after itself: send a plain message via `smtplib.SMTP` then assert it appears via
`GET /api/v1/messages`; send an AUTH PLAIN authenticated message; send a MIME multipart with a base64
attachment and assert the attachment lists and downloads byte-exact via `/parts/{id}?download=1`; send
an RFC 2047 encoded Subject and assert the decoded subject via the API; a negative AUTH test (wrong
password) returning the SMTP code as the asserted value; `DELETE` one and `DELETE` all; assert
`/healthz`. Real `assert`s inside the lambdas/defs (AssertionError → FAIL).

---

## 12. Packaging

### `Cargo.toml` (verbatim)

```toml
[package]
name = "minimail"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "minimail"
path = "src/main.rs"

[features]
default = []
tls = ["dep:rustls", "dep:rustls-pemfile"]

[dependencies]
rustls = { version = "0.23", optional = true }
rustls-pemfile = { version = "2", optional = true }

[profile.release]
opt-level = 3
lto = true
```

### `justfile` recipes (names verbatim from minibucket)

`default` (`@just --list`), `run *ARGS` (`cargo run -- {{ARGS}}`), `build`, `build-release`, `check`,
`clippy` (`cargo clippy --all-targets -- -D warnings`), `fmt`, `fmt-check`
(`cargo fmt --all -- --check`), `test`, `smoke` (`python smoketest.py`, doc-comment: start a server
first, e.g. `just run --smtp-bind 127.0.0.1:1025 --http-bind 127.0.0.1:8025 --user alice --password
alicepass`), `ci: fmt-check clippy test`, `docker-build` (`docker build -t minimail:dev .`),
`docker-run` (`docker run --rm -p 1025:1025 -p 8025:8025 minimail:dev`), `version`, `set-version
BUMP="patch"`, `release BUMP="patch"`, `clean`. Keep the header
`set windows-shell := ["pwsh.exe","-NoLogo","-NoProfile","-Command"]` and the banner-comment grouping.

### `Dockerfile` deltas vs minibucket

Port the two-stage BUILDPLATFORM-cross-compile musl + `FROM scratch` machinery verbatim. Changes:
- Binary name `minibucket` → `minimail` (build, `cp`, `COPY`, `ENTRYPOINT`).
- `src/ui/` reaches the builder via the existing `COPY src ./src` (assets live under `src/`); confirm
  it is not excluded by `.dockerignore`.
- `EXPOSE 1025 8025` (two ports).
- `CMD ["--smtp-bind","0.0.0.0:1025","--http-bind","0.0.0.0:8025","--root","/data"]`.
- Keep `/data` pre-creation + `VOLUME ["/data"]` (mail persistence is useful; nonroot `USER
  65532:65532`).
- Update OCI LABELs (title `minimail`, description, source, version `$VERSION`).
- Plain `cargo build --release --target ...` with **no `--features`** keeps `tls` off.

### `release.yml` deltas

Port the 5-target matrix + multi-arch buildx + gh-release verbatim; replace every `minibucket` literal
(artifact names, `target/.../release/minibucket`, image title). Docker job builds
`linux/amd64,linux/arm64` with `VERSION` build-arg. Keep `cargo build --release --target` with **no
`--features`**. Add (or confirm) a CI workflow running `just ci` (`fmt-check` + `clippy -D warnings` +
`test`) on PRs.

`scripts/set-version.mjs` + `scripts/release.mjs`: port verbatim; change the `Cargo.lock` regex crate
name to `minimail`; correct the "single lockfile entry" comment (minimail has deps, but the
name-anchored regex still matches and no `cargo update` is needed on a version bump).

### `.gitignore`

```
target
/mail
```

### `.dockerignore`

```
target/
mail/
data/
.git/
.github/
*.md
creds.example
smoketest.py
Dockerfile
.dockerignore
```
(Do **not** exclude `src/ui/` — it is needed for `include_str!` in the builder.)

### `creds.example`

```
# minimail credentials: one USER = PASSWORD per line.
# '#' starts a comment anywhere; both sides are trimmed.
# A password containing '#' or leading/trailing spaces cannot be represented.
alice = alicepass
bob   = bobpass
```

---

## 13. Implementation order

Each module lists the exact set of other modules whose signatures it may rely on. Build a layer only
after all lower layers are frozen.

**L0 — leaves, buildable immediately, in parallel (deps: std only):**
- `util.rs` — none
- `url.rs` — none
- `base64.rs` — none
- `qp.rs` — none
- `json.rs` — none
- `creds.rs` — none
- `assets.rs` — none (needs `src/ui/*` files to exist; the UI is written in parallel)
- `events.rs` — none
- `stream.rs` — none (default build); the `Tls` variant references `rustls` only under the feature

**L1 — one hop (in parallel once their L0 deps are frozen):**
- `mime.rs` — may rely on: `base64`, `qp`, `util`
- `http.rs` — may rely on: `util`, `url`
- `tls.rs` (`#[cfg(feature="tls")]`) — may rely on: `stream`

**L2:**
- `store.rs` — may rely on: `mime`, `json`, `util`

**L3 (freeze `Server` before L4):**
- `server.rs` — may rely on: `store`, `creds`, `events`, and (feature) `rustls` for the `tls` field

**L4 — the two protocol handlers, in parallel:**
- `smtp.rs` — may rely on: `server`, `store`, `creds`, `base64`, `util`, `events`, `stream`,
  (feature) `tls`
- `api.rs` — may rely on: `server`, `http`, `store`, `mime`, `json`, `base64`, `creds`, `events`,
  `url`, `util`, `assets`, `stream`

**L5 — wiring:**
- `main.rs` — may rely on: `server`, `store`, `creds`, `smtp`, `api`, `util`, `stream`, `http`,
  (feature) `tls`. Owns `Config`. Binds listeners; SMTP (and optional SMTPS) loops on spawned threads,
  HTTP loop on the main thread so `main` never returns.

The Web UI (`src/ui/index.html`, `app.css`, `app.js`, `favicon.svg`) is written in parallel with L0
against the §8 API and §9 spec; `assets.rs` embeds it.

---

## Appendix — Resolved judge findings

| # | Fatal flaw raised | Resolution |
|---|-------------------|------------|
| F1 | Angle 1 returns 555 on unrecognized MAIL params while advertising 8BITMIME/SMTPUTF8, killing Nodemailer/modern clients | §6: `parse_addr` extracts the address, `parse_size` honors only `SIZE=`; **all other MAIL/RCPT params are silently ignored, 555 is never emitted**. |
| F2 | No bare-LF tolerance in command/DATA/header readers (breaks raw telnet, hangs on `LF.LF`) | §6 command reader and `find_data_terminator` accept CRLF **and** bare LF; §7 `unfold_headers` ends the header block on CRLF or bare LF. |
| F3 | RFC 2047: must drop whitespace between adjacent encoded-words and concatenate same-charset bytes before decode | §7 `decode_encoded_words` mandates byte-accumulation before charset decode and adjacent-word whitespace elision; both covered by required mime tests. |
| F4 | Multipart boundary CRLF ownership unspecified → attachment corruption | §7: the line ending immediately preceding `--boundary` belongs to the delimiter and is stripped from the part body; required byte-exact binary-attachment test. |
| F5 | Advertising SMTPUTF8 with an ASCII-only command reader is inconsistent | §6 command reader is byte-transparent (`from_utf8_lossy`), UTF-8 reverse-paths are not rejected, and the SMTPUTF8 param is accepted-and-ignored. |
| F6 | Dot-unstuffing/terminator lived in socket-coupled readers; untestable | §5/§6: `dot_unstuff(&[u8])` and `find_data_terminator(&[u8])` are pure `pub` functions with mandated RFC 5321 5.2 test vectors. |
| F7 | Angle 1 STARTTLS infeasible: cannot recover `TcpStream` from `dyn ReadWrite` | Transport is `enum Stream { Plain(TcpStream), [Tls] }` with `into_tcp()`; `tls::accept` takes a concrete `TcpStream`. §10. |
| F8 | `mime.rs` LOC understated | Budgeted at **800 LOC** (§4), total ~5,160, inside the 4–6k window. |
| F9 | No read/write timeouts; idle clients/SSE pin threads | `stream.rs` exposes `set_read_timeout`/`set_write_timeout`; SMTP sets a 300s read timeout. SSE remains long-lived by design — documented dev-scale limit (§8, Risks). |
| F10 | SMTP `Session` coupled to IO, hard to unit-test | Pure socket-free `Session` (`handle_line`→`Step`, `on_data`→`Reply`), thin `serve()` driver. §5/§6/§11. |
| F11 | Angle 1 smtp had no way to serialize the SSE payload (no `json` dep, private serializer) | `Summary::to_json` is **public** on `store`; the SSE frame is owned by `events::Event::frame()`; smtp publishes `Event::Message(summary.to_json())`. |
| F12 | `Server` defined in `main.rs` creates a consumer-owns-shared-type cycle | `Server` (and `Hub`) owned by a dedicated `server.rs`/`events.rs`, frozen at L3 before L4. §4/§5. |
| F13 | SSE frame schema had no owner (multiple producers, UI consumer drift) | `events.rs::Event` is the single typed source of truth; every producer and the UI derive frames from `Event::frame()`. |
| F14 | Summary JSON serialized in two places | All summary serialization routes through `Summary::to_json`; the API projects fields from it. §5 store. |
| F15 | Store `Mutex` second-guesses minibucket's lock-free posture | `Store` is `#[derive(Clone)]` over `PathBuf`, **no lock**; rename atomicity + `AtomicU32` ids; prune-vs-intake race accepted and documented. §3. |
| F16 | Sidecar must be JSON-escaped, not `writeln!("k: {v}")` | Sidecar built via `json::Json::Obj().to_string()`; escaping tested with quote/newline/emoji subject. §3/§11. |
| F17 | DATA reader must be 8-bit/binary-safe, not `http::read_line_limited` | §6 mandates a separate byte-clean DATA reader; never reuse the UTF-8-rejecting HTTP line reader. |
| F18 | STARTTLS buffer-reset (RFC 3207 plaintext injection) | §6/§10: assert BufReader buffer empty before handshake (else 501), `into_inner`/`into_tcp` + fresh reader; required test. |
| F19 | API streaming helper socket type must match transport | All private `serve_*`/`get_*` helpers take `sock: &mut crate::stream::Stream`. §5 api. |
| F20 | `Json` f64 rounding of sizes/timestamps | `Json::Int(i64)` variant; `size`/`received_unix` serialized as `Int`. §5 json. |
