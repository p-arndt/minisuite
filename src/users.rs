// User store: flat file of `username=password:email:name:role1,role2`.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub struct User {
    pub username: String,
    pub password: String,
    pub email: String,      // "" if unset
    pub name: String,       // "" if unset
    pub roles: Vec<String>, // empty if unset
}

impl User {
    /// Stable opaque subject id: lowercase hex of the first 16 bytes of sha256(username).
    pub fn sub(&self) -> String {
        let digest = crate::sha256::sha256(self.username.as_bytes());
        crate::sha256::hex(&digest[..16])
    }

    /// Splits `name` on the first space: ("Alice", "Admin"). Empty strings if `name` is "".
    pub fn given_family(&self) -> (String, String) {
        match self.name.split_once(' ') {
            Some((given, family)) => (given.to_string(), family.to_string()),
            None => (self.name.clone(), String::new()),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Users {
    pub map: HashMap<String, User>,
    pub order: Vec<String>,
}

impl Users {
    pub fn new() -> Self {
        Self::default()
    }

    /// Overwrites by username, preserves first-seen order.
    pub fn add(&mut self, u: User) {
        if !self.map.contains_key(&u.username) {
            self.order.push(u.username.clone());
        }
        self.map.insert(u.username.clone(), u);
    }

    pub fn get(&self, username: &str) -> Option<&User> {
        self.map.get(username)
    }

    /// Constant-time-ish password check; returns the user only on an exact match.
    pub fn authenticate(&self, username: &str, password: &str) -> Option<&User> {
        let user = self.map.get(username)?;
        if ct_eq(user.password.as_bytes(), password.as_bytes()) {
            Some(user)
        } else {
            None
        }
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn load_file(path: &Path) -> io::Result<Users> {
        let text = fs::read_to_string(path)?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> io::Result<Users> {
        let mut users = Users::new();
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let eq = line.find('=').ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("line {}: expected username=password[:...]", i + 1),
                )
            })?;
            let username = line[..eq].trim();
            if username.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("line {}: empty username", i + 1),
                ));
            }
            let rest = &line[eq + 1..];
            // password:email:name:roles  -- everything after password optional.
            // Fields are trimmed (as in clients.rs) so that columns can be aligned
            // for readability; a password may therefore not begin or end with a space.
            let mut fields = rest.splitn(4, ':');
            let password = fields.next().unwrap_or("").trim().to_string();
            let email = fields.next().unwrap_or("").trim().to_string();
            let name = fields.next().unwrap_or("").trim().to_string();
            let roles = match fields.next() {
                Some(r) if !r.is_empty() => r
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
                _ => Vec::new(),
            };
            users.add(User {
                username: username.to_string(),
                password,
                email,
                name,
                roles,
            });
        }
        Ok(users)
    }

    /// Usernames in insertion order — the login page lists these as dev hints.
    pub fn list(&self) -> Vec<&User> {
        self.order.iter().filter_map(|u| self.map.get(u)).collect()
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
        p.push(format!("minicloak_users_{}_{}", nanos, name));
        p
    }

    #[test]
    fn aligned_columns_parse_the_same_as_tight_ones() {
        // People align these files by hand; the padding must not end up in the password.
        let tight = Users::parse("alice=alice:alice@example.com:Alice Admin:admin,staff").unwrap();
        let spaced =
            Users::parse("alice = alice : alice@example.com : Alice Admin : admin,staff").unwrap();
        assert_eq!(tight.get("alice"), spaced.get("alice"));
        assert_eq!(spaced.get("alice").unwrap().password, "alice");
        assert_eq!(spaced.get("alice").unwrap().name, "Alice Admin");
    }

    #[test]
    fn hash_only_starts_a_comment_at_the_start_of_a_line() {
        // '#' is a legal password character, so it cannot also mean "rest is a comment".
        let u = Users::parse("carol=pass#word").unwrap();
        assert_eq!(u.get("carol").unwrap().password, "pass#word");
        assert!(Users::parse("# carol=pass").unwrap().is_empty());
    }

    #[test]
    fn parse_full_and_optional() {
        let text = "\
# comment
alice=alice
bob=bob:bob@x.de
carol=c::Carol Q:admin
dave=d:dave@x.de:Dave Grohl:admin,user
";
        let u = Users::parse(text).unwrap();
        assert_eq!(u.list().len(), 4);

        let alice = u.get("alice").unwrap();
        assert_eq!(alice.password, "alice");
        assert_eq!(alice.email, "");
        assert_eq!(alice.name, "");
        assert!(alice.roles.is_empty());

        let bob = u.get("bob").unwrap();
        assert_eq!(bob.password, "bob");
        assert_eq!(bob.email, "bob@x.de");

        let carol = u.get("carol").unwrap();
        assert_eq!(carol.password, "c");
        assert_eq!(carol.email, "");
        assert_eq!(carol.name, "Carol Q");
        assert_eq!(carol.roles, vec!["admin".to_string()]);

        let dave = u.get("dave").unwrap();
        assert_eq!(dave.roles, vec!["admin".to_string(), "user".to_string()]);
        assert_eq!(
            dave.given_family(),
            ("Dave".to_string(), "Grohl".to_string())
        );
    }

    #[test]
    fn given_family_edge_cases() {
        let u = User {
            username: "x".into(),
            password: "x".into(),
            email: "".into(),
            name: "".into(),
            roles: vec![],
        };
        assert_eq!(u.given_family(), (String::new(), String::new()));
        let u2 = User {
            name: "Cher".into(),
            ..u.clone()
        };
        assert_eq!(u2.given_family(), ("Cher".to_string(), String::new()));
    }

    #[test]
    fn authenticate_checks_password() {
        let u = Users::parse("alice=secret\n").unwrap();
        assert!(u.authenticate("alice", "secret").is_some());
        assert!(u.authenticate("alice", "wrong").is_none());
        assert!(u.authenticate("alice", "secre").is_none());
        assert!(u.authenticate("nobody", "secret").is_none());
    }

    #[test]
    fn sub_is_stable_hex() {
        let u = User {
            username: "alice".into(),
            password: "x".into(),
            email: "".into(),
            name: "".into(),
            roles: vec![],
        };
        let s = u.sub();
        assert_eq!(s.len(), 32);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(s, u.sub());
    }

    #[test]
    fn add_overwrites_preserves_order() {
        let mut u = Users::new();
        u.add(User {
            username: "a".into(),
            password: "1".into(),
            email: "".into(),
            name: "".into(),
            roles: vec![],
        });
        u.add(User {
            username: "b".into(),
            password: "1".into(),
            email: "".into(),
            name: "".into(),
            roles: vec![],
        });
        u.add(User {
            username: "a".into(),
            password: "2".into(),
            email: "".into(),
            name: "".into(),
            roles: vec![],
        });
        let names: Vec<&str> = u.list().iter().map(|x| x.username.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert_eq!(u.get("a").unwrap().password, "2");
    }

    #[test]
    fn malformed_line_reports_number() {
        let err = Users::parse("alice=alice\nno_equals\n").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("line 2"));
        let err2 = Users::parse("=nopass\n").unwrap_err();
        assert!(err2.to_string().contains("line 1"));
    }

    #[test]
    fn load_file_roundtrip() {
        let p = tmp_path("ok");
        let mut f = fs::File::create(&p).unwrap();
        writeln!(f, "alice=alice:a@x.de:Alice A:admin").unwrap();
        drop(f);
        let u = Users::load_file(&p).unwrap();
        assert_eq!(u.get("alice").unwrap().email, "a@x.de");
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn load_file_missing_is_err() {
        assert!(Users::load_file(&tmp_path("missing")).is_err());
    }
}
