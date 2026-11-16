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

Then open **<http://localhost:9900>**. For each service the landing page lists the URLs and the logins that are actually configured, plus a `.env` block with a Copy button that you can paste into your app.

No Docker? Download a binary from the [releases page](https://github.com/p-arndt/minisuite/releases) and run `minisuite`. Everything listens on `127.0.0.1`, and state goes to `./data`.

## Connect your app

These are the defaults. If you change the config, the landing page shows your values instead.

**OIDC (minicloak).** Users `alice`/`alice` and `bob`/`bob`. The login page also has one-click buttons that sign you in without a password.

```dotenv
OIDC_ISSUER=http://localhost:9500/realms/dev
OIDC_CLIENT_ID=myapp            # confidential, redirect http://localhost:3000/*
OIDC_CLIENT_SECRET=s3cret
# or the public PKCE client "spa", redirect http://localhost:5173/*
```

**Mail (minimail).** Every message your app sends lands in the web UI at <http://localhost:8025>, which uses the same login.

```dotenv
SMTP_HOST=localhost
SMTP_PORT=1025
SMTP_USER=minimail
SMTP_PASSWORD=minimail
```

**S3 (minibucket).** Turn on path-style addressing in your client: `forcePathStyle: true` in the AWS SDK for JavaScript, `addressing_style = "path"` in boto3.

```dotenv
AWS_ENDPOINT_URL=http://localhost:9000
AWS_REGION=us-east-1
AWS_ACCESS_KEY_ID=minioadmin
AWS_SECRET_ACCESS_KEY=minioadmin
```

## Configuration

The defaults are enough for most setups. When you need to change something, create a config file:

```bash
minisuite init          # writes ./minisuite.toml, every line commented out
```

Uncomment what you need. The file has one section for the suite and one per service. A service key is that service's CLI flag without the dashes; `minicloak --help` lists them all.

```toml
[minisuite]
data = "state"                     # where keys, mail and objects live (default ./data)
only = ["minicloak", "minimail"]   # skip services you don't need

[minicloak]
user = ["alice=alice", "carol=secret:carol@example.com:Carol:admin"]
client = ["web=topsecret:http://localhost:8080/*"]

[minimail]
anonymous = true                   # accept mail without SMTP AUTH

[minibucket]
bind = "127.0.0.1:9100"
```

The same settings can also come from flags and environment variables. When more than one sets a value, the order is the same everywhere:

**command-line flag > environment variable > `minisuite.toml` > default**

The environment variable is the prefix plus the flag in upper case: `MINISUITE_DATA`, `MINICLOAK_USER`, `MINIMAIL_SMTP_BIND` for `--smtp-bind`, `MINIBUCKET_ACCESS_KEY`. Booleans take `1|true|yes|on` or `0|false|no|off`. Relative paths in the file resolve against the file's directory.

Typos are caught with the line number and a suggestion:

```
minisuite: minisuite.toml: line 2: [minicloak] unknown key `realn` (did you mean `realm`?)
```

Further reading:

- [`minisuite.example.toml`](minisuite.example.toml) explains every key.
- `minisuite --help` lists the suite's own flags.
- Each service's README covers everything it can do: [minicloak](crates/minicloak), [minimail](crates/minimail), [minibucket](crates/minibucket).

## Troubleshooting

**"Port 9500 is already taken".** Another minisuite or a container is probably still running; `docker ps` shows it. Stop it, or move the service to another port. The error message names the exact variable and toml key to use. `--only` runs a subset of the services.

**Tokens say `localhost`, but my container can't reach `localhost`.** By default the issuer comes from the `Host` header of each request. When your browser and your backend reach minicloak under different names, pin the issuer with `MINICLOAK_ISSUER=http://localhost:9500/realms/dev` and make that name resolve in both places.

**Starting from scratch.** All state lives in the data directory: `docker compose down -v`, or delete `./data`.

**Health checks.** `GET http://localhost:9900/healthz` returns `{"status":"ok","services":[...]}`.

## Other ways to run it

**One container per service.** [`compose.yml`](compose.yml) in this repo runs the three single-service images (`ghcr.io/p-arndt/{minicloak,minimail,minibucket}`) with every option commented. `docker compose up -d` pulls them; `docker compose -f compose.yml -f compose.build.yml up --build` builds them from this checkout.

**Native binaries.** Each [release](https://github.com/p-arndt/minisuite/releases) archive contains all four binaries plus the example configs. Builds exist for Linux x86_64/aarch64 (static musl), Windows x86_64, and macOS on Apple silicon and Intel. `minisuite --bind-all` listens on every interface instead of `127.0.0.1`.

**From source.**

```bash
git clone https://github.com/p-arndt/minisuite
cd minisuite
cargo build --release --workspace
# -> target/release/{minisuite,minicloak,minimail,minibucket}
```

The Docker images are built `FROM scratch`. They contain a static binary of a few MB, run as a non-root user and keep all state in `/data`.

## Development

Building, testing and releasing are described in [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT, see [LICENSE](LICENSE).
