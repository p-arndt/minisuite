# minicloak

A tiny, **dependency-free** OpenID Connect provider, written in pure Rust.

No `tokio`, no `hyper`, no `openssl`, no `ring`, no `jsonwebtoken` — just the standard
library. SHA-256, RSA (bigint, Miller-Rabin keygen, PKCS#1 v1.5 signing), base64,
JSON and HTTP/1.1 are all hand-written. The whole thing compiles into a single
binary you can drop on any machine.

```
$ cargo build --release
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

Grab a binary from the [releases page](https://github.com/p-arndt/minicloak/releases)
— Linux (x86_64 / aarch64, static musl), Windows (x86_64) and macOS (Apple silicon
/ Intel). There is nothing to install alongside it.

Or build from source:

```bash
git clone https://github.com/p-arndt/minicloak
cd minicloak
cargo build --release
```

The resulting `target/release/minicloak` is self-contained.

### Docker

```bash
docker pull ghcr.io/p-arndt/minicloak:latest
docker run --rm -p 9500:9500 -v minicloak-data:/data ghcr.io/p-arndt/minicloak:latest
```

Or build the image yourself:

```bash
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
  --users FILE             load users: username=password:email:name:role1,role2
  --clients FILE           load clients: client_id=secret:redirect_uri[,uri...]
  --user SPEC              add one user inline (repeatable)
  --client SPEC            add one client inline (repeatable)
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

A client secret of `public` (or an empty one) marks a public client, which must use PKCE.
A redirect URI may end in `/*` to allow any path below it, or be exactly `*` to allow any URI.
```

### Examples

Persist the signing key so tokens survive a restart, and widen the access-token
lifetime:

```bash
minicloak --key ./key.pem --access-ttl 3600
```

Load users and clients from files, skip the login form entirely (handy in CI):

```bash
minicloak --users users.txt --clients clients.txt --auto-login alice
```

Add a user and a client inline, without a file:

```bash
minicloak \
  --user "carol=carolpass:carol@example.com:Carol Quinn:admin,staff" \
  --client "cli=topsecret:http://localhost:8080/*"
```

## Configuration files

Both files are plain text: one record per line, `#` starts a comment, blank
lines are ignored. The same grammar is accepted inline via `--user` / `--client`.

### Users (`--users`)

```
username = password : email : name : role1,role2
```

Everything after `password` is optional. Split on the first `:` for the
password, then email, then display name, then a comma-separated role list. The
display name is split on its first space into `given_name` / `family_name`.

Fields are trimmed, so you may align the columns. A `#` only starts a comment at
the beginning of a line — it is a legal password character anywhere else.

```
# users.txt
alice = alice     : alice@example.com : Alice Admin : admin,staff
bob   = bob       : bob@example.com   : Bob Dev     : staff
carol = carolpass
dave  = davepass  : dave@example.com
```

`carol` has no email, name or roles; `dave` has an email only.

### Clients (`--clients`)

```
client_id = secret : redirect_uri[,redirect_uri...]
```

The value is split on its **first** `:` only, so redirect URIs keep their own
`://` and ports. A secret of `public` or an empty secret marks a **public**
client (no secret, PKCE required).

A redirect URI ending in `/*` matches any path below it. The wildcard must sit on
a path boundary: `http://localhost:5173*` is rejected, because its prefix ends
mid-authority and would also match `http://localhost:51739.evil.com`. A URI of
exactly `*` allows anything, which is occasionally handy and never safe.

```
# clients.txt
myapp = s3cret : http://localhost:3000/*
spa   = public : http://localhost:5173/*,http://localhost:5174/callback
api   = topsecret : http://localhost:8080/login/oauth2/code/minicloak
```

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

- **Passwords are plaintext** in the users file (and on the command line).
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
- An admin console or admin REST API (users and clients come from files/flags)
- Consent screens and scope-grant management
- Back-channel or front-channel logout propagation
- Token exchange, device flow, CIBA
- HTTPS

If you need any of those, you want real Keycloak (or Dex / Ory Hydra). minicloak
is for the case where you just need a real issuer on `localhost` to develop
against.

## Testing

```bash
cargo test              # 124 unit tests
python smoketest.py     # 63 end-to-end checks against a running server
```

`smoketest.py` drives the real HTTP surface and verifies every RS256 signature
independently in Python (plain integer arithmetic against the published JWKS),
so a bug in minicloak's own crypto cannot make the suite pass. Start a server
first, or use `just smoke`, which builds, launches one on port 19500, runs the
suite and shuts it down.

`just ci` runs the whole gate locally — the same one `.github/workflows/ci.yml`
runs on Linux, Windows and macOS. CI additionally checks the hand-written RSA
against OpenSSL: `openssl rsa -check` must accept the generated key, and
`openssl dgst -verify` must accept a token minted by minicloak.

## Releasing

The version lives in exactly one place, the `version` key of `[package]` in
`Cargo.toml`; the binary reads it back through `env!("CARGO_PKG_VERSION")` for
`--version` and its `Server:` header.

```bash
just version                 # print the current version
just set-version 0.2.0       # stamp it, without committing
just release                 # patch bump -> commit, tag v0.1.1, push
just release minor           # or: major, or an explicit 1.0.0
```

`just release` refuses to run on a dirty tree, so the release commit contains
only the version bump. Pushing the tag triggers the "Build and Publish Release"
workflow, which builds the binaries for every target, attaches them to a GitHub
release along with notes generated from the commit log, and pushes the
multi-arch container image to `ghcr.io`.

## License

MIT
