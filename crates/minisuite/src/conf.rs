// minisuite.toml -> argv for each service, plus the typed `[minisuite]` section.
//
// The suite deliberately does NOT know any crate's flag set. It turns each
// service `[section]` into a plain argument vector and hands that to the crate's
// own `parse_args`, so unknown keys, bad values and the resulting error messages
// all come from the crate that owns the flag. That keeps this file tiny and
// means a new flag in minimail works in minisuite.toml on the day it lands.
//
// `[minisuite]` is the exception: its keys configure the suite itself, so they
// are read into typed values instead of being turned into argv.
//
// The TOML parser itself is minicloak's (`minicloak::toml`), shared rather than
// duplicated: same subset, same line-numbered errors, no extra dependency.

use std::path::{Path, PathBuf};

use minicloak::toml::{Table, Value};

use crate::suggest::suggest;

/// Header of the section that holds the suite's own options.
pub const SUITE_SECTION: &str = "minisuite";

/// The `[minisuite]` section. Its keys are read by the suite itself and never
/// reach a crate, so they stay typed. `None` means the file did not set the key.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SuiteSettings {
    /// Relative paths are already joined onto the file's directory.
    pub data: Option<PathBuf>,
    pub bind_all: Option<bool>,
    pub landing_bind: Option<String>,
    pub landing: Option<bool>,
    /// Service names as written; validated by the caller.
    pub only: Option<Vec<String>>,
}

/// Everything read from one minisuite.toml.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConfigFile {
    pub services: Vec<Section>,
    pub suite: Option<SuiteSettings>,
}

/// One `[section]` of minisuite.toml, already converted to argv.
#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    /// Section header as written, e.g. `minimail`.
    pub name: String,
    /// The generated arguments, ready for the crate's `parse_args`.
    pub args: Vec<String>,
    /// For each element of `args`, the long flag it belongs to, so a flag whose
    /// service environment variable is set can be dropped as a whole.
    pub arg_flags: Vec<String>,
    /// Long flags this section sets, e.g. `--smtp-bind`. Used to decide which
    /// suite defaults to drop: a key in the file always wins over a default.
    pub flags: Vec<String>,
}

impl Section {
    pub fn sets(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f == flag)
    }

    /// The arguments, minus every flag `skip` rejects.
    pub fn args_except(&self, skip: &dyn Fn(&str) -> bool) -> Vec<String> {
        self.args
            .iter()
            .zip(&self.arg_flags)
            .filter(|(_, flag)| !skip(flag))
            .map(|(arg, _)| arg.clone())
            .collect()
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
    let mut arg_flags = Vec::new();
    let mut flags = Vec::new();

    for entry in &table.entries {
        let flag = flag_name(&entry.key);
        // Recorded even for `false`, since "the file mentions this key" is what
        // suppresses the suite default, not "the file passes this flag".
        if !flags.contains(&flag) {
            flags.push(flag.clone());
        }
        match &entry.value {
            Value::Bool(true) => {
                arg_flags.push(flag.clone());
                args.push(flag);
            }
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
                    arg_flags.push(flag.clone());
                    arg_flags.push(flag.clone());
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
                arg_flags.push(flag.clone());
                arg_flags.push(flag.clone());
                args.push(flag.clone());
                args.push(resolve(&flag, &v, base));
            }
        }
    }

    Ok(Section {
        name,
        args,
        arg_flags,
        flags,
    })
}

/// Reject a key the service does not accept, before it can surface as an
/// unrelated "unknown argument" from the service's own parser. The message names
/// the line and, when one is close, the key the author most likely meant.
pub fn check_keys(table: &Table, known_flags: &[&str]) -> Result<(), String> {
    for entry in &table.entries {
        let flag = flag_name(&entry.key);
        if known_flags.contains(&flag.as_str()) {
            continue;
        }
        let hint = match suggest(&flag, known_flags) {
            Some(s) => format!(" (did you mean `{}`?)", s.trim_start_matches("--")),
            None => String::new(),
        };
        return Err(format!(
            "line {}: [{}] unknown key `{}`{}",
            entry.line,
            table.name(),
            entry.key,
            hint
        ));
    }
    Ok(())
}

/// Read one `[minisuite]` table into `out`. A key repeated across headers keeps
/// the later value, the same way a repeated service header keeps appending.
fn suite_from_table(
    table: &Table,
    base: Option<&Path>,
    out: &mut SuiteSettings,
) -> Result<(), String> {
    for entry in &table.entries {
        let line = entry.line;
        // `landing-bind` and `landing_bind` are the same key, as for services.
        let key = entry.key.replace('-', "_");
        let wrong_type = |found: &Value| {
            format!(
                "line {}: [minisuite] {}: wrong type {}",
                line,
                entry.key,
                found.type_name()
            )
        };
        match (key.as_str(), &entry.value) {
            ("data", Value::Str(s)) => out.data = Some(resolve_dir(s, base)),
            ("landing_bind", Value::Str(s)) => out.landing_bind = Some(s.clone()),
            ("bind_all", Value::Bool(b)) => out.bind_all = Some(*b),
            ("landing", Value::Bool(b)) => out.landing = Some(*b),
            ("only", Value::Arr(items)) => {
                let mut names = Vec::new();
                for item in items {
                    match item {
                        Value::Str(s) => names.push(s.clone()),
                        other => {
                            return Err(format!(
                                "line {}: [minisuite] only: expected service names, found {}",
                                line,
                                other.type_name()
                            ))
                        }
                    }
                }
                out.only = Some(names);
            }
            ("data" | "landing_bind" | "bind_all" | "landing" | "only", other) => {
                return Err(wrong_type(other))
            }
            _ => {
                return Err(format!(
                    "line {}: [minisuite] unknown key {} (expected data, bind_all, landing_bind, landing or only)",
                    line, entry.key
                ))
            }
        }
    }
    Ok(())
}

/// `[minisuite] data` is a directory, not a flag value, so it is resolved here
/// rather than through the flag-based `resolve`.
fn resolve_dir(value: &str, base: Option<&Path>) -> PathBuf {
    match base {
        Some(dir) if Path::new(value).is_relative() => dir.join(value),
        _ => PathBuf::from(value),
    }
}

/// Parse a whole minisuite.toml. `known` maps each accepted service section to
/// the long flags its service accepts; `[minisuite]` is always accepted. A header
/// or key outside those lists is an error rather than a silently ignored typo.
pub fn parse_file(path: &Path, known: &[(&str, &[&str])]) -> Result<ConfigFile, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    let base: Option<PathBuf> = path.parent().map(Path::to_path_buf);
    let tables = minicloak::toml::parse(&text).map_err(|e| format!("{}: {}", path.display(), e))?;

    let mut file = ConfigFile::default();
    for table in &tables {
        let name = table.name();
        if name == SUITE_SECTION {
            let suite = file.suite.get_or_insert_with(SuiteSettings::default);
            suite_from_table(table, base.as_deref(), suite)
                .map_err(|e| format!("{}: {}", path.display(), e))?;
            continue;
        }
        let Some((_, flags)) = known.iter().find(|(section, _)| *section == name) else {
            let expected: Vec<&str> = known
                .iter()
                .map(|(section, _)| *section)
                .chain([SUITE_SECTION])
                .collect();
            return Err(format!(
                "{}: line {}: unknown section [{}] (expected one of {})",
                path.display(),
                table.line,
                name,
                expected.join(", ")
            ));
        };
        check_keys(table, flags).map_err(|e| format!("{}: {}", path.display(), e))?;
        let section = section_from_table(table, base.as_deref())
            .map_err(|e| format!("{}: {}", path.display(), e))?;
        // A repeated header just continues the same section.
        match file.services.iter_mut().find(|s| s.name == section.name) {
            Some(existing) => {
                existing.args.extend(section.args);
                existing.arg_flags.extend(section.arg_flags);
                for f in section.flags {
                    if !existing.flags.contains(&f) {
                        existing.flags.push(f);
                    }
                }
            }
            None => file.services.push(section),
        }
    }
    Ok(file)
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

    fn table(text: &str) -> Table {
        minicloak::toml::parse(text).expect("parse").remove(0)
    }

    #[test]
    fn known_keys_pass_in_either_spelling() {
        let t = table("[minicloak]\nrealm = \"dev\"\nkey_bits = 512\nno-cors = true\n");
        assert_eq!(check_keys(&t, minicloak::FLAGS), Ok(()));
    }

    #[test]
    fn a_typo_is_reported_with_its_line_and_a_suggestion() {
        let t = table("[minicloak]\nrealn = \"dev\"\n");
        assert_eq!(
            check_keys(&t, minicloak::FLAGS),
            Err("line 2: [minicloak] unknown key `realn` (did you mean `realm`?)".to_string())
        );
    }

    #[test]
    fn a_key_with_no_close_match_gets_no_suggestion() {
        let t = table("[minicloak]\nrealm = \"dev\"\nzzzzzzzz = 1\n");
        assert_eq!(
            check_keys(&t, minicloak::FLAGS),
            Err("line 3: [minicloak] unknown key `zzzzzzzz`".to_string())
        );
    }

    #[test]
    fn parse_file_names_the_file_and_the_line() {
        let path = std::env::temp_dir().join(format!(
            "minisuite-conf-test-{}-keys.toml",
            std::process::id()
        ));
        std::fs::write(&path, "[minicloak]\nrealn = \"dev\"\n").unwrap();
        let result = parse_file(&path, &[(minicloak::NAME, minicloak::FLAGS)]);
        std::fs::remove_file(&path).ok();
        let e = result.unwrap_err();
        assert_eq!(
            e,
            format!(
                "{}: line 2: [minicloak] unknown key `realn` (did you mean `realm`?)",
                path.display()
            )
        );
    }

    fn suite(text: &str, base: Option<&Path>) -> Result<SuiteSettings, String> {
        let tables = minicloak::toml::parse(text).expect("parse");
        let mut out = SuiteSettings::default();
        for t in &tables {
            suite_from_table(t, base, &mut out)?;
        }
        Ok(out)
    }

    #[test]
    fn minisuite_section_reads_every_key_typed() {
        let s = suite(
            "[minisuite]\ndata = \"state\"\nbind_all = true\nlanding-bind = \"127.0.0.1:9911\"\nlanding = false\nonly = [\"minicloak\", \"minimail\"]\n",
            Some(Path::new("/etc/mini")),
        )
        .unwrap();
        assert_eq!(s.data, Some(PathBuf::from("/etc/mini/state")));
        assert_eq!(s.bind_all, Some(true));
        assert_eq!(s.landing_bind.as_deref(), Some("127.0.0.1:9911"));
        assert_eq!(s.landing, Some(false));
        assert_eq!(
            s.only,
            Some(vec!["minicloak".to_string(), "minimail".to_string()])
        );
    }

    #[test]
    fn minisuite_data_keeps_absolute_paths() {
        let s = suite(
            "[minisuite]\ndata = \"/var/lib/mini\"\n",
            Some(Path::new("/etc/mini")),
        )
        .unwrap();
        assert_eq!(s.data, Some(PathBuf::from("/var/lib/mini")));
    }

    #[test]
    fn minisuite_keys_are_checked_for_name_and_type() {
        let e = suite("[minisuite]\ndatas = \"x\"\n", None).unwrap_err();
        assert!(e.contains("unknown key datas"), "{}", e);
        let e = suite("[minisuite]\nbind_all = \"yes\"\n", None).unwrap_err();
        assert!(e.contains("wrong type"), "{}", e);
        let e = suite("[minisuite]\nonly = [1]\n", None).unwrap_err();
        assert!(e.contains("expected service names"), "{}", e);
    }

    #[test]
    fn parse_file_takes_minisuite_and_still_rejects_other_sections() {
        let dir = std::env::temp_dir().join(format!("minisuite-conf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("minisuite.toml");
        let known = [
            (minicloak::NAME, minicloak::FLAGS),
            (minimail::NAME, minimail::FLAGS),
            (minibucket::NAME, minibucket::FLAGS),
        ];

        std::fs::write(
            &file,
            "[minisuite]\nlanding = false\n[minibucket]\nbind = \"127.0.0.1:9000\"\n",
        )
        .unwrap();
        let parsed = parse_file(&file, &known).unwrap();
        assert_eq!(parsed.suite.unwrap().landing, Some(false));
        assert_eq!(parsed.services.len(), 1);
        assert_eq!(parsed.services[0].name, "minibucket");

        std::fs::write(&file, "[minisuites]\nlanding = false\n").unwrap();
        let e = parse_file(&file, &known).unwrap_err();
        assert!(e.contains("unknown section [minisuites]"), "{}", e);
        assert!(e.contains("minisuite"), "{}", e);
    }
}
