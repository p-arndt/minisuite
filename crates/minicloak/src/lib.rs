// minicloak: a tiny, dependency-free OIDC provider for local development.
//
// The binary in src/main.rs is a thin shell around this library so that the
// minisuite launcher can embed the server in a thread instead of spawning a
// process. Nothing in here ever calls `std::process::exit`: every failure is a
// value the caller decides what to do with.

mod base64;
mod bigint;
mod clients;
mod config;
mod http;
mod json;
mod jwt;
mod oidc;
mod rand;
mod rsa;
mod sha256;
mod store;
mod toml;
mod url;
mod users;
mod util;

use std::io;
use std::io::Write as _;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::clients::{Client, Clients};
use crate::oidc::Server;
use crate::rsa::RsaKey;
use crate::store::Store;
use crate::users::{User, Users};

/// Program name, used for log prefixes by the launcher.
pub const NAME: &str = "minicloak";

/// Prefix for the environment variables that mirror the CLI flags.
const ENV_PREFIX: &str = "MINICLOAK_";

/// Separator for env vars that mirror a repeatable flag (`--user`, `--client`).
/// The spec syntax uses `=`, `:` and `,`, so `;` is the one punctuation left
/// that can never appear in a well-formed spec.
const ENV_LIST_SEP: char = ';';

const HELP: &str = "\
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

In the config file a client sets exactly one of `secret = \"...\"` or `public = true`.
An inline --client spec marks a public client with the secret `public`, or an empty one.
A public client must use PKCE.
A redirect URI may end in `/*` to allow any path below it, or be exactly `*` to allow any URI.";

/// Everything the CLI can set. `Default` matches the documented flag defaults,
/// so a launcher can start from `Config::default()` and tweak a field or two.
///
/// `users`/`clients` are raw specs in `--user`/`--client` syntax; they are only
/// parsed in [`prepare`], together with `config_path`.
#[derive(Clone, Debug)]
pub struct Config {
    pub bind: String,
    pub realm: String,
    pub issuer: Option<String>,
    pub config_path: Option<PathBuf>,
    pub users: Vec<String>,
    pub clients: Vec<String>,
    pub key_path: Option<PathBuf>,
    pub key_bits: usize,
    pub access_ttl: u64,
    pub refresh_ttl: u64,
    pub code_ttl: u64,
    pub session_ttl: u64,
    pub auto_login: Option<String>,
    pub quick_login: bool,
    pub cors: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:9500".to_string(),
            realm: "dev".to_string(),
            issuer: None,
            config_path: None,
            users: Vec::new(),
            clients: Vec::new(),
            key_path: None,
            key_bits: 2048,
            access_ttl: 300,
            refresh_ttl: 1800,
            code_ttl: 60,
            session_ttl: 36000,
            auto_login: None,
            quick_login: true,
            cors: true,
        }
    }
}

/// A reason to stop before serving. `code` is the process exit status the
/// binary should use: 0 for `--help`/`--version` (where `message` is the text
/// the user asked for), 2 for a bad flag or a bad value.
#[derive(Clone, Debug)]
pub struct CliError {
    pub message: String,
    pub code: i32,
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

fn usage_error(msg: impl AsRef<str>) -> CliError {
    CliError {
        message: format!("{}: {}", NAME, msg.as_ref()),
        code: 2,
    }
}

// --- flag table -------------------------------------------------------------
//
// One entry per flag, so the CLI loop and the env-var pass stay in sync by
// construction instead of by discipline. `label` is threaded into the error
// messages so the same apply function can blame either `--key-bits` or
// `MINICLOAK_KEY_BITS`.

type ValueFn = fn(&mut Config, &str, &str) -> Result<(), String>;
type SwitchFn = fn(&mut Config, bool);

enum Kind {
    /// Takes the next argument (or the whole env var) as its value.
    Value(ValueFn),
    /// Present-or-absent on the command line, a boolean in the environment.
    Switch(SwitchFn),
}

struct FlagSpec {
    /// Long form including the leading dashes.
    name: &'static str,
    kind: Kind,
    /// Whether the env var may carry several `;`-separated values.
    repeatable: bool,
}

fn num(label: &str, v: &str) -> Result<u64, String> {
    v.parse()
        .map_err(|_| format!("{} needs a number, got {:?}", label, v))
}

static FLAGS: &[FlagSpec] = &[
    FlagSpec {
        name: "--bind",
        repeatable: false,
        kind: Kind::Value(|c, _l, v| {
            c.bind = v.to_string();
            Ok(())
        }),
    },
    FlagSpec {
        name: "--realm",
        repeatable: false,
        kind: Kind::Value(|c, _l, v| {
            c.realm = v.to_string();
            Ok(())
        }),
    },
    FlagSpec {
        name: "--issuer",
        repeatable: false,
        kind: Kind::Value(|c, _l, v| {
            c.issuer = Some(v.trim_end_matches('/').to_string());
            Ok(())
        }),
    },
    FlagSpec {
        name: "--config",
        repeatable: false,
        kind: Kind::Value(|c, _l, v| {
            c.config_path = Some(PathBuf::from(v));
            Ok(())
        }),
    },
    FlagSpec {
        name: "--user",
        repeatable: true,
        kind: Kind::Value(|c, l, v| {
            // Parse eagerly so a typo is reported at flag position, not later.
            Users::parse(v).map_err(|e| format!("bad {} {:?}: {}", l, v, e))?;
            c.users.push(v.to_string());
            Ok(())
        }),
    },
    FlagSpec {
        name: "--client",
        repeatable: true,
        kind: Kind::Value(|c, l, v| {
            Clients::parse(v).map_err(|e| format!("bad {} {:?}: {}", l, v, e))?;
            c.clients.push(v.to_string());
            Ok(())
        }),
    },
    FlagSpec {
        name: "--key",
        repeatable: false,
        kind: Kind::Value(|c, _l, v| {
            c.key_path = Some(PathBuf::from(v));
            Ok(())
        }),
    },
    FlagSpec {
        name: "--key-bits",
        repeatable: false,
        kind: Kind::Value(|c, l, v| {
            c.key_bits = num(l, v)? as usize;
            Ok(())
        }),
    },
    FlagSpec {
        name: "--access-ttl",
        repeatable: false,
        kind: Kind::Value(|c, l, v| {
            c.access_ttl = num(l, v)?;
            Ok(())
        }),
    },
    FlagSpec {
        name: "--refresh-ttl",
        repeatable: false,
        kind: Kind::Value(|c, l, v| {
            c.refresh_ttl = num(l, v)?;
            Ok(())
        }),
    },
    FlagSpec {
        name: "--code-ttl",
        repeatable: false,
        kind: Kind::Value(|c, l, v| {
            c.code_ttl = num(l, v)?;
            Ok(())
        }),
    },
    FlagSpec {
        name: "--session-ttl",
        repeatable: false,
        kind: Kind::Value(|c, l, v| {
            c.session_ttl = num(l, v)?;
            Ok(())
        }),
    },
    FlagSpec {
        name: "--auto-login",
        repeatable: false,
        kind: Kind::Value(|c, _l, v| {
            c.auto_login = Some(v.to_string());
            Ok(())
        }),
    },
    FlagSpec {
        name: "--no-quick-login",
        repeatable: false,
        kind: Kind::Switch(|c, on| c.quick_login = !on),
    },
    FlagSpec {
        name: "--no-cors",
        repeatable: false,
        kind: Kind::Switch(|c, on| c.cors = !on),
    },
];

/// `--key-bits` -> `MINICLOAK_KEY_BITS`.
fn env_name(flag: &str) -> String {
    format!(
        "{}{}",
        ENV_PREFIX,
        flag.trim_start_matches('-')
            .to_uppercase()
            .replace('-', "_")
    )
}

fn parse_bool(label: &str, v: &str) -> Result<bool, String> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(format!(
            "{} expects 1|true|yes|on or 0|false|no|off, got {:?}",
            label, v
        )),
    }
}

/// Apply the `MINICLOAK_*` variables. An unset or empty variable is ignored, so
/// `MINICLOAK_ISSUER=` in a compose file means "no override" rather than "".
fn apply_env(cfg: &mut Config) -> Result<(), CliError> {
    for flag in FLAGS {
        let name = env_name(flag.name);
        let Ok(raw) = std::env::var(&name) else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        match flag.kind {
            Kind::Value(apply) => {
                if flag.repeatable {
                    for part in raw.split(ENV_LIST_SEP) {
                        let part = part.trim();
                        if part.is_empty() {
                            continue;
                        }
                        apply(cfg, &name, part).map_err(usage_error)?;
                    }
                } else {
                    apply(cfg, &name, &raw).map_err(usage_error)?;
                }
            }
            Kind::Switch(apply) => {
                let on = parse_bool(&name, &raw).map_err(usage_error)?;
                apply(cfg, on);
            }
        }
    }
    Ok(())
}

/// Build a [`Config`] from environment variables and command-line flags.
///
/// `args` must NOT contain argv[0]. Environment variables are applied first, so
/// an explicit flag always wins. `--help` and `--version` come back as an
/// [`CliError`] with `code == 0` and the text in `message`.
pub fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<Config, CliError> {
    let mut cfg = Config::default();
    apply_env(&mut cfg)?;

    let mut args = args.into_iter();
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                return Err(CliError {
                    message: HELP.to_string(),
                    code: 0,
                })
            }
            "-V" | "--version" => {
                return Err(CliError {
                    message: format!("{} {}", NAME, env!("CARGO_PKG_VERSION")),
                    code: 0,
                })
            }
            _ => {}
        }
        let Some(flag) = FLAGS.iter().find(|f| f.name == a) else {
            return Err(usage_error(format!("unknown argument: {}", a)));
        };
        match flag.kind {
            Kind::Value(apply) => {
                let v = args
                    .next()
                    .ok_or_else(|| usage_error(format!("{} needs a value", flag.name)))?;
                apply(&mut cfg, flag.name, &v).map_err(usage_error)?;
            }
            Kind::Switch(apply) => apply(&mut cfg, true),
        }
    }

    if cfg.key_bits < 512 || !cfg.key_bits.is_multiple_of(2) {
        return Err(usage_error("--key-bits must be even and at least 512"));
    }
    Ok(cfg)
}

// --- startup ----------------------------------------------------------------

/// A server that has already done everything that can fail: the socket is
/// bound, the signing key is loaded or generated, users and clients are
/// resolved. All that is left is to accept connections.
pub struct Prepared {
    listener: TcpListener,
    server: Arc<Server>,
    banner: String,
}

impl Prepared {
    /// The multi-line startup summary, with a trailing newline. The caller
    /// decides where it goes; the binary sends it to stderr.
    pub fn banner(&self) -> String {
        self.banner.clone()
    }

    /// The address the listener is actually bound to. Useful when the config
    /// asked for port 0.
    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// Run the accept loop. Only returns on a fatal listener error.
    pub fn serve(self) -> io::Result<()> {
        for stream in self.listener.incoming() {
            match stream {
                Ok(s) => {
                    let srv = Arc::clone(&self.server);
                    thread::spawn(move || {
                        if let Err(e) = handle(srv, s) {
                            eprintln!("[conn] {}", e);
                        }
                    });
                }
                Err(e) => eprintln!("[accept] {}", e),
            }
        }
        Ok(())
    }
}

fn err(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

/// Resolve the config file and the inline specs into the final registries.
/// The file is read first so that an inline `--user`/`--client` with the same
/// name overrides it, which is the whole point of the terse spec.
fn resolve(cfg: &Config) -> io::Result<(Users, Clients)> {
    let mut users = Users::new();
    let mut clients = Clients::new();

    if let Some(p) = &cfg.config_path {
        let (fu, fc) = config::load_file(Path::new(p))
            .map_err(|e| err(format!("failed to load {}: {}", p.display(), e)))?;
        for user in fu.list() {
            users.add(user.clone());
        }
        for client in fc.list() {
            clients.add(client.clone());
        }
    }
    for spec in &cfg.users {
        let parsed = Users::parse(spec).map_err(|e| err(format!("bad user {:?}: {}", spec, e)))?;
        for user in parsed.list() {
            users.add(user.clone());
        }
    }
    for spec in &cfg.clients {
        let parsed =
            Clients::parse(spec).map_err(|e| err(format!("bad client {:?}: {}", spec, e)))?;
        for client in parsed.list() {
            clients.add(client.clone());
        }
    }

    if users.is_empty() {
        users.add(User {
            username: "alice".into(),
            password: "alice".into(),
            email: "alice@example.com".into(),
            first_name: "Alice".into(),
            last_name: "Admin".into(),
            roles: vec!["admin".into(), "staff".into()],
        });
        users.add(User {
            username: "bob".into(),
            password: "bob".into(),
            email: "bob@example.com".into(),
            first_name: "Bob".into(),
            last_name: "Dev".into(),
            roles: vec!["staff".into()],
        });
    }
    if clients.is_empty() {
        clients.add(Client {
            id: "myapp".into(),
            secret: Some("s3cret".into()),
            redirect_uris: vec!["http://localhost:3000/*".into()],
        });
        clients.add(Client {
            id: "spa".into(),
            secret: None,
            redirect_uris: vec!["http://localhost:5173/*".into()],
        });
    }
    Ok((users, clients))
}

/// Load the signing key from `path`, generating and persisting it on first run.
/// Without a key path the key is ephemeral, so tokens do not survive a restart.
/// Returns the key plus the banner line describing where it came from.
fn load_or_generate_key(path: Option<&PathBuf>, bits: usize) -> io::Result<(RsaKey, String)> {
    if let Some(p) = path {
        if p.exists() {
            let pem = std::fs::read_to_string(p)
                .map_err(|e| err(format!("cannot read {}: {}", p.display(), e)))?;
            let key = RsaKey::from_pkcs1_pem(&pem)
                .map_err(|e| err(format!("cannot parse {}: {}", p.display(), e)))?;
            let line = format!("  signing key: {} ({} bit)\n", p.display(), key.n.bit_len());
            return Ok((key, line));
        }
    }
    let started = std::time::Instant::now();
    let key = RsaKey::generate(bits);
    let line = match path {
        Some(p) => {
            std::fs::write(p, key.to_pkcs1_pem())
                .map_err(|e| err(format!("cannot write {}: {}", p.display(), e)))?;
            format!(
                "  signing key: generated {} bit in {:?} -> {}\n",
                bits,
                started.elapsed(),
                p.display()
            )
        }
        None => format!(
            "  signing key: ephemeral {} bit, generated in {:?} (use --key to persist)\n",
            bits,
            started.elapsed()
        ),
    };
    Ok((key, line))
}

/// Do all the fallible startup work: bind the socket, read the config file,
/// load or generate the RSA key, validate `--auto-login`.
pub fn prepare(cfg: Config) -> io::Result<Prepared> {
    let (users, clients) = resolve(&cfg)?;

    if let Some(name) = &cfg.auto_login {
        if users.get(name).is_none() {
            return Err(err(format!("--auto-login {}: no such user", name)));
        }
    }

    let listener =
        TcpListener::bind(&cfg.bind).map_err(|e| err(format!("bind {}: {}", cfg.bind, e)))?;

    let mut banner = format!("{} listening on http://{}\n", NAME, cfg.bind);
    let (key, key_line) = load_or_generate_key(cfg.key_path.as_ref(), cfg.key_bits)?;
    banner.push_str(&key_line);
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
    banner.push_str(&format!(
        "  issuer:      {}{}\n",
        issuer_display, issuer_note
    ));
    banner.push_str(&format!(
        "  discovery:   {}/.well-known/openid-configuration\n",
        issuer_display
    ));
    banner.push_str(&format!("  kid:         {}\n", kid));
    banner.push_str(&format!(
        "  users:       {}\n",
        users
            .list()
            .iter()
            .map(|u| u.username.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    for c in clients.list() {
        banner.push_str(&format!(
            "  client:      {} ({}) -> {}\n",
            c.id,
            if c.is_public() {
                "public, PKCE required"
            } else {
                "confidential"
            },
            c.redirect_uris.join(", ")
        ));
    }
    if let Some(u) = &cfg.auto_login {
        banner.push_str(&format!(
            "  auto-login:  always signing in as {} (no login form)\n",
            u
        ));
    } else if cfg.quick_login {
        banner.push_str(
            "  quick-login: login page offers password-less sign-in (--no-quick-login to disable)\n",
        );
    }
    banner.push_str("  This is a development tool. Do not run it in production.\n");

    let server = Arc::new(Server {
        realm: cfg.realm,
        issuer_override: cfg.issuer,
        key,
        kid,
        users,
        clients,
        store: Mutex::new(Store::new()),
        access_ttl: cfg.access_ttl,
        refresh_ttl: cfg.refresh_ttl,
        code_ttl: cfg.code_ttl,
        session_ttl: cfg.session_ttl,
        auto_login: cfg.auto_login,
        quick_login: cfg.quick_login,
        cors: cfg.cors,
    });

    Ok(Prepared {
        listener,
        server,
        banner,
    })
}

fn handle(srv: Arc<Server>, stream: TcpStream) -> io::Result<()> {
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

#[cfg(test)]
mod lib_tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_match_the_help_text() {
        let c = parse_args(args(&[])).unwrap();
        assert_eq!(c.bind, "127.0.0.1:9500");
        assert_eq!(c.realm, "dev");
        assert_eq!(c.key_bits, 2048);
        assert!(c.quick_login && c.cors);
    }

    #[test]
    fn flags_override_defaults() {
        let c = parse_args(args(&[
            "--bind",
            "0.0.0.0:1",
            "--no-cors",
            "--no-quick-login",
            "--issuer",
            "http://x/",
        ]))
        .unwrap();
        assert_eq!(c.bind, "0.0.0.0:1");
        assert!(!c.cors && !c.quick_login);
        assert_eq!(c.issuer.as_deref(), Some("http://x"));
    }

    #[test]
    fn help_and_version_are_code_zero() {
        let e = parse_args(args(&["--help"])).unwrap_err();
        assert_eq!(e.code, 0);
        assert!(e.message.starts_with("minicloak —"));
        assert_eq!(parse_args(args(&["-V"])).unwrap_err().code, 0);
    }

    #[test]
    fn bad_input_is_code_two() {
        for bad in [
            vec!["--nope"],
            vec!["--bind"],
            vec!["--key-bits", "abc"],
            vec!["--key-bits", "17"],
            vec!["--user", "no-equals-sign"],
        ] {
            let e = parse_args(args(&bad)).unwrap_err();
            assert_eq!(e.code, 2, "{:?}", bad);
            assert!(e.message.starts_with("minicloak: "), "{}", e.message);
        }
    }

    #[test]
    fn env_name_scheme() {
        assert_eq!(env_name("--key-bits"), "MINICLOAK_KEY_BITS");
        assert_eq!(env_name("--no-quick-login"), "MINICLOAK_NO_QUICK_LOGIN");
        assert_eq!(env_name("--bind"), "MINICLOAK_BIND");
    }

    #[test]
    fn switch_env_values() {
        let mut c = Config::default();
        assert!(parse_bool("X", "ON").unwrap());
        assert!(!parse_bool("X", "off").unwrap());
        assert!(parse_bool("X", "maybe").is_err());
        if let Kind::Switch(f) = FLAGS.iter().find(|f| f.name == "--no-cors").unwrap().kind {
            f(&mut c, true);
        }
        assert!(!c.cors);
    }

    #[test]
    fn prepare_reports_a_bad_auto_login_user() {
        let cfg = Config {
            auto_login: Some("nobody".into()),
            ..Config::default()
        };
        let Err(e) = prepare(cfg) else {
            panic!("expected an error");
        };
        assert!(e.to_string().contains("no such user"), "{}", e);
    }

    #[test]
    fn prepare_binds_and_builds_a_banner() {
        let cfg = Config {
            bind: "127.0.0.1:0".into(),
            key_bits: 512,
            ..Config::default()
        };
        let p = prepare(cfg).unwrap();
        let b = p.banner();
        assert!(b.starts_with("minicloak listening on http://127.0.0.1:0\n"));
        assert!(b.ends_with('\n'));
        assert!(b.contains("users:       alice, bob"));
        assert_ne!(p.local_addr().unwrap().port(), 0);
    }

    #[test]
    fn prepared_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Prepared>();
    }
}
