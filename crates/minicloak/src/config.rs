// A single TOML file describing both the users and the clients of a realm.
//
// The whole document is parsed first (so file order is fixed) and only then
// interpreted, because we want every diagnostic to point at the line the human
// wrote and to name the exact key or table at fault -- a config that half-loads
// with a surprising client silently marked "public" is a security problem, not
// a convenience.

use std::fs;
use std::io;
use std::path::Path;

use crate::clients::{Client, Clients};
use crate::toml::{self, Table, Value};
use crate::users::{User, Users};

const USER_KEYS: &[&str] = &["password", "email", "first_name", "last_name", "roles"];
const CLIENT_KEYS: &[&str] = &["secret", "public", "redirect_uris"];

pub fn parse(text: &str) -> Result<(Users, Clients), toml::Error> {
    let mut users = Users::new();
    let mut clients = Clients::new();

    // Tables arrive in file order, so `add` (first-seen order) reproduces the
    // document's ordering in both stores.
    for table in toml::parse(text)? {
        match table.path.as_slice() {
            // A bare `[users]` / `[clients]` is the parent header TOML writers
            // often leave in; harmless empty, but it can never carry keys.
            [ns] if ns == "users" || ns == "clients" => {
                if let Some(first) = table.entries.first() {
                    return fail(
                        first.line,
                        format!(
                            "[{ns}] is a section header and cannot hold keys directly; \
                             move them under [{ns}.<name>]"
                        ),
                    );
                }
            }
            [ns, leaf] if ns == "users" => {
                if leaf.is_empty() {
                    return fail(table.line, "a user name cannot be empty");
                }
                users.add(parse_user(&table, leaf)?);
            }
            [ns, leaf] if ns == "clients" => {
                if leaf.is_empty() {
                    return fail(table.line, "a client id cannot be empty");
                }
                clients.add(parse_client(&table, leaf)?);
            }
            _ => return fail(table.line, format!("unknown table [{}]", table.name())),
        }
    }

    Ok((users, clients))
}

pub fn load_file(path: &Path) -> io::Result<(Users, Clients)> {
    let text = fs::read_to_string(path)?;
    // `?` leans on `From<toml::Error> for io::Error` to attach the line number.
    let stores = parse(&text)?;
    Ok(stores)
}

fn parse_user(table: &Table, name: &str) -> Result<User, toml::Error> {
    reject_unknown_keys(table, USER_KEYS, "users", name)?;

    // The one required field: a login with no password is never intended.
    let password = match table.get("password") {
        None => {
            return fail(
                table.line,
                format!("user '{name}' has no password; a 'password' key is required"),
            )
        }
        Some(e) => match e.value.as_str() {
            Some("") => {
                return fail(
                    e.line,
                    format!("the password for user '{name}' must not be empty"),
                )
            }
            Some(s) => s.to_string(),
            None => {
                return fail(
                    e.line,
                    format!("'password' must be a string, found {}", e.value.type_name()),
                )
            }
        },
    };

    Ok(User {
        username: name.to_string(),
        password,
        email: opt_str(table, "email")?,
        first_name: opt_str(table, "first_name")?,
        last_name: opt_str(table, "last_name")?,
        roles: str_array(table, "roles")?,
    })
}

fn parse_client(table: &Table, id: &str) -> Result<Client, toml::Error> {
    reject_unknown_keys(table, CLIENT_KEYS, "clients", id)?;

    let public_entry = table.get("public");
    let public = match public_entry {
        None => None,
        Some(e) => match e.value.as_bool() {
            Some(b) => Some(b),
            None => {
                return fail(
                    e.line,
                    format!("'public' must be a boolean, found {}", e.value.type_name()),
                )
            }
        },
    };

    let secret = match table.get("secret") {
        None => None,
        Some(e) => match e.value.as_str() {
            // An empty secret used to silently mean "public"; that footgun is
            // exactly what this format replaces, so it is now an error.
            Some("") => {
                return fail(
                    e.line,
                    format!(
                        "client '{id}' has an empty secret; \
                         declare a public client with public = true instead"
                    ),
                )
            }
            Some(s) => Some(s.to_string()),
            None => {
                return fail(
                    e.line,
                    format!("'secret' must be a string, found {}", e.value.type_name()),
                )
            }
        },
    };

    // Exactly one of `secret` or `public = true` may be in force; anything else
    // leaves the client's trust level ambiguous.
    let secret = match (public, &secret) {
        (Some(true), Some(_)) => {
            return fail(
                public_entry.unwrap().line,
                format!("client '{id}' cannot set both public = true and a secret"),
            )
        }
        (Some(true), None) => None,
        (_, Some(s)) => Some(s.clone()),
        (Some(false), None) => {
            return fail(
                public_entry.unwrap().line,
                format!(
                    "client '{id}' is public = false but has no secret; \
                         a confidential client needs one"
                ),
            )
        }
        (None, None) => {
            return fail(
                table.line,
                format!("client '{id}' must set either secret = \"...\" or public = true"),
            )
        }
    };

    let redirect_uris = str_array(table, "redirect_uris")?;
    if let Some(e) = table.get("redirect_uris") {
        // Reuse the exact wildcard check the clients registry enforces, so a URI
        // that ends mid-authority can never widen the host it stands for.
        for uri in &redirect_uris {
            crate::clients::validate_redirect_uri(uri)
                .map_err(|msg| toml::Error { line: e.line, msg })?;
        }
    }

    Ok(Client {
        id: id.to_string(),
        secret,
        redirect_uris,
    })
}

fn reject_unknown_keys(
    table: &Table,
    allowed: &[&str],
    ns: &str,
    leaf: &str,
) -> Result<(), toml::Error> {
    for e in &table.entries {
        if !allowed.contains(&e.key.as_str()) {
            return fail(
                e.line,
                format!(
                    "unknown key '{}' in [{ns}.{leaf}]; allowed keys are {}",
                    e.key,
                    allowed.join(", ")
                ),
            );
        }
    }
    Ok(())
}

/// An optional string field, defaulting to "" when absent.
fn opt_str(table: &Table, key: &str) -> Result<String, toml::Error> {
    match table.get(key) {
        None => Ok(String::new()),
        Some(e) => match e.value.as_str() {
            Some(s) => Ok(s.to_string()),
            None => fail(
                e.line,
                format!("'{key}' must be a string, found {}", e.value.type_name()),
            ),
        },
    }
}

/// An optional array of strings, defaulting to empty. A non-array, or an array
/// holding any non-string, is rejected at the entry's line.
fn str_array(table: &Table, key: &str) -> Result<Vec<String>, toml::Error> {
    match table.get(key) {
        None => Ok(Vec::new()),
        Some(e) => match &e.value {
            Value::Arr(items) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    match it.as_str() {
                        Some(s) => out.push(s.to_string()),
                        None => {
                            return fail(
                                e.line,
                                format!(
                                    "every value in '{key}' must be a string, found {}",
                                    it.type_name()
                                ),
                            )
                        }
                    }
                }
                Ok(out)
            }
            other => fail(
                e.line,
                format!(
                    "'{key}' must be an array of strings, found {}",
                    other.type_name()
                ),
            ),
        },
    }
}

fn fail<T>(line: usize, msg: impl Into<String>) -> Result<T, toml::Error> {
    Err(toml::Error {
        line,
        msg: msg.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> (Users, Clients) {
        parse(text).unwrap()
    }

    fn err(text: &str) -> toml::Error {
        parse(text).unwrap_err()
    }

    #[test]
    fn full_document_round_trips_into_both_stores_in_order() {
        let doc = r#"
[users.alice]
password   = "alice"
email      = "alice@example.com"
first_name = "Alice"
last_name  = "Admin"
roles      = ["admin", "staff"]

[users.bob]
password = "bob"

[clients.spa]
public        = true
redirect_uris = ["http://localhost:5173/*", "http://localhost:5174/callback"]

[clients.myapp]
secret        = "s3cret"
redirect_uris = ["http://localhost:3000/*"]
"#;
        let (users, clients) = ok(doc);

        let user_order: Vec<&str> = users.list().iter().map(|u| u.username.as_str()).collect();
        assert_eq!(user_order, ["alice", "bob"]);

        let alice = users.get("alice").unwrap();
        assert_eq!(alice.password, "alice");
        assert_eq!(alice.email, "alice@example.com");
        assert_eq!(alice.first_name, "Alice");
        assert_eq!(alice.last_name, "Admin");
        assert_eq!(alice.roles, vec!["admin".to_string(), "staff".to_string()]);

        let client_order: Vec<&str> = clients.list().iter().map(|c| c.id.as_str()).collect();
        assert_eq!(client_order, ["spa", "myapp"]);

        let spa = clients.get("spa").unwrap();
        assert!(spa.is_public());
        assert_eq!(
            spa.redirect_uris,
            vec![
                "http://localhost:5173/*".to_string(),
                "http://localhost:5174/callback".to_string()
            ]
        );

        let myapp = clients.get("myapp").unwrap();
        assert_eq!(myapp.secret, Some("s3cret".to_string()));
        assert!(!myapp.is_public());
    }

    #[test]
    fn public_flag_and_confidential_secret_are_distinguished() {
        let (_, c) = ok("[clients.a]\npublic = true\n[clients.b]\nsecret = \"x\"\n");
        assert!(c.get("a").unwrap().is_public());
        assert_eq!(c.get("a").unwrap().secret, None);
        assert_eq!(c.get("b").unwrap().secret, Some("x".to_string()));
        assert!(!c.get("b").unwrap().is_public());

        // secret with an explicit public = false is still confidential.
        let (_, c) = ok("[clients.b]\nsecret = \"x\"\npublic = false\n");
        assert_eq!(c.get("b").unwrap().secret, Some("x".to_string()));
    }

    #[test]
    fn optional_user_fields_default_to_empty() {
        let (u, _) = ok("[users.min]\npassword = \"p\"\n");
        let m = u.get("min").unwrap();
        assert_eq!(m.email, "");
        assert_eq!(m.first_name, "");
        assert_eq!(m.last_name, "");
        assert!(m.roles.is_empty());
    }

    #[test]
    fn roles_default_to_empty_when_absent() {
        let (u, _) = ok("[users.x]\npassword = \"p\"\n");
        assert!(u.get("x").unwrap().roles.is_empty());
    }

    #[test]
    fn a_quoted_name_with_a_dot_stays_one_user() {
        let (u, _) = ok("[users.\"ada@example.com\"]\npassword = \"p\"\n[users.\"jane.doe\"]\npassword = \"q\"\n");
        assert!(u.get("ada@example.com").is_some());
        assert_eq!(u.get("jane.doe").unwrap().password, "q");
        let order: Vec<&str> = u.list().iter().map(|x| x.username.as_str()).collect();
        assert_eq!(order, ["ada@example.com", "jane.doe"]);
    }

    #[test]
    fn a_bare_parent_header_is_allowed_only_without_keys() {
        assert!(parse("[users]\n[users.a]\npassword = \"p\"\n").is_ok());
        let e = err("[users]\npassword = \"p\"\n");
        assert_eq!(e.line, 2);
        assert!(e.msg.contains("cannot hold keys"));
    }

    #[test]
    fn an_unknown_table_names_the_header() {
        let e = err("[realm]\n");
        assert_eq!(e.line, 1);
        assert!(e.msg.contains("unknown table [realm]"));
        assert!(err("[users.a.b]\n")
            .msg
            .contains("unknown table [users.a.b]"));
    }

    #[test]
    fn an_empty_user_or_client_name_is_refused() {
        assert!(err("[users.\"\"]\n")
            .msg
            .contains("user name cannot be empty"));
        assert!(err("[clients.\"\"]\n")
            .msg
            .contains("client id cannot be empty"));
    }

    #[test]
    fn a_user_without_a_password_is_refused() {
        let e = err("[users.alice]\nemail = \"a@b.de\"\n");
        assert_eq!(e.line, 1);
        assert!(e.msg.contains("has no password"));
    }

    #[test]
    fn an_empty_password_is_refused() {
        let e = err("[users.alice]\npassword = \"\"\n");
        assert_eq!(e.line, 2);
        assert!(e.msg.contains("must not be empty"));
    }

    #[test]
    fn a_password_of_the_wrong_type_names_the_type() {
        let e = err("[users.alice]\npassword = true\n");
        assert_eq!(e.line, 2);
        assert!(e.msg.contains("'password' must be a string"));
        assert!(e.msg.contains("a boolean"));
    }

    #[test]
    fn an_optional_field_of_the_wrong_type_is_refused() {
        let e = err("[users.alice]\npassword = \"p\"\nemail = 7\n");
        assert_eq!(e.line, 3);
        assert!(e.msg.contains("'email' must be a string"));
        assert!(e.msg.contains("an integer"));
    }

    #[test]
    fn roles_must_be_an_array_of_only_strings() {
        let e = err("[users.a]\npassword = \"p\"\nroles = \"admin\"\n");
        assert!(e.msg.contains("'roles' must be an array of strings"));
        assert!(e.msg.contains("a string"));

        let e = err("[users.a]\npassword = \"p\"\nroles = [\"ok\", 3]\n");
        assert_eq!(e.line, 3);
        assert!(e.msg.contains("every value in 'roles' must be a string"));
        assert!(e.msg.contains("an integer"));
    }

    #[test]
    fn an_unknown_key_lists_the_accepted_ones() {
        let e = err("[users.a]\npassword = \"p\"\nage = 3\n");
        assert_eq!(e.line, 3);
        assert!(e.msg.contains("unknown key 'age' in [users.a]"));
        assert!(e
            .msg
            .contains("password, email, first_name, last_name, roles"));

        let e = err("[clients.a]\npublic = true\nscope = \"x\"\n");
        assert!(e.msg.contains("unknown key 'scope' in [clients.a]"));
        assert!(e.msg.contains("secret, public, redirect_uris"));
    }

    #[test]
    fn a_client_cannot_be_both_public_and_secret() {
        let e = err("[clients.a]\npublic = true\nsecret = \"x\"\n");
        assert_eq!(e.line, 2);
        assert!(e.msg.contains("cannot set both public = true and a secret"));
    }

    #[test]
    fn a_confidential_client_needs_a_secret() {
        let e = err("[clients.a]\npublic = false\n");
        assert_eq!(e.line, 2);
        assert!(e.msg.contains("public = false but has no secret"));
    }

    #[test]
    fn a_client_with_neither_key_is_refused() {
        let e = err("[clients.a]\nredirect_uris = [\"http://x/*\"]\n");
        assert_eq!(e.line, 1);
        assert!(e.msg.contains("must set either secret"));
    }

    #[test]
    fn an_empty_secret_is_refused_rather_than_meaning_public() {
        let e = err("[clients.a]\nsecret = \"\"\n");
        assert_eq!(e.line, 2);
        assert!(e.msg.contains("has an empty secret"));
    }

    #[test]
    fn public_of_the_wrong_type_is_refused() {
        let e = err("[clients.a]\npublic = \"yes\"\n");
        assert!(e.msg.contains("'public' must be a boolean"));
        assert!(e.msg.contains("a string"));
    }

    #[test]
    fn a_secret_of_the_wrong_type_is_refused() {
        let e = err("[clients.a]\nsecret = 5\n");
        assert!(e.msg.contains("'secret' must be a string"));
        assert!(e.msg.contains("an integer"));
    }

    #[test]
    fn a_redirect_uri_with_a_bad_wildcard_is_rejected() {
        // A wildcard right after the port could otherwise widen the host.
        let e = err("[clients.a]\npublic = true\nredirect_uris = [\"http://localhost:5173*\"]\n");
        assert_eq!(e.line, 3);
        assert!(e.msg.contains("wildcard"));
    }

    #[test]
    fn redirect_uris_must_be_an_array_of_only_strings() {
        let e = err("[clients.a]\npublic = true\nredirect_uris = \"http://x/*\"\n");
        assert!(e
            .msg
            .contains("'redirect_uris' must be an array of strings"));

        let e = err("[clients.a]\npublic = true\nredirect_uris = [\"http://x/*\", 1]\n");
        assert!(e
            .msg
            .contains("every value in 'redirect_uris' must be a string"));
    }
}
