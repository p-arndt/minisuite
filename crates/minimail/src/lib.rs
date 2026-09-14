// minimail: a tiny (default) dependency-free dev SMTP sink + web UI/JSON API.
//
// This is the library face of the binary: `parse_args` turns argv (plus the
// MINIMAIL_* environment) into a `Config`, `prepare` performs every fallible
// step (bind sockets, open the spool, load TLS material) and hands back a
// `Prepared` that only has to be `serve`d. Splitting it this way lets the
// minisuite launcher run minimail in a thread next to the other servers and
// still report startup failures itself — nothing in here ever calls
// `std::process::exit`.

mod api;
mod assets;
mod base64;
mod creds;
mod events;
mod http;
mod json;
mod mime;
mod qp;
mod server;
mod smtp;
mod store;
mod stream;
#[cfg(feature = "tls")]
mod tls;
mod url;
mod util;

use std::env;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

pub use crate::creds::Credentials;

use crate::server::{Hub, Server};
use crate::store::Store;
use crate::stream::Stream;

/// Crate name, used by the launcher for log prefixes.
pub const NAME: &str = "minimail";

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Everything the CLI can set. `Default` matches the documented CLI defaults.
#[derive(Clone, Debug)]
pub struct Config {
    pub smtp_bind: String,
    pub http_bind: String,
    pub root: PathBuf,
    pub creds: Credentials,
    pub anonymous: bool,
    pub hostname: String,
    pub max_size: usize,
    pub max_messages: Option<usize>,
    #[cfg(feature = "tls")]
    pub tls_cert: Option<PathBuf>,
    #[cfg(feature = "tls")]
    pub tls_key: Option<PathBuf>,
    #[cfg(feature = "tls")]
    pub smtps_bind: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            smtp_bind: "127.0.0.1:1025".to_string(),
            http_bind: "127.0.0.1:8025".to_string(),
            root: PathBuf::from("./mail"),
            creds: Credentials::new(),
            anonymous: false,
            hostname: system_hostname(),
            max_size: 26_214_400,
            max_messages: None,
            #[cfg(feature = "tls")]
            tls_cert: None,
            #[cfg(feature = "tls")]
            tls_key: None,
            #[cfg(feature = "tls")]
            smtps_bind: None,
        }
    }
}

/// A parse outcome that is not a `Config`. `--help`/`--version` are *not*
/// failures, hence the code: the caller prints `message` and exits with `code`.
#[derive(Clone, Debug)]
pub struct CliError {
    pub message: String,
    pub code: i32,
}

impl CliError {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            message: format!(
                "{}\n\nTry 'minimail --help' for more information.",
                message.into()
            ),
            code: 2,
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

pub const HELP: &str = "\
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
  -V, --version            show the version and exit

Every flag has an environment variable twin: MINIMAIL_ + the flag name in
upper case with dashes turned into underscores (--smtp-bind -> MINIMAIL_SMTP_BIND).
Boolean flags take 1|true|yes|on / 0|false|no|off. A flag on the command line
overrides its environment variable, which overrides the default.";

// ---------------------------------------------------------------------------
// Flag table
// ---------------------------------------------------------------------------

/// Parse state: the config plus the two half-credentials that only mean
/// something once both `--user` and `--password` have been seen.
struct Builder {
    cfg: Config,
    pending_user: Option<String>,
    pending_password: Option<String>,
}

/// One flag, described once and used by both the environment pass and the
/// command-line pass so the two can never drift apart. `apply` always takes a
/// string; boolean flags parse it themselves and get "true" when the bare flag
/// appears on the command line.
struct Flag {
    name: &'static str,
    takes_value: bool,
    apply: fn(&mut Builder, &str) -> Result<(), String>,
}

const FLAGS: &[Flag] = &[
    Flag {
        name: "smtp-bind",
        takes_value: true,
        apply: |b, v| {
            b.cfg.smtp_bind = v.to_string();
            Ok(())
        },
    },
    Flag {
        name: "http-bind",
        takes_value: true,
        apply: |b, v| {
            b.cfg.http_bind = v.to_string();
            Ok(())
        },
    },
    Flag {
        name: "root",
        takes_value: true,
        apply: |b, v| {
            b.cfg.root = PathBuf::from(v);
            Ok(())
        },
    },
    Flag {
        name: "credentials",
        takes_value: true,
        apply: |b, v| {
            let c = Credentials::load_file(Path::new(v))
                .map_err(|e| format!("failed to load credentials from {v}: {e}"))?;
            for (k, secret) in c.map {
                b.cfg.creds.add(&k, &secret);
            }
            Ok(())
        },
    },
    Flag {
        name: "user",
        takes_value: true,
        apply: |b, v| {
            b.pending_user = Some(v.to_string());
            Ok(())
        },
    },
    Flag {
        name: "password",
        takes_value: true,
        apply: |b, v| {
            b.pending_password = Some(v.to_string());
            Ok(())
        },
    },
    Flag {
        name: "anonymous",
        takes_value: false,
        apply: |b, v| {
            b.cfg.anonymous = parse_bool("--anonymous", v)?;
            Ok(())
        },
    },
    Flag {
        name: "hostname",
        takes_value: true,
        apply: |b, v| {
            b.cfg.hostname = v.to_string();
            Ok(())
        },
    },
    Flag {
        name: "max-size",
        takes_value: true,
        apply: |b, v| {
            b.cfg.max_size = v
                .parse()
                .map_err(|_| format!("--max-size: not a byte count: {v}"))?;
            Ok(())
        },
    },
    Flag {
        name: "max-messages",
        takes_value: true,
        apply: |b, v| {
            b.cfg.max_messages = Some(
                v.parse()
                    .map_err(|_| format!("--max-messages: not a number: {v}"))?,
            );
            Ok(())
        },
    },
    Flag {
        name: "tls-cert",
        takes_value: true,
        apply: set_tls_cert,
    },
    Flag {
        name: "tls-key",
        takes_value: true,
        apply: set_tls_key,
    },
    Flag {
        name: "smtps-bind",
        takes_value: true,
        apply: set_smtps_bind,
    },
];

// The three TLS flags are always *recognised* so that a default build can say
// what is actually wrong instead of "unknown arg".
#[cfg(feature = "tls")]
fn set_tls_cert(b: &mut Builder, v: &str) -> Result<(), String> {
    b.cfg.tls_cert = Some(PathBuf::from(v));
    Ok(())
}
#[cfg(not(feature = "tls"))]
fn set_tls_cert(_b: &mut Builder, _v: &str) -> Result<(), String> {
    Err(tls_disabled("--tls-cert"))
}

#[cfg(feature = "tls")]
fn set_tls_key(b: &mut Builder, v: &str) -> Result<(), String> {
    b.cfg.tls_key = Some(PathBuf::from(v));
    Ok(())
}
#[cfg(not(feature = "tls"))]
fn set_tls_key(_b: &mut Builder, _v: &str) -> Result<(), String> {
    Err(tls_disabled("--tls-key"))
}

#[cfg(feature = "tls")]
fn set_smtps_bind(b: &mut Builder, v: &str) -> Result<(), String> {
    b.cfg.smtps_bind = Some(v.to_string());
    Ok(())
}
#[cfg(not(feature = "tls"))]
fn set_smtps_bind(_b: &mut Builder, _v: &str) -> Result<(), String> {
    Err(tls_disabled("--smtps-bind"))
}

#[cfg(not(feature = "tls"))]
fn tls_disabled(flag: &str) -> String {
    format!("{flag} requires a build with --features tls")
}

fn parse_bool(flag: &str, v: &str) -> Result<bool, String> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => Err(format!("{flag}: expected a boolean, got {other:?}")),
    }
}

/// `MINIMAIL_SMTP_BIND` for `--smtp-bind`.
fn env_name(flag: &str) -> String {
    format!("MINIMAIL_{}", flag.to_uppercase().replace('-', "_"))
}

/// Parse `args` (which EXCLUDES argv[0]). Environment variables are applied
/// first, then the command line, so a flag always wins over its env twin.
pub fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<Config, CliError> {
    let mut b = Builder {
        cfg: Config::default(),
        pending_user: None,
        pending_password: None,
    };

    for flag in FLAGS {
        if let Ok(v) = env::var(env_name(flag.name)) {
            (flag.apply)(&mut b, &v).map_err(CliError::usage)?;
        }
    }
    take_pending_credential(&mut b);

    let mut args = args.into_iter();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--help" | "-h" => {
                return Err(CliError {
                    message: HELP.to_string(),
                    code: 0,
                })
            }
            "--version" | "-V" => {
                return Err(CliError {
                    message: format!("minimail {VERSION}"),
                    code: 0,
                })
            }
            _ => {}
        }
        let name = match a.strip_prefix("--") {
            Some(n) => n,
            None => return Err(CliError::usage(format!("unknown arg: {a}"))),
        };
        let flag = match FLAGS.iter().find(|f| f.name == name) {
            Some(f) => f,
            None => return Err(CliError::usage(format!("unknown arg: {a}"))),
        };
        let value = if flag.takes_value {
            match args.next() {
                Some(v) => v,
                None => return Err(CliError::usage(format!("missing value for {a}"))),
            }
        } else {
            "true".to_string()
        };
        (flag.apply)(&mut b, &value).map_err(CliError::usage)?;
        take_pending_credential(&mut b);
    }

    if b.pending_user.is_some() || b.pending_password.is_some() {
        return Err(CliError::usage(
            "--user and --password must be provided together",
        ));
    }

    #[cfg(feature = "tls")]
    if b.cfg.smtps_bind.is_some() && (b.cfg.tls_cert.is_none() || b.cfg.tls_key.is_none()) {
        return Err(CliError::usage(
            "--smtps-bind requires --tls-cert and --tls-key",
        ));
    }

    let mut cfg = b.cfg;
    ensure_default_credential(&mut cfg);
    Ok(cfg)
}

/// `--user a --password 1 --user b --password 2` adds two credentials, so the
/// pair is folded in as soon as both halves are known.
fn take_pending_credential(b: &mut Builder) {
    if b.pending_user.is_some() && b.pending_password.is_some() {
        let u = b.pending_user.take().unwrap_or_default();
        let p = b.pending_password.take().unwrap_or_default();
        b.cfg.creds.add(&u, &p);
    }
}

/// Authenticated-but-credential-less is unusable, so fall back to the shared
/// dev credential (minibucket parity). Idempotent: `prepare` re-applies it for
/// callers that built a `Config` by hand.
fn ensure_default_credential(cfg: &mut Config) {
    if !cfg.anonymous && cfg.creds.is_empty() {
        cfg.creds.add("minimail", "minimail");
    }
}

/// True when the only credential in effect is the seeded `minimail/minimail`.
fn uses_default_credential(cfg: &Config) -> bool {
    !cfg.anonymous
        && cfg.creds.map.len() == 1
        && cfg.creds.secret_for("minimail") == Some("minimail")
}

/// True when `addr` (a `host:port` bind string) only listens on this machine:
/// 127.0.0.0/8, `::1` or `localhost`. Anything else — including `0.0.0.0` and
/// `[::]` — is reachable from the network.
pub fn is_loopback_bind(addr: &str) -> bool {
    let host = if let Some(rest) = addr.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else if addr.matches(':').count() <= 1 {
        addr.split(':').next().unwrap_or("")
    } else {
        addr
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// The lines appended to the banner when a listener is reachable from the
/// network while the seeded dev credential is still in use. Empty otherwise.
fn exposure_warning(cfg: &Config) -> String {
    #[cfg(feature = "tls")]
    let smtps = cfg.smtps_bind.as_deref();
    #[cfg(not(feature = "tls"))]
    let smtps: Option<&str> = None;
    let exposed: Vec<&str> = [
        Some(cfg.smtp_bind.as_str()),
        Some(cfg.http_bind.as_str()),
        smtps,
    ]
    .into_iter()
    .flatten()
    .filter(|b| !is_loopback_bind(b))
    .collect();
    if exposed.is_empty() || !uses_default_credential(cfg) {
        return String::new();
    }
    format!(
        "\nWARNING: {} is bound to {} and reachable from other hosts, but:\n  \
         - the built-in dev credential minimail/minimail is active for SMTP AUTH and the web UI;\n    \
         set --user / --password (MINIMAIL_USER / MINIMAIL_PASSWORD) or --credentials to replace it\n  \
         Bind to 127.0.0.1 (--smtp-bind / --http-bind) unless every host on this network is trusted.\n",
        NAME,
        exposed.join(", ")
    )
}

// No std hostname API; probe env then /etc/hostname, else the crate name.
fn system_hostname() -> String {
    for var in ["HOSTNAME", "COMPUTERNAME"] {
        if let Ok(h) = env::var(var) {
            let t = h.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    if let Ok(h) = std::fs::read_to_string("/etc/hostname") {
        let t = h.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    "minimail".to_string()
}

// ---------------------------------------------------------------------------
// prepare / serve
// ---------------------------------------------------------------------------

/// A fully initialised server: sockets bound, spool open, TLS loaded. Nothing
/// past this point can fail at startup, so a launcher can bind everything up
/// front and only then hand the accept loops to a thread.
pub struct Prepared {
    server: Arc<Server>,
    smtp_listener: TcpListener,
    http_listener: TcpListener,
    #[cfg(feature = "tls")]
    smtps_listener: Option<TcpListener>,
    banner: String,
}

impl Prepared {
    /// The startup banner, with a trailing newline. The caller decides where it
    /// goes (the binary prints it to stderr).
    pub fn banner(&self) -> String {
        self.banner.clone()
    }

    /// Run both accept loops. Blocks forever; SMTP (and optional SMTPS) get
    /// their own threads, HTTP owns the calling thread.
    pub fn serve(self) -> io::Result<()> {
        let Prepared {
            server,
            smtp_listener,
            http_listener,
            #[cfg(feature = "tls")]
            smtps_listener,
            ..
        } = self;

        {
            let srv = Arc::clone(&server);
            thread::spawn(move || serve_loop(smtp_listener, srv, smtp::serve, false));
        }
        #[cfg(feature = "tls")]
        if let Some(listener) = smtps_listener {
            let srv = Arc::clone(&server);
            thread::spawn(move || serve_loop(listener, srv, smtp::serve, true));
        }

        serve_loop(http_listener, server, api::serve, false);
        Ok(())
    }
}

/// Do everything that can fail: open the spool, load TLS material, bind the
/// listeners. Every failure comes back as an `io::Error` with a human-readable
/// message — this function never exits the process.
pub fn prepare(cfg: Config) -> io::Result<Prepared> {
    let mut cfg = cfg;
    ensure_default_credential(&mut cfg);

    let store = Store::new(cfg.root.clone())
        .map_err(|e| io::Error::new(e.kind(), format!("root {}: {}", cfg.root.display(), e)))?;

    #[cfg(feature = "tls")]
    let tls = match (&cfg.tls_cert, &cfg.tls_key) {
        (Some(cert), Some(key)) => {
            Some(crate::tls::load_server_config(cert, key).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("tls cert {} / key {}: {e}", cert.display(), key.display()),
                )
            })?)
        }
        _ => None,
    };

    let server = Arc::new(Server {
        store,
        creds: cfg.creds.clone(),
        require_auth: !cfg.anonymous,
        hostname: cfg.hostname.clone(),
        version: VERSION,
        max_size: cfg.max_size,
        max_messages: cfg.max_messages,
        smtp_bind: cfg.smtp_bind.clone(),
        http_bind: cfg.http_bind.clone(),
        hub: Hub::new(),
        #[cfg(feature = "tls")]
        tls,
    });

    let smtp_listener = bind(&cfg.smtp_bind, "smtp")?;
    let http_listener = bind(&cfg.http_bind, "http")?;
    #[cfg(feature = "tls")]
    let smtps_listener = match &cfg.smtps_bind {
        Some(b) => Some(bind(b, "smtps")?),
        None => None,
    };

    Ok(Prepared {
        server,
        smtp_listener,
        http_listener,
        #[cfg(feature = "tls")]
        smtps_listener,
        banner: banner(&cfg),
    })
}

fn bind(addr: &str, what: &str) -> io::Result<TcpListener> {
    TcpListener::bind(addr)
        .map_err(|e| io::Error::new(e.kind(), format!("bind {what} {addr}: {e}")))
}

fn banner(cfg: &Config) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    // Infallible: writing into a String never fails.
    let _ = writeln!(s, "minimail {VERSION}");
    let _ = writeln!(s, "  SMTP  smtp://{}", cfg.smtp_bind);
    let _ = writeln!(s, "  HTTP  http://{}", cfg.http_bind);
    let _ = writeln!(s, "  root  {}", cfg.root.display());
    if cfg.anonymous {
        let _ = writeln!(s, "  auth  anonymous (no auth)");
    } else {
        let _ = writeln!(s, "  auth  {} credential(s)", cfg.creds.map.len());
    }
    let _ = writeln!(s, "  hostname  {}", cfg.hostname);
    let _ = writeln!(s, "  max-size  {} bytes", cfg.max_size);
    if let Some(n) = cfg.max_messages {
        let _ = writeln!(s, "  max-messages  {n}");
    }
    #[cfg(feature = "tls")]
    {
        if let Some(b) = &cfg.smtps_bind {
            let _ = writeln!(s, "  SMTPS  smtps://{b}");
        }
        if cfg.tls_cert.is_some() && cfg.tls_key.is_some() {
            let _ = writeln!(s, "  tls  enabled (STARTTLS)");
        }
    }
    s.push_str(&exposure_warning(cfg));
    s
}

/// One accept loop, thread-per-connection, `Arc::clone` per connection. A panicking
/// connection thread unwinds in isolation and never takes the process down.
/// `implicit_tls` wraps the accepted `TcpStream` via `tls::accept` before handoff.
fn serve_loop(
    listener: TcpListener,
    srv: Arc<Server>,
    handler: fn(&Server, Stream, Option<SocketAddr>) -> io::Result<()>,
    implicit_tls: bool,
) {
    for conn in listener.incoming() {
        match conn {
            Ok(tcp) => {
                let _ = tcp.set_nodelay(true);
                let peer = tcp.peer_addr().ok();
                let srv = Arc::clone(&srv);
                thread::spawn(move || {
                    let stream = match wrap_stream(&srv, tcp, implicit_tls) {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("[tls] {peer:?} {e}");
                            return;
                        }
                    };
                    if let Err(e) = handler(&srv, stream, peer) {
                        eprintln!("[conn] {peer:?} {e}");
                    }
                });
            }
            Err(e) => eprintln!("accept: {e}"),
        }
    }
}

#[cfg(feature = "tls")]
fn wrap_stream(srv: &Server, tcp: TcpStream, implicit_tls: bool) -> io::Result<Stream> {
    if implicit_tls {
        let cfg = srv
            .tls
            .as_ref()
            .ok_or_else(|| io::Error::other("smtps listener without tls config"))?;
        crate::tls::accept(tcp, cfg)
    } else {
        Ok(Stream::Plain(tcp))
    }
}

#[cfg(not(feature = "tls"))]
fn wrap_stream(_srv: &Server, tcp: TcpStream, _implicit_tls: bool) -> io::Result<Stream> {
    Ok(Stream::Plain(tcp))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    // `Prepared` is moved into a thread by the minisuite launcher.
    #[test]
    fn prepared_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Prepared>();
        assert_send::<Config>();
    }

    #[test]
    fn loopback_binds_are_recognised() {
        for a in [
            "127.0.0.1:1025",
            "127.1.2.3:1",
            "[::1]:8025",
            "::1",
            "localhost:1025",
            "LOCALHOST:1",
        ] {
            assert!(is_loopback_bind(a), "{}", a);
        }
        for a in [
            "0.0.0.0:1025",
            "[::]:8025",
            "192.168.1.10:1025",
            "example.com:1",
            "",
        ] {
            assert!(!is_loopback_bind(a), "{}", a);
        }
    }

    #[test]
    fn banner_warns_when_exposed_with_the_default_credential() {
        let cfg = parse_args(args(&[])).unwrap();
        assert!(!banner(&cfg).contains("WARNING"));

        let cfg = parse_args(args(&["--http-bind", "0.0.0.0:8025"])).unwrap();
        let b = banner(&cfg);
        assert!(b.contains("WARNING") && b.contains("0.0.0.0:8025"), "{}", b);
        assert!(b.contains("minimail/minimail") && b.contains("MINIMAIL_USER"));

        // Own credential or anonymous: nothing to warn about.
        let cfg = parse_args(args(&[
            "--http-bind",
            "0.0.0.0:8025",
            "--user",
            "a",
            "--password",
            "b",
        ]))
        .unwrap();
        assert!(!banner(&cfg).contains("WARNING"));
        let cfg = parse_args(args(&["--http-bind", "0.0.0.0:8025", "--anonymous"])).unwrap();
        assert!(!banner(&cfg).contains("WARNING"));
    }

    #[test]
    fn defaults_match_help_text() {
        let cfg = parse_args(args(&[])).unwrap();
        assert_eq!(cfg.smtp_bind, "127.0.0.1:1025");
        assert_eq!(cfg.http_bind, "127.0.0.1:8025");
        assert_eq!(cfg.root, PathBuf::from("./mail"));
        assert_eq!(cfg.max_size, 26_214_400);
        assert_eq!(cfg.max_messages, None);
        assert!(!cfg.anonymous);
        // no --anonymous and no credentials => the shared dev credential
        assert_eq!(cfg.creds.secret_for("minimail"), Some("minimail"));
    }

    #[test]
    fn flags_override() {
        let cfg = parse_args(args(&[
            "--smtp-bind",
            "0.0.0.0:2525",
            "--http-bind",
            "0.0.0.0:9025",
            "--root",
            "/tmp/x",
            "--anonymous",
            "--max-messages",
            "7",
            "--hostname",
            "mx.test",
        ]))
        .unwrap();
        assert_eq!(cfg.smtp_bind, "0.0.0.0:2525");
        assert_eq!(cfg.http_bind, "0.0.0.0:9025");
        assert_eq!(cfg.root, PathBuf::from("/tmp/x"));
        assert!(cfg.anonymous);
        assert_eq!(cfg.max_messages, Some(7));
        assert_eq!(cfg.hostname, "mx.test");
        assert!(cfg.creds.is_empty());
    }

    #[test]
    fn user_password_pairs() {
        let cfg = parse_args(args(&[
            "--user",
            "a",
            "--password",
            "1",
            "--user",
            "b",
            "--password",
            "2",
        ]))
        .unwrap();
        assert_eq!(cfg.creds.secret_for("a"), Some("1"));
        assert_eq!(cfg.creds.secret_for("b"), Some("2"));
    }

    #[test]
    fn lone_user_is_an_error() {
        let e = parse_args(args(&["--user", "a"])).unwrap_err();
        assert_eq!(e.code, 2);
        assert!(e.message.contains("must be provided together"));
    }

    #[test]
    fn help_and_version_are_code_zero() {
        let e = parse_args(args(&["--help"])).unwrap_err();
        assert_eq!(e.code, 0);
        assert!(e.message.starts_with("minimail —"));
        let e = parse_args(args(&["-V"])).unwrap_err();
        assert_eq!(e.code, 0);
        assert_eq!(e.message, format!("minimail {VERSION}"));
    }

    #[test]
    fn unknown_and_bad_values_are_code_two() {
        assert_eq!(parse_args(args(&["--nope"])).unwrap_err().code, 2);
        assert_eq!(parse_args(args(&["-x"])).unwrap_err().code, 2);
        assert_eq!(parse_args(args(&["--smtp-bind"])).unwrap_err().code, 2);
        assert_eq!(
            parse_args(args(&["--max-size", "big"])).unwrap_err().code,
            2
        );
    }

    #[test]
    fn bool_values() {
        assert!(parse_bool("--anonymous", "YES").unwrap());
        assert!(parse_bool("--anonymous", "On").unwrap());
        assert!(!parse_bool("--anonymous", "0").unwrap());
        assert!(!parse_bool("--anonymous", "OFF").unwrap());
        assert!(parse_bool("--anonymous", "maybe").is_err());
    }

    #[test]
    fn env_names() {
        assert_eq!(env_name("smtp-bind"), "MINIMAIL_SMTP_BIND");
        assert_eq!(env_name("max-messages"), "MINIMAIL_MAX_MESSAGES");
        assert_eq!(env_name("root"), "MINIMAIL_ROOT");
    }

    #[test]
    fn banner_ends_with_newline() {
        let cfg = parse_args(args(&[])).unwrap();
        let b = banner(&cfg);
        assert!(b.starts_with("minimail "));
        assert!(b.ends_with('\n'));
        assert!(b.contains("  SMTP  smtp://127.0.0.1:1025\n"));
    }
}
