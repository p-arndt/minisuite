// minibucket: a tiny, dependency-free S3-compatible object storage server.
//
// This is the library face of the crate: `parse_args` turns argv (and the
// MINIBUCKET_* environment) into a `Config`, `prepare` performs every step
// that can fail (bind the socket, create the root dir, load credentials) and
// hands back a `Prepared` that only has to be `serve()`d. Splitting it this
// way lets the `minisuite` launcher run minibucket in a thread next to the
// other servers and still report startup errors properly — nothing in here
// ever calls `std::process::exit`.

mod creds;
mod hmac;
mod http;
mod md5;
mod multipart;
mod s3;
mod sha256;
mod sigv4;
mod storage;
mod tagging;
mod url;
mod util;
mod versioning;

use std::env;
use std::fmt;
use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use crate::creds::Credentials;
use crate::http::Response;
use crate::s3::{error_response, Server};
use crate::storage::Storage;

pub const NAME: &str = "minibucket";

/// The crate version, kept in sync with Cargo.toml (which inherits it from the
/// workspace).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Everything the CLI (or the environment) can set. Nothing in here has been
/// validated yet — that happens in [`prepare`].
#[derive(Clone)]
pub struct Config {
    /// `--bind` / `MINIBUCKET_BIND`
    pub bind: String,
    /// `--root` / `MINIBUCKET_ROOT`
    pub root: PathBuf,
    /// `--access-key` + `--secret-key` pairs, in the order they were given.
    /// A pair is only complete once both halves have been seen.
    pub keys: Vec<(String, String)>,
    /// `--credentials` / `MINIBUCKET_CREDENTIALS`: files of `KEY=SECRET` lines.
    /// Read in [`prepare`] so a bad file is a returned error, not an exit.
    pub credential_files: Vec<PathBuf>,
    /// `--region` / `MINIBUCKET_REGION`
    pub region: String,
    /// `--domain` / `MINIBUCKET_DOMAIN`: virtual-hosted addressing for `*.D`
    pub domain: Option<String>,
    /// `--anonymous` / `MINIBUCKET_ANONYMOUS`: disable auth entirely
    pub anonymous: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:9000".to_string(),
            root: PathBuf::from("./data"),
            keys: Vec::new(),
            credential_files: Vec::new(),
            region: "us-east-1".to_string(),
            domain: None,
            anonymous: false,
        }
    }
}

// Hand-written so secrets never end up in a log line just because someone
// debug-printed the config.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let keys: Vec<&str> = self.keys.iter().map(|(a, _)| a.as_str()).collect();
        f.debug_struct("Config")
            .field("bind", &self.bind)
            .field("root", &self.root)
            .field("access_keys", &keys)
            .field("credential_files", &self.credential_files)
            .field("region", &self.region)
            .field("domain", &self.domain)
            .field("anonymous", &self.anonymous)
            .finish()
    }
}

/// A reason to stop before serving: either the user asked for `--help` /
/// `--version` (`code == 0`, print to stdout) or they got the invocation wrong
/// (`code == 2`, print to stderr).
#[derive(Clone, Debug)]
pub struct CliError {
    pub message: String,
    pub code: i32,
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

impl CliError {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            message: format!("{}: {}\nTry '{NAME} --help'.", NAME, message.into()),
            code: 2,
        }
    }
}

// ---------------------------------------------------------------------------
// Flag table
// ---------------------------------------------------------------------------

// Parsing state: the config plus the two half-filled slots for the
// --access-key/--secret-key pair, which only becomes a credential once both
// sides are known (in either order).
struct State {
    cfg: Config,
    pending_access: Option<String>,
    pending_secret: Option<String>,
}

impl State {
    // Whenever both halves are present, commit them and start over.
    fn flush_pair(&mut self) {
        if self.pending_access.is_some() && self.pending_secret.is_some() {
            let a = self.pending_access.take().unwrap();
            let s = self.pending_secret.take().unwrap();
            self.cfg.keys.push((a, s));
        }
    }
}

// A single option. `apply` is shared by the CLI and the environment so the two
// paths can never drift apart; `value` is the CLI argument, or the env-var
// contents, or "true" for a bare boolean flag on the command line.
struct Flag {
    /// Long name without the leading dashes.
    name: &'static str,
    /// Does the command-line form consume the following argv entry?
    takes_value: bool,
    apply: fn(&mut State, &str) -> Result<(), String>,
}

const FLAGS: &[Flag] = &[
    Flag {
        name: "bind",
        takes_value: true,
        apply: |s, v| {
            s.cfg.bind = v.to_string();
            Ok(())
        },
    },
    Flag {
        name: "root",
        takes_value: true,
        apply: |s, v| {
            s.cfg.root = PathBuf::from(v);
            Ok(())
        },
    },
    // access-key comes before secret-key in the table so that setting both via
    // the environment pairs them up in one pass.
    Flag {
        name: "access-key",
        takes_value: true,
        apply: |s, v| {
            s.pending_access = Some(v.to_string());
            s.flush_pair();
            Ok(())
        },
    },
    Flag {
        name: "secret-key",
        takes_value: true,
        apply: |s, v| {
            s.pending_secret = Some(v.to_string());
            s.flush_pair();
            Ok(())
        },
    },
    Flag {
        name: "credentials",
        takes_value: true,
        apply: |s, v| {
            s.cfg.credential_files.push(PathBuf::from(v));
            Ok(())
        },
    },
    Flag {
        name: "region",
        takes_value: true,
        apply: |s, v| {
            s.cfg.region = v.to_string();
            Ok(())
        },
    },
    Flag {
        name: "domain",
        takes_value: true,
        apply: |s, v| {
            s.cfg.domain = Some(v.to_string());
            Ok(())
        },
    },
    Flag {
        name: "anonymous",
        takes_value: false,
        apply: |s, v| {
            s.cfg.anonymous = parse_bool(v)?;
            Ok(())
        },
    },
];

// MINIBUCKET_ + the flag name, uppercased, dashes turned into underscores.
fn env_name(flag: &str) -> String {
    format!("MINIBUCKET_{}", flag.to_uppercase().replace('-', "_"))
}

fn parse_bool(v: &str) -> Result<bool, String> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => Err(format!(
            "invalid boolean value {:?} (expected 1/true/yes/on or 0/false/no/off)",
            other
        )),
    }
}

pub fn help_text() -> String {
    let mut s = String::new();
    s.push_str("minibucket — minimal S3-compatible server\n\n");
    s.push_str("Usage: minibucket [options]\n");
    s.push_str("  --bind ADDR              default 127.0.0.1:9000\n");
    s.push_str("  --root DIR               default ./data\n");
    s.push_str("  --access-key K           access key id (use with --secret-key)\n");
    s.push_str("  --secret-key S           secret key (must follow --access-key)\n");
    s.push_str("  --credentials FILE       load multiple KEY=SECRET lines\n");
    s.push_str("  --region R               default us-east-1\n");
    s.push_str("  --domain D               enable virtual-hosted addressing for bucket.D\n");
    s.push_str("  --anonymous              disable auth (dev only)\n");
    s.push_str("  -h, --help               show this help\n");
    s.push_str("  -V, --version            show the version\n");
    s.push('\n');
    s.push_str("Every option also reads an environment variable named MINIBUCKET_ plus the\n");
    s.push_str(
        "flag in upper case with dashes as underscores (--access-key -> MINIBUCKET_ACCESS_KEY);\n",
    );
    s.push_str(
        "booleans take 1/true/yes/on or 0/false/no/off. Command-line flags win over the environment.",
    );
    s
}

pub fn version_text() -> String {
    format!("{NAME} {VERSION}")
}

/// Parse `args` (which must NOT include argv[0]) into a [`Config`].
///
/// The environment is applied first, then the command line on top, so an
/// explicit flag always beats a `MINIBUCKET_*` variable.
pub fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<Config, CliError> {
    let mut st = State {
        cfg: Config::default(),
        pending_access: None,
        pending_secret: None,
    };

    for f in FLAGS {
        let name = env_name(f.name);
        if let Ok(v) = env::var(&name) {
            (f.apply)(&mut st, &v).map_err(|e| CliError::usage(format!("{}: {}", name, e)))?;
        }
    }

    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--help" | "-h" => {
                return Err(CliError {
                    message: help_text(),
                    code: 0,
                })
            }
            "--version" | "-V" => {
                return Err(CliError {
                    message: version_text(),
                    code: 0,
                })
            }
            _ => {}
        }
        let flag = a
            .strip_prefix("--")
            .and_then(|n| FLAGS.iter().find(|f| f.name == n))
            .ok_or_else(|| CliError::usage(format!("unknown arg: {}", a)))?;
        let value = if flag.takes_value {
            match it.next() {
                Some(v) => v,
                None => return Err(CliError::usage(format!("{} requires a value", a))),
            }
        } else {
            "true".to_string()
        };
        (flag.apply)(&mut st, &value).map_err(|e| CliError::usage(format!("{}: {}", a, e)))?;
    }

    if st.pending_access.is_some() || st.pending_secret.is_some() {
        return Err(CliError {
            message: "--access-key and --secret-key must be provided together".to_string(),
            code: 2,
        });
    }
    Ok(st.cfg)
}

// ---------------------------------------------------------------------------
// prepare / serve
// ---------------------------------------------------------------------------

/// A fully started server: the socket is bound, the root directory exists and
/// the credentials are loaded. Everything that can fail already has.
pub struct Prepared {
    listener: TcpListener,
    server: Arc<Server>,
    bind: String,
    root: PathBuf,
    anonymous: bool,
    region: String,
    domain: Option<String>,
    // Sorted so the banner is stable run to run (the credential store is a
    // HashMap).
    access_keys: Vec<String>,
}

fn err(msg: String) -> io::Error {
    io::Error::other(msg)
}

/// Bind the socket, create the root dir, load credentials.
///
/// Every failure comes back as an `Err` carrying a message meant for a human;
/// this function never terminates the process.
pub fn prepare(cfg: Config) -> io::Result<Prepared> {
    let mut credentials = Credentials::new();
    for path in &cfg.credential_files {
        let loaded = Credentials::load_file(path).map_err(|e| {
            err(format!(
                "failed to load credentials from {}: {}",
                path.display(),
                e
            ))
        })?;
        for (k, v) in loaded.map {
            credentials.add(&k, &v);
        }
    }
    for (access, secret) in &cfg.keys {
        credentials.add(access, secret);
    }
    if !cfg.anonymous && credentials.is_empty() {
        // Default dev credential.
        credentials.add("minioadmin", "minioadmin");
    }

    let storage = Storage::new(cfg.root.clone()).map_err(|e| {
        err(format!(
            "failed to create root {}: {}",
            cfg.root.display(),
            e
        ))
    })?;

    let listener = TcpListener::bind(&cfg.bind)
        .map_err(|e| err(format!("failed to bind {}: {}", cfg.bind, e)))?;

    let mut access_keys: Vec<String> = credentials.map.keys().cloned().collect();
    access_keys.sort();

    let server = Arc::new(Server {
        storage,
        credentials,
        require_auth: !cfg.anonymous,
        region: cfg.region.clone(),
        domain: cfg.domain.clone(),
    });

    Ok(Prepared {
        listener,
        server,
        bind: cfg.bind,
        root: cfg.root,
        anonymous: cfg.anonymous,
        region: cfg.region,
        domain: cfg.domain,
        access_keys,
    })
}

// Deliberately terse: the interesting part is where it listens, and the
// credential store must not show up in a log line.
impl fmt::Debug for Prepared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Prepared")
            .field("bind", &self.bind)
            .field("root", &self.root)
            .field("anonymous", &self.anonymous)
            .finish_non_exhaustive()
    }
}

impl Prepared {
    /// The address the listener actually bound to (useful when the config asked
    /// for port 0).
    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// The multi-line startup banner, including its trailing newline.
    pub fn banner(&self) -> String {
        let mut s = format!(
            "minibucket listening on http://{} (root: {})\n",
            self.bind,
            self.root.display()
        );
        if self.anonymous {
            s.push_str("  anonymous mode (no auth required)\n");
        } else {
            s.push_str(&format!("  region: {}\n", self.region));
            for k in &self.access_keys {
                s.push_str(&format!("  access-key: {}\n", k));
            }
            if let Some(d) = &self.domain {
                s.push_str(&format!("  virtual-hosted domain: *.{}\n", d));
            }
        }
        s
    }

    /// Run the accept loop. Blocks forever under normal operation.
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
                Err(e) => eprintln!("accept: {}", e),
            }
        }
        Ok(())
    }
}

fn handle(srv: Arc<Server>, stream: TcpStream) -> io::Result<()> {
    let _ = stream.set_nodelay(true);
    let peer = stream.peer_addr().ok();

    let mut sock = stream.try_clone()?;
    let req = match crate::http::read_request(stream) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[parse] {:?} {}", peer, e);
            return Ok(());
        }
    };

    eprintln!(
        "[req] {:?} {} {} (q={}) host={}",
        peer,
        req.method,
        req.raw_path,
        req.query_raw,
        req.headers.get("host").unwrap_or("-")
    );

    let mut chunk_ctx: Option<crate::sigv4::ChunkContext> = None;

    if srv.require_auth && crate::sigv4::is_presigned(&req.query_raw) {
        // ---- Presigned URL (query-string) authentication ----
        if let Err(resp) = authenticate_presigned(&srv, &req, &mut sock) {
            return resp;
        }
    } else if srv.require_auth {
        match crate::sigv4::parse_authorization(&req.headers) {
            Ok(info) => {
                let secret = match srv.credentials.secret_for(&info.access_key) {
                    Some(s) => s.to_string(),
                    None => {
                        return error_response(
                            &mut sock,
                            403,
                            "InvalidAccessKeyId",
                            "Unknown access key",
                            &crate::util::request_id(),
                            &req.path,
                        );
                    }
                };
                if let Err(e) = crate::sigv4::verify(
                    &req.method,
                    &req.raw_path,
                    &req.query_raw,
                    &req.headers,
                    &secret,
                    &info,
                ) {
                    eprintln!("[auth] verify failed: {:?}", e);
                    return error_response(
                        &mut sock,
                        403,
                        "SignatureDoesNotMatch",
                        "The signature does not match",
                        &crate::util::request_id(),
                        &req.path,
                    );
                }
                // Build chunk-signing context for streaming PUTs.
                if info.payload_hash == "STREAMING-AWS4-HMAC-SHA256-PAYLOAD" {
                    chunk_ctx = Some(crate::sigv4::ChunkContext::new(&secret, &info));
                }
            }
            Err(crate::sigv4::AuthError::Missing) => {
                return error_response(
                    &mut sock,
                    403,
                    "AccessDenied",
                    "Authorization required",
                    &crate::util::request_id(),
                    &req.path,
                );
            }
            Err(e) => {
                eprintln!("[auth] malformed: {:?}", e);
                return error_response(
                    &mut sock,
                    400,
                    "InvalidRequest",
                    "Malformed Authorization header",
                    &crate::util::request_id(),
                    &req.path,
                );
            }
        }
    }

    if let Err(e) = crate::s3::dispatch(&srv, req, &mut sock, chunk_ctx) {
        eprintln!("[handler] {}", e);
        let resp = Response::new(500).header("Connection", "close");
        let _ = resp.write_headers(&mut sock, Some(0));
    }
    let _ = sock.flush();
    Ok(())
}

// Verify a presigned (query-string) SigV4 request. On any failure this writes
// the appropriate S3 error response and returns Err(<that write result>), which
// handle() propagates. Ok(()) means the request is authenticated.
fn authenticate_presigned<R: std::io::BufRead>(
    srv: &Server,
    req: &crate::http::Request<R>,
    sock: &mut TcpStream,
) -> Result<(), io::Result<()>> {
    let pre = match crate::sigv4::parse_presigned(&req.query_raw) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[auth] presign malformed: {:?}", e);
            return Err(error_response(
                sock,
                400,
                "AuthorizationQueryParametersError",
                "Malformed presigned request",
                &crate::util::request_id(),
                &req.path,
            ));
        }
    };
    let secret = match srv.credentials.secret_for(&pre.info.access_key) {
        Some(s) => s.to_string(),
        None => {
            return Err(error_response(
                sock,
                403,
                "InvalidAccessKeyId",
                "Unknown access key",
                &crate::util::request_id(),
                &req.path,
            ));
        }
    };
    // Expiry window: signed-at + X-Amz-Expires must lie in the future, and the
    // window itself must be within S3's 1s..=7d bounds.
    match crate::util::parse_amz_date(&pre.info.amz_date) {
        Some(signed_at) if (1..=604_800).contains(&pre.expires) => {
            if crate::util::now_secs() > signed_at + pre.expires {
                return Err(error_response(
                    sock,
                    403,
                    "AccessDenied",
                    "Request has expired",
                    &crate::util::request_id(),
                    &req.path,
                ));
            }
        }
        _ => {
            return Err(error_response(
                sock,
                400,
                "AuthorizationQueryParametersError",
                "Invalid X-Amz-Date or X-Amz-Expires",
                &crate::util::request_id(),
                &req.path,
            ));
        }
    }
    if let Err(e) = crate::sigv4::verify_presigned(
        &req.method,
        &req.raw_path,
        &req.query_raw,
        &req.headers,
        &secret,
        &pre.info,
    ) {
        eprintln!("[auth] presign verify failed: {:?}", e);
        return Err(error_response(
            sock,
            403,
            "SignatureDoesNotMatch",
            "The signature does not match",
            &crate::util::request_id(),
            &req.path,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Config, CliError> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults_match_the_documented_ones() {
        let c = Config::default();
        assert_eq!(c.bind, "127.0.0.1:9000");
        assert_eq!(c.root, PathBuf::from("./data"));
        assert_eq!(c.region, "us-east-1");
        assert!(!c.anonymous);
        assert!(c.domain.is_none());
        assert!(c.keys.is_empty());
    }

    #[test]
    fn flags_are_parsed() {
        let c = parse(&[
            "--bind",
            "0.0.0.0:9001",
            "--root",
            "/tmp/x",
            "--region",
            "eu-central-1",
            "--domain",
            "s3.local",
            "--anonymous",
            "--credentials",
            "creds.txt",
        ])
        .unwrap();
        assert_eq!(c.bind, "0.0.0.0:9001");
        assert_eq!(c.root, PathBuf::from("/tmp/x"));
        assert_eq!(c.region, "eu-central-1");
        assert_eq!(c.domain.as_deref(), Some("s3.local"));
        assert!(c.anonymous);
        assert_eq!(c.credential_files, vec![PathBuf::from("creds.txt")]);
    }

    #[test]
    fn key_pairs_commit_in_either_order() {
        let c = parse(&["--access-key", "a", "--secret-key", "s"]).unwrap();
        assert_eq!(c.keys, vec![("a".to_string(), "s".to_string())]);
        let c = parse(&["--secret-key", "s", "--access-key", "a"]).unwrap();
        assert_eq!(c.keys, vec![("a".to_string(), "s".to_string())]);
    }

    #[test]
    fn half_a_key_pair_is_an_error() {
        let e = parse(&["--access-key", "a"]).unwrap_err();
        assert_eq!(e.code, 2);
        assert!(e.message.contains("must be provided together"));
    }

    #[test]
    fn help_and_version_exit_zero() {
        for f in ["--help", "-h"] {
            let e = parse(&[f]).unwrap_err();
            assert_eq!(e.code, 0);
            assert!(e.message.contains("Usage: minibucket"));
            // The env-var scheme must be discoverable from --help alone.
            assert!(e.message.contains("MINIBUCKET_"));
        }
        for f in ["--version", "-V"] {
            let e = parse(&[f]).unwrap_err();
            assert_eq!(e.code, 0);
            assert_eq!(e.message, format!("minibucket {}", VERSION));
        }
    }

    #[test]
    fn bad_flag_and_missing_value_exit_two() {
        assert_eq!(parse(&["--nope"]).unwrap_err().code, 2);
        assert_eq!(parse(&["--bind"]).unwrap_err().code, 2);
    }

    #[test]
    fn env_names_follow_the_scheme() {
        assert_eq!(env_name("bind"), "MINIBUCKET_BIND");
        assert_eq!(env_name("access-key"), "MINIBUCKET_ACCESS_KEY");
        assert_eq!(env_name("credentials"), "MINIBUCKET_CREDENTIALS");
        for f in FLAGS {
            assert!(env_name(f.name).starts_with("MINIBUCKET_"));
        }
    }

    #[test]
    fn booleans_accept_the_documented_spellings() {
        for v in ["1", "true", "TRUE", "Yes", "on"] {
            assert_eq!(parse_bool(v), Ok(true));
        }
        for v in ["0", "false", "NO", "Off"] {
            assert_eq!(parse_bool(v), Ok(false));
        }
        assert!(parse_bool("maybe").is_err());
    }

    #[test]
    fn debug_does_not_leak_secrets() {
        let c = parse(&["--access-key", "a", "--secret-key", "topsecret"]).unwrap();
        assert!(!format!("{:?}", c).contains("topsecret"));
    }

    // prepare() must never exit; a broken credentials file is a plain Err.
    #[test]
    fn prepare_reports_bad_credentials_file() {
        let cfg = Config {
            credential_files: vec![PathBuf::from("./definitely-not-here.creds")],
            ..Config::default()
        };
        let e = prepare(cfg).unwrap_err();
        assert!(e.to_string().contains("failed to load credentials"));
    }

    #[test]
    fn prepare_binds_and_builds_the_banner() {
        let mut root = std::env::temp_dir();
        root.push(format!(
            "minibucket_prepare_{}",
            crate::storage::new_version_id()
        ));
        let cfg = Config {
            bind: "127.0.0.1:0".to_string(),
            root: root.clone(),
            keys: vec![("alice".to_string(), "pw".to_string())],
            ..Config::default()
        };
        let ready = prepare(cfg).unwrap();
        // Port 0 means the OS picked one, so the listener is really bound.
        assert!(ready.local_addr().unwrap().port() != 0);
        let banner = ready.banner();
        assert!(banner.starts_with("minibucket listening on http://127.0.0.1:0"));
        assert!(banner.contains("  access-key: alice\n"));
        assert!(banner.ends_with('\n'));
        let _ = std::fs::remove_dir_all(&root);
    }

    // Prepared is moved into a thread by the minisuite launcher.
    #[test]
    fn prepared_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Prepared>();
        assert_send::<Config>();
    }
}
