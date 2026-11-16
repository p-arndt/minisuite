// minisuite: minicloak (OIDC) + minimail (SMTP sink) + minibucket (S3) in one
// process, plus a landing page that links all three.
//
// Each of the three crates exposes the same three-step library API —
// `parse_args` -> `prepare` -> `serve` — and none of them ever calls
// `std::process::exit`. That is what makes this launcher possible: it can bind
// every socket up front, report a failure with the name of the service that
// caused it, and only then hand three accept loops to three threads.
//
// The suite knows *no* flag of any of the three crates. It only produces argv
// and lets each crate parse it, so validation and error messages stay where the
// flag is defined.

pub mod conf;
pub mod landing;
mod suggest;

/// Every long flag the suite itself accepts, with its dashes. Kept next to
/// `HELP` and checked against it by a test.
pub const FLAGS: &[&str] = &[
    "--data",
    "--config",
    "--bind-all",
    "--landing-bind",
    "--no-landing",
    "--only",
];

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use crate::conf::Section;
use crate::landing::{Entry, Link};

pub const NAME: &str = "minisuite";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Prefix for the suite's own environment variables. Each service keeps its own
/// `MINICLOAK_*` / `MINIMAIL_*` / `MINIBUCKET_*` namespace untouched.
const ENV_PREFIX: &str = "MINISUITE_";

/// The starter file `minisuite init` writes. It lives in the crate because the
/// Docker build context only carries crates/; a test keeps the repository copy
/// at the root identical to it.
const TEMPLATE: &str = include_str!("../minisuite.example.toml");

pub const HELP: &str = "\
minisuite — minicloak + minimail + minibucket in one process

Usage: minisuite [options]
       minisuite init [PATH] [--force]

Commands:
  init [PATH]              write a commented starter minisuite.toml
                           (default ./minisuite.toml); --force overwrites it

Options:
  --config FILE            minisuite.toml to load (default ./minisuite.toml if present)
  --data DIR               state directory: minicloak key, minimail and minibucket
                           roots (default ./data)
  --bind-all               bind every service on 0.0.0.0 instead of 127.0.0.1
  --landing-bind ADDR      landing page address (default 127.0.0.1:9900)
  --no-landing             do not serve the landing page
  --only LIST              comma-separated subset of minicloak,minimail,minibucket
  -h, --help               show this help and exit
  -V, --version            print the version and exit

Config: minisuite.toml has a [minisuite] section (data, bind_all, landing_bind,
landing, only) and one section per service (minicloak, minimail, minibucket) whose
keys are that crate's flags. See `minisuite init` and minisuite.example.toml.

Precedence, highest first: flag > environment variable > minisuite.toml > default.
Suite options use MINISUITE_*, services their own MINICLOAK_*, MINIMAIL_*,
MINIBUCKET_* variables (--smtp-bind -> MINIMAIL_SMTP_BIND).
Relative paths in minisuite.toml resolve against that file's directory.

This is a development tool. Do not run it in production.";

// ---------------------------------------------------------------------------
// Services
// ---------------------------------------------------------------------------

/// The three servers the suite can run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Service {
    Minicloak,
    Minimail,
    Minibucket,
}

impl Service {
    /// Canonical order: the order services are started, bannered and listed.
    pub const ALL: [Service; 3] = [Service::Minicloak, Service::Minimail, Service::Minibucket];

    pub fn name(self) -> &'static str {
        match self {
            Service::Minicloak => minicloak::NAME,
            Service::Minimail => minimail::NAME,
            Service::Minibucket => minibucket::NAME,
        }
    }

    /// Prefix of the crate's own environment variables.
    fn env_prefix(self) -> &'static str {
        match self {
            Service::Minicloak => "MINICLOAK_",
            Service::Minimail => "MINIMAIL_",
            Service::Minibucket => "MINIBUCKET_",
        }
    }

    fn from_name(s: &str) -> Option<Service> {
        Service::ALL.into_iter().find(|svc| svc.name() == s)
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// What the binary was asked to do. `init` writes a file and never starts a server.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Run,
    Init { path: PathBuf, force: bool },
}

/// Which suite options a flag or `MINISUITE_*` variable set. The toml's
/// `[minisuite]` section only fills the options that are not set here.
#[derive(Clone, Debug, Default)]
struct Explicit {
    data: bool,
    bind_all: bool,
    landing: bool,
    only: bool,
}

/// Everything the suite's own CLI can set. Per-service settings are not in here:
/// they stay as argv until the owning crate parses them.
#[derive(Clone, Debug)]
pub struct Config {
    pub data: PathBuf,
    pub config_path: Option<PathBuf>,
    pub bind_all: bool,
    /// `None` means "not set yet": the toml may fill it, and when nothing does
    /// the address derives from `bind_all`, so `--bind-all` after
    /// `--landing-bind` cannot silently move an explicitly chosen address.
    pub landing_bind: Option<String>,
    pub landing: bool,
    pub only: Vec<Service>,
    pub command: Command,
    explicit: Explicit,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data: PathBuf::from("./data"),
            config_path: None,
            bind_all: false,
            landing_bind: None,
            landing: true,
            only: Service::ALL.to_vec(),
            command: Command::Run,
            explicit: Explicit::default(),
        }
    }
}

impl Config {
    /// The address the landing page listens on.
    pub fn landing_addr(&self) -> String {
        match &self.landing_bind {
            Some(a) => a.clone(),
            None if self.bind_all => "0.0.0.0:9900".to_string(),
            None => "127.0.0.1:9900".to_string(),
        }
    }
}

/// A reason to stop. `code` is what the binary should exit with: 0 for
/// `--help`/`--version` (the message is the text the user asked for), 2 for a
/// bad invocation, 1 for a server that could not start or did not stay up.
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

fn usage(msg: impl AsRef<str>) -> CliError {
    CliError {
        message: format!("{}: {}\nTry '{} --help'.", NAME, msg.as_ref(), NAME),
        code: 2,
    }
}

fn fatal(msg: impl AsRef<str>) -> CliError {
    CliError {
        message: format!("{}: {}", NAME, msg.as_ref()),
        code: 1,
    }
}

fn parse_bool(label: &str, v: &str) -> Result<bool, CliError> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(usage(format!(
            "{} expects 1|true|yes|on or 0|false|no|off, got {:?}",
            label, v
        ))),
    }
}

/// `--only minimail,minicloak` -> the two services, always in canonical order
/// so the banner and the landing page do not depend on how it was typed.
pub fn parse_only(label: &str, list: &str) -> Result<Vec<Service>, CliError> {
    let mut picked: Vec<Service> = Vec::new();
    for raw in list.split(',') {
        let name = raw.trim();
        if name.is_empty() {
            continue;
        }
        let svc = Service::from_name(name).ok_or_else(|| {
            usage(format!(
                "{}: unknown service {:?} (expected minicloak, minimail or minibucket)",
                label, name
            ))
        })?;
        if !picked.contains(&svc) {
            picked.push(svc);
        }
    }
    if picked.is_empty() {
        return Err(usage(format!("{} needs at least one service", label)));
    }
    Ok(Service::ALL
        .into_iter()
        .filter(|s| picked.contains(s))
        .collect())
}

/// Apply the `MINISUITE_*` variables. An empty variable is ignored, so
/// `MINISUITE_ONLY=` in a compose file means "no override" rather than "".
/// `env` is a parameter so precedence can be tested without the real environment.
fn apply_env(cfg: &mut Config, env: &dyn Fn(&str) -> Option<String>) -> Result<(), CliError> {
    let get = |flag: &str| -> Option<(String, String)> {
        let name = env_name(ENV_PREFIX, flag);
        match env(&name) {
            Some(v) if !v.is_empty() => Some((name, v)),
            _ => None,
        }
    };
    if let Some((_, v)) = get("--data") {
        cfg.data = PathBuf::from(v);
        cfg.explicit.data = true;
    }
    if let Some((_, v)) = get("--config") {
        cfg.config_path = Some(PathBuf::from(v));
    }
    if let Some((n, v)) = get("--bind-all") {
        cfg.bind_all = parse_bool(&n, &v)?;
        cfg.explicit.bind_all = true;
    }
    if let Some((_, v)) = get("--landing-bind") {
        cfg.landing_bind = Some(v);
    }
    if let Some((n, v)) = get("--no-landing") {
        cfg.landing = !parse_bool(&n, &v)?;
        cfg.explicit.landing = true;
    }
    if let Some((n, v)) = get("--only") {
        cfg.only = parse_only(&n, &v)?;
        cfg.explicit.only = true;
    }
    Ok(())
}

/// `--landing-bind` + `MINISUITE_` -> `MINISUITE_LANDING_BIND`. The same scheme
/// the three crates use, which is why it also derives their variables.
pub fn env_name(prefix: &str, flag: &str) -> String {
    format!(
        "{}{}",
        prefix,
        flag.trim_start_matches('-')
            .to_uppercase()
            .replace('-', "_")
    )
}

/// Build a [`Config`] from the environment and the command line.
///
/// `args` must NOT contain argv[0]. A leading `init` selects the scaffolding
/// command. Otherwise environment variables are applied first so an explicit
/// flag always wins. `--help`/`--version` come back as a [`CliError`] with
/// `code == 0` and the requested text in `message`.
pub fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<Config, CliError> {
    parse_with_env(args, &|name| std::env::var(name).ok())
}

/// [`parse_args`] with the environment injected.
fn parse_with_env<I: IntoIterator<Item = String>>(
    args: I,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Config, CliError> {
    let args: Vec<String> = args.into_iter().collect();
    if args.first().map(String::as_str) == Some("init") {
        return parse_init(&args[1..]);
    }

    let mut cfg = Config::default();
    apply_env(&mut cfg, env)?;

    let mut args = args.into_iter();
    while let Some(a) = args.next() {
        let mut value = |flag: &str| -> Result<String, CliError> {
            args.next()
                .ok_or_else(|| usage(format!("{} needs a value", flag)))
        };
        match a.as_str() {
            "-h" | "--help" => {
                return Err(CliError {
                    message: HELP.to_string(),
                    code: 0,
                })
            }
            "-V" | "--version" => {
                return Err(CliError {
                    message: format!("{} {}", NAME, VERSION),
                    code: 0,
                })
            }
            "--data" => {
                cfg.data = PathBuf::from(value("--data")?);
                cfg.explicit.data = true;
            }
            "--config" => cfg.config_path = Some(PathBuf::from(value("--config")?)),
            "--bind-all" => {
                cfg.bind_all = true;
                cfg.explicit.bind_all = true;
            }
            "--landing-bind" => cfg.landing_bind = Some(value("--landing-bind")?),
            "--no-landing" => {
                cfg.landing = false;
                cfg.explicit.landing = true;
            }
            "--only" => {
                cfg.only = parse_only("--only", &value("--only")?)?;
                cfg.explicit.only = true;
            }
            _ => return Err(usage(suggest::unknown_argument(&a, FLAGS))),
        }
    }
    Ok(cfg)
}

/// `minisuite init [PATH] [--force]`. The default target is ./minisuite.toml,
/// the file the suite picks up on its own.
fn parse_init(rest: &[String]) -> Result<Config, CliError> {
    let mut path: Option<PathBuf> = None;
    let mut force = false;
    for a in rest {
        match a.as_str() {
            "-h" | "--help" => {
                return Err(CliError {
                    message: HELP.to_string(),
                    code: 0,
                })
            }
            "--force" => force = true,
            flag if flag.starts_with('-') => {
                return Err(usage(format!("init: unknown argument: {}", flag)))
            }
            p => {
                if path.is_some() {
                    return Err(usage("init takes at most one PATH"));
                }
                path = Some(PathBuf::from(p));
            }
        }
    }
    Ok(Config {
        command: Command::Init {
            path: path.unwrap_or_else(|| PathBuf::from("minisuite.toml")),
            force,
        },
        ..Config::default()
    })
}

/// Write the starter file. Refuses to replace an existing file unless `force`.
/// `create_new` makes the existence check and the create a single step, so a
/// file that appears in between is not clobbered.
fn write_template(path: &Path, force: bool) -> Result<(), CliError> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .map_err(|e| fatal(format!("init: cannot create {}: {}", dir.display(), e)))?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true);
    if force {
        opts.create(true).truncate(true);
    } else {
        opts.create_new(true);
    }
    let mut file = opts.open(path).map_err(|e| {
        if e.kind() == io::ErrorKind::AlreadyExists {
            fatal(format!(
                "init: {} already exists (use --force to overwrite it)",
                path.display()
            ))
        } else {
            fatal(format!("init: cannot write {}: {}", path.display(), e))
        }
    })?;
    file.write_all(TEMPLATE.as_bytes())
        .map_err(|e| fatal(format!("init: cannot write {}: {}", path.display(), e)))?;

    println!("wrote {}", path.display());
    println!("edit it, then run `minisuite`");
    if path != Path::new("minisuite.toml") {
        println!(
            "(minisuite reads ./minisuite.toml by itself; use --config {} for this file)",
            path.display()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Suite defaults and precedence
// ---------------------------------------------------------------------------

/// One argument the suite would like to pass to a service unless something more
/// specific already covers it.
#[derive(Clone, Debug, PartialEq)]
pub struct SuiteDefault {
    pub flag: &'static str,
    pub value: String,
}

/// The arguments the suite adds on top of a crate's own defaults: the `--data`
/// layout, and — with `--bind-all` — a bind on every interface, since a
/// container that binds 127.0.0.1 is unreachable from the host.
pub fn suite_defaults(svc: Service, data: &Path, bind_all: bool) -> Vec<SuiteDefault> {
    let path = |sub: &str, file: Option<&str>| {
        let mut p = data.join(sub);
        if let Some(f) = file {
            p.push(f);
        }
        p.to_string_lossy().into_owned()
    };
    let mut out = Vec::new();
    let mut add = |flag: &'static str, value: String| out.push(SuiteDefault { flag, value });

    match svc {
        Service::Minicloak => {
            add("--key", path("minicloak", Some("key.pem")));
            if bind_all {
                add("--bind", "0.0.0.0:9500".into());
            }
        }
        Service::Minimail => {
            add("--root", path("minimail", None));
            if bind_all {
                add("--smtp-bind", "0.0.0.0:1025".into());
                add("--http-bind", "0.0.0.0:8025".into());
            }
        }
        Service::Minibucket => {
            add("--root", path("minibucket", None));
            if bind_all {
                add("--bind", "0.0.0.0:9000".into());
            }
        }
    }
    out
}

/// Assemble the argv for one service.
///
/// Precedence, highest first: the service's own environment, minisuite.toml,
/// the suite defaults, the crate defaults — the same order as for the suite's
/// own options. The crate's `parse_args` applies its environment before it reads
/// argv, so "env wins" is expressed by *omitting* every argument for a flag
/// whose variable is set; passing it would override the environment. A key in
/// the toml suppresses the matching suite default the same way.
///
/// `env_set` answers "is this service environment variable set?"; it is a
/// parameter so the rule can be tested without touching the real environment.
pub fn service_args(
    svc: Service,
    data: &Path,
    bind_all: bool,
    section: Option<&Section>,
    env_set: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    let mut argv = Vec::new();
    for d in suite_defaults(svc, data, bind_all) {
        if section.is_some_and(|s| s.sets(d.flag)) {
            continue;
        }
        if env_set(&env_name(svc.env_prefix(), d.flag)) {
            continue;
        }
        argv.push(d.flag.to_string());
        argv.push(d.value);
    }
    if let Some(s) = section {
        argv.extend(s.args_except(&|flag| env_set(&env_name(svc.env_prefix(), flag))));
    }
    argv
}

/// The real environment lookup: set and non-empty, matching how the three
/// crates themselves decide whether a variable counts.
fn env_is_set(name: &str) -> bool {
    matches!(std::env::var(name), Ok(v) if !v.is_empty())
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

/// A prepared server, type-erased just enough to keep them in one list. Boxed
/// because the three `Prepared` types differ a lot in size.
enum Ready {
    Minicloak(Box<minicloak::Prepared>),
    Minimail(Box<minimail::Prepared>),
    Minibucket(Box<minibucket::Prepared>),
}

impl Ready {
    fn banner(&self) -> String {
        match self {
            Ready::Minicloak(p) => p.banner(),
            Ready::Minimail(p) => p.banner(),
            Ready::Minibucket(p) => p.banner(),
        }
    }

    /// Credentials and the `.env` snippet for the landing page. Read from the
    /// prepared server, because users, keys and seeded defaults are only
    /// resolved while preparing.
    fn card(&self, links: &[Link]) -> landing::Card {
        let port = links.first().map_or(0, |l| l.port);
        match self {
            Ready::Minicloak(p) => landing::minicloak_card(p, port),
            Ready::Minimail(p) => landing::minimail_card(p),
            Ready::Minibucket(p) => landing::minibucket_card(p, port),
        }
    }

    fn serve(self) -> io::Result<()> {
        match self {
            Ready::Minicloak(p) => p.serve(),
            Ready::Minimail(p) => p.serve(),
            Ready::Minibucket(p) => p.serve(),
        }
    }
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

/// Suite-level warning for `--bind-all`: every service is now reachable from
/// the network while (unless overridden) still running with its dev defaults.
/// Each service also warns on its own; this one lists the whole picture.
const BIND_ALL_WARNING: &str = "\
WARNING: --bind-all: every service and the landing page listen on 0.0.0.0 and are
  reachable from other hosts. Unless you configured otherwise they still use the
  dev defaults:
  - minicloak: users alice/alice, bob/bob, client myapp/s3cret, and quick-login
    (password-less sign-in). MINICLOAK_USER / MINICLOAK_CLIENT / MINICLOAK_NO_QUICK_LOGIN=1
  - minimail:  minimail/minimail for SMTP AUTH and the web UI. MINIMAIL_USER / MINIMAIL_PASSWORD
  - minibucket: access key minioadmin, secret minioadmin. MINIBUCKET_ACCESS_KEY / MINIBUCKET_SECRET_KEY
  Drop --bind-all (or set MINISUITE_BIND_ALL=0) unless every host on this network is trusted.
";

/// The one-line replacement for `BIND_ALL_WARNING` inside a container, where
/// `--bind-all` is the default and publishing ports is the safeguard.
const BIND_ALL_NOTE: &str = "note: every service and the landing page listen on 0.0.0.0 inside a container; publish ports on 127.0.0.1 (e.g. -p 127.0.0.1:9900:9900) unless the network is trusted\n";

/// The container images set `MINISUITE_CONTAINER`; see the minicloak crate.
fn in_container() -> bool {
    container_flag(std::env::var("MINISUITE_CONTAINER").ok().as_deref())
}

fn container_flag(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

fn bind_all_notice(in_container: bool) -> &'static str {
    if in_container {
        BIND_ALL_NOTE
    } else {
        BIND_ALL_WARNING
    }
}

/// The port of a `host:port` bind string. Falls back to 0, which only shows up
/// in a link if a bind string was malformed — and in that case the bind itself
/// has already failed.
fn port_of(addr: &str) -> u16 {
    addr.rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(0)
}

/// Load minisuite.toml, if there is one. An explicit `--config` that does not
/// exist is an error; the implicit `./minisuite.toml` simply may not be there.
fn load_file(cfg: &Config) -> Result<conf::ConfigFile, CliError> {
    let known: [(&str, &[&str]); 3] = [
        (minicloak::NAME, minicloak::FLAGS),
        (minimail::NAME, minimail::FLAGS),
        (minibucket::NAME, minibucket::FLAGS),
    ];
    let (path, required) = match &cfg.config_path {
        Some(p) => (p.clone(), true),
        None => (PathBuf::from("minisuite.toml"), false),
    };
    if !required && !path.exists() {
        return Ok(conf::ConfigFile::default());
    }
    conf::parse_file(&path, &known).map_err(usage)
}

/// The configured socket a bind failure is about: a service flag, or the landing
/// page. Carries the flag so the hint can name the env var and the toml key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BindOwner {
    Service { svc: Service, flag: &'static str },
    Landing,
}

impl BindOwner {
    fn env_name(self) -> String {
        match self {
            BindOwner::Service { svc, flag } => env_name(svc.env_prefix(), flag),
            BindOwner::Landing => env_name(ENV_PREFIX, "--landing-bind"),
        }
    }
}

/// Each socket a startup will bind, with the configured owner and address.
type Binds = Vec<(BindOwner, String)>;

/// The `host:port` bind strings a service will try to bind, read back through
/// the crate's own parser so the suite never has to know the flag layout twice.
/// Empty when the argv does not parse; `plan` reports that error anyway.
fn service_binds(svc: Service, argv: &[String]) -> Binds {
    let argv = argv.to_vec();
    let flag = |flag: &'static str, addr: String| (BindOwner::Service { svc, flag }, addr);
    match svc {
        Service::Minicloak => minicloak::parse_args(argv)
            .map(|c| vec![flag("--bind", c.bind)])
            .unwrap_or_default(),
        Service::Minimail => minimail::parse_args(argv)
            .map(|c| {
                vec![
                    flag("--smtp-bind", c.smtp_bind),
                    flag("--http-bind", c.http_bind),
                ]
            })
            .unwrap_or_default(),
        Service::Minibucket => minibucket::parse_args(argv)
            .map(|c| vec![flag("--bind", c.bind)])
            .unwrap_or_default(),
    }
}

/// Ask the OS whether `addr` can be bound right now. Used after a failed start
/// rather than trusting the crate's error: minicloak and minibucket flatten the
/// io error into a string, so its kind is gone by the time it reaches us.
fn probe_bind(addr: &str) -> Option<io::ErrorKind> {
    std::net::TcpListener::bind(addr).err().map(|e| e.kind())
}

/// The first of `binds` that cannot be bound right now, with the reason.
fn find_conflict(binds: &Binds) -> Option<(BindOwner, &str, io::ErrorKind)> {
    binds
        .iter()
        .find_map(|(owner, addr)| probe_bind(addr).map(|kind| (*owner, addr.as_str(), kind)))
}

/// The message for a service that failed to start, with a hint when one of its
/// sockets is the reason.
fn startup_failure(name: &str, e: &io::Error, binds: &Binds) -> String {
    let mut msg = format!("{}: {}", name, e);
    if let Some((owner, addr, kind)) = find_conflict(binds) {
        if let Some(hint) = bind_hint(owner, addr, kind) {
            msg.push('\n');
            msg.push_str(&hint);
        }
    }
    msg
}

/// A port one above or below `port`, whichever exists: the example the hint
/// suggests, far enough away to be obviously a different listener.
fn suggested_port(port: u16) -> u16 {
    port.checked_add(100).unwrap_or(port.saturating_sub(100))
}

/// Explain a failed bind of `addr` by `owner`, with the ways out. `None` for
/// kinds that a hint would not help with; the raw io error already says enough.
fn bind_hint(owner: BindOwner, addr: &str, kind: io::ErrorKind) -> Option<String> {
    let port = port_of(addr);
    let host = addr.rsplit_once(':').map_or("127.0.0.1", |(h, _)| h);
    let env = owner.env_name();
    let suggested = format!("{}:{}", host, suggested_port(port));

    match kind {
        io::ErrorKind::AddrInUse => {
            let mut lines = vec![format!(
                "    Port {} is already taken — is another minisuite (or a container) running?",
                port
            )];
            match owner {
                BindOwner::Service { svc, flag } => {
                    let key = flag.trim_start_matches('-');
                    lines.push(format!(
                        "    Free it, or move {} elsewhere: {}={}, or in minisuite.toml:",
                        svc.name(),
                        env,
                        suggested
                    ));
                    lines.push(format!("      [{}]", svc.name()));
                    lines.push(format!("      {} = \"{}\"", key, suggested));
                    let others: Vec<&str> = Service::ALL
                        .iter()
                        .filter(|&&s| s != svc)
                        .map(|s| s.name())
                        .collect();
                    lines.push(format!(
                        "    Run only some services with --only {}.",
                        others.join(",")
                    ));
                }
                BindOwner::Landing => {
                    lines.push(format!(
                        "    Free it, or move the landing page elsewhere: {}={},",
                        env, suggested
                    ));
                    lines.push("    or pass --no-landing to start without it.".to_string());
                }
            }
            Some(lines.join("\n"))
        }
        io::ErrorKind::AddrNotAvailable => Some(format!(
            "    {} is not an address this machine has. Bind 127.0.0.1 for local use,\n    \
             or 0.0.0.0 for all interfaces: {}=127.0.0.1:{}",
            addr, env, port
        )),
        io::ErrorKind::PermissionDenied => Some(format!(
            "    Ports below 1024 need elevated privileges. Pick a port from 1024 up,\n    \
             for example {}={}:8080",
            env, host
        )),
        _ => None,
    }
}

/// Fill every suite option that no flag or `MINISUITE_*` variable set. The
/// `data` path was already resolved against the file's directory by `conf`.
fn apply_file(cfg: &mut Config, file: &conf::ConfigFile) -> Result<(), CliError> {
    let Some(suite) = &file.suite else {
        return Ok(());
    };
    if !cfg.explicit.data {
        if let Some(d) = &suite.data {
            cfg.data = d.clone();
        }
    }
    if !cfg.explicit.bind_all {
        if let Some(b) = suite.bind_all {
            cfg.bind_all = b;
        }
    }
    if cfg.landing_bind.is_none() {
        cfg.landing_bind = suite.landing_bind.clone();
    }
    if !cfg.explicit.landing {
        if let Some(l) = suite.landing {
            cfg.landing = l;
        }
    }
    if !cfg.explicit.only {
        if let Some(names) = &suite.only {
            cfg.only = parse_only("[minisuite] only", &names.join(","))?;
        }
    }
    Ok(())
}

/// Parse, prepare and run everything. Returns only on failure; the error is
/// already prefixed with `minisuite:` and, where one is to blame, the service.
pub fn run(mut cfg: Config) -> Result<(), CliError> {
    if let Command::Init { path, force } = &cfg.command {
        return write_template(path, *force);
    }
    // The toml is found from --config / MINISUITE_CONFIG alone, so it can be
    // read before the data directory and bind settings it may supply are used.
    let file = load_file(&cfg)?;
    apply_file(&mut cfg, &file)?;
    let sections = file.services;
    let section_for =
        |svc: Service| -> Option<&Section> { sections.iter().find(|s| s.name == svc.name()) };

    // Every directory first: on the scratch image only /data is writable and
    // there is no shell to mkdir -p, so the launcher has to do it.
    std::fs::create_dir_all(&cfg.data)
        .map_err(|e| fatal(format!("data dir {}: {}", cfg.data.display(), e)))?;
    for svc in &cfg.only {
        let dir = cfg.data.join(svc.name());
        std::fs::create_dir_all(&dir)
            .map_err(|e| fatal(format!("{}: {}: {}", svc.name(), dir.display(), e)))?;
    }

    // Parse every service's argv before preparing any of them, so a typo in the
    // toml is reported as a usage error instead of after two sockets are bound.
    let mut plans: Vec<(Service, Plan, Binds)> = Vec::new();
    for &svc in &cfg.only {
        let argv = service_args(svc, &cfg.data, cfg.bind_all, section_for(svc), &|n| {
            env_is_set(n)
        });
        let binds = service_binds(svc, &argv);
        plans.push((svc, plan(svc, argv)?, binds));
    }

    // Bind and load everything up front: if any service cannot start, none does.
    let mut running: Vec<(Service, Ready, Vec<Link>)> = Vec::new();
    for (svc, p, binds) in plans {
        let links = p.links.clone();
        let ready = p
            .prepare()
            .map_err(|e| fatal(startup_failure(svc.name(), &e, &binds)))?;
        running.push((svc, ready, links));
    }

    let listener = if cfg.landing {
        let addr = cfg.landing_addr();
        Some(std::net::TcpListener::bind(&addr).map_err(|e| {
            let hint = bind_hint(BindOwner::Landing, &addr, e.kind())
                .map(|h| format!("\n{}", h))
                .unwrap_or_default();
            fatal(format!("landing: bind {}: {}{}", addr, e, hint))
        })?)
    } else {
        None
    };

    eprint!("{}", banner(&cfg, &running));
    if cfg.bind_all {
        eprint!("\n{}", bind_all_notice(in_container()));
    }

    let entries: Vec<Entry> = running
        .iter()
        .map(|(svc, ready, links)| Entry {
            name: svc.name(),
            banner: ready.banner(),
            links: links.clone(),
            card: ready.card(links),
        })
        .collect();
    let entries = Arc::new(entries);

    // One thread per server. Nothing here ever exits the process: a thread that
    // stops reports why on this channel and `main` turns that into an exit code.
    let (tx, rx) = mpsc::channel::<String>();
    for (svc, ready, _) in running {
        let tx = tx.clone();
        let name = svc.name();
        thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                let msg = match ready.serve() {
                    Ok(()) => format!("{}: {}: stopped accepting connections", NAME, name),
                    Err(e) => format!("{}: {}: {}", NAME, name, e),
                };
                let _ = tx.send(msg);
            })
            .map_err(|e| fatal(format!("{}: cannot spawn thread: {}", name, e)))?;
    }
    if let Some(listener) = listener {
        let tx = tx.clone();
        let entries = Arc::clone(&entries);
        thread::Builder::new()
            .name("landing".to_string())
            .spawn(move || {
                let msg = match landing::serve(listener, entries) {
                    Ok(()) => format!("{}: landing: stopped accepting connections", NAME),
                    Err(e) => format!("{}: landing: {}", NAME, e),
                };
                let _ = tx.send(msg);
            })
            .map_err(|e| fatal(format!("landing: cannot spawn thread: {}", e)))?;
    }
    // Dropping our own sender makes `recv` fail once every server thread is
    // gone, instead of parking the process forever with nothing left running.
    drop(tx);

    let message = rx
        .recv()
        .unwrap_or_else(|_| format!("{}: every server stopped", NAME));
    Err(CliError { message, code: 1 })
}

/// A service whose argv has been accepted by its own crate: the links its ports
/// imply are already known, all that is left is the fallible `prepare`.
struct Plan {
    links: Vec<Link>,
    prepare: Box<dyn FnOnce() -> io::Result<Ready> + Send>,
}

impl Plan {
    fn prepare(self) -> io::Result<Ready> {
        (self.prepare)()
    }
}

/// Hand the argv to the owning crate. Its `CliError` is passed through as-is —
/// including `--help`, which a service section has no business asking for, but
/// which is still better reported by the crate that would print it.
fn plan(svc: Service, argv: Vec<String>) -> Result<Plan, CliError> {
    let blame = |e: minicloak::CliError| CliError {
        message: e.message,
        code: if e.code == 0 { 2 } else { e.code },
    };
    match svc {
        Service::Minicloak => {
            let c = minicloak::parse_args(argv).map_err(blame)?;
            let links = vec![Link {
                label: "OIDC discovery",
                port: port_of(&c.bind),
                path: format!("/realms/{}/.well-known/openid-configuration", c.realm),
            }];
            Ok(Plan {
                links,
                prepare: Box::new(move || {
                    minicloak::prepare(c).map(|p| Ready::Minicloak(Box::new(p)))
                }),
            })
        }
        Service::Minimail => {
            let c = minimail::parse_args(argv).map_err(|e| CliError {
                message: e.message,
                code: if e.code == 0 { 2 } else { e.code },
            })?;
            let links = vec![Link {
                label: "web UI + JSON API",
                port: port_of(&c.http_bind),
                path: "/".to_string(),
            }];
            Ok(Plan {
                links,
                prepare: Box::new(move || {
                    minimail::prepare(c).map(|p| Ready::Minimail(Box::new(p)))
                }),
            })
        }
        Service::Minibucket => {
            let c = minibucket::parse_args(argv).map_err(|e| CliError {
                message: e.message,
                code: if e.code == 0 { 2 } else { e.code },
            })?;
            let links = vec![Link {
                label: "S3 endpoint",
                port: port_of(&c.bind),
                path: "/".to_string(),
            }];
            Ok(Plan {
                links,
                prepare: Box::new(move || {
                    minibucket::prepare(c).map(|p| Ready::Minibucket(Box::new(p)))
                }),
            })
        }
    }
}

/// The combined startup banner: the suite's own summary, then every service's
/// banner verbatim, indented so it is obvious which server printed what. Each
/// service block starts with its own `name version` header.
fn banner(cfg: &Config, running: &[(Service, Ready, Vec<Link>)]) -> String {
    let mut rows: Vec<(&str, String)> = Vec::new();
    if cfg.landing {
        let addr = cfg.landing_addr();
        // A 0.0.0.0 link is not clickable; show the loopback address that is.
        let shown = if addr.starts_with("0.0.0.0") || addr.starts_with("[::]") {
            format!("127.0.0.1:{}", port_of(&addr))
        } else {
            addr
        };
        rows.push(("landing", format!("http://{}", shown)));
    } else {
        rows.push(("landing", "disabled (--no-landing)".to_string()));
    }
    rows.push(("data", cfg.data.display().to_string()));

    let mut s = render_banner(&format!("{} {}", NAME, VERSION), &rows);
    s.push_str("  This is a development tool. Do not run it in production.\n");
    for (_, ready, _) in running {
        s.push('\n');
        for line in ready.banner().lines() {
            s.push_str("  ");
            s.push_str(line);
            s.push('\n');
        }
    }
    s
}

/// The startup summary: a `name version` header, then `label  value` rows
/// aligned to the widest label.
fn render_banner(header: &str, rows: &[(&str, String)]) -> String {
    use std::fmt::Write as _;
    let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    let mut s = format!("{header}\n");
    for (label, value) in rows {
        // Infallible: writing into a String never fails.
        let _ = writeln!(s, "  {label:<width$}  {value}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn no_env(_: &str) -> bool {
        false
    }

    #[test]
    fn loopback_binds_are_recognised() {
        for a in [
            "127.0.0.1:9900",
            "127.1.2.3:1",
            "[::1]:9900",
            "::1",
            "localhost:9900",
            "LOCALHOST:1",
        ] {
            assert!(is_loopback_bind(a), "{}", a);
        }
        for a in [
            "0.0.0.0:9900",
            "[::]:9900",
            "192.168.1.10:9900",
            "example.com:1",
            "",
        ] {
            assert!(!is_loopback_bind(a), "{}", a);
        }
    }

    #[test]
    fn bind_all_notice_is_one_line_in_a_container() {
        assert_eq!(bind_all_notice(true), BIND_ALL_NOTE);
        assert_eq!(bind_all_notice(false), BIND_ALL_WARNING);
        assert_eq!(BIND_ALL_NOTE.lines().count(), 1);
        assert!(BIND_ALL_NOTE.contains("-p 127.0.0.1:9900:9900"));
        assert!(!BIND_ALL_NOTE.contains("WARNING"));
    }

    #[test]
    fn container_flag_accepts_only_the_truthy_spellings() {
        for v in ["1", "true", "YES", " on "] {
            assert!(container_flag(Some(v)), "{}", v);
        }
        for v in ["", "0", "false", "no", "off", "2"] {
            assert!(!container_flag(Some(v)), "{}", v);
        }
        assert!(!container_flag(None));
    }

    #[test]
    fn suite_banner_states_the_development_warning_once() {
        let cfg = parse_args(args(&[])).unwrap();
        let b = banner(&cfg, &[]);
        assert!(b.starts_with(&format!("{} {}\n", NAME, VERSION)), "{}", b);
        assert_eq!(b.matches("development tool").count(), 1, "{}", b);
        assert!(b.contains("  landing  http://127.0.0.1:9900\n"), "{}", b);
    }

    #[test]
    fn bind_all_warning_names_every_dev_default() {
        for needle in [
            "0.0.0.0",
            "alice/alice",
            "minimail/minimail",
            "minioadmin",
            "MINICLOAK_NO_QUICK_LOGIN",
        ] {
            assert!(BIND_ALL_WARNING.contains(needle), "{}", needle);
        }
    }

    #[test]
    fn defaults_match_the_help_text() {
        let c = parse_args(args(&[])).unwrap();
        assert_eq!(c.data, PathBuf::from("./data"));
        assert!(c.landing && !c.bind_all);
        assert_eq!(c.landing_addr(), "127.0.0.1:9900");
        assert_eq!(c.only, Service::ALL.to_vec());
    }

    #[test]
    fn bind_all_moves_the_landing_page_but_an_explicit_bind_wins() {
        let c = parse_args(args(&["--bind-all"])).unwrap();
        assert_eq!(c.landing_addr(), "0.0.0.0:9900");
        let c = parse_args(args(&["--bind-all", "--landing-bind", "127.0.0.1:1"])).unwrap();
        assert_eq!(c.landing_addr(), "127.0.0.1:1");
        let c = parse_args(args(&["--landing-bind", "127.0.0.1:1", "--bind-all"])).unwrap();
        assert_eq!(c.landing_addr(), "127.0.0.1:1");
    }

    #[test]
    fn help_and_version_are_code_zero() {
        let e = parse_args(args(&["--help"])).unwrap_err();
        assert_eq!(e.code, 0);
        assert!(e.message.starts_with("minisuite —"));
        let e = parse_args(args(&["-V"])).unwrap_err();
        assert_eq!(e.code, 0);
        assert_eq!(e.message, format!("minisuite {}", VERSION));
    }

    #[test]
    fn bad_input_is_code_two() {
        for bad in [
            vec!["--nope"],
            vec!["--data"],
            vec!["--only"],
            vec!["--only", "minimail,nope"],
            vec!["--only", " , "],
        ] {
            let e = parse_args(args(&bad)).unwrap_err();
            assert_eq!(e.code, 2, "{:?}", bad);
            assert!(e.message.starts_with("minisuite: "), "{}", e.message);
        }
    }

    #[test]
    fn only_is_deduped_and_normalised_to_canonical_order() {
        let c = parse_args(args(&["--only", "minibucket, minicloak ,minibucket"])).unwrap();
        assert_eq!(c.only, vec![Service::Minicloak, Service::Minibucket]);
        let c = parse_args(args(&["--only", "minimail"])).unwrap();
        assert_eq!(c.only, vec![Service::Minimail]);
    }

    #[test]
    fn env_name_scheme_matches_the_crates() {
        assert_eq!(
            env_name(ENV_PREFIX, "--landing-bind"),
            "MINISUITE_LANDING_BIND"
        );
        assert_eq!(env_name("MINIMAIL_", "--smtp-bind"), "MINIMAIL_SMTP_BIND");
        assert_eq!(env_name("MINIBUCKET_", "--bind"), "MINIBUCKET_BIND");
        assert_eq!(env_name("MINICLOAK_", "--key"), "MINICLOAK_KEY");
    }

    #[test]
    fn suite_defaults_lay_out_the_data_directory() {
        let d = Path::new("/d");
        let mc = suite_defaults(Service::Minicloak, d, false);
        assert_eq!(mc.len(), 1);
        assert_eq!(mc[0].flag, "--key");
        assert!(mc[0].value.ends_with("key.pem"));
        assert!(mc[0].value.contains("minicloak"));

        let mm = suite_defaults(Service::Minimail, d, false);
        assert_eq!(mm.len(), 1);
        assert_eq!(mm[0].flag, "--root");
        assert!(mm[0].value.ends_with("minimail"));
    }

    #[test]
    fn bind_all_adds_a_bind_per_listener() {
        let d = Path::new("/d");
        let flags: Vec<&str> = suite_defaults(Service::Minimail, d, true)
            .iter()
            .map(|x| x.flag)
            .collect();
        assert_eq!(flags, vec!["--root", "--smtp-bind", "--http-bind"]);
        let flags: Vec<&str> = suite_defaults(Service::Minibucket, d, true)
            .iter()
            .map(|x| x.flag)
            .collect();
        assert_eq!(flags, vec!["--root", "--bind"]);
    }

    #[test]
    fn a_service_env_var_suppresses_the_matching_suite_default() {
        let d = Path::new("/d");
        // Nothing set: --bind-all supplies both minimail binds.
        let a = service_args(Service::Minimail, d, true, None, &no_env);
        assert!(a.contains(&"--smtp-bind".to_string()));
        assert!(a.contains(&"--http-bind".to_string()));

        // MINIMAIL_SMTP_BIND set: the suite must NOT pass --smtp-bind, or argv
        // would beat the environment the user explicitly set.
        let a = service_args(Service::Minimail, d, true, None, &|n| {
            n == "MINIMAIL_SMTP_BIND"
        });
        assert!(!a.contains(&"--smtp-bind".to_string()), "{:?}", a);
        assert!(a.contains(&"--http-bind".to_string()), "{:?}", a);
        assert!(a.contains(&"--root".to_string()), "{:?}", a);

        // The root default goes away just as readily.
        let a = service_args(Service::Minimail, d, false, None, &|n| n == "MINIMAIL_ROOT");
        assert!(a.is_empty(), "{:?}", a);
    }

    fn section(text: &str) -> Section {
        let tables = minicloak::toml::parse(text).unwrap();
        conf::section_from_table(&tables[0], None).unwrap()
    }

    #[test]
    fn the_toml_beats_the_suite_default() {
        let d = Path::new("/d");
        let s = section("[minibucket]\nbind = \"127.0.0.1:9321\"\n");
        let a = service_args(Service::Minibucket, d, true, Some(&s), &|_| false);
        assert_eq!(a.iter().filter(|x| *x == "--bind").count(), 1, "{:?}", a);
        assert!(
            a.windows(2).any(|w| w == ["--bind", "127.0.0.1:9321"]),
            "{:?}",
            a
        );
    }

    #[test]
    fn the_service_env_beats_the_toml() {
        let d = Path::new("/d");
        let s = section(
            "[minicloak]\nbind = \"127.0.0.1:9321\"\nuser = [\"a=a\", \"b=b\"]\nrealm = \"x\"\n",
        );
        // Every argument of an env-covered flag goes, including all repeats;
        // the crate then reads the variable itself.
        let a = service_args(Service::Minicloak, d, true, Some(&s), &|n| {
            n == "MINICLOAK_BIND" || n == "MINICLOAK_USER"
        });
        assert!(!a.iter().any(|x| x == "--bind" || x == "--user"), "{:?}", a);
        assert!(a.windows(2).any(|w| w == ["--realm", "x"]), "{:?}", a);
    }

    #[test]
    fn a_toml_key_suppresses_the_suite_default_even_when_it_is_false() {
        let d = Path::new("/d");
        let s = section("[minibucket]\nroot = \"/elsewhere\"\n");
        let a = service_args(Service::Minibucket, d, false, Some(&s), &no_env);
        assert_eq!(a, vec!["--root", "/elsewhere"]);
    }

    #[test]
    fn suite_defaults_come_before_the_toml_arguments() {
        let d = Path::new("/d");
        let s = section("[minicloak]\nrealm = \"other\"\n");
        let a = service_args(Service::Minicloak, d, false, Some(&s), &no_env);
        assert_eq!(a[0], "--key");
        assert_eq!(&a[2..], ["--realm", "other"]);
    }

    #[test]
    fn ports_come_from_the_bind_string() {
        assert_eq!(port_of("127.0.0.1:9500"), 9500);
        assert_eq!(port_of("0.0.0.0:1025"), 1025);
        assert_eq!(port_of("[::1]:8025"), 8025);
        assert_eq!(port_of("nonsense"), 0);
    }

    #[test]
    fn a_service_flag_error_is_reported_by_the_crate_and_stays_code_two() {
        let Err(e) = plan(Service::Minimail, args(&["--not-a-flag"])) else {
            panic!("expected a usage error");
        };
        assert_eq!(e.code, 2);
        assert!(e.message.contains("minimail"), "{}", e.message);
        // --help inside a section is a mistake, not a request to print help.
        let Err(e) = plan(Service::Minicloak, args(&["--help"])) else {
            panic!("expected a usage error");
        };
        assert_eq!(e.code, 2);
    }

    #[test]
    fn a_service_plan_knows_its_links() {
        let p = plan(Service::Minicloak, args(&["--bind", "0.0.0.0:9501"])).unwrap();
        assert_eq!(p.links[0].port, 9501);
        assert!(p.links[0].path.starts_with("/realms/dev/"));
        let p = plan(Service::Minimail, args(&["--http-bind", "0.0.0.0:18025"])).unwrap();
        assert_eq!(p.links[0].port, 18025);
    }

    #[test]
    fn an_unknown_toml_section_is_a_usage_error() {
        let dir = std::env::temp_dir().join("minisuite-test-unknown-section");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("minisuite.toml");
        std::fs::write(&file, "[minimails]\nroot = \"x\"\n").unwrap();
        let cfg = Config {
            config_path: Some(file),
            ..Config::default()
        };
        let e = load_file(&cfg).unwrap_err();
        assert_eq!(e.code, 2);
        assert!(e.message.contains("unknown section"), "{}", e.message);
    }

    fn svc_owner(svc: Service, flag: &'static str) -> BindOwner {
        BindOwner::Service { svc, flag }
    }

    #[test]
    fn address_in_use_hint_for_minicloak_names_env_toml_and_the_others() {
        let hint = bind_hint(
            svc_owner(Service::Minicloak, "--bind"),
            "127.0.0.1:9500",
            io::ErrorKind::AddrInUse,
        )
        .unwrap();
        assert!(hint.contains("Port 9500 is already taken"), "{}", hint);
        assert!(hint.contains("MINICLOAK_BIND=127.0.0.1:9600"), "{}", hint);
        assert!(hint.contains("      [minicloak]\n"), "{}", hint);
        assert!(hint.contains("      bind = \"127.0.0.1:9600\""), "{}", hint);
        assert!(hint.ends_with("--only minimail,minibucket."), "{}", hint);
    }

    #[test]
    fn address_in_use_hint_for_minimail_names_the_failing_flag() {
        let smtp = bind_hint(
            svc_owner(Service::Minimail, "--smtp-bind"),
            "127.0.0.1:1025",
            io::ErrorKind::AddrInUse,
        )
        .unwrap();
        assert!(smtp.contains("Port 1025 is already taken"), "{}", smtp);
        assert!(
            smtp.contains("MINIMAIL_SMTP_BIND=127.0.0.1:1125"),
            "{}",
            smtp
        );
        assert!(
            smtp.contains("      smtp-bind = \"127.0.0.1:1125\""),
            "{}",
            smtp
        );
        assert!(smtp.contains("--only minicloak,minibucket."), "{}", smtp);

        let http = bind_hint(
            svc_owner(Service::Minimail, "--http-bind"),
            "127.0.0.1:8025",
            io::ErrorKind::AddrInUse,
        )
        .unwrap();
        assert!(
            http.contains("MINIMAIL_HTTP_BIND=127.0.0.1:8125"),
            "{}",
            http
        );
        assert!(
            http.contains("      http-bind = \"127.0.0.1:8125\""),
            "{}",
            http
        );
    }

    #[test]
    fn address_in_use_hint_for_minibucket_uses_its_own_env_var() {
        let hint = bind_hint(
            svc_owner(Service::Minibucket, "--bind"),
            "0.0.0.0:9000",
            io::ErrorKind::AddrInUse,
        )
        .unwrap();
        assert!(hint.contains("MINIBUCKET_BIND=0.0.0.0:9100"), "{}", hint);
        assert!(hint.contains("--only minicloak,minimail."), "{}", hint);
    }

    #[test]
    fn address_in_use_hint_for_landing_points_at_its_flag_and_no_landing() {
        let hint = bind_hint(
            BindOwner::Landing,
            "127.0.0.1:9900",
            io::ErrorKind::AddrInUse,
        )
        .unwrap();
        assert!(
            hint.contains("MINISUITE_LANDING_BIND=127.0.0.1:10000"),
            "{}",
            hint
        );
        assert!(hint.contains("--no-landing"), "{}", hint);
        assert!(!hint.contains("--only"), "{}", hint);
        assert!(!hint.contains("minisuite.toml"), "{}", hint);
    }

    #[test]
    fn other_failures_get_a_hint_only_where_one_helps() {
        let svc = svc_owner(Service::Minicloak, "--bind");
        assert!(bind_hint(svc, "127.0.0.1:9500", io::ErrorKind::Other).is_none());
        let na = bind_hint(svc, "10.9.9.9:9500", io::ErrorKind::AddrNotAvailable).unwrap();
        assert!(na.contains("10.9.9.9:9500 is not an address"), "{}", na);
        assert!(na.contains("MINICLOAK_BIND=127.0.0.1:9500"), "{}", na);
        let denied = bind_hint(svc, "127.0.0.1:80", io::ErrorKind::PermissionDenied).unwrap();
        assert!(denied.contains("below 1024"), "{}", denied);
    }

    #[test]
    fn suggested_port_stays_in_range_at_the_top() {
        assert_eq!(suggested_port(9500), 9600);
        assert_eq!(suggested_port(65500), 65400);
    }

    #[test]
    fn a_real_second_bind_is_address_in_use_and_gets_a_hint() {
        // Hold a port, then try to bind it again: the kind the hint matches on
        // must be what the OS reports for a taken port.
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = held.local_addr().unwrap().to_string();
        let second = std::net::TcpListener::bind(&addr).unwrap_err();
        assert_eq!(second.kind(), io::ErrorKind::AddrInUse);
        assert_eq!(probe_bind(&addr), Some(io::ErrorKind::AddrInUse));

        let hint = bind_hint(
            svc_owner(Service::Minicloak, "--bind"),
            &addr,
            second.kind(),
        )
        .unwrap();
        assert!(hint.contains(&format!("Port {} is already taken", port_of(&addr))));
    }

    #[test]
    fn find_conflict_picks_the_bind_that_is_taken() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = held.local_addr().unwrap().to_string();
        // Free the second address by binding and dropping it, so it is available.
        let free = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .to_string();
        let binds = vec![
            (svc_owner(Service::Minimail, "--smtp-bind"), free),
            (svc_owner(Service::Minimail, "--http-bind"), taken.clone()),
        ];
        let (owner, addr, kind) = find_conflict(&binds).unwrap();
        assert_eq!(owner, svc_owner(Service::Minimail, "--http-bind"));
        assert_eq!(addr, taken);
        assert_eq!(kind, io::ErrorKind::AddrInUse);
    }

    #[test]
    fn startup_failure_appends_the_hint_for_a_conflicting_bind() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = held.local_addr().unwrap().to_string();
        let binds = vec![(svc_owner(Service::Minicloak, "--bind"), addr.clone())];
        let e = io::Error::other(format!("bind {}: Address already in use", addr));
        let msg = startup_failure("minicloak", &e, &binds);
        assert!(msg.starts_with("minicloak: bind 127.0.0.1:"), "{}", msg);
        assert!(msg.contains("\n    Port "), "{}", msg);
        assert!(msg.contains("MINICLOAK_BIND="), "{}", msg);
    }

    #[test]
    fn startup_failure_without_a_conflict_is_the_plain_message() {
        let binds = vec![(
            svc_owner(Service::Minicloak, "--bind"),
            "127.0.0.1:0".to_string(),
        )];
        let e = io::Error::other("key: permission denied");
        assert_eq!(
            startup_failure("minicloak", &e, &binds),
            "minicloak: key: permission denied"
        );
    }

    #[test]
    fn service_binds_come_from_the_crate_parser() {
        let argv = args(&[
            "--smtp-bind",
            "127.0.0.1:2525",
            "--http-bind",
            "127.0.0.1:8026",
        ]);
        let binds = service_binds(Service::Minimail, &argv);
        assert_eq!(
            binds,
            vec![
                (
                    svc_owner(Service::Minimail, "--smtp-bind"),
                    "127.0.0.1:2525".to_string()
                ),
                (
                    svc_owner(Service::Minimail, "--http-bind"),
                    "127.0.0.1:8026".to_string()
                ),
            ]
        );
        assert!(service_binds(Service::Minicloak, &args(&["--bad-flag"])).is_empty());
    }

    #[test]
    fn every_documented_flag_is_listed() {
        for line in HELP.lines() {
            let Some(rest) = line.strip_prefix("  ") else {
                continue;
            };
            let Some(flag) = rest.split_whitespace().next() else {
                continue;
            };
            if !flag.starts_with("--") || flag == "--help" || flag == "--version" {
                continue;
            }
            assert!(FLAGS.contains(&flag), "{} is in HELP but not FLAGS", flag);
        }
    }

    #[test]
    fn a_misspelled_suite_flag_gets_a_hint() {
        let e = parse_args(args(&["--landing-bnd", "127.0.0.1:1"])).unwrap_err();
        assert_eq!(e.code, 2);
        assert!(
            e.message.starts_with(
                "minisuite: unknown argument: --landing-bnd (did you mean --landing-bind?)\n"
            ),
            "{}",
            e.message
        );
    }

    fn scratch(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "minisuite-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The startup pipeline (environment, arguments, file) with a fake
    /// environment. The toml is written into a fresh directory and found through
    /// MINISUITE_CONFIG, so the real environment and working directory stay out.
    fn load_with(tag: &str, cli: &[&str], env: &[(&str, &str)], toml: &str) -> (Config, PathBuf) {
        let dir = scratch(tag);
        let file = dir.join("minisuite.toml");
        std::fs::write(&file, toml).unwrap();
        let mut vars: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        vars.push((
            "MINISUITE_CONFIG".to_string(),
            file.to_string_lossy().into_owned(),
        ));
        let env_fn = |name: &str| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        let mut cfg = parse_with_env(args(cli), &env_fn).unwrap();
        let loaded = load_file(&cfg).unwrap();
        apply_file(&mut cfg, &loaded).unwrap();
        (cfg, dir)
    }

    #[test]
    fn the_minisuite_section_supplies_the_suite_options() {
        let (cfg, dir) = load_with(
            "toml-keys",
            &[],
            &[],
            "[minisuite]\ndata = \"state\"\nbind_all = true\nlanding_bind = \"127.0.0.1:9911\"\nlanding = false\nonly = [\"minimail\"]\n",
        );
        assert_eq!(cfg.data, dir.join("state"));
        assert!(cfg.bind_all);
        assert_eq!(cfg.landing_addr(), "127.0.0.1:9911");
        assert!(!cfg.landing);
        assert_eq!(cfg.only, vec![Service::Minimail]);
    }

    #[test]
    fn without_a_minisuite_section_the_defaults_stand() {
        let (cfg, _) = load_with("no-section", &[], &[], "[minibucket]\nroot = \"x\"\n");
        assert_eq!(cfg.data, PathBuf::from("./data"));
        assert!(cfg.landing && !cfg.bind_all);
        assert_eq!(cfg.only, Service::ALL.to_vec());
    }

    #[test]
    fn an_env_variable_beats_the_toml() {
        let (cfg, _) = load_with(
            "env-beats-toml",
            &[],
            &[
                ("MINISUITE_DATA", "/from-env"),
                ("MINISUITE_BIND_ALL", "0"),
                ("MINISUITE_ONLY", "minicloak"),
                ("MINISUITE_LANDING_BIND", "127.0.0.1:7"),
                ("MINISUITE_NO_LANDING", "1"),
            ],
            "[minisuite]\ndata = \"state\"\nbind_all = true\nonly = [\"minimail\"]\nlanding = true\n",
        );
        assert_eq!(cfg.data, PathBuf::from("/from-env"));
        assert!(!cfg.bind_all);
        assert_eq!(cfg.only, vec![Service::Minicloak]);
        assert_eq!(cfg.landing_addr(), "127.0.0.1:7");
        assert!(!cfg.landing);
    }

    #[test]
    fn a_flag_beats_the_env_and_the_toml() {
        let (cfg, _) = load_with(
            "cli-beats-all",
            &[
                "--data",
                "/from-cli",
                "--only",
                "minibucket",
                "--bind-all",
                "--landing-bind",
                "127.0.0.1:8",
                "--no-landing",
            ],
            &[
                ("MINISUITE_DATA", "/from-env"),
                ("MINISUITE_ONLY", "minicloak"),
                ("MINISUITE_BIND_ALL", "0"),
                ("MINISUITE_LANDING_BIND", "127.0.0.1:7"),
            ],
            "[minisuite]\ndata = \"state\"\nonly = [\"minimail\"]\nbind_all = false\nlanding = true\n",
        );
        assert_eq!(cfg.data, PathBuf::from("/from-cli"));
        assert_eq!(cfg.only, vec![Service::Minibucket]);
        assert!(cfg.bind_all);
        assert_eq!(cfg.landing_addr(), "127.0.0.1:8");
        assert!(!cfg.landing);
    }

    #[test]
    fn a_suite_only_list_with_an_unknown_service_is_a_usage_error() {
        let file = conf::ConfigFile {
            services: Vec::new(),
            suite: Some(conf::SuiteSettings {
                only: Some(vec!["nope".to_string()]),
                ..conf::SuiteSettings::default()
            }),
        };
        let mut cfg = Config::default();
        let e = apply_file(&mut cfg, &file).unwrap_err();
        assert_eq!(e.code, 2);
        assert!(e.message.contains("unknown service"), "{}", e.message);
    }

    #[test]
    fn init_defaults_to_the_file_the_suite_reads() {
        let c = parse_args(args(&["init"])).unwrap();
        assert_eq!(
            c.command,
            Command::Init {
                path: PathBuf::from("minisuite.toml"),
                force: false
            }
        );
    }

    #[test]
    fn init_takes_a_path_and_force_in_either_order() {
        let want = Command::Init {
            path: PathBuf::from("/x/y.toml"),
            force: true,
        };
        let c = parse_args(args(&["init", "/x/y.toml", "--force"])).unwrap();
        assert_eq!(c.command, want);
        let c = parse_args(args(&["init", "--force", "/x/y.toml"])).unwrap();
        assert_eq!(c.command, want);
    }

    #[test]
    fn init_rejects_stray_arguments_and_only_leads_the_command_line() {
        for bad in [vec!["init", "--data", "x"], vec!["init", "a", "b"]] {
            let e = parse_args(args(&bad)).unwrap_err();
            assert_eq!(e.code, 2, "{:?}", bad);
        }
        let e = parse_args(args(&["--data", "x", "init"])).unwrap_err();
        assert_eq!(e.code, 2);
        assert!(e.message.contains("unknown argument"), "{}", e.message);
    }

    #[test]
    fn init_refuses_to_overwrite_unless_forced() {
        let path = scratch("init-overwrite").join("minisuite.toml");
        write_template(&path, false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), TEMPLATE);

        std::fs::write(&path, "mine").unwrap();
        let e = write_template(&path, false).unwrap_err();
        assert_eq!(e.code, 1);
        assert!(e.message.contains("already exists"), "{}", e.message);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mine");

        write_template(&path, true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), TEMPLATE);
    }

    #[test]
    fn init_creates_missing_parent_directories() {
        let path = scratch("init-nested").join("a").join("minisuite.toml");
        write_template(&path, false).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn the_starter_template_is_a_valid_config_with_every_section() {
        let path = scratch("template-valid").join("minisuite.toml");
        std::fs::write(&path, TEMPLATE).unwrap();
        let cfg = Config {
            config_path: Some(path),
            ..Config::default()
        };
        let file = load_file(&cfg).unwrap();
        assert_eq!(file.services.len(), 3);
        assert!(file.suite.is_some());
    }

    #[test]
    fn the_repository_example_matches_the_crate_template() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../minisuite.example.toml");
        let repo =
            std::fs::read_to_string(&root).unwrap_or_else(|e| panic!("{}: {}", root.display(), e));
        assert_eq!(
            repo, TEMPLATE,
            "minisuite.example.toml (root) and crates/minisuite/minisuite.example.toml differ"
        );
    }
}
