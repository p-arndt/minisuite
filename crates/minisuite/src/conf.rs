// minisuite.toml -> argv.
//
// The suite deliberately does NOT know any crate's flag set. It turns each
// `[section]` into a plain argument vector and hands that to the crate's own
// `parse_args`, so unknown keys, bad values and the resulting error messages
// all come from the crate that owns the flag. That keeps this file tiny and
// means a new flag in minimail works in minisuite.toml on the day it lands.
//
// The TOML parser itself is minicloak's (`minicloak::toml`), shared rather than
// duplicated: same subset, same line-numbered errors, no extra dependency.

use std::path::{Path, PathBuf};

use minicloak::toml::{Table, Value};

/// One `[section]` of minisuite.toml, already converted to argv.
#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    /// Section header as written, e.g. `minimail`.
    pub name: String,
    /// The generated arguments, ready for the crate's `parse_args`.
    pub args: Vec<String>,
    /// Long flags this section sets, e.g. `--smtp-bind`. Used to decide which
    /// suite defaults to drop: a key in the file always wins over a default.
    pub flags: Vec<String>,
}

impl Section {
    pub fn sets(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f == flag)
    }
}

/// `smtp_bind` and `smtp-bind` both mean `--smtp-bind`: TOML keys conventionally
/// use underscores, CLI flags use dashes, and forcing the user to remember which
/// one this file wants would be a pointless trap.
pub fn flag_name(key: &str) -> String {
    format!("--{}", key.replace('_', "-"))
}

/// Flags whose value is a filesystem path. A relative path in minisuite.toml is
/// resolved against the file's own directory, so a config that is checked in
/// next to the toml keeps working no matter where the suite is started from.
fn is_path_flag(flag: &str) -> bool {
    matches!(
        flag,
        "--config" | "--key" | "--root" | "--credentials" | "--tls-cert" | "--tls-key"
    )
}

fn resolve(flag: &str, value: &str, base: Option<&Path>) -> String {
    match base {
        Some(dir) if is_path_flag(flag) && !value.is_empty() && Path::new(value).is_relative() => {
            dir.join(value).to_string_lossy().into_owned()
        }
        _ => value.to_string(),
    }
}

/// Render one scalar as the string a CLI would have received.
fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::Str(s) => Some(s.clone()),
        Value::Int(i) => Some(i.to_string()),
        _ => None,
    }
}

/// Convert one table's entries into argv.
///
/// * `key = "v"`  -> `--key v`
/// * `key = 7`    -> `--key 7`
/// * `key = [..]` -> the flag repeated once per element (repeatable flags)
/// * `key = true` -> the bare flag
/// * `key = false`-> nothing at all, so a default can be un-set by writing it
///   out explicitly rather than by deleting the line
pub fn section_from_table(table: &Table, base: Option<&Path>) -> Result<Section, String> {
    let name = table.name();
    let mut args = Vec::new();
    let mut flags = Vec::new();

    for entry in &table.entries {
        let flag = flag_name(&entry.key);
        // Recorded even for `false`, since "the file mentions this key" is what
        // suppresses the suite default, not "the file passes this flag".
        if !flags.contains(&flag) {
            flags.push(flag.clone());
        }
        match &entry.value {
            Value::Bool(true) => args.push(flag),
            Value::Bool(false) => {}
            Value::Arr(items) => {
                for item in items {
                    let Some(v) = scalar(item) else {
                        return Err(format!(
                            "line {}: [{}] {}: arrays may only hold strings or integers, found {}",
                            entry.line,
                            name,
                            entry.key,
                            item.type_name()
                        ));
                    };
                    args.push(flag.clone());
                    args.push(resolve(&flag, &v, base));
                }
            }
            other => {
                // Unreachable for the current Value set, but keeps the match
                // honest if minicloak's parser ever grows a type.
                let Some(v) = scalar(other) else {
                    return Err(format!(
                        "line {}: [{}] {}: unsupported value {}",
                        entry.line,
                        name,
                        entry.key,
                        other.type_name()
                    ));
                };
                args.push(flag.clone());
                args.push(resolve(&flag, &v, base));
            }
        }
    }

    Ok(Section { name, args, flags })
}

/// Parse a whole minisuite.toml. `known` lists the accepted section names; a
/// header outside that list is an error rather than a silently ignored typo.
pub fn parse_file(path: &Path, known: &[&str]) -> Result<Vec<Section>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    let base: Option<PathBuf> = path.parent().map(Path::to_path_buf);
    let tables = minicloak::toml::parse(&text).map_err(|e| format!("{}: {}", path.display(), e))?;

    let mut out: Vec<Section> = Vec::new();
    for table in &tables {
        let name = table.name();
        if !known.contains(&name.as_str()) {
            return Err(format!(
                "{}: line {}: unknown section [{}] (expected one of {})",
                path.display(),
                table.line,
                name,
                known.join(", ")
            ));
        }
        let section = section_from_table(table, base.as_deref())
            .map_err(|e| format!("{}: {}", path.display(), e))?;
        // A repeated header just continues the same section.
        match out.iter_mut().find(|s| s.name == section.name) {
            Some(existing) => {
                existing.args.extend(section.args);
                for f in section.flags {
                    if !existing.flags.contains(&f) {
                        existing.flags.push(f);
                    }
                }
            }
            None => out.push(section),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sec(text: &str) -> Section {
        let tables = minicloak::toml::parse(text).expect("parse");
        section_from_table(&tables[0], None).expect("convert")
    }

    #[test]
    fn underscores_and_dashes_both_become_dashed_flags() {
        assert_eq!(flag_name("smtp_bind"), "--smtp-bind");
        assert_eq!(flag_name("smtp-bind"), "--smtp-bind");
        assert_eq!(flag_name("bind"), "--bind");
        let s = sec("[minimail]\nsmtp_bind = \"127.0.0.1:1\"\nhttp-bind = \"127.0.0.1:2\"\n");
        assert_eq!(
            s.args,
            vec!["--smtp-bind", "127.0.0.1:1", "--http-bind", "127.0.0.1:2"]
        );
        assert!(s.sets("--smtp-bind") && s.sets("--http-bind"));
    }

    #[test]
    fn arrays_repeat_the_flag() {
        let s = sec("[minicloak]\nuser = [\"alice=alice\", \"bob=bob\"]\n");
        assert_eq!(s.args, vec!["--user", "alice=alice", "--user", "bob=bob"]);
    }

    #[test]
    fn true_is_a_bare_flag_and_false_is_omitted() {
        let s = sec("[minimail]\nanonymous = true\n");
        assert_eq!(s.args, vec!["--anonymous"]);

        let s = sec("[minimail]\nanonymous = false\n");
        assert!(s.args.is_empty());
        // ...but the key still counts as "set" so it can suppress a default.
        assert!(s.sets("--anonymous"));
    }

    #[test]
    fn integers_become_string_values() {
        let s = sec("[minicloak]\nkey_bits = 512\n");
        assert_eq!(s.args, vec!["--key-bits", "512"]);
    }

    #[test]
    fn relative_path_values_resolve_against_the_toml_directory() {
        let tables = minicloak::toml::parse("[minicloak]\nconfig = \"minicloak.toml\"\n").unwrap();
        let s = section_from_table(&tables[0], Some(Path::new("/etc/mini"))).unwrap();
        assert_eq!(s.args[0], "--config");
        assert!(s.args[1].ends_with("minicloak.toml"));
        assert!(s.args[1].len() > "minicloak.toml".len(), "{}", s.args[1]);

        // A non-path flag is never touched, even when it looks like one.
        let tables = minicloak::toml::parse("[minicloak]\nrealm = \"dev\"\n").unwrap();
        let s = section_from_table(&tables[0], Some(Path::new("/etc/mini"))).unwrap();
        assert_eq!(s.args, vec!["--realm", "dev"]);
    }

    #[test]
    fn absolute_paths_are_left_alone() {
        let abs = if cfg!(windows) {
            "C:/keys/k.pem"
        } else {
            "/keys/k.pem"
        };
        let text = format!("[minicloak]\nkey = \"{}\"\n", abs);
        let tables = minicloak::toml::parse(&text).unwrap();
        let s = section_from_table(&tables[0], Some(Path::new("/etc/mini"))).unwrap();
        assert_eq!(s.args, vec!["--key".to_string(), abs.to_string()]);
    }

    #[test]
    fn a_boolean_inside_an_array_is_rejected() {
        let tables = minicloak::toml::parse("[minicloak]\nuser = [true]\n").unwrap();
        let e = section_from_table(&tables[0], None).unwrap_err();
        assert!(e.contains("arrays may only hold"), "{}", e);
    }
}
