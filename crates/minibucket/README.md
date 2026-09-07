# minibucket

A tiny, **dependency-free** S3-compatible object storage server, written in pure Rust.

No `tokio`, no `hyper`, no `aws-sdk`, no `ring` just the standard library. The whole thing is around 3.5k lines of code and compiles into a single static binary you can drop on any machine.

```
$ cargo build --release -p minibucket
$ ./target/release/minibucket
minibucket listening on http://127.0.0.1:9000 (root: ./data)
  region: us-east-1
  access-key: minioadmin
```

Then point any S3 client at it:

```bash
aws --endpoint-url http://127.0.0.1:9000 s3 mb s3://photos
aws --endpoint-url http://127.0.0.1:9000 s3 cp cat.jpg s3://photos/
aws --endpoint-url http://127.0.0.1:9000 s3 ls s3://photos/
```

## Why?

Most local S3 servers (MinIO, LocalStack, s3mock) are great but they're either large Go binaries, JVM-heavy, or pull in hundreds of dependencies. minibucket exists to answer one question: *how small can an honestly-useful S3 server be if you write everything yourself?*

It's useful for:

- **Local development** when you don't want a Docker container running just for object storage.
- **Air-gapped or embedded** deployments where binary size and audit surface matter.
- **Learning** every wire-format detail (SigV4, chunked uploads, XML responses) lives in plain Rust you can read in an afternoon.

## Features

- **S3 wire-compatible**: works with the AWS CLI, `boto3`, `aws-sdk-*`, MinIO client, `s3cmd`, etc.
- **AWS Signature V4** authentication, including `STREAMING-AWS4-HMAC-SHA256-PAYLOAD` chunked uploads.
- **Buckets**: create, delete, list.
- **Objects**: PUT, GET, HEAD, DELETE, COPY, range requests, conditional headers.
- **Multipart uploads**: initiate, upload-part, complete, abort, list.
- **Object tagging**: PUT/GET/DELETE tagging.
- **Bucket versioning**: enable, suspend, version listings, version-aware GET/DELETE.
- **Path-style** (`host/bucket/key`) and **virtual-hosted** (`bucket.host/key`) addressing.
- **Multi-tenant credentials** via `--credentials` file.
- **Anonymous mode** for throwaway dev setups.

## Install

minibucket lives in the [minisuite](https://github.com/p-arndt/minisuite)
workspace. Build it from the repository root:

```bash
git clone https://github.com/p-arndt/minisuite
cd minisuite
cargo build --release -p minibucket
```

The resulting `target/release/minibucket` is self-contained.

### Docker

Prebuilt images are published to GitHub Container Registry for `linux/amd64`
and `linux/arm64`:

```bash
docker pull ghcr.io/p-arndt/minibucket:latest
```

The image is built from `scratch` and contains nothing but the static binary,
so it's only a few MB. It listens on `0.0.0.0:9000` and stores data in the
`/data` volume by default:

```bash
docker run --rm -p 9000:9000 -v minibucket-data:/data ghcr.io/p-arndt/minibucket:latest
```

The default `CMD` is `--bind 0.0.0.0:9000 --root /data`. Override it to pass any
of the options below — for example, anonymous mode:

```bash
docker run --rm -p 9000:9000 -v minibucket-data:/data \
  ghcr.io/p-arndt/minibucket:latest --bind 0.0.0.0:9000 --root /data --anonymous
```

Every option can also be given as an environment variable, which is usually
nicer in compose files:

```bash
docker run --rm -p 9000:9000 -v minibucket-data:/data   -e MINIBUCKET_ACCESS_KEY=alice -e MINIBUCKET_SECRET_KEY=alicepass   ghcr.io/p-arndt/minibucket:latest
```

To build the image yourself, use the workspace Dockerfile at the repository
root (see the minisuite README for the exact invocation).

## Usage

```
Usage: minibucket [options]
  --bind ADDR              default 127.0.0.1:9000
  --root DIR               default ./data
  --access-key K           access key id (use with --secret-key)
  --secret-key S           secret key (must follow --access-key)
  --credentials FILE       load multiple KEY=SECRET lines
  --region R               default us-east-1
  --domain D               enable virtual-hosted addressing for bucket.D
  --anonymous              disable auth (dev only)
  -h, --help               show this help
  -V, --version            show the version
```

### Environment variables

Every flag has an environment-variable twin: `MINIBUCKET_` plus the flag name in
upper case with dashes replaced by underscores. Boolean flags accept
`1`/`true`/`yes`/`on` and `0`/`false`/`no`/`off` (case-insensitive).

| Flag | Environment variable |
| --- | --- |
| `--bind` | `MINIBUCKET_BIND` |
| `--root` | `MINIBUCKET_ROOT` |
| `--access-key` | `MINIBUCKET_ACCESS_KEY` |
| `--secret-key` | `MINIBUCKET_SECRET_KEY` |
| `--credentials` | `MINIBUCKET_CREDENTIALS` |
| `--region` | `MINIBUCKET_REGION` |
| `--domain` | `MINIBUCKET_DOMAIN` |
| `--anonymous` | `MINIBUCKET_ANONYMOUS` |

The environment is read first and command-line flags override it:

```bash
MINIBUCKET_BIND=0.0.0.0:9000 MINIBUCKET_ANONYMOUS=1 minibucket
```

### Examples

Run with default dev credentials (`minioadmin` / `minioadmin`):

```bash
minibucket --root ./data
```

Run with custom credentials and virtual-hosted addressing:

```bash
minibucket \
  --bind 0.0.0.0:9000 \
  --root /var/lib/minibucket \
  --access-key AKIAEXAMPLE --secret-key wJalrXUtnFEMI/K7MDENG \
  --domain s3.local
```

Then `bucket.s3.local:9000` resolves to the bucket `bucket`.

Multi-user setup:

```bash
# creds.txt
alice=secret-for-alice
bob=secret-for-bob

minibucket --credentials creds.txt
```

Anonymous (no auth — local dev only):

```bash
minibucket --anonymous
```

### Use as a library

The crate is also a library, so you can embed the server in your own process —
that is how the `minisuite` launcher runs it alongside the other services. The
split is deliberate: `parse_args` only reads configuration, `prepare` does
everything that can fail (bind the socket, create the root directory, load
credentials) and `serve` blocks. Nothing in the library calls
`std::process::exit`.

```rust
let cfg = minibucket::parse_args(std::env::args().skip(1))?; // or Config::default()
let ready = minibucket::prepare(cfg)?;
eprint!("{}", ready.banner());
std::thread::spawn(move || ready.serve());
```

`Prepared` is `Send`, and `Config` is `Clone + Debug` with public fields, so you
can also build one by hand:

```rust
let cfg = minibucket::Config {
    bind: "127.0.0.1:9000".to_string(),
    root: "./data".into(),
    keys: vec![("alice".to_string(), "alicepass".to_string())],
    ..Default::default()
};
```

## On-disk layout

Objects live as plain files under `--root`:

```
data/
  <bucket>/
    .minibucket/           # metadata: tags, versioning config, multipart state
    <key>                  # object bytes
```

You can `ls`, `cat`, back up, or sync the directory with normal tools, there's no opaque database.

## Project layout

```
src/
  main.rs        # thin CLI wrapper around the library
  lib.rs         # config/env parsing, startup, accept loop, per-connection auth
  http.rs        # minimal HTTP/1.1 request parser + response writer
  s3.rs          # S3 API dispatch and XML responses
  sigv4.rs       # AWS SigV4 signing + streaming chunk verification
  sha256.rs      # SHA-256
  hmac.rs        # HMAC-SHA256
  md5.rs         # MD5 (for ETags)
  storage.rs     # filesystem-backed object store
  multipart.rs   # multipart upload state machine
  tagging.rs     # object tagging
  versioning.rs  # bucket versioning
  creds.rs       # credential store
  url.rs         # URL encode/decode
  util.rs        # date/time, request IDs, misc
```

Every crypto primitive (SHA-256, HMAC, MD5) is hand-rolled  small, readable, and dependency-free. **Not** a substitute for an audited crypto library if you're putting this on the public internet.

## Compatibility

Tested against:

- `aws` CLI v2
- `boto3`
- MinIO `mc` client
- `s3cmd`

If your client does something minibucket doesn't understand, open an issue with the request and response, most gaps are a few hours of work.

## Not included

minibucket deliberately leaves out:

- IAM policies, bucket policies, ACLs (single-tier credentials only)
- Server-side encryption
- Lifecycle rules, replication, inventory, analytics
- HTTPS (terminate TLS with a reverse proxy)
- Event notifications

If you need any of those, you probably want MinIO or actual S3.

## License

MIT
