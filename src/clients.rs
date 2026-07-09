// Client registry: flat file of `client_id=secret:redirect_uri[,redirect_uri...]`.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub struct Client {
    pub id: String,
    pub secret: Option<String>, // None => public client
    pub redirect_uris: Vec<String>,
}

impl Client {
    pub fn is_public(&self) -> bool {
        self.secret.is_none()
    }

    /// Exact match, or prefix match when a registered URI ends in '*'.
    /// Always false if `redirect_uris` is empty or `uri` is empty.
    pub fn allows_redirect(&self, uri: &str) -> bool {
        if uri.is_empty() {
            return false;
        }
        self.redirect_uris.iter().any(|reg| {
            if let Some(prefix) = reg.strip_suffix('*') {
                uri.starts_with(prefix)
            } else {
                reg == uri
            }
        })
    }

    /// Constant-time secret comparison. Always false for a public client.
    pub fn check_secret(&self, secret: &str) -> bool {
        match &self.secret {
            Some(s) => ct_eq(s.as_bytes(), secret.as_bytes()),
            None => false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Clients {
    pub map: HashMap<String, Client>,
    pub order: Vec<String>,
}

impl Clients {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, c: Client) {
        if !self.map.contains_key(&c.id) {
            self.order.push(c.id.clone());
        }
        self.map.insert(c.id.clone(), c);
    }

    pub fn get(&self, id: &str) -> Option<&Client> {
        self.map.get(id)
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn load_file(path: &Path) -> io::Result<Clients> {
        let text = fs::read_to_string(path)?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> io::Result<Clients> {
        let mut clients = Clients::new();
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let eq = line.find('=').ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("line {}: expected client_id=secret:uris", i + 1),
                )
            })?;
            let id = line[..eq].trim();
            if id.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("line {}: empty client_id", i + 1),
                ));
            }
            let value = &line[eq + 1..];
            // Split value on the FIRST ':' only -- URIs contain ':' and '//'.
            let (secret_raw, uris_raw) = match value.split_once(':') {
                Some((s, u)) => (s, u),
                None => (value, ""),
            };
            let secret_raw = secret_raw.trim();
            // A literal "public" or an empty secret means a public client.
            let secret = if secret_raw.is_empty() || secret_raw == "public" {
                None
            } else {
                Some(secret_raw.to_string())
            };
            let redirect_uris: Vec<String> = uris_raw
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            for uri in &redirect_uris {
                validate_redirect_uri(uri).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("line {}: {}", i + 1, e))
                })?;
            }
            clients.add(Client {
                id: id.to_string(),
                secret,
                redirect_uris,
            });
        }
        Ok(clients)
    }

    pub fn list(&self) -> Vec<&Client> {
        self.order
            .iter()
            .filter_map(|id| self.map.get(id))
            .collect()
    }
}

/// A wildcard may only stand for a whole path segment onwards.
///
/// `http://localhost:5173*` would otherwise also match `http://localhost:51739.evil.com`,
/// because the prefix ends mid-authority. Requiring `/` before the `*` pins the
/// wildcard to a path boundary, so it can never widen the host.
fn validate_redirect_uri(uri: &str) -> Result<(), String> {
    if uri == "*" {
        return Ok(()); // explicit "anything goes" escape hatch
    }
    match uri.find('*') {
        None => Ok(()),
        Some(i) if i == uri.len() - 1 && uri.ends_with("/*") => Ok(()),
        Some(i) if i == uri.len() - 1 => Err(format!(
            "redirect uri {:?}: a trailing wildcard must follow a '/', as in \"{}/*\"",
            uri,
            uri.trim_end_matches('*')
        )),
        Some(_) => Err(format!(
            "redirect uri {:?}: '*' is only allowed at the very end",
            uri
        )),
    }
}

/// Constant-time byte comparison: accumulate XOR into a single byte, compare once.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("minicloak_clients_{}_{}", nanos, name));
        p
    }

    #[test]
    fn parse_secret_and_uris_with_colons() {
        let c = Clients::parse("myapp=s3cret:http://localhost:3000/callback\n").unwrap();
        let app = c.get("myapp").unwrap();
        assert_eq!(app.secret, Some("s3cret".to_string()));
        assert_eq!(
            app.redirect_uris,
            vec!["http://localhost:3000/callback".to_string()]
        );
        assert!(!app.is_public());
    }

    #[test]
    fn parse_multiple_uris() {
        let c = Clients::parse("web=public:http://localhost:5173/*,http://localhost:5174/cb\n")
            .unwrap();
        let w = c.get("web").unwrap();
        assert_eq!(w.redirect_uris.len(), 2);
        assert!(w.is_public());
    }

    #[test]
    fn public_client_variants() {
        let c = Clients::parse("a=public:http://x/cb\nb=:http://y/cb\n").unwrap();
        assert!(c.get("a").unwrap().is_public());
        assert!(c.get("b").unwrap().is_public());
        assert!(c.get("a").unwrap().secret.is_none());
        assert!(!c.get("a").unwrap().check_secret("public"));
        assert!(!c.get("a").unwrap().check_secret(""));
    }

    #[test]
    fn allows_redirect_exact_and_wildcard() {
        let c = Clients::parse("app=s:http://localhost:5173/*,https://exact/cb\n").unwrap();
        let app = c.get("app").unwrap();
        assert!(app.allows_redirect("http://localhost:5173/callback"));
        assert!(app.allows_redirect("http://localhost:5173/"));
        assert!(!app.allows_redirect("http://localhost:9999/callback"));
        assert!(app.allows_redirect("https://exact/cb"));
        assert!(!app.allows_redirect("https://exact/cb/extra"));
        assert!(!app.allows_redirect(""));
    }

    #[test]
    fn allows_redirect_empty_list() {
        let c = Client {
            id: "x".into(),
            secret: None,
            redirect_uris: vec![],
        };
        assert!(!c.allows_redirect("http://anything"));
    }

    #[test]
    fn check_secret_constant() {
        let c = Clients::parse("app=topsecret:http://x/cb\n").unwrap();
        let app = c.get("app").unwrap();
        assert!(app.check_secret("topsecret"));
        assert!(!app.check_secret("topsecre"));
        assert!(!app.check_secret("wrong"));
    }

    #[test]
    fn add_overwrites_preserves_order() {
        let mut c = Clients::new();
        c.add(Client {
            id: "a".into(),
            secret: None,
            redirect_uris: vec![],
        });
        c.add(Client {
            id: "b".into(),
            secret: None,
            redirect_uris: vec![],
        });
        c.add(Client {
            id: "a".into(),
            secret: Some("s".into()),
            redirect_uris: vec![],
        });
        let ids: Vec<&str> = c.list().iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
        assert_eq!(c.get("a").unwrap().secret, Some("s".to_string()));
    }

    #[test]
    fn malformed_line() {
        let err = Clients::parse("app=s:http://x/cb\nno_equals\n").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("line 2"));
    }

    #[test]
    fn load_file_roundtrip() {
        let p = tmp_path("ok");
        let mut f = fs::File::create(&p).unwrap();
        writeln!(f, "# clients").unwrap();
        writeln!(f, "app=s3cret:http://localhost:3000/callback").unwrap();
        drop(f);
        let c = Clients::load_file(&p).unwrap();
        assert!(c.get("app").unwrap().check_secret("s3cret"));
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn wildcard_must_sit_on_a_path_boundary() {
        // The dangerous one: the prefix ends mid-authority, so a lookalike host matches.
        let bad = Clients::parse("spa=public:http://localhost:5173*");
        assert!(
            bad.is_err(),
            "a wildcard directly after the port must be refused"
        );
        let c = Client {
            id: "spa".into(),
            secret: None,
            redirect_uris: vec!["http://localhost:5173".into()],
        };
        assert!(!c.allows_redirect("http://localhost:51739.evil.com/steal"));

        assert!(Clients::parse("spa=public:http://localhost:5173/*").is_ok());
        assert!(Clients::parse("spa=public:*").is_ok());
        assert!(
            Clients::parse("spa=public:http://a/*/cb").is_err(),
            "inner '*' is not a prefix"
        );
    }

    #[test]
    fn wildcard_matching_cannot_widen_the_host() {
        let c = Client {
            id: "spa".into(),
            secret: None,
            redirect_uris: vec!["http://localhost:5173/*".into()],
        };
        assert!(c.allows_redirect("http://localhost:5173/callback"));
        assert!(c.allows_redirect("http://localhost:5173/deep/link?x=1"));
        assert!(!c.allows_redirect("http://localhost:5173.evil.com/steal"));
        assert!(
            !c.allows_redirect("https://localhost:5173/callback"),
            "scheme must match"
        );
        assert!(!c.allows_redirect(""));
    }
}
