// In-memory state for the auth-code flow, refresh tokens, and SSO sessions.
// Keys are opaque random strings supplied by the caller; this module never
// generates randomness. `expires_at` is an absolute unix second; an entry is
// expired when `now >= expires_at`.

use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct AuthCode {
    pub client_id: String,
    pub username: String,
    pub redirect_uri: String,
    pub scope: String,
    pub nonce: String,                 // "" if absent
    pub code_challenge: String,        // "" if absent
    pub code_challenge_method: String, // "S256" | "plain" | ""
    pub auth_time: u64,
    pub sid: String,
    pub expires_at: u64,
}

#[derive(Clone, Debug)]
pub struct RefreshToken {
    pub client_id: String,
    pub username: String, // "" for client_credentials
    pub scope: String,
    pub sid: String,
    pub auth_time: u64,
    pub expires_at: u64,
}

#[derive(Clone, Debug)]
pub struct Session {
    pub username: String,
    pub auth_time: u64,
    pub expires_at: u64,
}

#[derive(Default)]
pub struct Store {
    pub codes: HashMap<String, AuthCode>,
    pub refresh: HashMap<String, RefreshToken>,
    pub sessions: HashMap<String, Session>,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put_code(&mut self, code: String, c: AuthCode) {
        self.codes.insert(code, c);
    }

    /// Single-use: removes the code. Returns None if absent or expired at `now`.
    pub fn take_code(&mut self, code: &str, now: u64) -> Option<AuthCode> {
        // Remove unconditionally so a replayed expired code cannot linger.
        let c = self.codes.remove(code)?;
        if now >= c.expires_at {
            None
        } else {
            Some(c)
        }
    }

    pub fn put_refresh(&mut self, token: String, r: RefreshToken) {
        self.refresh.insert(token, r);
    }

    /// Rotation: removes it. None if absent or expired.
    pub fn take_refresh(&mut self, token: &str, now: u64) -> Option<RefreshToken> {
        let r = self.refresh.remove(token)?;
        if now >= r.expires_at {
            None
        } else {
            Some(r)
        }
    }

    pub fn revoke_refresh(&mut self, token: &str) -> bool {
        self.refresh.remove(token).is_some()
    }

    /// Drops every refresh token belonging to `sid`; returns how many. Used on logout.
    pub fn revoke_session_refresh(&mut self, sid: &str) -> usize {
        let before = self.refresh.len();
        self.refresh.retain(|_, r| r.sid != sid);
        before - self.refresh.len()
    }

    pub fn put_session(&mut self, sid: String, s: Session) {
        self.sessions.insert(sid, s);
    }

    /// Peeks without removing. None if absent or expired.
    pub fn get_session(&self, sid: &str, now: u64) -> Option<&Session> {
        let s = self.sessions.get(sid)?;
        if now >= s.expires_at {
            None
        } else {
            Some(s)
        }
    }

    pub fn drop_session(&mut self, sid: &str) -> Option<Session> {
        self.sessions.remove(sid)
    }

    /// Drops everything expired at `now`. Returns (codes, refresh, sessions) counts dropped.
    pub fn gc(&mut self, now: u64) -> (usize, usize, usize) {
        let c0 = self.codes.len();
        self.codes.retain(|_, c| now < c.expires_at);
        let r0 = self.refresh.len();
        self.refresh.retain(|_, r| now < r.expires_at);
        let s0 = self.sessions.len();
        self.sessions.retain(|_, s| now < s.expires_at);
        (
            c0 - self.codes.len(),
            r0 - self.refresh.len(),
            s0 - self.sessions.len(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(exp: u64) -> AuthCode {
        AuthCode {
            client_id: "app".into(),
            username: "alice".into(),
            redirect_uri: "http://x/cb".into(),
            scope: "openid".into(),
            nonce: "".into(),
            code_challenge: "".into(),
            code_challenge_method: "".into(),
            auth_time: 100,
            sid: "sid1".into(),
            expires_at: exp,
        }
    }

    fn refresh(sid: &str, exp: u64) -> RefreshToken {
        RefreshToken {
            client_id: "app".into(),
            username: "alice".into(),
            scope: "openid".into(),
            sid: sid.into(),
            auth_time: 100,
            expires_at: exp,
        }
    }

    fn session(exp: u64) -> Session {
        Session {
            username: "alice".into(),
            auth_time: 100,
            expires_at: exp,
        }
    }

    #[test]
    fn code_is_single_use() {
        let mut s = Store::new();
        s.put_code("c1".into(), code(1000));
        assert!(s.take_code("c1", 500).is_some());
        assert!(s.take_code("c1", 500).is_none());
    }

    #[test]
    fn expired_code_returns_none_and_is_removed() {
        let mut s = Store::new();
        s.put_code("c1".into(), code(1000));
        assert!(s.take_code("c1", 1000).is_none()); // now >= expires_at
        assert!(!s.codes.contains_key("c1")); // removed even though expired
    }

    #[test]
    fn refresh_rotation() {
        let mut s = Store::new();
        s.put_refresh("r1".into(), refresh("sid1", 2000));
        let r = s.take_refresh("r1", 100).unwrap();
        assert_eq!(r.client_id, "app");
        assert!(s.take_refresh("r1", 100).is_none());
    }

    #[test]
    fn expired_refresh_removed() {
        let mut s = Store::new();
        s.put_refresh("r1".into(), refresh("sid1", 1000));
        assert!(s.take_refresh("r1", 1000).is_none());
        assert!(!s.refresh.contains_key("r1"));
    }

    #[test]
    fn revoke_refresh_reports_presence() {
        let mut s = Store::new();
        s.put_refresh("r1".into(), refresh("sid1", 2000));
        assert!(s.revoke_refresh("r1"));
        assert!(!s.revoke_refresh("r1"));
    }

    #[test]
    fn revoke_session_refresh_only_matching_sid() {
        let mut s = Store::new();
        s.put_refresh("r1".into(), refresh("sid1", 2000));
        s.put_refresh("r2".into(), refresh("sid1", 2000));
        s.put_refresh("r3".into(), refresh("sid2", 2000));
        let n = s.revoke_session_refresh("sid1");
        assert_eq!(n, 2);
        assert!(s.refresh.contains_key("r3"));
        assert_eq!(s.refresh.len(), 1);
    }

    #[test]
    fn get_session_respects_expiry() {
        let mut s = Store::new();
        s.put_session("sid1".into(), session(1000));
        assert!(s.get_session("sid1", 999).is_some());
        assert!(s.get_session("sid1", 1000).is_none()); // expired but still stored
        assert!(s.sessions.contains_key("sid1"));
        assert!(s.get_session("missing", 0).is_none());
    }

    #[test]
    fn drop_session_removes() {
        let mut s = Store::new();
        s.put_session("sid1".into(), session(1000));
        assert!(s.drop_session("sid1").is_some());
        assert!(s.drop_session("sid1").is_none());
    }

    #[test]
    fn gc_counts_and_leaves_live_alone() {
        let mut s = Store::new();
        s.put_code("c_live".into(), code(2000));
        s.put_code("c_dead".into(), code(500));
        s.put_refresh("r_live".into(), refresh("sid1", 2000));
        s.put_refresh("r_dead".into(), refresh("sid1", 500));
        s.put_session("s_live".into(), session(2000));
        s.put_session("s_dead1".into(), session(500));
        s.put_session("s_dead2".into(), session(1000));

        let (nc, nr, ns) = s.gc(1000);
        assert_eq!((nc, nr, ns), (1, 1, 2));
        assert!(s.codes.contains_key("c_live"));
        assert!(s.refresh.contains_key("r_live"));
        assert!(s.sessions.contains_key("s_live"));
        assert_eq!(s.codes.len(), 1);
        assert_eq!(s.refresh.len(), 1);
        assert_eq!(s.sessions.len(), 1);
    }
}
