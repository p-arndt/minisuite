// The landing page on :9900 — one page that tells you where everything is.
//
// A deliberately minimal HTTP/1.0-style server: read the request head, write
// one response, close. There is no keep-alive, no chunking and no routing table
// beyond three cases, because this exists only so a developer opening
// http://localhost:9900 can find every endpoint, dev credential and the .env
// lines an app needs without grepping a terminal for the banner.
//
// Every URL and host is built from the *request's* Host header rather than from
// the bind address, so the page works unchanged whether it is reached on
// localhost, on a LAN IP, or through a published container port on another
// machine. That is why values are kept as `Value`s until render time.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

/// One clickable entry under a service heading.
#[derive(Clone)]
pub struct Link {
    pub label: &'static str,
    pub port: u16,
    /// Path (and query) appended to `http://<host>:<port>`.
    pub path: String,
}

/// A setting value. Anything derived from the request's host stays symbolic
/// until render time, so one card serves every way the page can be reached.
#[derive(Clone)]
pub enum Value {
    /// Shown as is.
    Text(String),
    /// `http://<host>:<port><path>`, rendered as a link.
    Http { port: u16, path: String },
    /// `<host>:<port>`.
    Socket { port: u16 },
    /// The request's host, without scheme or port.
    Host,
}

impl Value {
    fn render(&self, host: &str) -> String {
        match self {
            Value::Text(s) => s.clone(),
            Value::Http { port, path } => format!("http://{}:{}{}", host, port, path),
            Value::Socket { port } => format!("{}:{}", host, port),
            Value::Host => host.to_string(),
        }
    }
}

/// One labelled row of a card. In a `.env` snippet the label is the variable
/// name.
#[derive(Clone)]
pub struct Field {
    pub label: String,
    pub value: Value,
}

impl Field {
    pub fn new(label: &str, value: Value) -> Self {
        Self {
            label: label.to_string(),
            value,
        }
    }
}

/// What a service shows beyond its links: its credentials and settings, the
/// `.env` snippet for an app, and remarks the snippet cannot express.
#[derive(Default)]
pub struct Card {
    pub fields: Vec<Field>,
    pub env: Vec<Field>,
    pub notes: Vec<String>,
}

/// A running service as the landing page sees it.
pub struct Entry {
    pub name: &'static str,
    pub banner: String,
    pub links: Vec<Link>,
    pub card: Card,
}

/// Accept loop. Only returns if the listener itself dies.
pub fn serve(listener: TcpListener, entries: Arc<Vec<Entry>>) -> std::io::Result<()> {
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let entries = Arc::clone(&entries);
                std::thread::spawn(move || {
                    if let Err(e) = handle(s, &entries) {
                        eprintln!("[landing] {}", e);
                    }
                });
            }
            Err(e) => eprintln!("[landing accept] {}", e),
        }
    }
    Ok(())
}

fn handle(stream: TcpStream, entries: &[Entry]) -> std::io::Result<()> {
    let _ = stream.set_nodelay(true);
    let mut out = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    // Only the target matters; the method is checked for GET/HEAD below.
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();

    let mut host: Option<String> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.split_once(':') {
            if v.0.eq_ignore_ascii_case("host") {
                host = Some(v.1.trim().to_string());
            }
        }
    }
    // A request body is neither expected nor read: the three routes below are
    // GET/HEAD only, and the response closes the connection anyway.
    let path = target.split(['?', '#']).next().unwrap_or("/");
    let host = host_name(host.as_deref());

    let (status, ctype, body) = match (method.as_str(), path) {
        ("GET" | "HEAD", "/") => ("200 OK", "text/html; charset=utf-8", page(&host, entries)),
        ("GET" | "HEAD", "/healthz") => ("200 OK", "application/json", healthz(entries)),
        _ => (
            "404 Not Found",
            "text/plain; charset=utf-8",
            "not found\n".to_string(),
        ),
    };

    write!(
        out,
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        status,
        ctype,
        body.len()
    )?;
    if method != "HEAD" {
        out.write_all(body.as_bytes())?;
    }
    out.flush()
}

/// The host part of a `Host:` header, without the port, ready to be pasted
/// between `http://` and `:<port>`.
///
/// `example.com:9900` -> `example.com`, `[::1]:9900` -> `[::1]`. A missing or
/// empty header (HTTP/1.0 clients, raw netcat) falls back to `127.0.0.1`, which
/// is the only host we can guess that is more useful than nothing. `0.0.0.0` is
/// a bind address, never a reachable one, so it is rewritten too.
pub fn host_name(header: Option<&str>) -> String {
    let raw = header.unwrap_or("").trim();
    let host = if let Some(rest) = raw.strip_prefix('[') {
        // IPv6 literal: keep the brackets, drop anything after them.
        match rest.split_once(']') {
            Some((inner, _)) => format!("[{}]", inner),
            None => String::new(),
        }
    } else {
        raw.split(':').next().unwrap_or("").to_string()
    };
    match host.as_str() {
        "" | "0.0.0.0" | "[::]" => "127.0.0.1".to_string(),
        _ => host,
    }
}

fn healthz(entries: &[Entry]) -> String {
    let names: Vec<String> = entries.iter().map(|e| format!("{:?}", e.name)).collect();
    format!("{{\"status\":\"ok\",\"services\":[{}]}}", names.join(","))
}

/// Escape the five characters that can break out of text or an attribute. The
/// banners are our own output, but they contain user-supplied names.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

const CSS: &str = "\
:root { color-scheme: light dark; --fg:#1b1b1f; --bg:#fbfbfd; --dim:#5b5b66; --line:#e2e2e8; --card:#fff; --link:#0b57d0 }
@media (prefers-color-scheme: dark) {
  :root { --fg:#e6e6ea; --bg:#141418; --dim:#9a9aa6; --line:#2b2b33; --card:#1c1c22; --link:#8ab4f8 }
}
* { box-sizing: border-box }
body { margin:0; padding:2.5rem 1.25rem; background:var(--bg); color:var(--fg);
       font:15px/1.55 ui-sans-serif,system-ui,-apple-system,Segoe UI,Roboto,sans-serif }
main { max-width:56rem; margin:0 auto }
h1 { font-size:1.6rem; margin:0 0 .25rem }
p.sub { color:var(--dim); margin:0 0 2rem }
section { background:var(--card); border:1px solid var(--line); border-radius:10px;
          padding:1rem 1.25rem; margin:0 0 1rem }
h2 { font-size:1.05rem; margin:0 0 .6rem; letter-spacing:.01em }
ul { list-style:none; margin:0 0 .8rem; padding:0 }
li { margin:.15rem 0 }
li span { display:inline-block; min-width:11rem; color:var(--dim) }
a { color:var(--link) }
pre { margin:0; padding:.7rem .8rem; overflow-x:auto; border-radius:7px;
      background:color-mix(in srgb, var(--fg) 6%, transparent);
      font:12.5px/1.5 ui-monospace,SFMono-Regular,Consolas,monospace; color:var(--dim) }
footer { color:var(--dim); font-size:12.5px; margin-top:2rem }
.env { margin:.8rem 0 0 }
.env-head { display:flex; justify-content:space-between; align-items:center;
            margin:0 0 .35rem; color:var(--dim); font-size:13px }
.env pre { color:var(--fg) }
button.copy { font:inherit; font-size:12.5px; color:var(--link); background:none;
              border:1px solid var(--line); border-radius:6px; padding:.1rem .6rem; cursor:pointer }
.note { color:var(--dim); font-size:13px; margin:.45rem 0 0 }
details { margin:.8rem 0 0 }
summary { color:var(--dim); font-size:13px; cursor:pointer }
details pre { margin-top:.5rem }";

/// Copies a snippet with the async clipboard API. That API only exists in
/// secure contexts, so a page opened over plain http on a LAN address falls back
/// to selecting the text for the user to copy with the keyboard.
const COPY_SCRIPT: &str = "\
(function () {
  function select(pre) {
    var range = document.createRange();
    range.selectNodeContents(pre);
    var sel = window.getSelection();
    sel.removeAllRanges();
    sel.addRange(range);
  }
  function flash(btn, label) {
    btn.textContent = label;
    setTimeout(function () { btn.textContent = 'Copy'; }, 1500);
  }
  document.addEventListener('click', function (ev) {
    var btn = ev.target.closest('button[data-copy]');
    var pre = btn && document.getElementById(btn.getAttribute('data-copy'));
    if (!pre) return;
    if (!navigator.clipboard || !window.isSecureContext) {
      select(pre);
      flash(btn, 'Selected');
      return;
    }
    navigator.clipboard.writeText(pre.textContent).then(
      function () { flash(btn, 'Copied'); },
      function () { select(pre); flash(btn, 'Selected'); });
  });
})();";

/// Render the whole page for one request's host.
pub fn page(host: &str, entries: &[Entry]) -> String {
    let mut s = String::new();
    s.push_str("<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\n");
    s.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n");
    s.push_str("<title>minisuite</title>\n<style>\n");
    s.push_str(CSS);
    s.push_str("\n</style></head><body><main>\n");
    s.push_str(&format!(
        "<h1>minisuite <small style=\"font-weight:400;color:var(--dim)\">{}</small></h1>\n",
        esc(crate::VERSION)
    ));
    s.push_str("<p class=\"sub\">A local development backend. Do not expose it to a network you do not trust. \
Copy a service's .env block into your app.</p>\n");

    for e in entries {
        section(&mut s, host, e);
    }

    s.push_str("<footer>GET /healthz for a machine-readable status.</footer>\n");
    s.push_str("<script>\n");
    s.push_str(COPY_SCRIPT);
    s.push_str("\n</script>\n");
    s.push_str("</main></body></html>\n");
    s
}

fn section(s: &mut String, host: &str, e: &Entry) {
    s.push_str(&format!("<section><h2>{}</h2>\n<ul>\n", esc(e.name)));
    for l in &e.links {
        let url = format!("http://{}:{}{}", host, l.port, l.path);
        s.push_str(&format!(
            "<li><span>{}</span>{}</li>\n",
            esc(l.label),
            anchor(&url)
        ));
    }
    for f in &e.card.fields {
        let shown = match &f.value {
            Value::Http { .. } => anchor(&f.value.render(host)),
            other => esc(&other.render(host)),
        };
        s.push_str(&format!(
            "<li><span>{}</span>{}</li>\n",
            esc(&f.label),
            shown
        ));
    }
    s.push_str("</ul>\n");

    if !e.card.env.is_empty() {
        let id = format!("env-{}", e.name);
        s.push_str(&format!(
            "<div class=\"env\">\n<div class=\"env-head\"><span>.env for your app</span>\
<button type=\"button\" class=\"copy\" data-copy=\"{}\">Copy</button></div>\n\
<pre id=\"{}\">{}</pre>\n",
            esc(&id),
            esc(&id),
            esc(&env_snippet(host, &e.card.env))
        ));
        for n in &e.card.notes {
            s.push_str(&format!("<p class=\"note\">{}</p>\n", esc(n)));
        }
        s.push_str("</div>\n");
    }

    s.push_str(&format!(
        "<details><summary>Startup log</summary>\n<pre>{}</pre></details>\n</section>\n",
        esc(&e.banner)
    ));
}

fn anchor(url: &str) -> String {
    let u = esc(url);
    format!("<a href=\"{}\">{}</a>", u, u)
}

fn env_snippet(host: &str, env: &[Field]) -> String {
    env.iter()
        .map(|f| env_line(&f.label, &f.value.render(host)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One `KEY=value` line in dotenv syntax. A value outside a conservative
/// character set is double-quoted, with the characters that would end or
/// escape the quotes (and a newline) written as escapes. A password like
/// `pa ss"1` must not split into two variables.
fn env_line(key: &str, value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@%+,=[]".contains(c));
    if plain {
        format!("{}={}", key, value)
    } else {
        let quoted = value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n");
        format!("{}=\"{}\"", key, quoted)
    }
}

/// minicloak: realm, issuer, users and clients, and the OIDC variables an app
/// needs. The issuer follows the request's host unless `--issuer` pins it.
pub fn minicloak_card(p: &minicloak::Prepared, port: u16) -> Card {
    let issuer = match p.issuer_override() {
        Some(iss) => Value::Text(iss.to_string()),
        None => Value::Http {
            port,
            path: format!("/realms/{}", p.realm()),
        },
    };
    let mut fields = vec![
        Field::new("realm", Value::Text(p.realm().to_string())),
        Field::new("issuer", issuer.clone()),
    ];
    for u in p.users() {
        fields.push(Field::new(
            "user",
            Value::Text(format!("{} (password {})", u.username, u.password)),
        ));
    }
    let clients = p.clients();
    for c in &clients {
        let auth = match &c.secret {
            Some(secret) => format!("secret {}", secret),
            None => "public, PKCE required".to_string(),
        };
        let redirects = if c.redirect_uris.is_empty() {
            "no redirect URI".to_string()
        } else {
            format!("redirect {}", c.redirect_uris.join(", "))
        };
        fields.push(Field::new(
            "client",
            Value::Text(format!("{} ({}), {}", c.id, auth, redirects)),
        ));
    }

    let mut env = vec![Field::new("OIDC_ISSUER", issuer)];
    let mut notes = Vec::new();
    if let Some(c) = clients.iter().find(|c| c.secret.is_some()) {
        env.push(Field::new("OIDC_CLIENT_ID", Value::Text(c.id.clone())));
        if let Some(secret) = &c.secret {
            env.push(Field::new(
                "OIDC_CLIENT_SECRET",
                Value::Text(secret.clone()),
            ));
        }
    } else if let Some(c) = clients.first() {
        env.push(Field::new("OIDC_CLIENT_ID", Value::Text(c.id.clone())));
        notes.push(format!(
            "{} is a public client without a secret: the app must use PKCE.",
            c.id
        ));
    } else {
        notes.push(
            "No clients are configured; add one with --client or [clients.<id>].".to_string(),
        );
    }
    Card { fields, env, notes }
}

/// minimail: the SMTP endpoint and, unless anonymous, the accounts and the
/// SMTP_USER/SMTP_PASSWORD pair an app logs in with.
pub fn minimail_card(p: &minimail::Prepared) -> Card {
    let smtp_port = p.smtp_local_addr().map_or(0, |a| a.port());
    let mut fields = vec![Field::new("SMTP", Value::Socket { port: smtp_port })];
    let mut env = vec![
        Field::new("SMTP_HOST", Value::Host),
        Field::new("SMTP_PORT", Value::Text(smtp_port.to_string())),
    ];
    let mut notes = Vec::new();
    if p.auth_required() {
        let accounts = p.credentials();
        for (user, password) in &accounts {
            fields.push(Field::new(
                "user",
                Value::Text(format!("{} (password {})", user, password)),
            ));
        }
        if let Some((user, password)) = accounts.first() {
            env.push(Field::new("SMTP_USER", Value::Text(user.clone())));
            env.push(Field::new("SMTP_PASSWORD", Value::Text(password.clone())));
        }
    } else {
        fields.push(Field::new(
            "auth",
            Value::Text("anonymous, no login needed".to_string()),
        ));
        notes.push("Auth is disabled: SMTP_USER and SMTP_PASSWORD are not needed.".to_string());
    }
    Card { fields, env, notes }
}

/// minibucket: the S3 endpoint, region and access keys, plus a note on
/// addressing style. minibucket only serves virtual-hosted requests when it was
/// started with `--domain`.
pub fn minibucket_card(p: &minibucket::Prepared, port: u16) -> Card {
    let mut fields = vec![Field::new("region", Value::Text(p.region().to_string()))];
    let mut env = vec![
        Field::new(
            "AWS_ENDPOINT_URL",
            Value::Http {
                port,
                path: String::new(),
            },
        ),
        Field::new("AWS_REGION", Value::Text(p.region().to_string())),
    ];
    let mut notes = Vec::new();
    if p.anonymous() {
        fields.push(Field::new(
            "auth",
            Value::Text("anonymous, request signatures are not checked".to_string()),
        ));
        notes.push(
            "Auth is disabled: AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY are optional."
                .to_string(),
        );
    } else {
        let keys = p.credentials();
        for (access, secret) in &keys {
            fields.push(Field::new(
                "access key",
                Value::Text(format!("{} (secret {})", access, secret)),
            ));
        }
        if let Some((access, secret)) = keys.first() {
            env.push(Field::new("AWS_ACCESS_KEY_ID", Value::Text(access.clone())));
            env.push(Field::new(
                "AWS_SECRET_ACCESS_KEY",
                Value::Text(secret.clone()),
            ));
        }
    }
    if p.domain().is_none() {
        notes.push(
            "Path-style addressing (http://host:port/bucket/key) is required: configure the \
client for it, e.g. addressing_style = path in boto3, forcePathStyle: true in the \
AWS SDK for JavaScript."
                .to_string(),
        );
    }
    Card { fields, env, notes }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn entries() -> Vec<Entry> {
        entries_with_password("alice")
    }

    fn entries_with_password(password: &str) -> Vec<Entry> {
        vec![Entry {
            name: "minicloak",
            banner: "users: <alice> & bob\n".to_string(),
            links: vec![Link {
                label: "discovery",
                port: 9500,
                path: "/realms/dev/.well-known/openid-configuration".to_string(),
            }],
            card: Card {
                fields: vec![Field::new(
                    "user",
                    Value::Text(format!("alice (password {})", password)),
                )],
                env: vec![
                    Field::new(
                        "OIDC_ISSUER",
                        Value::Http {
                            port: 9500,
                            path: "/realms/dev".to_string(),
                        },
                    ),
                    Field::new("OIDC_CLIENT_ID", Value::Text("myapp".to_string())),
                    Field::new("OIDC_CLIENT_SECRET", Value::Text("s3cret".to_string())),
                ],
                notes: vec!["Notes with <markup> & more.".to_string()],
            },
        }]
    }

    /// A directory for a test's real server state, removed by the caller.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("minisuite-landing-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// `label value` for every field, rendered for `box.local`.
    fn field_rows(card: &Card) -> Vec<String> {
        card.fields
            .iter()
            .map(|f| format!("{} {}", f.label, f.value.render("box.local")))
            .collect()
    }

    fn env_text(card: &Card) -> String {
        env_snippet("box.local", &card.env)
    }

    #[test]
    fn host_header_loses_its_port() {
        assert_eq!(host_name(Some("localhost:9900")), "localhost");
        assert_eq!(host_name(Some("example.com")), "example.com");
        assert_eq!(host_name(Some("  10.0.0.5:9900 ")), "10.0.0.5");
    }

    #[test]
    fn ipv6_literals_keep_their_brackets() {
        assert_eq!(host_name(Some("[::1]:9900")), "[::1]");
        assert_eq!(host_name(Some("[fe80::1]")), "[fe80::1]");
    }

    #[test]
    fn unusable_hosts_fall_back_to_loopback() {
        assert_eq!(host_name(None), "127.0.0.1");
        assert_eq!(host_name(Some("")), "127.0.0.1");
        assert_eq!(host_name(Some("0.0.0.0:9900")), "127.0.0.1");
        assert_eq!(host_name(Some("[::]:9900")), "127.0.0.1");
    }

    #[test]
    fn links_are_built_from_the_request_host() {
        let html = page(&host_name(Some("box.local:9900")), &entries());
        assert!(
            html.contains("http://box.local:9500/realms/dev/.well-known/openid-configuration"),
            "{}",
            html
        );
    }

    #[test]
    fn banners_are_html_escaped() {
        let html = page("127.0.0.1", &entries());
        assert!(html.contains("&lt;alice&gt; &amp; bob"));
        assert!(!html.contains("<alice>"));
    }

    #[test]
    fn healthz_lists_every_running_service() {
        assert_eq!(
            healthz(&entries()),
            "{\"status\":\"ok\",\"services\":[\"minicloak\"]}"
        );
    }

    #[test]
    fn page_shows_credentials_and_env_snippet_for_the_request_host() {
        let html = page(&host_name(Some("box.local:9900")), &entries());
        assert!(
            html.contains("<span>user</span>alice (password alice)"),
            "{}",
            html
        );
        assert!(
            html.contains(
                "OIDC_ISSUER=http://box.local:9500/realms/dev\n\
                 OIDC_CLIENT_ID=myapp\n\
                 OIDC_CLIENT_SECRET=s3cret"
            ),
            "{}",
            html
        );
    }

    #[test]
    fn env_lines_quote_only_values_that_are_not_plain() {
        assert_eq!(env_line("A", "s3cret"), "A=s3cret");
        assert_eq!(env_line("A", "[::1]:9500"), "A=[::1]:9500");
        assert_eq!(env_line("A", ""), "A=\"\"");
        assert_eq!(env_line("A", "pa ss\"1"), "A=\"pa ss\\\"1\"");
        assert_eq!(env_line("A", "a\\b\nc"), "A=\"a\\\\b\\nc\"");
    }

    #[test]
    fn credentials_are_escaped_in_the_page() {
        let html = page("127.0.0.1", &entries_with_password("p\"<x>&"));
        assert!(!html.contains("<x>"), "raw markup leaked: {}", html);
        assert!(
            html.contains("<span>user</span>alice (password p&quot;&lt;x&gt;&amp;)"),
            "{}",
            html
        );
        assert!(
            html.contains("OIDC_ISSUER=http://127.0.0.1:9500/realms/dev"),
            "{}",
            html
        );
    }

    #[test]
    fn env_snippet_is_escaped_as_html_but_copies_the_original_text() {
        let mut entries = entries_with_password("x");
        entries[0].card.env = vec![Field::new(
            "SMTP_PASSWORD",
            Value::Text("p\"<x>&".to_string()),
        )];
        let html = page("127.0.0.1", &entries);
        assert!(
            html.contains(
                "<pre id=\"env-minicloak\">SMTP_PASSWORD=&quot;p\\&quot;&lt;x&gt;&amp;&quot;</pre>"
            ),
            "{}",
            html
        );
    }

    #[test]
    fn copy_button_points_at_its_snippet() {
        let html = page("127.0.0.1", &entries());
        assert!(
            html.contains(
                "<button type=\"button\" class=\"copy\" data-copy=\"env-minicloak\">Copy</button>"
            ),
            "{}",
            html
        );
        assert!(
            html.contains("<pre id=\"env-minicloak\">OIDC_ISSUER="),
            "{}",
            html
        );
        assert!(html.contains("navigator.clipboard.writeText"), "{}", html);
    }

    #[test]
    fn notes_are_escaped_and_shown_under_the_snippet() {
        let html = page("127.0.0.1", &entries());
        assert!(
            html.contains("<p class=\"note\">Notes with &lt;markup&gt; &amp; more.</p>"),
            "{}",
            html
        );
    }

    #[test]
    fn startup_log_is_collapsed() {
        let html = page("127.0.0.1", &entries());
        assert!(
            html.contains(
                "<details><summary>Startup log</summary>\n<pre>users: &lt;alice&gt; &amp; bob\n</pre></details>"
            ),
            "{}",
            html
        );
    }

    #[test]
    fn minicloak_card_shows_configured_accounts_and_no_defaults() {
        let p = minicloak::prepare(
            minicloak::parse_args(args(&[
                "--bind",
                "127.0.0.1:0",
                "--key-bits",
                "512",
                "--user",
                "carol=pw1",
                "--client",
                "web=public:http://localhost:5173/*",
                "--client",
                "myapp=s3cret:http://localhost:3000/*",
            ]))
            .unwrap(),
        )
        .unwrap();
        let card = minicloak_card(&p, 9500);
        let rows = field_rows(&card);
        assert!(
            rows.contains(&"user carol (password pw1)".to_string()),
            "{:?}",
            rows
        );
        assert!(!rows.iter().any(|r| r.contains("alice")), "{:?}", rows);
        assert!(
            rows.contains(
                &"client myapp (secret s3cret), redirect http://localhost:3000/*".to_string()
            ),
            "{:?}",
            rows
        );
        assert!(
            rows.contains(
                &"client web (public, PKCE required), redirect http://localhost:5173/*".to_string()
            ),
            "{:?}",
            rows
        );
        assert_eq!(
            env_text(&card),
            "OIDC_ISSUER=http://box.local:9500/realms/dev\n\
             OIDC_CLIENT_ID=myapp\n\
             OIDC_CLIENT_SECRET=s3cret"
        );
    }

    #[test]
    fn minicloak_card_falls_back_to_seeded_defaults() {
        let p = minicloak::prepare(
            minicloak::parse_args(args(&["--bind", "127.0.0.1:0", "--key-bits", "512"])).unwrap(),
        )
        .unwrap();
        let card = minicloak_card(&p, 9500);
        let rows = field_rows(&card);
        assert!(
            rows.contains(&"user alice (password alice)".to_string()),
            "{:?}",
            rows
        );
        assert!(
            rows.contains(&"user bob (password bob)".to_string()),
            "{:?}",
            rows
        );
        assert!(env_text(&card).contains("OIDC_CLIENT_SECRET=s3cret"));
    }

    #[test]
    fn minicloak_pinned_issuer_is_used_verbatim() {
        let p = minicloak::prepare(
            minicloak::parse_args(args(&[
                "--bind",
                "127.0.0.1:0",
                "--key-bits",
                "512",
                "--issuer",
                "https://auth.example.test/realms/dev",
            ]))
            .unwrap(),
        )
        .unwrap();
        let card = minicloak_card(&p, 9500);
        assert!(
            env_text(&card).starts_with("OIDC_ISSUER=https://auth.example.test/realms/dev\n"),
            "{}",
            env_text(&card)
        );
    }

    #[test]
    fn minimail_card_uses_the_configured_account() {
        let dir = scratch("minimail-account");
        let p = minimail::prepare(
            minimail::parse_args(args(&[
                "--smtp-bind",
                "127.0.0.1:0",
                "--http-bind",
                "127.0.0.1:0",
                "--root",
                dir.to_str().unwrap(),
                "--user",
                "ann",
                "--password",
                "pw1",
            ]))
            .unwrap(),
        )
        .unwrap();
        let port = p.smtp_local_addr().unwrap().port();
        let card = minimail_card(&p);
        assert_eq!(
            env_text(&card),
            format!(
                "SMTP_HOST=box.local\nSMTP_PORT={}\nSMTP_USER=ann\nSMTP_PASSWORD=pw1",
                port
            )
        );
        assert!(field_rows(&card).contains(&"user ann (password pw1)".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn minimail_card_without_auth_has_no_login_variables() {
        let dir = scratch("minimail-anonymous");
        let p = minimail::prepare(
            minimail::parse_args(args(&[
                "--smtp-bind",
                "127.0.0.1:0",
                "--http-bind",
                "127.0.0.1:0",
                "--root",
                dir.to_str().unwrap(),
                "--anonymous",
            ]))
            .unwrap(),
        )
        .unwrap();
        let card = minimail_card(&p);
        assert!(
            !env_text(&card).contains("SMTP_USER"),
            "{}",
            env_text(&card)
        );
        assert!(field_rows(&card).contains(&"auth anonymous, no login needed".to_string()));
        assert!(card.notes.iter().any(|n| n.contains("Auth is disabled")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn minimail_card_shows_the_seeded_default_login() {
        let dir = scratch("minimail-default");
        let p = minimail::prepare(
            minimail::parse_args(args(&[
                "--smtp-bind",
                "127.0.0.1:0",
                "--http-bind",
                "127.0.0.1:0",
                "--root",
                dir.to_str().unwrap(),
            ]))
            .unwrap(),
        )
        .unwrap();
        let card = minimail_card(&p);
        assert!(env_text(&card).contains("SMTP_USER=minimail\nSMTP_PASSWORD=minimail"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn minibucket_card_uses_the_configured_keys_and_path_style() {
        let dir = scratch("minibucket");
        let p = minibucket::prepare(
            minibucket::parse_args(args(&[
                "--bind",
                "127.0.0.1:0",
                "--root",
                dir.to_str().unwrap(),
                "--access-key",
                "AKIATEST",
                "--secret-key",
                "sec ret",
                "--region",
                "eu-west-1",
            ]))
            .unwrap(),
        )
        .unwrap();
        let port = p.local_addr().unwrap().port();
        let card = minibucket_card(&p, port);
        assert_eq!(
            env_text(&card),
            format!(
                "AWS_ENDPOINT_URL=http://box.local:{}\nAWS_REGION=eu-west-1\n\
                 AWS_ACCESS_KEY_ID=AKIATEST\nAWS_SECRET_ACCESS_KEY=\"sec ret\"",
                port
            )
        );
        assert!(field_rows(&card).contains(&"access key AKIATEST (secret sec ret)".to_string()));
        assert!(card.notes.iter().any(|n| n.contains("Path-style")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
