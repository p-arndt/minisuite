// User store. Users are declared in the TOML config file (see config.rs); the
// one-liner `username=password:email:name:role1,role2` parsed here backs the
// repeatable `--user` flag, where a terse spec beats a file.

use std::collections::HashMap;
use std::io;

#[derive(Clone, Debug, PartialEq)]
pub struct User {
    pub username: String,
    pub password: String,
    pub email: String,      // "" if unset
    pub first_name: String, // "" if unset
    pub last_name: String,  // "" if unset
    pub roles: Vec<String>, // empty if unset
}

impl User {
    /// Stable opaque subject id: lowercase hex of the first 16 bytes of sha256(username).
    pub fn sub(&self) -> String {
        let digest = crate::sha256::sha256(self.username.as_bytes());
        crate::sha256::hex(&digest[..16])
    }

    /// Display name "First Last". "" when both are empty; "Cher" when last_name is "".
    pub fn name(&self) -> String {
        format!("{} {}", self.first_name, self.last_name)
            .trim()
            .to_string()
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
            // The one-liner keeps a single display-name column; split it on the first
            // space so "Alice Admin" -> ("Alice", "Admin") and "Cher" -> ("Cher", "").
            let name = fields.next().unwrap_or("").trim();
            let (first_name, last_name) = match name.split_once(' ') {
                Some((first, last)) => (first.to_string(), last.to_string()),
                None => (name.to_string(), String::new()),
            };
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
                first_name,
                last_name,
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

    #[test]
    fn aligned_columns_parse_the_same_as_tight_ones() {
        // People align these files by hand; the padding must not end up in the password.
        let tight = Users::parse("alice=alice:alice@example.com:Alice Admin:admin,staff").unwrap();
        let spaced =
            Users::parse("alice = alice : alice@example.com : Alice Admin : admin,staff").unwrap();
        assert_eq!(tight.get("alice"), spaced.get("alice"));
        assert_eq!(spaced.get("alice").unwrap().password, "alice");
        assert_eq!(spaced.get("alice").unwrap().first_name, "Alice");
        assert_eq!(spaced.get("alice").unwrap().last_name, "Admin");
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
        assert_eq!(alice.first_name, "");
        assert_eq!(alice.last_name, "");
        assert!(alice.roles.is_empty());

        let bob = u.get("bob").unwrap();
        assert_eq!(bob.password, "bob");
        assert_eq!(bob.email, "bob@x.de");

        let carol = u.get("carol").unwrap();
        assert_eq!(carol.password, "c");
        assert_eq!(carol.email, "");
        assert_eq!(carol.first_name, "Carol");
        assert_eq!(carol.last_name, "Q");
        assert_eq!(carol.roles, vec!["admin".to_string()]);

        let dave = u.get("dave").unwrap();
        assert_eq!(dave.roles, vec!["admin".to_string(), "user".to_string()]);
        assert_eq!(dave.first_name, "Dave");
        assert_eq!(dave.last_name, "Grohl");
    }

    #[test]
    fn one_liner_splits_name_column_on_first_space() {
        // Third column is a full display name; split on the first space only.
        let u = Users::parse("dave=d:dave@x.de:Dave Van Halen:user").unwrap();
        let dave = u.get("dave").unwrap();
        assert_eq!(dave.first_name, "Dave");
        assert_eq!(dave.last_name, "Van Halen");

        let cher = Users::parse("cher=c::Cher").unwrap();
        let cher = cher.get("cher").unwrap();
        assert_eq!(cher.first_name, "Cher");
        assert_eq!(cher.last_name, "");
    }

    #[test]
    fn name_joins_first_and_last() {
        let base = User {
            username: "x".into(),
            password: "x".into(),
            email: "".into(),
            first_name: "".into(),
            last_name: "".into(),
            roles: vec![],
        };
        assert_eq!(base.name(), "");
        let first_only = User {
            first_name: "Cher".into(),
            ..base.clone()
        };
        assert_eq!(first_only.name(), "Cher");
        let both = User {
            first_name: "Alice".into(),
            last_name: "Admin".into(),
            ..base.clone()
        };
        assert_eq!(both.name(), "Alice Admin");
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
            first_name: "".into(),
            last_name: "".into(),
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
            first_name: "".into(),
            last_name: "".into(),
            roles: vec![],
        });
        u.add(User {
            username: "b".into(),
            password: "1".into(),
            email: "".into(),
            first_name: "".into(),
            last_name: "".into(),
            roles: vec![],
        });
        u.add(User {
            username: "a".into(),
            password: "2".into(),
            email: "".into(),
            first_name: "".into(),
            last_name: "".into(),
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
}
