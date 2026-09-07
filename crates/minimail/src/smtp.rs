// SMTP sink server (RFC 5321/4954/3207): pure Session state machine + thin serve() driver.
// Never relays. Pure std (+ optional tls). See SPEC §6.

use std::io::{self, BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::time::Duration;

use crate::events::Event;
use crate::server::Server;
use crate::store::Envelope;
use crate::stream::Stream;

// base64("Username:") / base64("Password:") — the exact AUTH LOGIN challenges (RFC 4954).
const CHALLENGE_USERNAME: &str = "VXNlcm5hbWU6";
const CHALLENGE_PASSWORD: &str = "UGFzc3dvcmQ6";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub code: u16,
    pub lines: Vec<String>,
}

#[derive(Debug)]
pub enum Step {
    Reply(Reply),
    NeedData(Reply),
    // Only produced under `tls`; the default build returns 502 for STARTTLS instead.
    #[cfg_attr(not(feature = "tls"), allow(dead_code))]
    StartTls(Reply),
    Quit(Reply),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Cmd {
    Helo(String),
    Ehlo(String),
    Mail(String),
    Rcpt(String),
    Data,
    Rset,
    Noop,
    Quit,
    Vrfy(String),
    Expn(String),
    Help,
    Auth(String),
    StartTls,
    AuthContinue(String),
    Unknown,
}

enum AuthState {
    None,
    Plain,
    LoginUser,
    LoginPass(String),
}

enum State {
    Start,
    Greeted,
    Mail,
    Rcpt,
}

pub struct Session<'a> {
    srv: &'a Server,
    peer: String,
    helo: Option<String>,
    esmtp: bool,
    state: State,
    // Whether this connection is already TLS; only consulted under the `tls` feature.
    #[cfg_attr(not(feature = "tls"), allow(dead_code))]
    tls: bool,
    authed: Option<String>,
    auth_state: AuthState,
    mail_from: Option<String>,
    rcpt_to: Vec<String>,
    errors: u32,
}

impl<'a> Session<'a> {
    pub fn new(srv: &'a Server, peer: Option<SocketAddr>, tls: bool) -> Self {
        Session {
            srv,
            peer: peer.map(|p| p.to_string()).unwrap_or_default(),
            helo: None,
            esmtp: false,
            state: State::Start,
            tls,
            authed: None,
            auth_state: AuthState::None,
            mail_from: None,
            rcpt_to: Vec::new(),
            errors: 0,
        }
    }

    pub fn greeting(&self) -> Reply {
        Reply {
            code: 220,
            lines: vec![format!("{} ESMTP minimail ready", self.srv.hostname)],
        }
    }

    /// PURE: no socket IO. Advances state and returns the wire action. AUTH
    /// continuation lines are routed here too (auth_state decides).
    pub fn handle_line(&mut self, line: &str) -> Step {
        let step = self.dispatch(line);
        self.finish(step)
    }

    /// PURE (socket-free): store the dot-unstuffed DATA payload, publish, reset txn.
    pub fn on_data(&mut self, raw: &[u8]) -> Reply {
        let env = Envelope {
            mail_from: self.mail_from.clone().unwrap_or_default(),
            rcpt_to: self.rcpt_to.clone(),
            helo: self.helo.clone().unwrap_or_default(),
            remote: self.peer.clone(),
            auth_user: self.authed.clone(),
        };
        let result = self.srv.store.put(raw, &env, self.srv.max_messages);
        self.abort_txn();
        match result {
            Ok(summary) => {
                let id = summary.id.clone();
                self.srv.hub.publish(Event::Message(summary.to_json()));
                self.reply(250, "2.0.0", &format!("Ok: queued as {id}"))
            }
            Err(_) => self.reply(
                451,
                "4.3.0",
                "Requested action aborted: local error in processing",
            ),
        }
    }

    /// Forget helo + auth after a successful STARTTLS handshake (RFC 3207).
    #[cfg_attr(not(feature = "tls"), allow(dead_code))]
    pub fn reset_after_starttls(&mut self) {
        self.helo = None;
        self.esmtp = false;
        self.authed = None;
        self.auth_state = AuthState::None;
        self.mail_from = None;
        self.rcpt_to.clear();
        self.state = State::Start;
        self.tls = true;
    }

    // ---- internal ----

    fn dispatch(&mut self, line: &str) -> Step {
        let cmd = if matches!(self.auth_state, AuthState::None) {
            parse_command(line)
        } else {
            Cmd::AuthContinue(line.to_string())
        };
        match cmd {
            Cmd::AuthContinue(blob) => self.cmd_auth_continue(&blob),
            Cmd::Ehlo(d) => self.cmd_ehlo(&d),
            Cmd::Helo(d) => self.cmd_helo(&d),
            Cmd::Mail(p) => self.cmd_mail(&p),
            Cmd::Rcpt(p) => self.cmd_rcpt(&p),
            Cmd::Data => self.cmd_data(),
            Cmd::Rset => {
                self.reset_txn();
                self.state = if self.helo.is_some() {
                    State::Greeted
                } else {
                    State::Start
                };
                Step::Reply(self.reply(250, "2.0.0", "Ok"))
            }
            Cmd::Noop => Step::Reply(self.reply(250, "2.0.0", "Ok")),
            Cmd::Quit => Step::Quit(self.reply(
                221,
                "2.0.0",
                &format!("{} closing connection", self.srv.hostname),
            )),
            Cmd::Vrfy(_) => {
                Step::Reply(self.reply(252, "2.1.5", "Cannot VRFY user, but will accept message"))
            }
            Cmd::Expn(_) => Step::Reply(self.reply(502, "5.5.1", "Command not implemented")),
            Cmd::Help => Step::Reply(self.reply(
                214,
                "2.0.0",
                "Commands: HELO EHLO MAIL RCPT DATA RSET NOOP VRFY QUIT AUTH STARTTLS HELP",
            )),
            Cmd::Auth(p) => self.cmd_auth(&p),
            Cmd::StartTls => self.cmd_starttls(),
            Cmd::Unknown => Step::Reply(self.reply(500, "5.5.2", "Command unrecognized")),
        }
    }

    fn cmd_ehlo(&mut self, domain: &str) -> Step {
        self.helo = Some(domain.to_string());
        self.esmtp = true;
        self.reset_txn();
        self.state = State::Greeted;
        let mut lines = vec![
            format!("{} greets {domain}", self.srv.hostname),
            "PIPELINING".to_string(),
            "8BITMIME".to_string(),
            "SMTPUTF8".to_string(),
            format!("SIZE {}", self.srv.max_size),
            "ENHANCEDSTATUSCODES".to_string(),
            "AUTH PLAIN LOGIN".to_string(),
        ];
        #[cfg(feature = "tls")]
        if self.srv.tls.is_some() && !self.tls {
            lines.push("STARTTLS".to_string());
        }
        lines.push("HELP".to_string());
        Step::Reply(Reply { code: 250, lines })
    }

    fn cmd_helo(&mut self, domain: &str) -> Step {
        self.helo = Some(domain.to_string());
        self.esmtp = false;
        self.reset_txn();
        self.state = State::Greeted;
        Step::Reply(Reply {
            code: 250,
            lines: vec![self.srv.hostname.clone()],
        })
    }

    fn cmd_mail(&mut self, param: &str) -> Step {
        match self.state {
            State::Start => {
                return Step::Reply(self.reply(503, "5.5.1", "Bad sequence of commands"))
            }
            State::Mail | State::Rcpt => {
                return Step::Reply(self.reply(503, "5.5.1", "Bad sequence of commands"))
            }
            State::Greeted => {}
        }
        if self.srv.require_auth && self.authed.is_none() {
            return Step::Reply(self.reply(530, "5.7.0", "Authentication required"));
        }
        let addr = match parse_addr(param) {
            Some(a) => a,
            None => return Step::Reply(self.reply(501, "5.5.2", "Syntax error in parameters")),
        };
        if parse_size(param).is_some_and(|sz| sz > self.srv.max_size) {
            return Step::Reply(self.reply(552, "5.3.4", "Message size exceeds fixed limit"));
        }
        self.mail_from = Some(addr);
        self.state = State::Mail;
        Step::Reply(self.reply(250, "2.1.0", "Ok"))
    }

    fn cmd_rcpt(&mut self, param: &str) -> Step {
        match self.state {
            State::Mail | State::Rcpt => {}
            _ => return Step::Reply(self.reply(503, "5.5.1", "Bad sequence of commands")),
        }
        let addr = match parse_addr(param) {
            Some(a) => a,
            None => return Step::Reply(self.reply(501, "5.5.2", "Syntax error in parameters")),
        };
        if self.rcpt_to.len() >= 100 {
            return Step::Reply(self.reply(452, "4.5.3", "Too many recipients"));
        }
        self.rcpt_to.push(addr);
        self.state = State::Rcpt;
        Step::Reply(self.reply(250, "2.1.5", "Ok"))
    }

    fn cmd_data(&mut self) -> Step {
        match self.state {
            State::Rcpt => Step::NeedData(Reply {
                code: 354,
                lines: vec!["End data with <CR><LF>.<CR><LF>".to_string()],
            }),
            State::Mail => Step::Reply(self.reply(554, "5.5.1", "No valid recipients")),
            _ => Step::Reply(self.reply(503, "5.5.1", "Bad sequence of commands")),
        }
    }

    fn cmd_auth(&mut self, param: &str) -> Step {
        if self.authed.is_some() {
            return Step::Reply(self.reply(503, "5.5.1", "Bad sequence of commands"));
        }
        if !matches!(self.state, State::Start | State::Greeted) {
            return Step::Reply(self.reply(503, "5.5.1", "Bad sequence of commands"));
        }
        let (mech, rest) = match param.split_once(' ') {
            Some((m, r)) => (m, r.trim()),
            None => (param, ""),
        };
        match mech.to_ascii_uppercase().as_str() {
            "PLAIN" => {
                if rest.is_empty() {
                    self.auth_state = AuthState::Plain;
                    Step::Reply(Reply {
                        code: 334,
                        lines: vec![String::new()],
                    })
                } else {
                    self.finish_plain(rest)
                }
            }
            "LOGIN" => {
                if rest.is_empty() {
                    self.auth_state = AuthState::LoginUser;
                    Step::Reply(Reply {
                        code: 334,
                        lines: vec![CHALLENGE_USERNAME.to_string()],
                    })
                } else {
                    match crate::base64::decode(rest) {
                        Some(u) => {
                            self.auth_state =
                                AuthState::LoginPass(String::from_utf8_lossy(&u).into_owned());
                            Step::Reply(Reply {
                                code: 334,
                                lines: vec![CHALLENGE_PASSWORD.to_string()],
                            })
                        }
                        None => Step::Reply(self.reply(501, "5.5.2", "Syntax error in parameters")),
                    }
                }
            }
            _ => Step::Reply(self.reply(504, "5.5.4", "Unrecognized authentication mechanism")),
        }
    }

    fn cmd_auth_continue(&mut self, line: &str) -> Step {
        if line == "*" {
            self.auth_state = AuthState::None;
            return Step::Reply(self.reply(501, "5.5.2", "Syntax error in parameters"));
        }
        let st = std::mem::replace(&mut self.auth_state, AuthState::None);
        match st {
            AuthState::Plain => self.finish_plain(line),
            AuthState::LoginUser => match crate::base64::decode(line) {
                Some(u) => {
                    self.auth_state =
                        AuthState::LoginPass(String::from_utf8_lossy(&u).into_owned());
                    Step::Reply(Reply {
                        code: 334,
                        lines: vec![CHALLENGE_PASSWORD.to_string()],
                    })
                }
                None => Step::Reply(self.reply(501, "5.5.2", "Syntax error in parameters")),
            },
            AuthState::LoginPass(user) => match crate::base64::decode(line) {
                Some(p) => {
                    let pass = String::from_utf8_lossy(&p).into_owned();
                    self.complete_auth(user, &pass)
                }
                None => Step::Reply(self.reply(501, "5.5.2", "Syntax error in parameters")),
            },
            AuthState::None => Step::Reply(self.reply(503, "5.5.1", "Bad sequence of commands")),
        }
    }

    fn finish_plain(&mut self, b64: &str) -> Step {
        self.auth_state = AuthState::None;
        match decode_auth_plain(b64) {
            Some((user, pass)) => self.complete_auth(user, &pass),
            None => Step::Reply(self.reply(501, "5.5.2", "Syntax error in parameters")),
        }
    }

    fn complete_auth(&mut self, user: String, pass: &str) -> Step {
        // In anonymous mode auth is accepted unconditionally but still records the user.
        let ok = !self.srv.require_auth || self.verify(&user, pass);
        if ok {
            self.authed = Some(user);
            Step::Reply(self.reply(235, "2.7.0", "Authentication successful"))
        } else {
            Step::Reply(self.reply(535, "5.7.8", "Authentication credentials invalid"))
        }
    }

    fn verify(&self, user: &str, pass: &str) -> bool {
        match self.srv.creds.secret_for(user) {
            // Unknown user and bad password both fail (same 535 upstream — no leak).
            Some(secret) => crate::creds::constant_time_eq(secret.as_bytes(), pass.as_bytes()),
            None => false,
        }
    }

    fn cmd_starttls(&mut self) -> Step {
        #[cfg(feature = "tls")]
        {
            if !matches!(self.state, State::Start | State::Greeted) {
                return Step::Reply(self.reply(503, "5.5.1", "Bad sequence of commands"));
            }
            if self.srv.tls.is_some() && !self.tls {
                return Step::StartTls(self.reply(220, "2.0.0", "Ready to start TLS"));
            }
            Step::Reply(self.reply(502, "5.5.1", "Command not implemented"))
        }
        #[cfg(not(feature = "tls"))]
        {
            Step::Reply(self.reply(502, "5.5.1", "Command not implemented"))
        }
    }

    // Single-line reply builder. The enhanced-status triple is emitted only after
    // EHLO (esmtp); HELO and pre-greeting replies omit it (SPEC §6).
    fn reply(&self, code: u16, enh: &str, text: &str) -> Reply {
        let line = if self.esmtp && !enh.is_empty() {
            format!("{enh} {text}")
        } else {
            text.to_string()
        };
        Reply {
            code,
            lines: vec![line],
        }
    }

    // Every 5xx bumps the hard-error counter; past 10 the connection is dropped.
    fn finish(&mut self, step: Step) -> Step {
        let code = match &step {
            Step::Reply(r) | Step::NeedData(r) | Step::StartTls(r) | Step::Quit(r) => r.code,
        };
        if (500..600).contains(&code) {
            self.errors += 1;
            if self.errors > 10 {
                let host = self.srv.hostname.clone();
                return Step::Quit(self.reply(
                    421,
                    "4.7.0",
                    &format!("{host} too many errors, closing"),
                ));
            }
        }
        step
    }

    fn reset_txn(&mut self) {
        self.mail_from = None;
        self.rcpt_to.clear();
    }

    fn abort_txn(&mut self) {
        self.reset_txn();
        self.state = State::Greeted;
    }

    fn line_too_long(&mut self) -> Step {
        let step = Step::Reply(self.reply(500, "5.5.2", "Command line too long"));
        self.finish(step)
    }

    fn timeout_reply(&self) -> Reply {
        self.reply(
            421,
            "4.4.2",
            &format!("{} timeout, closing connection", self.srv.hostname),
        )
    }
}

// ---- pure, unit-tested helpers ----

/// ASCII-case-insensitive verb parse. The remainder is preserved verbatim for
/// MAIL/RCPT (where SIZE=/BODY= params live).
pub fn parse_command(line: &str) -> Cmd {
    let line = line.trim_end_matches(['\r', '\n']);
    let trimmed = line.trim_start();
    let (verb, rest) = match trimmed.split_once(' ') {
        Some((v, r)) => (v, r),
        None => (trimmed, ""),
    };
    match verb.to_ascii_uppercase().as_str() {
        "HELO" => Cmd::Helo(rest.trim().to_string()),
        "EHLO" => Cmd::Ehlo(rest.trim().to_string()),
        "MAIL" => Cmd::Mail(rest.to_string()),
        "RCPT" => Cmd::Rcpt(rest.to_string()),
        "DATA" => Cmd::Data,
        "RSET" => Cmd::Rset,
        "NOOP" => Cmd::Noop,
        "QUIT" => Cmd::Quit,
        "VRFY" => Cmd::Vrfy(rest.trim().to_string()),
        "EXPN" => Cmd::Expn(rest.trim().to_string()),
        "HELP" => Cmd::Help,
        "AUTH" => Cmd::Auth(rest.trim().to_string()),
        "STARTTLS" => Cmd::StartTls,
        _ => Cmd::Unknown,
    }
}

/// "FROM:<a@b> SIZE=1" -> "a@b"; "<>" -> ""; no angle brackets -> None.
pub fn parse_addr(param: &str) -> Option<String> {
    let start = param.find('<')?;
    let end = param[start + 1..].find('>')? + start + 1;
    Some(param[start + 1..end].trim().to_string())
}

/// SIZE=nnnn; all other params ignored. None if absent or unparseable.
pub fn parse_size(param: &str) -> Option<usize> {
    for tok in param.split_whitespace() {
        if tok
            .get(..5)
            .is_some_and(|h| h.eq_ignore_ascii_case("SIZE="))
        {
            return tok[5..].parse::<usize>().ok();
        }
    }
    None
}

/// authzid\0authcid\0passwd -> (authcid, passwd). None on bad base64 or structure.
pub fn decode_auth_plain(b64: &str) -> Option<(String, String)> {
    let raw = crate::base64::decode(b64)?;
    let mut parts = raw.splitn(3, |&b| b == 0);
    let _authzid = parts.next()?;
    let authcid = parts.next()?;
    let passwd = parts.next()?;
    Some((
        String::from_utf8_lossy(authcid).into_owned(),
        String::from_utf8_lossy(passwd).into_owned(),
    ))
}

/// Strip one leading '.' from each line (RFC 5321 §4.5.2). The payload passed here
/// has already had its terminating "." line removed.
pub fn dot_unstuff(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut at_line_start = true;
    for &b in raw {
        if at_line_start && b == b'.' {
            at_line_start = false; // drop exactly one dot, copy the rest of the line
            continue;
        }
        out.push(b);
        at_line_start = b == b'\n';
    }
    out
}

/// Byte index at which the payload ends (exclusive of the terminator), accepting
/// "\r\n.\r\n", bare "\n.\n", and an empty-body "." line at the very start.
pub fn find_data_terminator(buf: &[u8]) -> Option<usize> {
    if buf.starts_with(b".\r\n") || buf.starts_with(b".\n") {
        return Some(0);
    }
    find_pattern_terminator(buf)
}

fn find_pattern_terminator(buf: &[u8]) -> Option<usize> {
    // The CRLF (or bare LF) that precedes the "." line is the last data line's
    // own terminator and belongs to the message; only the ".<CRLF>" line itself
    // is stripped. Keep that trailing newline (SPEC §6: "terminating `.` line
    // removed, CRLF preserved").
    let crlf = find_sub(buf, b"\r\n.\r\n").map(|a| a + 2);
    let lf = find_sub(buf, b"\n.\n").map(|b| b + 1);
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

// ---- thin socket driver ----

enum LineResult {
    Eof,
    Line(String),
    TooLong,
}

/// Uniform handler signature shared with api::serve.
pub fn serve(srv: &Server, stream: Stream, peer: Option<SocketAddr>) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(300)))?;
    stream.set_write_timeout(Some(Duration::from_secs(300)))?;
    let _ = stream.set_nodelay(true);
    let tls = stream.is_tls();
    let mut session = Session::new(srv, peer, tls);
    let mut reader = BufReader::new(stream);

    write_reply(reader.get_mut(), &session.greeting())?;

    loop {
        let step = match read_command_line(&mut reader) {
            Ok(LineResult::Eof) => return Ok(()),
            Ok(LineResult::TooLong) => session.line_too_long(),
            Ok(LineResult::Line(l)) => session.handle_line(&l),
            Err(e) if is_timeout(&e) => {
                let _ = write_reply(reader.get_mut(), &session.timeout_reply());
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        match step {
            Step::Reply(r) => write_reply(reader.get_mut(), &r)?,
            Step::NeedData(r) => {
                write_reply(reader.get_mut(), &r)?;
                let dr = read_and_store_data(srv, &mut session, &mut reader)?;
                write_reply(reader.get_mut(), &dr)?;
            }
            Step::Quit(r) => {
                write_reply(reader.get_mut(), &r)?;
                return Ok(());
            }
            Step::StartTls(r) => {
                #[cfg(feature = "tls")]
                {
                    write_reply(reader.get_mut(), &r)?;
                    // RFC 3207 anti-injection: no plaintext may be buffered past 220.
                    if !reader.buffer().is_empty() {
                        let bad = session.reply(501, "5.5.2", "Syntax error in parameters");
                        let _ = write_reply(reader.get_mut(), &bad);
                        return Ok(());
                    }
                    let tcp = reader.into_inner().into_tcp()?;
                    let cfg = srv.tls.as_ref().unwrap();
                    match crate::tls::accept(tcp, cfg) {
                        Ok(tls_stream) => {
                            session.reset_after_starttls();
                            reader = BufReader::new(tls_stream);
                        }
                        Err(_) => return Ok(()),
                    }
                }
                #[cfg(not(feature = "tls"))]
                {
                    let _ = r; // unreachable: handle_line returns 502 when tls is off
                }
            }
        }
    }
}

fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

// Byte-transparent command reader: reads to LF, tolerates a missing CR, from_utf8_lossy
// so a UTF-8 reverse-path survives. Capped at 4096 octets.
fn read_command_line<R: BufRead>(r: &mut R) -> io::Result<LineResult> {
    let mut buf: Vec<u8> = Vec::with_capacity(128);
    loop {
        let mut b = [0u8; 1];
        let n = r.read(&mut b)?;
        if n == 0 {
            if buf.is_empty() {
                return Ok(LineResult::Eof);
            }
            break; // EOF mid-line: treat what we have as the final line
        }
        if b[0] == b'\n' {
            break;
        }
        if buf.len() >= 4096 {
            drain_to_lf(r)?;
            return Ok(LineResult::TooLong);
        }
        buf.push(b[0]);
    }
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    Ok(LineResult::Line(String::from_utf8_lossy(&buf).into_owned()))
}

fn drain_to_lf<R: BufRead>(r: &mut R) -> io::Result<()> {
    let mut b = [0u8; 1];
    loop {
        let n = r.read(&mut b)?;
        if n == 0 || b[0] == b'\n' {
            return Ok(());
        }
    }
}

// Separate 8-bit-clean DATA reader (never the UTF-8 command reader). Accumulates raw
// bytes, detects the terminator incrementally, enforces max_size while draining.
fn read_and_store_data<R: BufRead>(
    srv: &Server,
    session: &mut Session,
    reader: &mut R,
) -> io::Result<Reply> {
    let mut buf: Vec<u8> = Vec::new();
    let mut scanned = 0usize;
    let mut over = false;
    let mut dropped = false; // true once over-size bytes have been discarded from the front
    let mut chunk = [0u8; 8192];
    let term_at;
    loop {
        // Near the start use the full detector (it also handles the empty-body "."
        // terminator at index 0); once past the head, scan a 4-byte straddle window.
        // Once we've dropped front bytes, the head detector's leading-"." special
        // case must not fire on a mid-stream tail, so force the straddle scanner.
        let from = scanned.saturating_sub(4);
        let hit = if from == 0 && !dropped {
            find_data_terminator(&buf)
        } else {
            find_pattern_terminator(&buf[from..]).map(|rel| from + rel)
        };
        if let Some(p) = hit {
            term_at = p;
            break;
        }
        // Over the limit: we will reply 552 and never keep the body, so stop
        // hoarding it. Discard all but a 4-byte straddle tail (so a terminator
        // spanning the truncation boundary is still detected). Memory stays O(chunk).
        if over && buf.len() > 4 {
            let drop = buf.len() - 4;
            buf.drain(..drop);
            dropped = true;
        }
        scanned = buf.len();
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            term_at = buf.len(); // EOF before terminator: lenient, store what we have
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > srv.max_size {
            over = true;
        }
    }
    if over {
        session.abort_txn();
        return Ok(session.reply(552, "5.3.4", "Message size exceeds fixed limit"));
    }
    let payload = dot_unstuff(&buf[..term_at]);
    Ok(session.on_data(&payload))
}

fn write_reply<W: Write>(w: &mut W, reply: &Reply) -> io::Result<()> {
    let mut out = String::new();
    let n = reply.lines.len();
    for (i, line) in reply.lines.iter().enumerate() {
        let sep = if i + 1 < n { '-' } else { ' ' };
        out.push_str(&format!("{}{sep}{line}\r\n", reply.code));
    }
    if reply.lines.is_empty() {
        out.push_str(&format!("{} \r\n", reply.code));
    }
    w.write_all(out.as_bytes())?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::creds::Credentials;
    use crate::events::Hub;
    use crate::store::Store;
    use std::fs;
    use std::path::PathBuf;

    struct ScopedRoot(PathBuf);
    impl Drop for ScopedRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn scoped_root(tag: &str) -> ScopedRoot {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!("minimail_smtp_{tag}_{nanos}"));
        ScopedRoot(p)
    }

    fn test_server(require_auth: bool, root: &ScopedRoot) -> Server {
        let store = Store::new(root.0.clone()).unwrap();
        let mut creds = Credentials::new();
        creds.add("alice", "password");
        Server {
            store,
            creds,
            require_auth,
            hostname: "mail.test".to_string(),
            version: "0.1.0",
            max_size: 1000,
            max_messages: None,
            smtp_bind: "127.0.0.1:1025".to_string(),
            http_bind: "127.0.0.1:8025".to_string(),
            hub: Hub::new(),
            #[cfg(feature = "tls")]
            tls: None,
        }
    }

    fn code_of(step: &Step) -> u16 {
        match step {
            Step::Reply(r) | Step::NeedData(r) | Step::StartTls(r) | Step::Quit(r) => r.code,
        }
    }

    fn reply_of(step: Step) -> Reply {
        match step {
            Step::Reply(r) | Step::NeedData(r) | Step::StartTls(r) | Step::Quit(r) => r,
        }
    }

    #[test]
    fn happy_path_transcript() {
        let root = scoped_root("happy");
        let srv = test_server(false, &root);
        let mut s = Session::new(&srv, None, false);

        let g = s.greeting();
        assert_eq!(g.code, 220);
        assert!(g.lines[0].contains("ESMTP minimail ready"));

        let ehlo = s.handle_line("EHLO client.example");
        let r = reply_of(ehlo);
        assert_eq!(r.code, 250);
        assert!(r.lines[0].contains("greets client.example"));
        assert!(r.lines.iter().any(|l| l == "PIPELINING"));
        assert!(r.lines.iter().any(|l| l == "8BITMIME"));
        assert!(r.lines.iter().any(|l| l == "AUTH PLAIN LOGIN"));
        assert_eq!(r.lines.last().unwrap(), "HELP");

        assert_eq!(code_of(&s.handle_line("MAIL FROM:<a@b.com>")), 250);
        assert_eq!(code_of(&s.handle_line("RCPT TO:<c@d.com>")), 250);

        let data = s.handle_line("DATA");
        assert!(matches!(data, Step::NeedData(_)));
        assert_eq!(code_of(&data), 354);

        let reply = s.on_data(b"Subject: hi\r\n\r\nhello world\r\n");
        assert_eq!(reply.code, 250);
        assert!(reply.lines[0].contains("Ok: queued as "));

        let quit = s.handle_line("QUIT");
        assert!(matches!(quit, Step::Quit(_)));
        assert_eq!(code_of(&quit), 221);
    }

    #[test]
    fn multiline_ehlo_render() {
        let root = scoped_root("ehlo_render");
        let srv = test_server(false, &root);
        let mut s = Session::new(&srv, None, false);
        let r = reply_of(s.handle_line("EHLO x"));
        // Render via the real writer and assert the "-" vs " " separators.
        let mut wire_bytes: Vec<u8> = Vec::new();
        super::write_reply(&mut wire_bytes, &r).unwrap();
        let wire = String::from_utf8(wire_bytes).unwrap();
        assert!(wire.starts_with("250-mail.test greets x\r\n"));
        assert!(wire.contains("250-PIPELINING\r\n"));
        assert!(wire.ends_with("250 HELP\r\n"));
    }

    #[test]
    fn out_of_order_verbs_503() {
        let root = scoped_root("order");
        let srv = test_server(false, &root);
        let mut s = Session::new(&srv, None, false);
        s.handle_line("EHLO x");
        // RCPT before MAIL
        assert_eq!(code_of(&s.handle_line("RCPT TO:<c@d>")), 503);
        // DATA with no transaction
        assert_eq!(code_of(&s.handle_line("DATA")), 503);
        // MAIL before greeting
        let mut s2 = Session::new(&srv, None, false);
        assert_eq!(code_of(&s2.handle_line("MAIL FROM:<a@b>")), 503);
    }

    #[test]
    fn mail_params_accepted_never_555() {
        let root = scoped_root("params");
        let srv = test_server(false, &root);
        let mut s = Session::new(&srv, None, false);
        s.handle_line("EHLO x");
        let step = s.handle_line("MAIL FROM:<a@b> BODY=8BITMIME SMTPUTF8 AUTH=<>");
        assert_eq!(code_of(&step), 250);
    }

    #[test]
    fn size_exceeded_552() {
        let root = scoped_root("size");
        let srv = test_server(false, &root); // max_size = 1000
        let mut s = Session::new(&srv, None, false);
        s.handle_line("EHLO x");
        let step = s.handle_line("MAIL FROM:<a@b> SIZE=5000");
        assert_eq!(code_of(&step), 552);
    }

    #[test]
    fn rset_clears_state() {
        let root = scoped_root("rset");
        let srv = test_server(false, &root);
        let mut s = Session::new(&srv, None, false);
        s.handle_line("EHLO x");
        assert_eq!(code_of(&s.handle_line("MAIL FROM:<a@b>")), 250);
        assert_eq!(code_of(&s.handle_line("RCPT TO:<c@d>")), 250);
        assert_eq!(code_of(&s.handle_line("RSET")), 250);
        // transaction cleared: RCPT now out of sequence again
        assert_eq!(code_of(&s.handle_line("RCPT TO:<c@d>")), 503);
        // and MAIL is accepted afresh
        assert_eq!(code_of(&s.handle_line("MAIL FROM:<x@y>")), 250);
    }

    #[test]
    fn auth_plain_initial_good_and_bad() {
        let root = scoped_root("plain");
        let srv = test_server(true, &root);
        // good
        let mut s = Session::new(&srv, None, false);
        s.handle_line("EHLO x");
        let good = crate::base64::encode(b"\0alice\0password");
        assert_eq!(code_of(&s.handle_line(&format!("AUTH PLAIN {good}"))), 235);
        // bad password
        let mut s2 = Session::new(&srv, None, false);
        s2.handle_line("EHLO x");
        let bad = crate::base64::encode(b"\0alice\0wrong");
        assert_eq!(code_of(&s2.handle_line(&format!("AUTH PLAIN {bad}"))), 535);
    }

    #[test]
    fn auth_plain_challenge_flow() {
        let root = scoped_root("plainchal");
        let srv = test_server(true, &root);
        let mut s = Session::new(&srv, None, false);
        s.handle_line("EHLO x");
        let chal = reply_of(s.handle_line("AUTH PLAIN"));
        assert_eq!(chal.code, 334);
        assert_eq!(chal.lines[0], ""); // empty challenge line
                                       // continuation
        let mut s2 = Session::new(&srv, None, false);
        s2.handle_line("EHLO x");
        assert_eq!(code_of(&s2.handle_line("AUTH PLAIN")), 334);
        let blob = crate::base64::encode(b"\0alice\0password");
        assert_eq!(code_of(&s2.handle_line(&blob)), 235);
    }

    #[test]
    fn auth_login_good_and_bad() {
        let root = scoped_root("login");
        let srv = test_server(true, &root);
        // good
        let mut s = Session::new(&srv, None, false);
        s.handle_line("EHLO x");
        let u = reply_of(s.handle_line("AUTH LOGIN"));
        assert_eq!(u.code, 334);
        assert_eq!(u.lines[0], "VXNlcm5hbWU6");
        let p = reply_of(s.handle_line(&crate::base64::encode(b"alice")));
        assert_eq!(p.code, 334);
        assert_eq!(p.lines[0], "UGFzc3dvcmQ6");
        assert_eq!(
            code_of(&s.handle_line(&crate::base64::encode(b"password"))),
            235
        );
        // bad password
        let mut s2 = Session::new(&srv, None, false);
        s2.handle_line("EHLO x");
        s2.handle_line("AUTH LOGIN");
        s2.handle_line(&crate::base64::encode(b"alice"));
        assert_eq!(
            code_of(&s2.handle_line(&crate::base64::encode(b"nope"))),
            535
        );
    }

    #[test]
    fn unknown_user_same_code_as_bad_pass() {
        let root = scoped_root("unknown");
        let srv = test_server(true, &root);
        let mut s = Session::new(&srv, None, false);
        s.handle_line("EHLO x");
        let blob = crate::base64::encode(b"\0ghost\0whatever");
        assert_eq!(code_of(&s.handle_line(&format!("AUTH PLAIN {blob}"))), 535);
    }

    #[test]
    fn mail_requires_auth_when_gated() {
        let root = scoped_root("gated");
        let srv = test_server(true, &root);
        let mut s = Session::new(&srv, None, false);
        s.handle_line("EHLO x");
        assert_eq!(code_of(&s.handle_line("MAIL FROM:<a@b>")), 530);
    }

    #[test]
    fn error_counter_trips_421() {
        let root = scoped_root("errcount");
        let srv = test_server(false, &root);
        let mut s = Session::new(&srv, None, false);
        s.handle_line("EHLO x");
        let mut last = None;
        for _ in 0..11 {
            last = Some(s.handle_line("BOGUS"));
        }
        let last = last.unwrap();
        assert!(matches!(last, Step::Quit(_)));
        assert_eq!(code_of(&last), 421);
    }

    #[test]
    fn dot_unstuff_vectors() {
        assert_eq!(dot_unstuff(b".hidden\r\n"), b"hidden\r\n");
        // a line that is just ".." unstuffs to "."
        assert_eq!(dot_unstuff(b"..\r\n"), b".\r\n");
        // mid-line dot untouched
        assert_eq!(dot_unstuff(b"a.b\r\n"), b"a.b\r\n");
        // leading dot at very start
        assert_eq!(dot_unstuff(b".x"), b"x");
        assert_eq!(
            dot_unstuff(b"..foo\r\nbar\r\n..\r\n"),
            b".foo\r\nbar\r\n.\r\n"
        );
    }

    #[test]
    fn find_terminator_crlf_and_bare_lf() {
        // The last data line's CRLF/LF is preserved; only the ".<CRLF>" line is stripped.
        assert_eq!(find_data_terminator(b"hello\r\n.\r\n"), Some(7));
        assert_eq!(find_data_terminator(b"hello\n.\n"), Some(6));
        assert_eq!(find_data_terminator(b"no terminator here"), None);
        // empty body: "." right at the start
        assert_eq!(find_data_terminator(b".\r\n"), Some(0));
        assert_eq!(find_data_terminator(b".\n"), Some(0));
    }

    #[test]
    fn parse_helpers() {
        assert_eq!(
            parse_command("mail from:<a@b>"),
            Cmd::Mail("from:<a@b>".to_string())
        );
        assert_eq!(parse_command("QUIT"), Cmd::Quit);
        assert_eq!(parse_command("StArTtLs"), Cmd::StartTls);
        assert_eq!(parse_command("frobnicate"), Cmd::Unknown);

        assert_eq!(parse_addr("FROM:<a@b> SIZE=1"), Some("a@b".to_string()));
        assert_eq!(parse_addr("<>"), Some(String::new()));
        assert_eq!(parse_addr("no brackets"), None);

        assert_eq!(parse_size("FROM:<a@b> SIZE=4096"), Some(4096));
        assert_eq!(parse_size("FROM:<a@b> BODY=8BITMIME"), None);

        let blob = crate::base64::encode(b"\0user\0pw");
        assert_eq!(
            decode_auth_plain(&blob),
            Some(("user".to_string(), "pw".to_string()))
        );
        assert_eq!(decode_auth_plain("!!!notbase64!!!"), None);
    }

    #[test]
    fn login_challenges_match_base64() {
        assert_eq!(crate::base64::encode(b"Username:"), CHALLENGE_USERNAME);
        assert_eq!(crate::base64::encode(b"Password:"), CHALLENGE_PASSWORD);
    }

    #[test]
    fn reset_after_starttls_forgets_everything() {
        let root = scoped_root("starttls");
        let srv = test_server(false, &root);
        let mut s = Session::new(&srv, None, false);
        s.handle_line("EHLO x");
        s.handle_line("MAIL FROM:<a@b>");
        s.reset_after_starttls();
        // back at Start: MAIL is now out of sequence again
        assert_eq!(code_of(&s.handle_line("MAIL FROM:<a@b>")), 503);
    }
}
