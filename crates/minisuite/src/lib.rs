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

use std::io;
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

pub const HELP: &str = "\
minisuite — minicloak + minimail + minibucket in one process

Usage: minisuite [options]
  --data DIR               state directory, default ./data
                           (minicloak key -> DIR/minicloak/key.pem,
                            minimail root -> DIR/minimail,
                            minibucket root -> DIR/minibucket)
  --config FILE            minisuite.toml; default ./minisuite.toml if present
  --bind-all               bind every service on 0.0.0.0 instead of 127.0.0.1
  --landing-bind ADDR      default 127.0.0.1:9900 (0.0.0.0:9900 with --bind-all)
  --no-landing             do not serve the landing page
  --only LIST              comma-separated subset of minicloak,minimail,minibucket
  -h, --help               show this help and exit
  -V, --version            print the version and exit

Every flag has an environment twin: MINISUITE_ + the flag name in upper case
with dashes turned into underscores (--landing-bind -> MINISUITE_LANDING_BIND).
Booleans take 1|true|yes|on or 0|false|no|off. A flag beats its env var.

Services are configured exactly as they are on their own command line. In
minisuite.toml each [section] is one service and each key is that crate's flag
without the leading dashes (underscores or dashes, both work):

  [minicloak]
  bind = \"127.0.0.1:9500\"          # key = \"value\"  ->  --key value
  user = [\"alice=alice\"]           # an array       ->  the flag, repeated
  [minimail]
  anonymous = true                 # true           ->  the bare flag
                                   # false          ->  nothing at all

Per service, highest priority first:
  1. minisuite.toml
  2. the service's own environment (MINIMAIL_SMTP_BIND, MINIBUCKET_BIND, ...)
  3. minisuite's defaults: the --data layout and, with --bind-all, the binds
  4. the crate's own defaults
A suite default is skipped whenever the toml sets that key or the matching
service env var is set, so `MINIMAIL_SMTP_BIND=0.0.0.0:2525 minisuite --bind-all`
does what it looks like. Relative paths in minisuite.toml resolve against the
directory of the toml file.

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

/// Everything the suite's own CLI can set. Per-service settings are not in here:
/// they stay as argv until the owning crate parses them.
#[derive(Clone, Debug)]
pub struct Config {
    pub data: PathBuf,
    pub config_path: Option<PathBuf>,
    pub bind_all: bool,
    /// `None` means "derive from `bind_all`", so `--bind-all` after
    /// `--landing-bind` cannot silently move an explicitly chosen address.
    pub landing_bind: Option<String>,
    pub landing: bool,
    pub only: Vec<Service>,
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
fn apply_env(cfg: &mut Config) -> Result<(), CliError> {
    let get = |flag: &str| -> Option<(String, String)> {
        let name = env_name(ENV_PREFIX, flag);
        match std::env::var(&name) {
            Ok(v) if !v.is_empty() => Some((name, v)),
            _ => None,
        }
    };
    if let Some((_, v)) = get("--data") {
        cfg.data = PathBuf::from(v);
    }
    if let Some((_, v)) = get("--config") {
        cfg.config_path = Some(PathBuf::from(v));
    }
    if let Some((n, v)) = get("--bind-all") {
        cfg.bind_all = parse_bool(&n, &v)?;
    }
    if let Some((_, v)) = get("--landing-bind") {
        cfg.landing_bind = Some(v);
    }
    if let Some((n, v)) = get("--no-landing") {
        cfg.landing = !parse_bool(&n, &v)?;
    }
    if let Some((n, v)) = get("--only") {
        cfg.only = parse_only(&n, &v)?;
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
/// `args` must NOT contain argv[0]. Environment variables are applied first so
/// an explicit flag always wins. `--help`/`--version` come back as a [`CliError`]
/// with `code == 0` and the requested text in `message`.
pub fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<Config, CliError> {
    let mut cfg = Config::default();
    apply_env(&mut cfg)?;

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
            "--data" => cfg.data = PathBuf::from(value("--data")?),
            "--config" => cfg.config_path = Some(PathBuf::from(value("--config")?)),
            "--bind-all" => cfg.bind_all = true,
            "--landing-bind" => cfg.landing_bind = Some(value("--landing-bind")?),
            "--no-landing" => cfg.landing = false,
            "--only" => cfg.only = parse_only("--only", &value("--only")?)?,
            _ => return Err(usage(format!("unknown argument: {}", a))),
        }
    }
    Ok(cfg)
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
/// Precedence, highest first: minisuite.toml, the service's own environment,
/// the suite defaults, the crate defaults. The crate's `parse_args` already
/// applies its environment before it reads argv, so "env beats a suite default"
/// is expressed by *omitting* that default — passing it would win, which is
/// exactly what we do not want. A key present in the toml suppresses the
/// default the same way, and its own argument is appended last so it also beats
/// the environment.
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
        argv.extend(s.args.iter().cloned());
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

    fn serve(self) -> io::Result<()> {
        match self {
            Ready::Minicloak(p) => p.serve(),
            Ready::Minimail(p) => p.serve(),
            Ready::Minibucket(p) => p.serve(),
        }
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
fn load_sections(cfg: &Config) -> Result<Vec<Section>, CliError> {
    let names: Vec<&str> = Service::ALL.iter().map(|s| s.name()).collect();
    let (path, required) = match &cfg.config_path {
        Some(p) => (p.clone(), true),
        None => (PathBuf::from("minisuite.toml"), false),
    };
    if !required && !path.exists() {
        return Ok(Vec::new());
    }
    conf::parse_file(&path, &names).map_err(usage)
}

/// Parse, prepare and run everything. Returns only on failure; the error is
/// already prefixed with `minisuite:` and, where one is to blame, the service.
pub fn run(cfg: Config) -> Result<(), CliError> {
    let sections = load_sections(&cfg)?;
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
    let mut plans: Vec<(Service, Plan)> = Vec::new();
    for &svc in &cfg.only {
        let argv = service_args(svc, &cfg.data, cfg.bind_all, section_for(svc), &|n| {
            env_is_set(n)
        });
        plans.push((svc, plan(svc, argv)?));
    }

    // Bind and load everything up front: if any service cannot start, none does.
    let mut running: Vec<(Service, Ready, Vec<Link>)> = Vec::new();
    for (svc, p) in plans {
        let links = p.links.clone();
        let ready = p
            .prepare()
            .map_err(|e| fatal(format!("{}: {}", svc.name(), e)))?;
        running.push((svc, ready, links));
    }

    let listener = if cfg.landing {
        let addr = cfg.landing_addr();
        Some(
            std::net::TcpListener::bind(&addr)
                .map_err(|e| fatal(format!("landing: bind {}: {}", addr, e)))?,
        )
    } else {
        None
    };

    eprint!("{}", banner(&cfg, &running));

    let entries: Vec<Entry> = running
        .iter()
        .map(|(svc, ready, links)| Entry {
            name: svc.name(),
            banner: ready.banner(),
            links: links.clone(),
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

/// The combined startup banner: the suite's own two lines, then every service's
/// banner verbatim, indented so it is obvious which server printed what.
fn banner(cfg: &Config, running: &[(Service, Ready, Vec<Link>)]) -> String {
    let mut s = format!("{} {}\n", NAME, VERSION);
    s.push_str(&format!("  data     {}\n", cfg.data.display()));
    if cfg.landing {
        let addr = cfg.landing_addr();
        // A 0.0.0.0 link is not clickable; show the loopback address that is.
        let shown = if addr.starts_with("0.0.0.0") || addr.starts_with("[::]") {
            format!("127.0.0.1:{}", port_of(&addr))
        } else {
            addr
        };
        s.push_str(&format!("  landing  http://{}\n", shown));
    } else {
        s.push_str("  landing  disabled (--no-landing)\n");
    }
    for (svc, ready, _) in running {
        s.push_str(&format!("\n{}\n", svc.name()));
        for line in ready.banner().lines() {
            s.push_str("  ");
            s.push_str(line);
            s.push('\n');
        }
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
    fn the_toml_beats_both_the_env_and_the_suite_default() {
        let d = Path::new("/d");
        let s = section("[minibucket]\nbind = \"127.0.0.1:9321\"\n");
        // Env set as well: the toml still wins, because the default is skipped
        // and the toml's own argument is appended (and argv beats the env).
        let a = service_args(Service::Minibucket, d, true, Some(&s), &|n| {
            n == "MINIBUCKET_BIND"
        });
        assert_eq!(a.iter().filter(|x| *x == "--bind").count(), 1, "{:?}", a);
        assert!(
            a.windows(2).any(|w| w == ["--bind", "127.0.0.1:9321"]),
            "{:?}",
            a
        );
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
        let e = load_sections(&cfg).unwrap_err();
        assert_eq!(e.code, 2);
        assert!(e.message.contains("unknown section"), "{}", e.message);
    }
}
