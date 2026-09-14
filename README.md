# minisuite

Three tiny, **dependency-free** dev servers, written in pure Rust, in one workspace and (if you want) one process:

| | What | Ports | Stands in for |
|---|---|---|---|
| [**minicloak**](crates/minicloak) | OpenID Connect provider, real RS256 JWTs | 9500 | Keycloak, Dex, Hydra |
| [**minimail**](crates/minimail) | SMTP sink with web UI + JSON API | 1025 SMTP, 8025 HTTP | MailHog, Mailpit |
| [**minibucket**](crates/minibucket) | S3-compatible object storage | 9000 | MinIO, LocalStack |
| **minisuite** | all three in one binary, plus a landing page | 9900 | `docker compose up` for the above |

No `tokio`, no `hyper`, no `serde`, no crypto crates. SHA-256, RSA, SigV4, MIME, JSON, TOML and HTTP/1.1 are all hand-written on `std`. Each binary is a few MB, static, and starts in milliseconds.

```
$ minisuite
minisuite 0.2.0
  data     ./data
  landing  http://127.0.0.1:9900

minicloak
  minicloak listening on http://127.0.0.1:9500
    discovery:   http://127.0.0.1:9500/realms/dev/.well-known/openid-configuration
    users:       alice, bob
    client:      myapp (confidential) -> http://localhost:3000/*
    client:      spa (public, PKCE required) -> http://localhost:5173/*

minimail
    SMTP  smtp://127.0.0.1:1025
    HTTP  http://127.0.0.1:8025

minibucket
  minibucket listening on http://127.0.0.1:9000
    access-key: minioadmin
```

Open <http://127.0.0.1:9900> for a page that links to everything and shows the dev credentials.

These are **development tools**. They bind to localhost by default, ship with trivial default credentials, and must not face a network you do not trust.

## Quick start

### Docker, everything in one container

```bash
docker run --rm \
  -p 9500:9500 -p 9000:9000 -p 1025:1025 -p 8025:8025 -p 9900:9900 \
  -v minisuite-data:/data \
  ghcr.io/p-arndt/minisuite:latest
```

The image is built from `scratch`, contains nothing but the static binary, runs as a non-root user and keeps all state in the `/data` volume.

### Docker Compose, one container per service

```bash
docker compose up -d
```

[`compose.yml`](compose.yml) runs the three single-service images with named volumes. Host ports are published on `127.0.0.1` only; drop the prefix in the `ports:` entry to expose a service to the LAN (and replace its default credentials first). Every option is an environment variable, so tweaking credentials or ports is a one-line edit:

```yaml
    environment:
      MINIBUCKET_ACCESS_KEY: "alice"
      MINIBUCKET_SECRET_KEY: "alicepass"
```

Use `docker compose -f compose.yml -f compose.build.yml up --build` to build the images from this checkout instead of pulling them.

### Native binary

Grab an archive from the [releases page](https://github.com/p-arndt/minisuite/releases). Each one contains all four binaries for its platform (Linux x86_64 / aarch64 static musl, Windows x86_64, macOS Apple silicon / Intel) plus the example config files. Nothing to install alongside.

Or build from source:

```bash
git clone https://github.com/p-arndt/minisuite
cd minisuite
cargo build --release --workspace
# -> target/release/{minisuite,minicloak,minimail,minibucket}
```

## Configuration

Every server is configured the same three ways, and they compose:

1. **CLI flags**, listed by `<binary> --help`.
2. **Environment variables**: `MINICLOAK_`, `MINIMAIL_`, `MINIBUCKET_` or `MINISUITE_` plus the flag name in upper case with dashes turned into underscores. `--smtp-bind` becomes `MINIMAIL_SMTP_BIND`. Boolean flags take `1|true|yes|on` or `0|false|no|off`. A flag beats its env var.
3. **`minisuite.toml`** for the suite binary. Each `[section]` is one service and each key is that crate's flag without the dashes; minisuite rebuilds the command line and hands it to the crate, so the crate's `--help` is the authoritative list of keys:

```toml
[minicloak]
user = ["alice=alice", "bob=bob"]         # arrays for repeatable flags

[minimail]
smtp_bind = "127.0.0.1:1025"              # underscores or dashes, both work
anonymous = true                          # true -> the bare flag

[minibucket]
access_key = "minioadmin"
secret_key = "minioadmin"
```

Per service the precedence is `minisuite.toml` > that service's env vars > minisuite's own defaults (the `--data` layout, `--bind-all`) > the crate's defaults. So `MINIMAIL_SMTP_BIND=0.0.0.0:2525 minisuite --bind-all` really does move SMTP to 2525. See [`minisuite.example.toml`](minisuite.example.toml) for every key with comments.

### minisuite flags

```
  --data DIR               state directory, default ./data
                           (minicloak key -> DIR/minicloak/key.pem,
                            minimail root -> DIR/minimail,
                            minibucket root -> DIR/minibucket)
  --config FILE            minisuite.toml; default ./minisuite.toml if present
  --bind-all               bind every service on 0.0.0.0 instead of 127.0.0.1
  --landing-bind ADDR      default 127.0.0.1:9900 (0.0.0.0:9900 with --bind-all)
  --no-landing             do not serve the landing page
  --only LIST              comma-separated subset of minicloak,minimail,minibucket
```

## Default dev credentials

| Service | Credential |
|---|---|
| minicloak users | `alice` / `alice`, `bob` / `bob` |
| minicloak clients | `myapp` (secret `s3cret`, redirect `http://localhost:3000/*`), `spa` (public, PKCE, `http://localhost:5173/*`) |
| minimail | `minimail` / `minimail` for SMTP AUTH and the web UI, or `--anonymous` |
| minibucket | access key `minioadmin`, secret `minioadmin`, region `us-east-1` |

Each crate's README documents how to replace them, and the per-crate example files (`crates/minicloak/minicloak.example.toml`, `crates/*/creds.example`) ship in every release archive.

## Development

Everything runs through [`just`](https://github.com/casey/just) (`winget install Casey.Just` or `cargo install just`). `just` alone lists the recipes.

| Recipe | What it does |
|---|---|
| `just run [crate] [args]` | `cargo run` a binary; default is the suite |
| `just test`, `just clippy`, `just fmt` | the usual, across the workspace |
| `just smoke [crate]` | build, start the server, run the Python end-to-end suite, stop. `all` also runs every suite against one `minisuite` process |
| `just ci` | fmt-check + clippy + test + smoke, the same gate as CI |
| `just docker [target]` | build one scratch image from the multi-target [`Dockerfile`](Dockerfile) |
| `just docker-run [target]` | run it with the right port mapping |
| `just release [patch\|minor\|major\|x.y.z]` | cut a release, see below |

The smoke tests need Python 3 and, for minibucket, `pip install boto3`.

### Layout

```
crates/minicloak     OIDC provider        (lib + bin)
crates/minimail      SMTP sink            (lib + bin)
crates/minibucket    S3 server            (lib + bin)
crates/minisuite     launcher + landing   (depends on the three above)
Dockerfile           one builder stage, four scratch runtime stages
compose.yml          three published images; compose.build.yml overlay builds locally
scripts/smoke.mjs    cross-platform smoke runner used by `just smoke` and CI
.github/workflows    ci.yml (fmt/clippy/test on 3 OS, smoke, openssl interop, docker)
                     release.yml (5 targets, 4 images on ghcr.io)
```

Each crate is a library with a thin `main.rs`: `parse_args` (flags + env), `prepare` (bind sockets, load state, fail cleanly) and `Prepared::serve`. That is what lets minisuite run them in-process, and what lets you embed one in your own integration tests. The three crates were merged from their original repositories with `git subtree`, so their full history is in this repo.

### Releasing

There is exactly one version, `[workspace.package] version` in the root `Cargo.toml`; every crate inherits it and every image is tagged with it. Releases are cut with [stamp](https://github.com/p-arndt/stamp):

```bash
just release-dry minor   # the plan and every preflight check, nothing written
just release minor       # bump, refresh Cargo.lock, commit, tag v0.x.0, push
```

Pushing the tag triggers the release workflow, which validates the tag with the stamp action, builds the archives for all five targets and pushes `ghcr.io/p-arndt/{minicloak,minimail,minibucket,minisuite}` for `linux/amd64` and `linux/arm64`.

## License

MIT, see [LICENSE](LICENSE).
