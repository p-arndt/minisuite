# Contributing

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

## Layout

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

## Releasing

There is exactly one version, `[workspace.package] version` in the root `Cargo.toml`; every crate inherits it and every image is tagged with it. Releases are cut with [stamp](https://github.com/p-arndt/stamp):

```bash
just release-dry minor   # the plan and every preflight check, nothing written
just release minor       # bump, refresh Cargo.lock, commit, tag v0.x.0, push
```

Pushing the tag triggers the release workflow, which builds the archives for all five targets and pushes `ghcr.io/p-arndt/{minicloak,minimail,minibucket,minisuite}` for `linux/amd64` and `linux/arm64`.
