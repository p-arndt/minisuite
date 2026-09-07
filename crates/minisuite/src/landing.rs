// The landing page on :9900 — one page that tells you where everything is.
//
// A deliberately minimal HTTP/1.0-style server: read the request head, write
// one response, close. There is no keep-alive, no chunking and no routing table
// beyond three cases, because this exists only so a human opening
// http://localhost:9900 can find the OIDC discovery document without grepping a
// terminal for the banner.
//
// Every link is built from the *request's* Host header rather than from the
// bind address, so the page works unchanged whether it is reached on localhost,
// on a LAN IP, or through a published container port on another machine.

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

/// A running service as the landing page sees it.
pub struct Entry {
    pub name: &'static str,
    pub banner: String,
    pub links: Vec<Link>,
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
footer { color:var(--dim); font-size:12.5px; margin-top:2rem }";

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
    s.push_str("<p class=\"sub\">A local development backend. Do not expose it to a network you do not trust.</p>\n");

    for e in entries {
        s.push_str(&format!("<section><h2>{}</h2>\n<ul>\n", esc(e.name)));
        for l in &e.links {
            let url = format!("http://{}:{}{}", host, l.port, l.path);
            s.push_str(&format!(
                "<li><span>{}</span><a href=\"{}\">{}</a></li>\n",
                esc(l.label),
                esc(&url),
                esc(&url)
            ));
        }
        s.push_str(&format!(
            "</ul>\n<pre>{}</pre>\n</section>\n",
            esc(&e.banner)
        ));
    }

    s.push_str("<footer>GET /healthz for a machine-readable status.</footer>\n");
    s.push_str("</main></body></html>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<Entry> {
        vec![Entry {
            name: "minicloak",
            banner: "users: <alice> & bob\n".to_string(),
            links: vec![Link {
                label: "discovery",
                port: 9500,
                path: "/realms/dev/.well-known/openid-configuration".to_string(),
            }],
        }]
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
}
