# minimail

A tiny, **dependency-free** development SMTP sink with a web UI and JSON API, written in pure Rust.

No `tokio`, no `hyper`, no `serde`, no `lettre`, no crypto crate — just the standard library. It speaks SMTP on one port, accepts every message, and **never delivers onward**. Captured mail is retrieved through an embedded web UI and a JSON API on a second port. The whole thing is around 5k lines and compiles into a single static binary you can drop on any machine.

```
$ cargo build --release
$ ./target/release/minimail
minimail 0.1.0
  SMTP  smtp://127.0.0.1:1025
  HTTP  http://127.0.0.1:8025
  root  ./mail
  auth  1 credential(s)
```

Point your app's SMTP config at `127.0.0.1:1025`, then open <http://127.0.0.1:8025> to read what it sent.

## Why?

Most local mail catchers (MailHog, Mailpit) are great, but they're Go binaries with a build chain to match. minimail exists to answer one question: *how small can an honestly-useful dev mail sink be if you write everything yourself?* — the MIME parser, the SMTP state machine, the HTTP server, the JSON, the base64, all of it, on `std`.

It's useful for:

- **Local development** when you want to see the mail your app sends without wiring up a real provider or risking a message escaping to a customer.
- **CI and integration tests** — send to it, then assert over the JSON API. It's a single static binary with no runtime.
- **Learning** — every wire-format detail (SMTP AUTH, quoted-printable, RFC 2047 encoded-words, multipart trees, SSE) lives in plain Rust you can read in an afternoon.

It is a **dev tool**. It binds to localhost by default, has no access control beyond one optional shared credential, and treats captured mail as untrusted — see [Not included](#not-included).

## Features

- **SMTP sink** (RFC 5321): EHLO/HELO, MAIL, RCPT, DATA, RSET, NOOP, VRFY, QUIT. Advertises `PIPELINING`, `8BITMIME`, `SMTPUTF8`, `SIZE`, `ENHANCEDSTATUSCODES`, `AUTH PLAIN LOGIN`. Accepts every message and stores it — never relays.
- **SMTP AUTH** PLAIN + LOGIN (RFC 4954) against a `KEY=SECRET` credentials file, or `--anonymous` to require none.
- **Full MIME parsing**: header unfolding, RFC 2047 encoded-words (B and Q), multipart trees, quoted-printable + base64 transfer decoding, charset transcoding (us-ascii, utf-8, latin-1/-15, windows-1252) to UTF-8, attachment extraction.
- **JSON API**: list, search, read, delete messages; fetch the raw `.eml`, the decoded HTML body, or any MIME part (with HTTP range support).
- **Embedded web UI**: served from the binary, no build step, no CDN.
- **Live updates** over Server-Sent Events — new mail shows up in the UI as it arrives.
- **Plain-file storage** under `--root`: one byte-exact `.eml` plus a `.json` sidecar per message. `ls`, `cat`, `grep`, and `rsync` all work.
- **Retention cap** via `--max-messages N` (prune oldest).
- **Optional TLS** (STARTTLS + implicit SMTPS) behind a default-off `tls` Cargo feature. The default build pulls in **zero** dependencies.

## Install

```bash
git clone https://github.com/p-arndt/minimail
cd minimail
cargo build --release
```

The resulting `target/release/minimail` is self-contained.

### Docker

Prebuilt images are published to GitHub Container Registry for `linux/amd64`
and `linux/arm64`:

```bash
docker pull ghcr.io/p-arndt/minimail:latest
```

The image is built from `scratch` and contains nothing but the static binary,
so it's only a few MB. It exposes SMTP on `1025` and HTTP on `8025` and stores
mail in the `/data` volume:

```bash
docker run --rm -p 1025:1025 -p 8025:8025 \
  -v minimail-data:/data ghcr.io/p-arndt/minimail:latest
```

The default `CMD` is `--smtp-bind 0.0.0.0:1025 --http-bind 0.0.0.0:8025 --root /data`.
Override it to pass any of the options below — for example, a fixed credential:

```bash
docker run --rm -p 1025:1025 -p 8025:8025 -v minimail-data:/data \
  ghcr.io/p-arndt/minimail:latest \
  --smtp-bind 0.0.0.0:1025 --http-bind 0.0.0.0:8025 --root /data \
  --user dev --password dev
```

Or with Docker Compose:

```yaml
services:
  minimail:
    image: ghcr.io/p-arndt/minimail:latest
    ports:
      - "1025:1025"   # SMTP
      - "8025:8025"   # HTTP UI + API
    volumes:
      - minimail-data:/data
      - ./minimail.creds:/minimail.creds:ro
    command: >
      --smtp-bind 0.0.0.0:1025 --http-bind 0.0.0.0:8025 --root /data
      --credentials /minimail.creds

volumes:
  minimail-data:
```

with a `minimail.creds` file next to the compose file — one `KEY=SECRET` per
line (`#` starts a comment; the first `=` splits, so secrets may contain `=`):

```
# key=secret, used for both SMTP AUTH and HTTP Basic on the UI/API
dev=devpassword
```

Then `docker compose up` and point your app at `127.0.0.1:1025`. For a single
throwaway credential you can skip the file and pass `--user dev --password dev`
in `command:` instead; or `--anonymous` for no auth at all.

To build the image yourself:

```bash
docker build -t minimail .
```

## Usage

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

With no `--anonymous` and no credentials given, minimail adds a default dev
credential `minimail` / `minimail` (same posture as minibucket), used for both
SMTP AUTH and HTTP Basic on the API/UI. The `--tls-*` and `--smtps-bind` flags
are listed above so you know the feature exists, but on the **default build**
they error at parse time — you need a `--features tls` build to use them (see
[TLS](#tls)).

### Examples

Run anonymously (no auth on SMTP or the UI), keeping only the last 500 messages:

```bash
minimail --anonymous --max-messages 500
```

**Send a test message** with [swaks](https://github.com/jetmore/swaks):

```bash
swaks --server 127.0.0.1:1025 \
  --from alice@example.com --to bob@example.com \
  --header 'Subject: Hello from swaks' --body 'It works.'
```

...or with Python's `smtplib`:

```python
import smtplib
from email.message import EmailMessage

msg = EmailMessage()
msg["From"] = "alice@example.com"
msg["To"] = "bob@example.com"
msg["Subject"] = "Hello from Python"
msg.set_content("It works. ☀")

with smtplib.SMTP("127.0.0.1", 1025) as s:
    s.send_message(msg)
```

**Read it back** over the JSON API (add `-u minimail:minimail` if you didn't run
`--anonymous`):

```bash
# newest-first list (supports ?limit= ?offset= ?q=)
curl http://127.0.0.1:8025/api/v1/messages

# search subject/from/to
curl 'http://127.0.0.1:8025/api/v1/messages?q=hello'

# full detail: summary + decoded headers + text/html bodies + MIME part tree
curl http://127.0.0.1:8025/api/v1/messages/<id>

# the pristine .eml (Range requests supported)
curl http://127.0.0.1:8025/api/v1/messages/<id>/raw

# the decoded HTML body, or a specific part (?download to force a download)
curl http://127.0.0.1:8025/api/v1/messages/<id>/html
curl http://127.0.0.1:8025/api/v1/messages/<id>/parts/2 -o attachment.bin

# server info, and delete-all
curl http://127.0.0.1:8025/api/v1/info
curl -X DELETE http://127.0.0.1:8025/api/v1/messages
```

`GET /api/v1/events` is a Server-Sent Events stream that pushes `message`,
`delete`, and `clear` frames as they happen; `GET /healthz` is always public and
returns `ok`.

**Open the UI** at <http://127.0.0.1:8025> to browse, search, and read mail (with
live updates) in a browser.

## On-disk layout

Every message is two plain files under `--root`:

```
mail/
  messages/
    <id>.eml          raw DATA bytes, byte-exact (dot-unstuffed, CRLF preserved)
    <id>.json         sidecar: envelope + parsed summary (UTF-8 JSON)
```

The `.eml` is the exact post-transparency DATA payload — **no `Received:` header is
prepended**, so it stays pristine. All transport/envelope metadata (the real
`MAIL FROM`, `RCPT TO`, peer address, auth user, HELO name) lives **only in the
`.json` sidecar** — worth knowing if you `grep` the `.eml` for the envelope
sender and don't find it. Attachments are not stored separately; they're
re-extracted from the `.eml` on demand, so the `.eml` is the single source of
truth. Message ids are time-sortable, so a plain directory sort is chronological.

## Project layout

```
src/
  main.rs      # CLI, Config, accept loops, startup banner
  server.rs    # shared Server state
  smtp.rs      # SMTP session state machine + serve() driver
  api.rs       # HTTP JSON API + UI + SSE dispatch
  http.rs      # minimal HTTP/1.1 request parser + response writer
  mime.rs      # MIME parse: headers, 2047, multipart, CTE, charset
  store.rs     # file-backed spool: ids, atomic write, retention, Summary
  stream.rs    # transport enum (plain TCP, or TLS under the feature)
  events.rs    # SSE hub + typed event frames
  assets.rs    # embedded web UI assets (include_str!)
  creds.rs     # credential store + constant_time_eq
  json.rs      # JSON value + serializer + parser + escaper
  base64.rs    # RFC 4648 base64
  qp.rs        # quoted-printable + RFC 2047 Q decode
  util.rs      # date format/parse (RFC 5322 + ISO-8601), ids
  url.rs       # percent decode/encode, query parse
  tls.rs       # rustls wrapper (entire file behind --features tls)
  ui/          # index.html, app.css, app.js, favicon.svg (compiled in)
```

Every codec and primitive (base64, quoted-printable, JSON, the MIME parser, the
date engine) is hand-rolled — small, readable, dependency-free. The MIME parser
never panics: it parses leniently and returns partial results on malformed input,
because captured mail is attacker-controlled.

## TLS

TLS is **off by default** and the default build has zero dependencies. Build with
the feature to get STARTTLS and implicit SMTPS:

```bash
cargo build --release --features tls
minimail --tls-cert cert.pem --tls-key key.pem       # advertises STARTTLS
minimail --tls-cert cert.pem --tls-key key.pem \
         --smtps-bind 127.0.0.1:465                   # + implicit SMTPS listener
```

The feature pulls in exactly two crates — `rustls` and `rustls-pemfile` — and
`src/tls.rs` is the only file that names them; it's `#[cfg]`-compiled out
entirely otherwise. minimail does not generate certificates; bring your own PEM
cert chain and key. TLS is **SMTP-only** — the web UI/API stays plain HTTP by
design (dev tool; put it behind a reverse proxy if you need HTTPS there).

## Compatibility

Tested with:

- Python `smtplib` / `email` (the end-to-end `smoketest.py` drives a real SMTP
  conversation and asserts the result through the JSON API — including a
  byte-exact binary attachment round-trip and a byte-exact `/raw` fetch).
- `swaks` and hand-driven `telnet`/`openssl s_client` sessions.

If your client does something minimail doesn't understand, open an issue with the
SMTP transcript — most gaps are a few hours of work.

## Not included

minimail deliberately leaves out:

- **POP3 and IMAP** — retrieval is HTTP-only (the JSON API + UI).
- **Any onward delivery or relay** — it's a sink; mail goes in and stops.
- **Spam/virus filtering, DKIM/SPF checks** — it accepts everything.
- **Certificate generation** — bring your own PEM under `--features tls`.
- **HTTPS for the web UI**, multi-mailbox/multi-tenant layout, a database, a
  thread pool or connection cap, graceful shutdown.

It is a **development tool**: localhost, low-stakes, single shared credential.
Don't put it on the public internet. If you need a real mailbox server, you want
something else — this one is proud of being small.

## License

MIT
</content>
</invoke>
