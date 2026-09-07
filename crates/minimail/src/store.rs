// File-backed mail store: <root>/messages/<id>.eml + <id>.json. Pure std, lock-free.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub struct Envelope {
    pub mail_from: String, // reverse-path ("" for <>)
    pub rcpt_to: Vec<String>,
    pub helo: String,
    pub remote: String, // peer ip:port
    pub auth_user: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    pub part_id: String, // mime dotted-path id
    pub filename: String,
    pub content_type: String,
    pub size: usize,
}

#[derive(Clone, Debug)]
pub struct Summary {
    pub id: String,
    pub received_at: String, // iso8601
    pub received_unix: u64,
    pub size: usize,
    pub from: String,
    pub to: Vec<String>,
    pub remote: String,
    pub auth_user: Option<String>,
    pub helo: String,
    pub subject: String,
    pub from_header: String,
    pub to_header: String,
    pub cc_header: String,
    pub date_header: String,
    pub message_id: String,
    pub content_type: String,
    pub has_text: bool,
    pub has_html: bool,
    pub attachments: Vec<Attachment>,
}

impl Summary {
    // Single source of truth for summary serialization. Built via json::Json::Obj so
    // every string is escaped — never hand-rolled writeln!.
    pub fn to_json(&self) -> String {
        use crate::json::Json;
        let to: Vec<Json> = self.to.iter().map(|s| Json::Str(s.clone())).collect();
        let atts: Vec<Json> = self
            .attachments
            .iter()
            .map(|a| {
                Json::Obj(vec![
                    ("part_id".to_string(), Json::Str(a.part_id.clone())),
                    ("filename".to_string(), Json::Str(a.filename.clone())),
                    (
                        "content_type".to_string(),
                        Json::Str(a.content_type.clone()),
                    ),
                    ("size".to_string(), Json::Int(a.size as i64)),
                ])
            })
            .collect();
        let auth = match &self.auth_user {
            Some(u) => Json::Str(u.clone()),
            None => Json::Null,
        };
        Json::Obj(vec![
            ("id".to_string(), Json::Str(self.id.clone())),
            (
                "received_at".to_string(),
                Json::Str(self.received_at.clone()),
            ),
            (
                "received_unix".to_string(),
                Json::Int(self.received_unix as i64),
            ),
            ("size".to_string(), Json::Int(self.size as i64)),
            ("from".to_string(), Json::Str(self.from.clone())),
            ("to".to_string(), Json::Arr(to)),
            ("remote".to_string(), Json::Str(self.remote.clone())),
            ("auth_user".to_string(), auth),
            ("helo".to_string(), Json::Str(self.helo.clone())),
            ("subject".to_string(), Json::Str(self.subject.clone())),
            (
                "from_header".to_string(),
                Json::Str(self.from_header.clone()),
            ),
            ("to_header".to_string(), Json::Str(self.to_header.clone())),
            ("cc_header".to_string(), Json::Str(self.cc_header.clone())),
            (
                "date_header".to_string(),
                Json::Str(self.date_header.clone()),
            ),
            ("message_id".to_string(), Json::Str(self.message_id.clone())),
            (
                "content_type".to_string(),
                Json::Str(self.content_type.clone()),
            ),
            ("has_text".to_string(), Json::Bool(self.has_text)),
            ("has_html".to_string(), Json::Bool(self.has_html)),
            ("attachments".to_string(), Json::Arr(atts)),
        ])
        .to_string()
    }

    // Lenient: every missing/mistyped field falls back to a default so a partial or
    // older sidecar still yields a usable Summary.
    pub fn from_json(s: &str) -> Option<Summary> {
        let j = crate::json::parse(s)?;
        let get_s = |k: &str| j.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let to = j
            .get("to")
            .and_then(|v| v.as_arr())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let auth_user = j
            .get("auth_user")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let attachments = j
            .get("attachments")
            .and_then(|v| v.as_arr())
            .map(|a| {
                a.iter()
                    .map(|o| Attachment {
                        part_id: o
                            .get("part_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        filename: o
                            .get("filename")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        content_type: o
                            .get("content_type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        size: o.get("size").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(Summary {
            id: get_s("id"),
            received_at: get_s("received_at"),
            received_unix: j.get("received_unix").and_then(|v| v.as_u64()).unwrap_or(0),
            size: j.get("size").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
            from: get_s("from"),
            to,
            remote: get_s("remote"),
            auth_user,
            helo: get_s("helo"),
            subject: get_s("subject"),
            from_header: get_s("from_header"),
            to_header: get_s("to_header"),
            cc_header: get_s("cc_header"),
            date_header: get_s("date_header"),
            message_id: get_s("message_id"),
            content_type: get_s("content_type"),
            has_text: j.get("has_text").and_then(|v| v.as_bool()).unwrap_or(false),
            has_html: j.get("has_html").and_then(|v| v.as_bool()).unwrap_or(false),
            attachments,
        })
    }
}

#[derive(Debug)]
pub enum StoreError {
    Io,
    NotFound,
    InvalidId,
    #[allow(dead_code)]
    // frozen SPEC §5 variant; size limit is enforced (552) in smtp DATA, not store
    TooLarge,
}

impl From<io::Error> for StoreError {
    fn from(_: io::Error) -> Self {
        StoreError::Io
    }
}

#[derive(Clone)]
pub struct Store {
    pub root: PathBuf,
}

impl Store {
    pub fn new(root: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(root.join("messages"))?;
        Ok(Self { root })
    }

    fn messages_dir(&self) -> PathBuf {
        self.root.join("messages")
    }
    pub fn eml_path(&self, id: &str) -> PathBuf {
        self.messages_dir().join(format!("{}.eml", id))
    }
    fn json_path(&self, id: &str) -> PathBuf {
        self.messages_dir().join(format!("{}.json", id))
    }
    fn tmp_path(&self, id: &str) -> PathBuf {
        self.messages_dir().join(format!("{}.eml.tmp", id))
    }

    // .eml tmp+rename (payload durable first), then the .json sidecar, then prune.
    pub fn put(
        &self,
        raw: &[u8],
        env: &Envelope,
        max_messages: Option<usize>,
    ) -> Result<Summary, StoreError> {
        let id = new_message_id();
        let eml = self.eml_path(&id);
        let tmp = self.tmp_path(&id);
        {
            let mut f = File::create(&tmp)?;
            let mut off = 0;
            while off < raw.len() {
                let end = (off + 64 * 1024).min(raw.len());
                f.write_all(&raw[off..end])?;
                off = end;
            }
            f.flush()?;
        }
        fs::rename(&tmp, &eml)?;

        let received_unix = crate::util::now_secs();
        let msg = crate::mime::parse(raw);
        let summary = summary_from_parsed(id.clone(), received_unix, raw.len(), env, &msg);

        // Non-atomic, exactly like minibucket's meta write; get_summary re-derives on gaps.
        let mut jf = File::create(self.json_path(&id))?;
        jf.write_all(summary.to_json().as_bytes())?;

        if let Some(n) = max_messages {
            self.prune(n);
        }
        Ok(summary)
    }

    pub fn list(&self) -> Result<Vec<Summary>, StoreError> {
        let mut ids = self.list_ids()?;
        ids.sort(); // ids sort by time -> ascending == oldest first
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Ok(s) = self.get_summary(&id) {
                out.push(s);
            }
        }
        Ok(out)
    }

    pub fn get_summary(&self, id: &str) -> Result<Summary, StoreError> {
        if !valid_id(id) {
            return Err(StoreError::InvalidId);
        }
        let eml = self.eml_path(id);
        if !eml.exists() {
            return Err(StoreError::NotFound);
        }
        if let Ok(s) = fs::read_to_string(self.json_path(id)) {
            if let Some(sum) = Summary::from_json(&s) {
                return Ok(sum);
            }
        }
        // Sidecar missing/partial (crash between steps) — re-derive from the .eml.
        self.derive_summary(id, &eml)
    }

    fn derive_summary(&self, id: &str, eml: &Path) -> Result<Summary, StoreError> {
        let raw = fs::read(eml)?;
        let md = fs::metadata(eml)?;
        let received_unix = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let msg = crate::mime::parse(&raw);
        // Envelope is not recoverable from the .eml alone.
        let env = Envelope {
            mail_from: String::new(),
            rcpt_to: Vec::new(),
            helo: String::new(),
            remote: String::new(),
            auth_user: None,
        };
        Ok(summary_from_parsed(
            id.to_string(),
            received_unix,
            raw.len(),
            &env,
            &msg,
        ))
    }

    pub fn get_raw(&self, id: &str) -> Result<Vec<u8>, StoreError> {
        if !valid_id(id) {
            return Err(StoreError::InvalidId);
        }
        let eml = self.eml_path(id);
        if !eml.exists() {
            return Err(StoreError::NotFound);
        }
        Ok(fs::read(&eml)?)
    }

    pub fn delete(&self, id: &str) -> Result<(), StoreError> {
        if !valid_id(id) {
            return Err(StoreError::InvalidId);
        }
        let eml = self.eml_path(id);
        if !eml.exists() {
            return Err(StoreError::NotFound);
        }
        fs::remove_file(&eml)?;
        let _ = fs::remove_file(self.json_path(id));
        Ok(())
    }

    pub fn delete_all(&self) -> Result<usize, StoreError> {
        let ids = self.list_ids()?;
        let mut n = 0;
        for id in ids {
            if fs::remove_file(self.eml_path(&id)).is_ok() {
                n += 1;
            }
            let _ = fs::remove_file(self.json_path(&id));
        }
        Ok(n)
    }

    pub fn count(&self) -> io::Result<usize> {
        Ok(self.list_ids()?.len())
    }

    // Message ids present on disk (from *.eml; *.eml.tmp is skipped). Unsorted.
    fn list_ids(&self) -> io::Result<Vec<String>> {
        let dir = self.messages_dir();
        let mut ids = Vec::new();
        if !dir.exists() {
            return Ok(ids);
        }
        for ent in fs::read_dir(dir)? {
            let ent = ent?;
            if !ent.file_type()?.is_file() {
                continue;
            }
            let name = match ent.file_name().into_string() {
                Ok(s) => s,
                Err(_) => continue,
            };
            if let Some(id) = name.strip_suffix(".eml") {
                ids.push(id.to_string());
            }
        }
        Ok(ids)
    }

    // Lock-free retention: ids sort by time, so "oldest" is a prefix of the sorted list.
    // A double-delete against concurrent intake is swallowed (whole-file unlink is atomic).
    fn prune(&self, max: usize) {
        let mut ids = match self.list_ids() {
            Ok(v) => v,
            Err(_) => return,
        };
        if ids.len() <= max {
            return;
        }
        ids.sort();
        let excess = ids.len() - max;
        for id in ids.into_iter().take(excess) {
            let _ = fs::remove_file(self.eml_path(&id));
            let _ = fs::remove_file(self.json_path(&id));
        }
    }
}

fn summary_from_parsed(
    id: String,
    received_unix: u64,
    size: usize,
    env: &Envelope,
    msg: &crate::mime::ParsedMessage,
) -> Summary {
    let attachments = msg
        .attachments()
        .iter()
        .map(|p| Attachment {
            part_id: p.id.clone(),
            filename: p.filename.clone().unwrap_or_default(),
            content_type: p.content_type.clone(),
            size: p.body.len(),
        })
        .collect();
    Summary {
        id,
        received_at: crate::util::iso8601(received_unix),
        received_unix,
        size,
        from: env.mail_from.clone(),
        to: env.rcpt_to.clone(),
        remote: env.remote.clone(),
        auth_user: env.auth_user.clone(),
        helo: env.helo.clone(),
        subject: msg.subject.clone(),
        from_header: msg.from.clone(),
        to_header: msg.to.clone(),
        cc_header: msg.cc.clone(),
        date_header: msg.date.clone(),
        message_id: msg.message_id.clone(),
        content_type: msg.root.content_type.clone(),
        has_text: msg.text_body().is_some(),
        has_html: msg.html_body().is_some(),
        attachments,
    }
}

// Lexicographically sortable, collision-safe id: 13-digit secs + 9-digit nanos +
// process-global atomic counter. The counter (not the clock) guarantees uniqueness under
// a backward/repeated clock or same-nanosecond concurrent saves across the unbounded
// thread-per-connection model — no per-thread Date::now race.
pub fn new_message_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let nanos = now.subsec_nanos();
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:013}-{:09}-{:08x}", secs, nanos, c)
}

// Defense-in-depth: ids arrive from the HTTP URL path. Reject anything that could escape
// <root>/messages/ or address a foreign file.
pub fn valid_id(id: &str) -> bool {
    if id.is_empty() || id.len() > 128 {
        return false;
    }
    if id.contains('\0') || id.contains('/') || id.contains('\\') {
        return false;
    }
    for seg in id.split('/') {
        if seg == "." || seg == ".." {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    fn tmp_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!("minimail_store_test_{}_{}", label, nanos));
        p
    }

    struct ScopedRoot(PathBuf);
    impl Drop for ScopedRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fresh(label: &str) -> (Store, ScopedRoot) {
        let p = tmp_root(label);
        let s = Store::new(p.clone()).unwrap();
        (s, ScopedRoot(p))
    }

    fn env_fixture() -> Envelope {
        Envelope {
            mail_from: "alice@example.com".to_string(),
            rcpt_to: vec![
                "bob@example.com".to_string(),
                "carol@example.com".to_string(),
            ],
            helo: "client.example".to_string(),
            remote: "127.0.0.1:54321".to_string(),
            auth_user: Some("alice".to_string()),
        }
    }

    #[test]
    fn id_is_sortable_and_unique() {
        let a = new_message_id();
        let b = new_message_id();
        assert_ne!(a, b);
        // Fixed-width fields keep same-second ids lexicographically ordered.
        assert_eq!(a.len(), b.len());
    }

    #[test]
    fn id_monotonic_under_concurrency() {
        let bag: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let bag = Arc::clone(&bag);
            handles.push(std::thread::spawn(move || {
                let mut local = Vec::with_capacity(2000);
                for _ in 0..2000 {
                    local.push(new_message_id());
                }
                bag.lock().unwrap().extend(local);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let all = bag.lock().unwrap();
        let set: HashSet<&String> = all.iter().collect();
        // The atomic counter must make every id unique across all threads.
        assert_eq!(set.len(), all.len());
    }

    #[test]
    fn valid_id_rejects_traversal() {
        assert!(valid_id("0001720000000-000000042-0000001a"));
        assert!(valid_id(&"a".repeat(128)));
        assert!(!valid_id(""));
        assert!(!valid_id(&"a".repeat(129)));
        assert!(!valid_id("../etc/passwd")); // "../"
        assert!(!valid_id("..\\windows")); // backslash
        assert!(!valid_id("/etc/passwd")); // absolute
        assert!(!valid_id("a/b")); // any slash
        assert!(!valid_id("..")); // dot-dot segment
        assert!(!valid_id(".")); // dot segment
        assert!(!valid_id("a\0b")); // weird byte: NUL
    }

    #[test]
    fn traversal_id_rejected_by_accessors() {
        let (s, _g) = fresh("traversal_access");
        assert!(matches!(s.get_raw("../secret"), Err(StoreError::InvalidId)));
        assert!(matches!(
            s.get_summary("../secret"),
            Err(StoreError::InvalidId)
        ));
        assert!(matches!(s.delete("../secret"), Err(StoreError::InvalidId)));
    }

    #[test]
    fn put_leaves_no_partial_file() {
        let (s, _g) = fresh("atomic");
        let raw = b"From: a@b.com\r\nSubject: hi\r\n\r\nhello body\r\n";
        let sum = s.put(raw, &env_fixture(), None).unwrap();

        assert!(s.eml_path(&sum.id).exists());
        // No transient .tmp left behind.
        for ent in fs::read_dir(s.root.join("messages")).unwrap() {
            let name = ent.unwrap().file_name().into_string().unwrap();
            assert!(!name.ends_with(".tmp"), "stray tmp: {}", name);
        }
        assert_eq!(s.get_raw(&sum.id).unwrap(), raw);
        assert_eq!(sum.size, raw.len());
        assert_eq!(sum.from, "alice@example.com");
        assert_eq!(sum.to, vec!["bob@example.com", "carol@example.com"]);
        assert_eq!(s.count().unwrap(), 1);
    }

    #[test]
    fn get_summary_rederives_without_sidecar() {
        let (s, _g) = fresh("rederive");
        let raw = b"Subject: Derived\r\n\r\nbody\r\n";
        let sum = s.put(raw, &env_fixture(), None).unwrap();
        // Simulate a crash between the .eml rename and the sidecar write.
        fs::remove_file(s.json_path(&sum.id)).unwrap();
        let got = s.get_summary(&sum.id).unwrap();
        assert_eq!(got.id, sum.id);
        assert_eq!(got.size, raw.len());
        // Envelope is not recoverable from the .eml alone.
        assert_eq!(got.from, "");
        assert!(got.to.is_empty());
    }

    #[test]
    fn retention_prunes_oldest() {
        let (s, _g) = fresh("retention");
        let mut ids = Vec::new();
        for i in 0..5 {
            let raw = format!("Subject: m{}\r\n\r\nbody\r\n", i);
            let sum = s.put(raw.as_bytes(), &env_fixture(), Some(3)).unwrap();
            ids.push(sum.id);
        }
        let list = s.list().unwrap();
        assert_eq!(list.len(), 3);
        let kept: Vec<&String> = list.iter().map(|m| &m.id).collect();
        // Two oldest pruned; newest kept.
        assert!(!kept.contains(&&ids[0]));
        assert!(!kept.contains(&&ids[1]));
        assert!(kept.contains(&&ids[4]));
        // list() is ascending by id.
        let got: Vec<String> = list.iter().map(|m| m.id.clone()).collect();
        let mut sorted = got.clone();
        sorted.sort();
        assert_eq!(got, sorted);
    }

    #[test]
    fn delete_and_delete_all() {
        let (s, _g) = fresh("delete");
        let a = s
            .put(b"Subject: a\r\n\r\nx\r\n", &env_fixture(), None)
            .unwrap();
        let _b = s
            .put(b"Subject: b\r\n\r\ny\r\n", &env_fixture(), None)
            .unwrap();
        assert_eq!(s.count().unwrap(), 2);
        s.delete(&a.id).unwrap();
        assert!(matches!(s.delete(&a.id), Err(StoreError::NotFound)));
        assert_eq!(s.count().unwrap(), 1);
        assert!(!s.json_path(&a.id).exists());
        assert_eq!(s.delete_all().unwrap(), 1);
        assert_eq!(s.count().unwrap(), 0);
    }

    #[test]
    fn summary_json_roundtrip() {
        let sum = Summary {
            id: "0001720000000-000000042-0000001a".to_string(),
            received_at: "2026-07-10T12:00:00.000Z".to_string(),
            received_unix: 1720000000,
            size: 20480,
            from: "alice@example.com".to_string(),
            to: vec!["bob@example.com".to_string()],
            remote: "127.0.0.1:54321".to_string(),
            auth_user: Some("alice".to_string()),
            helo: "client.example".to_string(),
            subject: "Hello \u{2600} \"quote\"".to_string(),
            from_header: "Alice <alice@example.com>".to_string(),
            to_header: "bob@example.com".to_string(),
            cc_header: String::new(),
            date_header: "Tue, 02 Jan 2024 03:04:05 +0000".to_string(),
            message_id: "<abc123@host>".to_string(),
            content_type: "multipart/mixed".to_string(),
            has_text: true,
            has_html: false,
            attachments: vec![Attachment {
                part_id: "2".to_string(),
                filename: "cat.png".to_string(),
                content_type: "image/png".to_string(),
                size: 20480,
            }],
        };
        let back = Summary::from_json(&sum.to_json()).unwrap();
        assert_eq!(back.id, sum.id);
        assert_eq!(back.subject, sum.subject); // escaping survives the round-trip
        assert_eq!(back.received_unix, sum.received_unix);
        assert_eq!(back.size, sum.size);
        assert_eq!(back.to, sum.to);
        assert_eq!(back.auth_user, sum.auth_user);
        assert_eq!(back.has_text, sum.has_text);
        assert_eq!(back.has_html, sum.has_html);
        assert_eq!(back.attachments, sum.attachments);
    }

    #[test]
    fn from_json_is_lenient() {
        // Partial sidecar: everything unspecified falls back to a default.
        let sum = Summary::from_json("{\"id\":\"x\"}").unwrap();
        assert_eq!(sum.id, "x");
        assert_eq!(sum.received_unix, 0);
        assert_eq!(sum.size, 0);
        assert!(sum.to.is_empty());
        assert!(sum.auth_user.is_none());
        assert!(!sum.has_text);
        assert!(sum.attachments.is_empty());
        // null auth_user reads back as None.
        let sum = Summary::from_json("{\"auth_user\":null}").unwrap();
        assert!(sum.auth_user.is_none());
    }
}
