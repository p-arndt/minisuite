// minimail: a tiny (default) dependency-free dev SMTP sink + web UI/JSON API.

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

use crate::creds::Credentials;
use crate::server::{Hub, Server};
use crate::store::Store;
use crate::stream::Stream;

struct Config {
    smtp_bind: String,
    http_bind: String,
    root: PathBuf,
    creds: Credentials,
    anonymous: bool,
    hostname: String,
    max_size: usize,
    max_messages: Option<usize>,
    #[cfg(feature = "tls")]
    tls_cert: Option<PathBuf>,
    #[cfg(feature = "tls")]
    tls_key: Option<PathBuf>,
    #[cfg(feature = "tls")]
    smtps_bind: Option<String>,
}

const HELP: &str = "\
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
  -h, --help               show this help and exit";

fn parse_args() -> Config {
    let mut cfg = Config {
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
    };
    let mut pending_user: Option<String> = None;
    let mut pending_password: Option<String> = None;
    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--smtp-bind" => cfg.smtp_bind = args.next().unwrap_or(cfg.smtp_bind),
            "--http-bind" => cfg.http_bind = args.next().unwrap_or(cfg.http_bind),
            "--root" => cfg.root = PathBuf::from(args.next().unwrap_or_default()),
            "--credentials" => {
                let p = args.next().unwrap_or_default();
                let c = Credentials::load_file(Path::new(&p)).unwrap_or_else(|e| {
                    eprintln!("failed to load credentials from {}: {}", p, e);
                    std::process::exit(2);
                });
                for (k, v) in c.map {
                    cfg.creds.add(&k, &v);
                }
            }
            "--user" => pending_user = args.next(),
            "--password" => pending_password = args.next(),
            "--anonymous" => cfg.anonymous = true,
            "--hostname" => cfg.hostname = args.next().unwrap_or(cfg.hostname),
            "--max-size" => {
                cfg.max_size = args
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(cfg.max_size)
            }
            "--max-messages" => cfg.max_messages = args.next().and_then(|s| s.parse().ok()),
            #[cfg(feature = "tls")]
            "--tls-cert" => cfg.tls_cert = args.next().map(PathBuf::from),
            #[cfg(feature = "tls")]
            "--tls-key" => cfg.tls_key = args.next().map(PathBuf::from),
            #[cfg(feature = "tls")]
            "--smtps-bind" => cfg.smtps_bind = args.next(),
            "--help" | "-h" => {
                println!("{HELP}");
                std::process::exit(0);
            }
            _ => {
                eprintln!("unknown arg: {}", a);
                std::process::exit(2);
            }
        }
        if let (Some(u), Some(p)) = (pending_user.clone(), pending_password.clone()) {
            cfg.creds.add(&u, &p);
            pending_user = None;
            pending_password = None;
        }
    }
    if pending_user.is_some() || pending_password.is_some() {
        eprintln!("--user and --password must be provided together");
        std::process::exit(2);
    }
    if !cfg.anonymous && cfg.creds.is_empty() {
        // Default dev credential (minibucket parity).
        cfg.creds.add("minimail", "minimail");
    }
    #[cfg(feature = "tls")]
    if cfg.smtps_bind.is_some() && (cfg.tls_cert.is_none() || cfg.tls_key.is_none()) {
        eprintln!("--smtps-bind requires --tls-cert and --tls-key");
        std::process::exit(2);
    }
    cfg
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

fn banner(cfg: &Config) {
    eprintln!("minimail {}", env!("CARGO_PKG_VERSION"));
    eprintln!("  SMTP  smtp://{}", cfg.smtp_bind);
    eprintln!("  HTTP  http://{}", cfg.http_bind);
    eprintln!("  root  {}", cfg.root.display());
    if cfg.anonymous {
        eprintln!("  auth  anonymous (no auth)");
    } else {
        eprintln!("  auth  {} credential(s)", cfg.creds.map.len());
    }
    eprintln!("  hostname  {}", cfg.hostname);
    eprintln!("  max-size  {} bytes", cfg.max_size);
    if let Some(n) = cfg.max_messages {
        eprintln!("  max-messages  {}", n);
    }
    #[cfg(feature = "tls")]
    {
        if let Some(b) = &cfg.smtps_bind {
            eprintln!("  SMTPS  smtps://{}", b);
        }
        if cfg.tls_cert.is_some() && cfg.tls_key.is_some() {
            eprintln!("  tls  enabled (STARTTLS)");
        }
    }
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
                            eprintln!("[tls] {:?} {}", peer, e);
                            return;
                        }
                    };
                    if let Err(e) = handler(&srv, stream, peer) {
                        eprintln!("[conn] {:?} {}", peer, e);
                    }
                });
            }
            Err(e) => eprintln!("accept: {}", e),
        }
    }
}

#[cfg(feature = "tls")]
fn wrap_stream(srv: &Server, tcp: TcpStream, implicit_tls: bool) -> io::Result<Stream> {
    if implicit_tls {
        let cfg = srv.tls.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "smtps listener without tls config")
        })?;
        crate::tls::accept(tcp, cfg)
    } else {
        Ok(Stream::Plain(tcp))
    }
}

#[cfg(not(feature = "tls"))]
fn wrap_stream(_srv: &Server, tcp: TcpStream, _implicit_tls: bool) -> io::Result<Stream> {
    Ok(Stream::Plain(tcp))
}

fn main() {
    let cfg = parse_args();
    let store = Store::new(cfg.root.clone()).expect("init store");

    #[cfg(feature = "tls")]
    let tls = match (&cfg.tls_cert, &cfg.tls_key) {
        (Some(cert), Some(key)) => {
            Some(crate::tls::load_server_config(cert, key).expect("load tls cert/key"))
        }
        _ => None,
    };

    let server = Arc::new(Server {
        store,
        creds: cfg.creds.clone(),
        require_auth: !cfg.anonymous,
        hostname: cfg.hostname.clone(),
        version: env!("CARGO_PKG_VERSION"),
        max_size: cfg.max_size,
        max_messages: cfg.max_messages,
        smtp_bind: cfg.smtp_bind.clone(),
        http_bind: cfg.http_bind.clone(),
        hub: Hub::new(),
        #[cfg(feature = "tls")]
        tls,
    });

    let smtp_listener = TcpListener::bind(&cfg.smtp_bind).expect("bind smtp");
    let http_listener = TcpListener::bind(&cfg.http_bind).expect("bind http");
    #[cfg(feature = "tls")]
    let smtps_listener = cfg
        .smtps_bind
        .as_ref()
        .map(|b| TcpListener::bind(b).expect("bind smtps"));

    banner(&cfg);

    // SMTP (and optional SMTPS) accept on spawned threads; the HTTP loop owns the
    // main thread so main() never returns.
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
}
