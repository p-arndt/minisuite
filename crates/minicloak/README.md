# minicloak

A tiny, **dependency-free** OpenID Connect provider, written in pure Rust.

No `tokio`, no `hyper`, no `openssl`, no `ring`, no `jsonwebtoken` — just the standard
library. SHA-256, RSA (bigint, Miller-Rabin keygen, PKCS#1 v1.5 signing), base64,
JSON and HTTP/1.1 are all hand-written. The whole thing compiles into a single
binary you can drop on any machine.

```
$ cargo build --release -p minicloak
$ ./target/release/minicloak
minicloak listening on http://127.0.0.1:9500
  signing key: ephemeral 2048 bit, generated in 172ms (use --key to persist)
  issuer:      http://127.0.0.1:9500/realms/dev
  discovery:   http://127.0.0.1:9500/realms/dev/.well-known/openid-configuration
  kid:         q1r...
  users:       alice, bob
  client:      myapp (confidential) -> http://localhost:3000/*
  client:      spa (public, PKCE required) -> http://localhost:5173/*
```

Then point any OIDC client at the discovery document:

```bash
curl http://127.0.0.1:9500/realms/dev/.well-known/openid-configuration
```

## Why?

Most local identity providers (Keycloak, Dex, Ory Hydra) are great but they're
either JVM-heavy, need a database, or pull in a container stack just so you can
test a login flow. minicloak exists to answer one question: *how small can an
honestly-useful OIDC provider be if you write everything yourself?*

It's useful for:

- **Local development** against a real OIDC provider issuing real RS256 JWTs,
  without running Keycloak in Docker.
- **Integration tests** that need an issuer, a JWKS endpoint and a token
  endpoint you can start in-process in a few hundred milliseconds.
- **Learning** — every wire-format detail (the authorization-code dance, PKCE,
  discovery, JWKS, the JWT itself) lives in plain Rust you can read in an
  afternoon.

Tokens are ordinary RS256 JWTs; the public key is published at the JWKS
endpoint, so anything that can validate a standard signature (Spring Security,
`keycloak-js`, `oidc-client-ts`, `jose`, PyJWT, ...) works unchanged.

## Install

Grab a binary from the [releases page](https://github.com/p-arndt/minisuite/releases)
— Linux (x86_64 / aarch64, static musl), Windows (x86_64) and macOS (Apple silicon
/ Intel). There is nothing to install alongside it.

minicloak lives in the [minisuite](https://github.com/p-arndt/minisuite) workspace,
so build it from the repository root:

```bash
git clone https://github.com/p-arndt/minisuite
cd minisuite
cargo build --release -p minicloak
```

The resulting `target/release/minicloak` is self-contained.

### Docker

```bash
docker pull ghcr.io/p-arndt/minicloak:latest
docker run --rm -p 9500:9500 -v minicloak-data:/data ghcr.io/p-arndt/minicloak:latest
```

Or build the image yourself. The Dockerfile now lives at the workspace root, so
build from there (see the minisuite README for the exact invocation):

```bash
cd /path/to/minisuite
docker build -t minicloak .
docker run --rm -p 9500:9500 -v minicloak-data:/data minicloak
```

The image is built from `scratch` and contains nothing but the static binary.
It listens on `0.0.0.0:9500` and persists its signing key in the `/data` volume
so tokens survive a restart. The default `CMD` is
`--bind 0.0.0.0:9500 --key /data/key.pem`; override it to pass any of the
options below.

## Quick start

Run it with no arguments. You get realm `dev`, two users (`alice`/`alice` and
`bob`/`bob`) and two clients (`myapp`, confidential, secret `s3cret`; and `spa`,
public, PKCE required):

```bash
minicloak
```

Fetch the discovery document — everything a client needs is in here:

```bash
curl http://127.0.0.1:9500/realms/dev/.well-known/openid-configuration
```

Now run a full authorization-code exchange. Normally a browser drives the login
form; with `curl` you post the same fields the form would. First get a code
(this is what the browser's form submit does after `alice` signs in):

```bash
curl -si -X POST http://127.0.0.1:9500/realms/dev/protocol/openid-connect/auth \
  --data-urlencode "response_type=code" \
  --data-urlencode "client_id=myapp" \
  --data-urlencode "redirect_uri=http://localhost:3000/callback" \
  --data-urlencode "scope=openid profile email" \
  --data-urlencode "state=xyz" \
  --data-urlencode "username=alice" \
  --data-urlencode "password=alice"
# HTTP/1.1 302 Found
# Location: http://localhost:3000/callback?code=<CODE>&state=xyz
```

Then exchange the code for tokens, authenticating the client with HTTP Basic:

```bash
curl -s -X POST http://127.0.0.1:9500/realms/dev/protocol/openid-connect/token \
  -u myapp:s3cret \
  --data-urlencode "grant_type=authorization_code" \
  --data-urlencode "code=<CODE>" \
  --data-urlencode "redirect_uri=http://localhost:3000/callback"
# { "access_token": "...", "id_token": "...", "refresh_token": "...",
#   "token_type": "Bearer", "expires_in": 300, "scope": "openid profile email" }
```

The `spa` client is public and must use PKCE instead of a secret; see
`smoketest.py` for that flow end to end.

## Usage

```
minicloak — minimal OIDC provider for development

Usage: minicloak [options]
  --bind ADDR              default 127.0.0.1:9500
  --realm NAME             default dev
  --issuer URL             override the issuer (default: http://<Host header>/realms/<realm>)
  --config FILE            load users and clients from a TOML file
  --user SPEC              add one user inline: username=password:email:name:role1,role2
  --client SPEC            add one client inline: client_id=secret:redirect_uri[,uri...]
  --key FILE               RSA private key PEM; generated and written if absent
  --key-bits N             key size when generating, default 2048
  --access-ttl SECS        access token lifetime, default 300
  --refresh-ttl SECS       refresh token lifetime, default 1800
  --code-ttl SECS          authorization code lifetime, default 60
  --session-ttl SECS       browser SSO session lifetime, default 36000
  --auto-login USER        skip the login form, always sign in as USER
  --no-quick-login         disable the password-less user buttons on the login page
  --no-cors                do not send CORS headers
  -h, --help               show this help
  -V, --version            print the version and exit

Every flag has an environment variable: MINICLOAK_ + the flag name uppercased
with dashes turned into underscores (--key-bits -> MINICLOAK_KEY_BITS). Booleans
take 1|true|yes|on or 0|false|no|off; MINICLOAK_USER and MINICLOAK_CLIENT hold
several specs separated by ';'. A flag on the command line beats the env var.

In the config file a client sets exactly one of `secret = "..."` or `public = true`.
An inline --client spec marks a public client with the secret `public`, or an empty one.
A public client must use PKCE.
A redirect URI may end in `/*` to allow any path below it, or be exactly `*` to allow any URI.
```

### Environment variables

Every flag has an equivalent environment variable, which is handy in Docker,
compose files and CI where there is no command line to edit. The name is
`MINICLOAK_` plus the flag name uppercased, with dashes turned into underscores:

| Flag | Variable | Flag | Variable |
| --- | --- | --- | --- |
| `--bind` | `MINICLOAK_BIND` | `--access-ttl` | `MINICLOAK_ACCESS_TTL` |
| `--realm` | `MINICLOAK_REALM` | `--refresh-ttl` | `MINICLOAK_REFRESH_TTL` |
| `--issuer` | `MINICLOAK_ISSUER` | `--code-ttl` | `MINICLOAK_CODE_TTL` |
| `--config` | `MINICLOAK_CONFIG` | `--session-ttl` | `MINICLOAK_SESSION_TTL` |
| `--user` | `MINICLOAK_USER` | `--auto-login` | `MINICLOAK_AUTO_LOGIN` |
| `--client` | `MINICLOAK_CLIENT` | `--no-quick-login` | `MINICLOAK_NO_QUICK_LOGIN` |
| `--key` | `MINICLOAK_KEY` | `--no-cors` | `MINICLOAK_NO_CORS` |
| `--key-bits` | `MINICLOAK_KEY_BITS` | | |

Rules:

- **Precedence** is flag > environment variable > default, so a flag on the
  command line always wins.
- **Booleans** (`MINICLOAK_NO_QUICK_LOGIN`, `MINICLOAK_NO_CORS`) take
  `1`, `true`, `yes` or `on` to turn the flag on, and `0`, `false`, `no` or
  `off` to leave it off. The comparison is case-insensitive.
- **Repeatable flags** (`MINICLOAK_USER`, `MINICLOAK_CLIENT`) hold several specs
  in one variable, separated by a **semicolon** `;`. The spec syntax itself uses
  `=`, `:` and `,`, so a semicolon can never appear inside a well-formed spec.
- An **unset or empty** variable is ignored, so `MINICLOAK_ISSUER=` in a compose
  file means "no override" rather than an empty issuer.

```bash
export MINICLOAK_BIND=0.0.0.0:9500
export MINICLOAK_KEY=/data/key.pem
export MINICLOAK_AUTO_LOGIN=carol
export MINICLOAK_USER="carol=carolpass:carol@example.com:Carol Quinn:admin;dave=davepass"
export MINICLOAK_NO_CORS=true
minicloak
```

### Examples

Persist the signing key so tokens survive a restart, and widen the access-token
lifetime:

```bash
minicloak --key ./key.pem --access-ttl 3600
```

Load users and clients from a config file, skip the login form entirely (handy in CI):

```bash
minicloak --config minicloak.toml --auto-login alice
```

Add a user and a client inline, without a file:

```bash
minicloak \
  --user "carol=carolpass:carol@example.com:Carol Quinn:admin,staff" \
  --client "cli=topsecret:http://localhost:8080/*"
```

### Use as a library

The crate ships a library next to the binary, so another program can run the
same server in a thread. Startup is split in two: `parse_args` turns arguments
(and the `MINICLOAK_*` variables) into a `Config`, and `prepare` does everything
that can fail — bind the socket, read the config file, load or generate the
signing key. Nothing in the library ever calls `std::process::exit`.

```rust
// Config::default() == the documented flag defaults; tweak the fields you care
// about, or take them from the command line with parse_args.
let mut cfg = minicloak::Config::default();
cfg.bind = "127.0.0.1:9500".to_string();
cfg.key_path = Some("./key.pem".into());

let ready = minicloak::prepare(cfg)?;   // io::Result<Prepared>
eprint!("{}", ready.banner());          // the usual startup summary
std::thread::spawn(move || ready.serve()); // Prepared is Send; serve() blocks
```

`parse_args` takes an iterator of arguments **without** argv[0] and returns
`Result<Config, CliError>`. A `CliError` with `code == 0` is `--help` or
`--version`, where `message` is the text the user asked for; `code == 2` is a
bad flag or a bad value.

```rust
match minicloak::parse_args(std::env::args().skip(1)) {
    Ok(cfg) => { /* ... */ }
    Err(e) if e.code == 0 => println!("{}", e.message),
    Err(e) => { eprintln!("{}", e.message); std::process::exit(e.code) }
}
```

## Configuration file

`--config FILE` loads every user and client from one TOML file. The field names
deliberately mirror the **Keycloak admin console**, since that is what most users
are migrating from. See [`minicloak.example.toml`](minicloak.example.toml).

```toml
# minicloak.toml

[users.alice]
password   = "alice"
email      = "alice@example.com"
first_name = "Alice"
last_name  = "Admin"
roles      = ["admin", "staff"]

# password only — email, name and roles are all optional
[users.carol]
password = "carolpass"

# a username containing a dot or an '@' needs a quoted key
[users."ada@example.com"]
password = "adapass"

[clients.myapp]
secret        = "s3cret"
redirect_uris = ["http://localhost:3000/*"]

[clients.spa]
public        = true
redirect_uris = ["http://localhost:5173/*", "http://localhost:5174/callback"]
```

### Users

`password` is required. `email`, `first_name`, `last_name` and `roles` are all
optional. `first_name` / `last_name` map to the `given_name` / `family_name`
claims, and the `name` claim is derived as `"First Last"`. A username containing
a dot or an `@` needs a quoted key: `[users."ada@example.com"]`.

### Clients

A client sets **exactly one** of `secret = "..."` or `public = true`.
`public = true` is Keycloak's "Client authentication: Off": the client has no
secret and must use PKCE. An empty secret is an error (it used to silently mean
"public" — that footgun is why the format changed).

A redirect URI ending in `/*` matches any path below it. The wildcard must sit on
a path boundary: `http://localhost:5173*` is rejected, because its prefix ends
mid-authority and would also match `http://localhost:51739.evil.com`. A URI of
exactly `*` allows anything, which is occasionally handy and never safe.

### The TOML subset

This is a real, small subset of TOML: `#` comments, quoted strings, `true` /
`false`, and string arrays. Inline tables, `[[array-of-tables]]`, floats and
multi-line strings are rejected with a line number.

### Inline flags (`--user` / `--client`)

For a terse one-liner — handy in a Docker or CI invocation — add a single user or
client with `--user` / `--client` (both repeatable), which keep a compact
colon-separated syntax:

```
--user   username=password:email:name:role1,role2
--client client_id=secret:redirect_uri[,redirect_uri...]
```

Everything after `password` (or after `secret`) is optional. `--user`'s third
column is a display name, split on its first space into `given_name` /
`family_name`. For example:

```bash
minicloak \
  --user "carol=carolpass:carol@example.com:Carol Quinn:admin,staff" \
  --client "cli=topsecret:http://localhost:8080/*"
```

`--config` is read first and the inline specs are applied on top, so a `--user`
with the same username overrides the one from the file — that is the point of
the terse spec. Defining no users or clients at all falls back to the built-in
`alice`/`bob` and `myapp`/`spa`.

## Endpoints

Every endpoint is served under Keycloak's path layout
(`/realms/{realm}/protocol/openid-connect/...`) and under a short alias, so both
a hardcoded Keycloak client and a plain discovery-driven one find their way. The
realm defaults to `dev`.

| Purpose                | Keycloak path                                             | Alias                          |
|------------------------|-----------------------------------------------------------|--------------------------------|
| Discovery              | `/realms/{realm}/.well-known/openid-configuration`        | `/.well-known/openid-configuration` |
| JWKS (public keys)     | `/realms/{realm}/protocol/openid-connect/certs`           | `/jwks.json`, `/.well-known/jwks.json` |
| Authorization          | `/realms/{realm}/protocol/openid-connect/auth`            | `/authorize`                   |
| Token                  | `/realms/{realm}/protocol/openid-connect/token`           | `/token`                       |
| UserInfo               | `/realms/{realm}/protocol/openid-connect/userinfo`        | `/userinfo`                    |
| Introspection          | `/realms/{realm}/protocol/openid-connect/token/introspect`| `/introspect`                  |
| Revocation             | `/realms/{realm}/protocol/openid-connect/revoke`          | `/revoke`                      |
| End session (logout)   | `/realms/{realm}/protocol/openid-connect/logout`          | `/logout`                      |

Supported grant types: `authorization_code` (with PKCE, `S256` and `plain`),
`refresh_token` (with rotation), `client_credentials` and `password`. Supported
scopes: `openid`, `profile`, `email`, `roles`, `offline_access`. The discovery
document advertises `jwks_uri` as the `/certs` form.

## Using it from a client

Point a **generic OIDC library** at the issuer
`http://127.0.0.1:9500/realms/dev` (or the discovery URL directly); it will read
the endpoints and the JWKS out of the discovery document. minicloak issues RS256
tokens with a stable `kid`, so standard signature validation just works.

For a **Spring** resource server, set:

```yaml
spring:
  security:
    oauth2:
      resourceserver:
        jwt:
          issuer-uri: http://127.0.0.1:9500/realms/dev
```

Roles land in both a top-level `roles` claim and a Keycloak-style
`realm_access.roles`, so `JwtGrantedAuthoritiesConverter` presets find them.

For **`keycloak-js`**, use `url: "http://127.0.0.1:9500"`, `realm: "dev"`,
`clientId: "spa"`. The `spa` client is public and PKCE-enforced, which is what
`keycloak-js` does by default.

A quick **`client_credentials`** call for a service-to-service token (no user,
no refresh token, subject `service-account-myapp`):

```bash
curl -s -X POST http://127.0.0.1:9500/realms/dev/protocol/openid-connect/token \
  -u myapp:s3cret \
  --data-urlencode "grant_type=client_credentials"
```

## Signing keys

Tokens are signed with **RS256**. The matching public key is published at the
JWKS endpoint (`/certs`, alias `/jwks.json`) as a standard JWK. The `kid` is an
RFC 7638 thumbprint of the public key, so it is stable for a given key across
restarts and lets a client cache the key.

Without `--key` the RSA key is **ephemeral** — generated fresh on each start, so
tokens do not survive a restart. Pass `--key FILE` to generate the key once and
reuse it: minicloak writes the PEM on first run and reads it back afterwards.
2048-bit generation takes roughly 170ms; signing a token is a few milliseconds.

The written PEM is a standard PKCS#1 `RSA PRIVATE KEY`. You can inspect it with
OpenSSL:

```bash
openssl rsa -in key.pem -check -noout
```

and OpenSSL accepts minicloak's signatures via `openssl dgst -sha256 -verify`.

## Security / not for production

**Do not run this on anything reachable from the internet.** It is a development
tool and it makes development-convenient trade-offs that are wrong for
production:

- **Passwords are plaintext** in the config file (and on the command line).
- **Quick-login** buttons on the login page sign a user in with **no password**
  at all — dev convenience only. Disable with `--no-quick-login`; the same
  applies to `--auto-login`, which skips authentication entirely.
- **No HTTPS.** Terminate TLS with a reverse proxy if you must expose it.
- **No rate limiting, no account lockout, no brute-force protection.**
- **No client registration and no consent screen** — everything is preconfigured.

Bind it to `localhost` (the default) and keep it there.

## What it deliberately does not do

minicloak leaves out, on purpose:

- User federation, LDAP / Active Directory, social login
- An admin console or admin REST API (users and clients come from a config file or flags)
- Consent screens and scope-grant management
- Back-channel or front-channel logout propagation
- Token exchange, device flow, CIBA
- HTTPS

If you need any of those, you want real Keycloak (or Dex / Ory Hydra). minicloak
is for the case where you just need a real issuer on `localhost` to develop
against.

## Testing

From the workspace root:

```bash
cargo test -p minicloak                  # 171 unit tests
python crates/minicloak/smoketest.py     # 63 end-to-end checks against a running server
```

`smoketest.py` drives the real HTTP surface and verifies every RS256 signature
independently in Python (plain integer arithmetic against the published JWKS),
so a bug in minicloak's own crypto cannot make the suite pass. Start a server
first, or use the workspace's `just smoke`, which builds, launches one on port
19500, runs the suite and shuts it down.

The workspace `just ci` runs the whole gate locally — the same one CI runs on
Linux, Windows and macOS. CI additionally checks the hand-written RSA against
OpenSSL: `openssl rsa -check` must accept the generated key, and
`openssl dgst -verify` must accept a token minted by minicloak.

## Releasing

The version comes from `[workspace.package]` in the root `Cargo.toml` — one
number for the whole suite. The binary reads it back through
`env!("CARGO_PKG_VERSION")` for `--version` and its `Server:` header. See the
minisuite README for the release workflow.

## License

MIT — see the `LICENSE` file at the root of the minisuite repository.
