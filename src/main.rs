// minicloak: a tiny, dependency-free OIDC provider for local development.

mod base64;
mod bigint;
mod clients;
mod http;
mod json;
mod jwt;
mod oidc;
mod rand;
mod rsa;
mod sha256;
mod store;
mod url;
mod users;
mod util;

use std::env;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::clients::{Client, Clients};
use crate::oidc::Server;
use crate::rsa::RsaKey;
use crate::store::Store;
use crate::users::{User, Users};

struct Config {
    bind: String,
    realm: String,
    issuer: Option<String>,
    users: Users,
    clients: Clients,
    key_path: Option<PathBuf>,
    key_bits: usize,
    access_ttl: u64,
    refresh_ttl: u64,
    code_ttl: u64,
    session_ttl: u64,
    auto_login: Option<String>,
    quick_login: bool,
    cors: bool,
}

const HELP: &str = "\
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

A client secret of `public` (or an empty one) marks a public client, which must use PKCE.
A redirect URI may end in `/*` to allow any path below it, or be exactly `*` to allow any URI.";

fn fail(msg: &str) -> ! {
    eprintln!("minicloak: {}", msg);
    std::process::exit(2);
}

fn parse_args() -> Config {
    let mut cfg = Config {
        bind: "127.0.0.1:9500".to_string(),
        realm: "dev".to_string(),
        issuer: None,
        users: Users::new(),
        clients: Clients::new(),
        key_path: None,
        key_bits: 2048,
        access_ttl: 300,
        refresh_ttl: 1800,
        code_ttl: 60,
        session_ttl: 36000,
        auto_login: None,
        quick_login: true,
        cors: true,
    };

    let mut args = env::args().skip(1);
    let next = |flag: &str, args: &mut dyn Iterator<Item = String>| -> String {
        args.next()
            .unwrap_or_else(|| fail(&format!("{} needs a value", flag)))
    };
    let num = |flag: &str, v: String| -> u64 {
        v.parse()
            .unwrap_or_else(|_| fail(&format!("{} needs a number, got {:?}", flag, v)))
    };

    while let Some(a) = args.next() {
        match a.as_str() {
            "--bind" => cfg.bind = next("--bind", &mut args),
            "--realm" => cfg.realm = next("--realm", &mut args),
            "--issuer" => {
                cfg.issuer = Some(
                    next("--issuer", &mut args)
                        .trim_end_matches('/')
                        .to_string(),
                )
            }
            "--users" => {
                let p = next("--users", &mut args);
                let u = Users::load_file(Path::new(&p))
                    .unwrap_or_else(|e| fail(&format!("failed to load users from {}: {}", p, e)));
                for user in u.list() {
                    cfg.users.add(user.clone());
                }
            }
            "--clients" => {
                let p = next("--clients", &mut args);
                let c = Clients::load_file(Path::new(&p))
                    .unwrap_or_else(|e| fail(&format!("failed to load clients from {}: {}", p, e)));
                for client in c.list() {
                    cfg.clients.add(client.clone());
                }
            }
            "--user" => {
                let spec = next("--user", &mut args);
                let parsed = Users::parse(&spec)
                    .unwrap_or_else(|e| fail(&format!("bad --user {:?}: {}", spec, e)));
                for user in parsed.list() {
                    cfg.users.add(user.clone());
                }
            }
            "--client" => {
                let spec = next("--client", &mut args);
                let parsed = Clients::parse(&spec)
                    .unwrap_or_else(|e| fail(&format!("bad --client {:?}: {}", spec, e)));
                for client in parsed.list() {
                    cfg.clients.add(client.clone());
                }
            }
            "--key" => cfg.key_path = Some(PathBuf::from(next("--key", &mut args))),
            "--key-bits" => {
                cfg.key_bits = num("--key-bits", next("--key-bits", &mut args)) as usize
            }
            "--access-ttl" => cfg.access_ttl = num("--access-ttl", next("--access-ttl", &mut args)),
            "--refresh-ttl" => {
                cfg.refresh_ttl = num("--refresh-ttl", next("--refresh-ttl", &mut args))
            }
            "--code-ttl" => cfg.code_ttl = num("--code-ttl", next("--code-ttl", &mut args)),
            "--session-ttl" => {
                cfg.session_ttl = num("--session-ttl", next("--session-ttl", &mut args))
            }
            "--auto-login" => cfg.auto_login = Some(next("--auto-login", &mut args)),
            "--no-quick-login" => cfg.quick_login = false,
            "--no-cors" => cfg.cors = false,
            "-h" | "--help" => {
                println!("{}", HELP);
                std::process::exit(0);
            }
            _ => fail(&format!("unknown argument: {}", a)),
        }
    }

    if cfg.key_bits < 512 || !cfg.key_bits.is_multiple_of(2) {
        fail("--key-bits must be even and at least 512");
    }
    if cfg.users.is_empty() {
        cfg.users.add(User {
            username: "alice".into(),
            password: "alice".into(),
            email: "alice@example.com".into(),
            name: "Alice Admin".into(),
            roles: vec!["admin".into(), "staff".into()],
        });
        cfg.users.add(User {
            username: "bob".into(),
            password: "bob".into(),
            email: "bob@example.com".into(),
            name: "Bob Dev".into(),
            roles: vec!["staff".into()],
        });
    }
    if cfg.clients.is_empty() {
        cfg.clients.add(Client {
            id: "myapp".into(),
            secret: Some("s3cret".into()),
            redirect_uris: vec!["http://localhost:3000/*".into()],
        });
        cfg.clients.add(Client {
            id: "spa".into(),
            secret: None,
            redirect_uris: vec!["http://localhost:5173/*".into()],
        });
    }
    if let Some(name) = &cfg.auto_login {
        if cfg.users.get(name).is_none() {
            fail(&format!("--auto-login {}: no such user", name));
        }
    }
    cfg
}

/// Load the signing key from `path`, generating and persisting it on first run.
/// Without `--key` the key is ephemeral, so tokens do not survive a restart.
fn load_or_generate_key(path: Option<&PathBuf>, bits: usize) -> RsaKey {
    if let Some(p) = path {
        if p.exists() {
            let pem = std::fs::read_to_string(p)
                .unwrap_or_else(|e| fail(&format!("cannot read {}: {}", p.display(), e)));
            let key = RsaKey::from_pkcs1_pem(&pem)
                .unwrap_or_else(|e| fail(&format!("cannot parse {}: {}", p.display(), e)));
            eprintln!("  signing key: {} ({} bit)", p.display(), key.n.bit_len());
            return key;
        }
    }
    let started = std::time::Instant::now();
    let key = RsaKey::generate(bits);
    match path {
        Some(p) => {
            std::fs::write(p, key.to_pkcs1_pem())
                .unwrap_or_else(|e| fail(&format!("cannot write {}: {}", p.display(), e)));
            eprintln!(
                "  signing key: generated {} bit in {:?} -> {}",
                bits,
                started.elapsed(),
                p.display()
            );
        }
        None => eprintln!(
            "  signing key: ephemeral {} bit, generated in {:?} (use --key to persist)",
            bits,
            started.elapsed()
        ),
    }
    key
}

fn main() {
    let cfg = parse_args();

    eprintln!("minicloak listening on http://{}", cfg.bind);
    let key = load_or_generate_key(cfg.key_path.as_ref(), cfg.key_bits);
    let kid = jwt::kid(&key);

    // With no --issuer the real issuer is derived per request from the Host header,
    // so printing the bind address here would be a lie for 0.0.0.0 (as in Docker).
    let (issuer_display, issuer_note) = match &cfg.issuer {
        Some(iss) => (iss.clone(), ""),
        None => {
            let host = if cfg.bind.starts_with("0.0.0.0") || cfg.bind.starts_with("[::]") {
                format!("127.0.0.1{}", &cfg.bind[cfg.bind.rfind(':').unwrap_or(0)..])
            } else {
                cfg.bind.clone()
            };
            (
                format!("http://{}/realms/{}", host, cfg.realm),
                "  (derived from each request's Host header; --issuer to pin it)",
            )
        }
    };
    eprintln!("  issuer:      {}{}", issuer_display, issuer_note);
    eprintln!(
        "  discovery:   {}/.well-known/openid-configuration",
        issuer_display
    );
    eprintln!("  kid:         {}", kid);
    eprintln!(
        "  users:       {}",
        cfg.users
            .list()
            .iter()
            .map(|u| u.username.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    for c in cfg.clients.list() {
        eprintln!(
            "  client:      {} ({}) -> {}",
            c.id,
            if c.is_public() {
                "public, PKCE required"
            } else {
                "confidential"
            },
            c.redirect_uris.join(", ")
        );
    }
    if let Some(u) = &cfg.auto_login {
        eprintln!("  auto-login:  always signing in as {} (no login form)", u);
    } else if cfg.quick_login {
        eprintln!(
            "  quick-login: login page offers password-less sign-in (--no-quick-login to disable)"
        );
    }
    eprintln!("  This is a development tool. Do not run it in production.");

    let server = Arc::new(Server {
        realm: cfg.realm,
        issuer_override: cfg.issuer,
        key,
        kid,
        users: cfg.users,
        clients: cfg.clients,
        store: Mutex::new(Store::new()),
        access_ttl: cfg.access_ttl,
        refresh_ttl: cfg.refresh_ttl,
        code_ttl: cfg.code_ttl,
        session_ttl: cfg.session_ttl,
        auto_login: cfg.auto_login,
        quick_login: cfg.quick_login,
        cors: cfg.cors,
    });

    let listener =
        TcpListener::bind(&cfg.bind).unwrap_or_else(|e| fail(&format!("bind {}: {}", cfg.bind, e)));
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let srv = Arc::clone(&server);
                thread::spawn(move || {
                    if let Err(e) = handle(srv, s) {
                        eprintln!("[conn] {}", e);
                    }
                });
            }
            Err(e) => eprintln!("[accept] {}", e),
        }
    }
}

fn handle(srv: Arc<Server>, stream: TcpStream) -> std::io::Result<()> {
    let _ = stream.set_nodelay(true);
    let mut sock = stream.try_clone()?;

    let mut req = match http::read_request(stream) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[parse] {}", e);
            return Ok(());
        }
    };

    // Expired codes and sessions are only ever dropped lazily; a dev server never
    // runs long enough for that to matter, and it keeps the hot path lock-light.
    srv.store.lock().unwrap().gc(util::now_secs());

    let resp = oidc::dispatch(&srv, &mut req);
    eprintln!("[req] {} {} -> {}", req.method, req.raw_path, resp.status);
    resp.write_to(&mut sock)?;
    sock.flush()
}
