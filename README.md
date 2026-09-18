# minisuite

**Auth, mail and S3 for local development, in one small container.**

Three tiny dev servers in pure Rust with zero dependencies. Run them together as one process or each on its own:

| | What | Ports | Stands in for |
|---|---|---|---|
| [**minicloak**](crates/minicloak) | OpenID Connect provider, real RS256 JWTs | 9500 | Keycloak, Dex, Hydra |
| [**minimail**](crates/minimail) | SMTP sink with web UI + JSON API | 1025 SMTP, 8025 HTTP | MailHog, Mailpit |
| [**minibucket**](crates/minibucket) | S3-compatible object storage | 9000 | MinIO, LocalStack |
| **minisuite** | all three in one binary, plus a landing page | 9900 | the three above in one container |

> [!WARNING]
> These are **development tools**. They ship with trivial default credentials and must not face a network you do not trust.

## Quick start

Save this as `compose.yml` and run `docker compose up -d`:

```yaml
services:
  minisuite:
    image: ghcr.io/p-arndt/minisuite:latest
    ports:
      - "127.0.0.1:9900:9900"   # landing page
      - "127.0.0.1:9500:9500"   # minicloak (OIDC)
      - "127.0.0.1:1025:1025"   # minimail SMTP
      - "127.0.0.1:8025:8025"   # minimail web UI + API
      - "127.0.0.1:9000:9000"   # minibucket (S3)
    volumes:
      - minisuite-data:/data

volumes:
  minisuite-data:
```

Then open <http://localhost:9900>. The landing page links to every service and shows the dev credentials:

| Service | URL | Default login |
|---|---|---|
| OIDC issuer | `http://localhost:9500/realms/dev` | users `alice` / `alice`, `bob` / `bob` |
| OIDC clients | | `myapp` / `s3cret` (redirect `http://localhost:3000/*`), `spa` (public, PKCE, `http://localhost:5173/*`) |
| SMTP | `smtp://localhost:1025` | `minimail` / `minimail` |
| Mail web UI + API | <http://localhost:8025> | `minimail` / `minimail` |
| S3 endpoint | `http://localhost:9000` | `minioadmin` / `minioadmin`, region `us-east-1` |

Or with a single command:

```bash
docker run --rm \
  -p 9900:9900 -p 9500:9500 -p 1025:1025 -p 8025:8025 -p 9000:9000 \
  -v minisuite-data:/data \
  ghcr.io/p-arndt/minisuite:latest
```

The image is built from `scratch`, contains nothing but a static binary of a few MB, runs as a non-root user and keeps all state in `/data`.

## Configuration

Every option is a CLI flag, an environment variable, or a key in `minisuite.toml`.

**Environment variables** are the service prefix (`MINICLOAK_`, `MINIMAIL_`, `MINIBUCKET_`, `MINISUITE_`) plus the flag name in upper case with dashes turned into underscores: `--smtp-bind` becomes `MINIMAIL_SMTP_BIND`. Booleans take `1|true|yes|on` or `0|false|no|off`. A flag beats its env var.

**`minisuite.toml`** has one `[section]` per service; each key is that service's flag without the leading dashes. [`minisuite.example.toml`](minisuite.example.toml) lists every key with comments.

```toml
[minicloak]
user = ["alice=alice", "bob=bob"]

[minimail]
anonymous = true

[minibucket]
access_key = "minioadmin"
```

Precedence per service: `minisuite.toml` > env vars > minisuite's own defaults (`--data` layout, `--bind-all`) > the service's defaults.

Each service's full option list is in its README ([minicloak](crates/minicloak), [minimail](crates/minimail), [minibucket](crates/minibucket)) and in `<binary> --help`. The suite binary itself adds:

```
  --data DIR               state directory, default ./data (one subdirectory per service)
  --config FILE            minisuite.toml; default ./minisuite.toml if present
  --bind-all               bind every service on 0.0.0.0 instead of 127.0.0.1
  --landing-bind ADDR      default 127.0.0.1:9900 (0.0.0.0:9900 with --bind-all)
  --no-landing             do not serve the landing page
  --only LIST              comma-separated subset of minicloak,minimail,minibucket
```

## Other ways to run it

**One container per service.** [`compose.yml`](compose.yml) in this repo runs the three single-service images (`ghcr.io/p-arndt/{minicloak,minimail,minibucket}`) with every option commented. `docker compose up -d` pulls them; `docker compose -f compose.yml -f compose.build.yml up --build` builds them from this checkout.

**Native binary.** Grab an archive from the [releases page](https://github.com/p-arndt/minisuite/releases). Each contains all four binaries for its platform (Linux x86_64 / aarch64 static musl, Windows x86_64, macOS Apple silicon / Intel) plus the example config files. Run `minisuite` and everything listens on `127.0.0.1`; add `--bind-all` to listen on every interface.

**From source.**

```bash
git clone https://github.com/p-arndt/minisuite
cd minisuite
cargo build --release --workspace
# -> target/release/{minisuite,minicloak,minimail,minibucket}
```

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

### Releasing

There is exactly one version, `[workspace.package] version` in the root `Cargo.toml`; every crate inherits it and every image is tagged with it. Releases are cut with [stamp](https://github.com/p-arndt/stamp):

```bash
just release-dry minor   # the plan and every preflight check, nothing written
just release minor       # bump, refresh Cargo.lock, commit, tag v0.x.0, push
```

Pushing the tag triggers the release workflow, which builds the archives for all five targets and pushes `ghcr.io/p-arndt/{minicloak,minimail,minibucket,minisuite}` for `linux/amd64` and `linux/arm64`.

## License

MIT, see [LICENSE](LICENSE).
