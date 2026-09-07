// MIME message parsing (RFC 5322 headers, RFC 2045-2047-2231 bodies). Pure std.
// Never panics; parses leniently and returns partial results on malformed input.

use crate::base64;
use crate::qp;

pub struct Part {
    pub headers: Vec<(String, String)>, // unfolded, original case, raw values
    pub content_type: String,           // lowercased "type/subtype"; default "text/plain"
    pub charset: Option<String>,        // lowercased; from Content-Type charset=
    pub encoding: String,               // lowercased CTE; default "7bit"
    pub disposition: Option<String>,    // "inline" | "attachment"
    pub filename: Option<String>,       // RFC 2231/2047-decoded
    pub content_id: Option<String>,     // Content-ID without <>
    pub is_attachment: bool,
    pub id: String,    // dotted path: root "0", children "1","1.1","2"...
    pub body: Vec<u8>, // transfer-decoded bytes (leaf only; empty for multipart)
    pub children: Vec<Part>,
}

pub struct ParsedMessage {
    pub headers: Vec<(String, String)>, // top-level, unfolded, original case
    pub root: Part,
    pub subject: String,    // RFC 2047-decoded
    pub from: String,       // decoded From:
    pub to: String,         // decoded To:
    pub cc: String,         // decoded Cc:
    pub date: String,       // raw Date: header value
    pub message_id: String, // raw Message-ID: header value
}

impl ParsedMessage {
    pub fn text_body(&self) -> Option<String> {
        self.first_body_leaf("text/plain")
    }

    pub fn html_body(&self) -> Option<String> {
        self.first_body_leaf("text/html")
    }

    fn first_body_leaf(&self, ct: &str) -> Option<String> {
        self.flat_parts()
            .into_iter()
            .find(|p| p.children.is_empty() && !p.is_attachment && p.content_type == ct)
            .map(|p| charset_to_utf8(&p.body, p.charset.as_deref().unwrap_or("utf-8")))
    }

    pub fn part_by_id(&self, id: &str) -> Option<&Part> {
        self.flat_parts().into_iter().find(|p| p.id == id)
    }

    pub fn attachments(&self) -> Vec<&Part> {
        self.flat_parts()
            .into_iter()
            .filter(|p| p.is_attachment)
            .collect()
    }

    pub fn flat_parts(&self) -> Vec<&Part> {
        let mut out = Vec::new();
        fn walk<'a>(p: &'a Part, out: &mut Vec<&'a Part>) {
            out.push(p);
            for c in &p.children {
                walk(c, out);
            }
        }
        walk(&self.root, &mut out);
        out
    }
}

pub fn parse(raw: &[u8]) -> ParsedMessage {
    let mut root = parse_part(raw, "0");
    // Classification needs to know which non-text leaves an HTML part references via cid:.
    let mut htmls = Vec::new();
    collect_htmls(&root, &mut htmls);
    classify(&mut root, &htmls);

    let headers = root.headers.clone();
    let subject = decode_header(&headers, "Subject");
    let from = decode_header(&headers, "From");
    let to = decode_header(&headers, "To");
    let cc = decode_header(&headers, "Cc");
    let date = header_value(&headers, "Date").unwrap_or("").to_string();
    let message_id = header_value(&headers, "Message-ID")
        .unwrap_or("")
        .to_string();
    ParsedMessage {
        headers,
        root,
        subject,
        from,
        to,
        cc,
        date,
        message_id,
    }
}

fn decode_header(headers: &[(String, String)], name: &str) -> String {
    header_value(headers, name)
        .map(decode_encoded_words)
        .unwrap_or_default()
}

fn parse_part(raw: &[u8], id: &str) -> Part {
    let (headers, body_off) = unfold_headers(raw);

    let ct_raw = header_value(&headers, "Content-Type").unwrap_or("");
    let (mut content_type, params) = parse_content_type(ct_raw);
    if content_type.is_empty() {
        content_type = "text/plain".to_string();
    }
    let charset = param_get(&params, "charset").map(|s| s.trim().to_ascii_lowercase());
    let encoding = header_value(&headers, "Content-Transfer-Encoding")
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "7bit".to_string());

    let disp_raw = header_value(&headers, "Content-Disposition").unwrap_or("");
    let (disp_type, disp_params) = parse_content_type(disp_raw);
    let disposition = if disp_type.is_empty() {
        None
    } else {
        Some(disp_type)
    };

    // filename: Content-Disposition filename -> Content-Type name -> None (both 2047-decoded).
    let filename = param_get(&disp_params, "filename")
        .map(decode_encoded_words)
        .filter(|f| !f.is_empty())
        .or_else(|| {
            param_get(&params, "name")
                .map(decode_encoded_words)
                .filter(|f| !f.is_empty())
        });

    let content_id = header_value(&headers, "Content-ID")
        .map(|s| {
            s.trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string()
        })
        .filter(|s| !s.is_empty());

    let body_bytes = raw.get(body_off..).unwrap_or(&[]);

    let mut part = Part {
        headers,
        content_type,
        charset,
        encoding,
        disposition,
        filename,
        content_id,
        is_attachment: false,
        id: id.to_string(),
        body: Vec::new(),
        children: Vec::new(),
    };

    if part.content_type.starts_with("multipart/") {
        if let Some(boundary) = param_get(&params, "boundary") {
            if !boundary.is_empty() {
                for (i, seg) in split_multipart(body_bytes, boundary)
                    .into_iter()
                    .enumerate()
                {
                    let child_id = if id == "0" {
                        (i + 1).to_string()
                    } else {
                        format!("{id}.{}", i + 1)
                    };
                    part.children.push(parse_part(&seg, &child_id));
                }
            }
        }
        // multipart nodes carry no body of their own
    } else {
        part.body = decode_body(&part.encoding, body_bytes);
    }

    part
}

fn collect_htmls(p: &Part, out: &mut Vec<String>) {
    if p.children.is_empty() && p.content_type == "text/html" {
        out.push(charset_to_utf8(
            &p.body,
            p.charset.as_deref().unwrap_or("utf-8"),
        ));
    }
    for c in &p.children {
        collect_htmls(c, out);
    }
}

fn classify(p: &mut Part, htmls: &[String]) {
    for c in &mut p.children {
        classify(c, htmls);
    }
    if p.content_type.starts_with("multipart/") {
        p.is_attachment = false;
        return;
    }
    let disp_attach = p.disposition.as_deref() == Some("attachment");
    let is_text = p.content_type == "text/plain" || p.content_type == "text/html";
    p.is_attachment = if disp_attach || p.filename.is_some() {
        true
    } else if !is_text {
        // A non-text leaf referenced by cid: from an HTML part is an inline related
        // resource, not an attachment.
        let referenced = p
            .content_id
            .as_ref()
            .is_some_and(|cid| htmls.iter().any(|h| references_cid(h, cid)));
        !referenced
    } else {
        false
    };
}

/// True if `html` references `cid:<id>` as a whole token. A plain substring test would
/// let `cid:img1` match a `cid:img10` reference, misclassifying the inline resource.
fn references_cid(html: &str, cid: &str) -> bool {
    let needle = format!("cid:{cid}");
    html.match_indices(&needle).any(|(idx, _)| {
        // The reference is a whole token only if it is not immediately followed by
        // another Content-ID token character. End-of-string counts as a boundary.
        match html[idx + needle.len()..].chars().next() {
            Some(c) => !is_cid_token_char(c),
            None => true,
        }
    })
}

fn is_cid_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-/=?^_`{|}~.@".contains(c)
}

/// Split raw bytes into unfolded (name, value) headers + the byte offset of the body.
/// Accepts CRLF or bare-LF line endings; header block ends at the first empty line.
pub fn unfold_headers(raw: &[u8]) -> (Vec<(String, String)>, usize) {
    let n = raw.len();
    let mut raw_lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut have_cur = false;
    let mut body_offset = n;
    let mut i = 0;

    while i < n {
        let start = i;
        while i < n && raw[i] != b'\n' {
            i += 1;
        }
        let mut end = i; // exclusive, at '\n' or n
        let next = if i < n { i + 1 } else { n };
        if end > start && raw[end - 1] == b'\r' {
            end -= 1;
        }
        let line = &raw[start..end];
        if line.is_empty() {
            body_offset = next;
            break;
        }
        if line[0] == b' ' || line[0] == b'\t' {
            // continuation: replace the fold + leading whitespace with a single space
            let cont = String::from_utf8_lossy(line);
            let cont = cont.trim_start();
            if have_cur {
                cur.push(' ');
                cur.push_str(cont);
            } else {
                cur = cont.to_string();
                have_cur = true;
            }
        } else {
            if have_cur {
                raw_lines.push(std::mem::take(&mut cur));
            }
            cur = String::from_utf8_lossy(line).into_owned();
            have_cur = true;
        }
        i = next;
    }
    if have_cur {
        raw_lines.push(cur);
    }

    let mut headers = Vec::with_capacity(raw_lines.len());
    for h in raw_lines {
        if let Some(idx) = h.find(':') {
            let name = h[..idx].trim().to_string();
            let value = h[idx + 1..].trim().to_string();
            if !name.is_empty() {
                headers.push((name, value));
            }
        }
    }
    (headers, body_offset)
}

pub fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn param_get<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

// ---- RFC 2047 encoded-words --------------------------------------------------

enum Tok {
    Ew(String, u8, Vec<u8>), // (charset, 'B'|'Q', decoded bytes)
    Ws(String),
    Text(String),
}

/// RFC 2047. Decodes B and Q encoded-words; concatenates the raw BYTES of
/// consecutive same-charset words BEFORE charset->UTF-8 (so a multibyte char
/// split across two words survives); elides linear whitespace BETWEEN two
/// adjacent encoded-words; preserves whitespace between an encoded-word and
/// plain text.
pub fn decode_encoded_words(s: &str) -> String {
    let toks = tokenize_words(s);
    let mut out = String::new();
    let mut pending: Option<(String, u8, Vec<u8>)> = None;
    let mut prev_ew = false;

    for k in 0..toks.len() {
        match &toks[k] {
            Tok::Ew(cs, enc, bytes) => {
                match &mut pending {
                    Some((pcs, penc, buf)) if *penc == *enc && pcs.eq_ignore_ascii_case(cs) => {
                        buf.extend_from_slice(bytes);
                    }
                    _ => {
                        flush_pending(&mut pending, &mut out);
                        pending = Some((cs.clone(), *enc, bytes.clone()));
                    }
                }
                prev_ew = true;
            }
            Tok::Text(t) => {
                flush_pending(&mut pending, &mut out);
                out.push_str(t);
                prev_ew = false;
            }
            Tok::Ws(w) => {
                let next_is_ew = matches!(toks.get(k + 1), Some(Tok::Ew(..)));
                if prev_ew && next_is_ew {
                    // whitespace between two adjacent encoded-words is elided
                } else {
                    flush_pending(&mut pending, &mut out);
                    out.push_str(w);
                }
                // Ws does not break encoded-word adjacency: prev_ew unchanged.
            }
        }
    }
    flush_pending(&mut pending, &mut out);
    out
}

fn flush_pending(pending: &mut Option<(String, u8, Vec<u8>)>, out: &mut String) {
    if let Some((cs, _enc, bytes)) = pending.take() {
        out.push_str(&charset_to_utf8(&bytes, &cs));
    }
}

fn tokenize_words(s: &str) -> Vec<Tok> {
    let b = s.as_bytes();
    let n = b.len();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < n {
        if let Some((cs, enc, bytes, end)) = try_encoded_word(b, i) {
            toks.push(Tok::Ew(cs, enc, bytes));
            i = end;
            continue;
        }
        if b[i] == b' ' || b[i] == b'\t' {
            let start = i;
            while i < n && (b[i] == b' ' || b[i] == b'\t') {
                i += 1;
            }
            toks.push(Tok::Ws(s[start..i].to_string()));
        } else {
            let start = i;
            i += 1; // current byte is not an encoded-word start (checked above)
            while i < n {
                if b[i] == b' ' || b[i] == b'\t' {
                    break;
                }
                if b[i] == b'=' && try_encoded_word(b, i).is_some() {
                    break;
                }
                i += 1;
            }
            toks.push(Tok::Text(s[start..i].to_string()));
        }
    }
    toks
}

/// Parse `=?charset?B|Q?text?=` at position `i`. Returns (charset, enc, decoded, end).
fn try_encoded_word(b: &[u8], i: usize) -> Option<(String, u8, Vec<u8>, usize)> {
    if b.get(i) != Some(&b'=') || b.get(i + 1) != Some(&b'?') {
        return None;
    }
    let mut j = i + 2;
    let cs_start = j;
    while j < b.len() && b[j] != b'?' {
        if b[j] == b' ' || b[j] == b'\t' {
            return None;
        }
        j += 1;
    }
    if j >= b.len() || j == cs_start {
        return None;
    }
    let charset_raw = &b[cs_start..j];
    j += 1; // past '?'
    let enc = *b.get(j)?;
    let enc = enc.to_ascii_uppercase();
    if enc != b'B' && enc != b'Q' {
        return None;
    }
    j += 1;
    if b.get(j) != Some(&b'?') {
        return None;
    }
    j += 1; // past second '?'
    let txt_start = j;
    while j + 1 < b.len() && !(b[j] == b'?' && b[j + 1] == b'=') {
        if b[j] == b' ' || b[j] == b'\t' {
            return None; // whitespace terminates a word; a raw space means malformed
        }
        j += 1;
    }
    if !(j + 1 < b.len() && b[j] == b'?' && b[j + 1] == b'=') {
        return None;
    }
    let text = &b[txt_start..j];
    let end = j + 2;

    // charset may carry an RFC 2231 language suffix: "utf-8*en"; drop it.
    let cs_full = String::from_utf8_lossy(charset_raw);
    let charset = cs_full.split('*').next().unwrap_or("").to_string();

    let decoded = match enc {
        b'B' => base64::decode(&String::from_utf8_lossy(text)).unwrap_or_default(),
        _ => qp::decode_q(text),
    };
    Some((charset, enc, decoded, end))
}

// ---- Content-Type / Content-Disposition params -------------------------------

/// One RFC 2231 parameter fragment: (section index, extended?, indexed?, raw value).
type ParamSection = (usize, bool, bool, String);

/// Returns (lowercased "type/subtype", params) with lowercased param keys.
/// Handles RFC 2231 continued (name*0*=,name*1*=) and extended
/// (name*=charset'lang'pct-encoded) parameters, reassembled and percent-decoded.
pub fn parse_content_type(v: &str) -> (String, Vec<(String, String)>) {
    let segs = split_semis(v);
    let ct = match segs.first() {
        Some(s) => s
            .trim()
            .to_ascii_lowercase()
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string(),
        None => String::new(),
    };

    // group by base attribute name, preserving first-seen order
    let mut groups: Vec<(String, Vec<ParamSection>)> = Vec::new();
    for seg in segs.iter().skip(1) {
        let eq = match seg.find('=') {
            Some(e) => e,
            None => continue,
        };
        let attr = seg[..eq].trim();
        if attr.is_empty() {
            continue;
        }
        let val = seg[eq + 1..].trim().to_string();
        let (base, section, ext, indexed) = parse_attr(attr);
        if let Some(g) = groups.iter_mut().find(|(b, _)| *b == base) {
            g.1.push((section, ext, indexed, val));
        } else {
            groups.push((base, vec![(section, ext, indexed, val)]));
        }
    }

    let mut params = Vec::with_capacity(groups.len());
    for (base, mut entries) in groups {
        entries.sort_by_key(|e| e.0);
        params.push((base, reassemble_param(&entries)));
    }
    (ct, params)
}

/// Parse an RFC 2231 attribute name into (base_lower, section, extended, indexed).
fn parse_attr(attr: &str) -> (String, usize, bool, bool) {
    let ext = attr.ends_with('*');
    let core = if ext { &attr[..attr.len() - 1] } else { attr };
    if let Some(star) = core.rfind('*') {
        if let Ok(n) = core[star + 1..].parse::<usize>() {
            return (core[..star].to_ascii_lowercase(), n, ext, true);
        }
    }
    (core.to_ascii_lowercase(), 0, ext, false)
}

fn reassemble_param(entries: &[ParamSection]) -> String {
    if entries.len() == 1 && !entries[0].1 && !entries[0].2 {
        return unquote(&entries[0].3);
    }
    let mut bytes: Vec<u8> = Vec::new();
    let mut charset = String::new();
    let mut first_ext = true;
    for (_section, ext, _indexed, raw) in entries {
        if *ext {
            let enc = if first_ext && raw.contains('\'') {
                let mut it = raw.splitn(3, '\'');
                let cs = it.next().unwrap_or("");
                let _lang = it.next().unwrap_or("");
                let rest = it.next().unwrap_or("");
                if charset.is_empty() {
                    charset = cs.to_string();
                }
                rest.to_string()
            } else {
                raw.clone()
            };
            first_ext = false;
            bytes.extend_from_slice(&pct_decode(&enc));
        } else {
            bytes.extend_from_slice(unquote(raw).as_bytes());
        }
    }
    if charset.is_empty() {
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        charset_to_utf8(&bytes, &charset)
    }
}

/// Split on ';' honoring double-quoted strings (and their backslash escapes).
fn split_semis(v: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    let mut esc = false;
    for ch in v.chars() {
        if esc {
            cur.push(ch);
            esc = false;
            continue;
        }
        match ch {
            '\\' if in_q => {
                cur.push(ch);
                esc = true;
            }
            '"' => {
                in_q = !in_q;
                cur.push(ch);
            }
            ';' if !in_q => {
                let t = cur.trim();
                if !t.is_empty() {
                    parts.push(t.to_string());
                }
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    let t = cur.trim();
    if !t.is_empty() {
        parts.push(t.to_string());
    }
    parts
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    let b = t.as_bytes();
    if b.len() >= 2 && b[0] == b'"' && b[b.len() - 1] == b'"' {
        let inner = &t[1..t.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else {
                out.push(c);
            }
        }
        out
    } else {
        t.to_string()
    }
}

fn pct_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ---- multipart tree ----------------------------------------------------------

/// Split a multipart body into raw part bytes. The line ending immediately
/// preceding a `--boundary` delimiter belongs to the delimiter and is stripped
/// from the part body (so binary attachments verify byte-exact). Preamble and
/// epilogue are discarded; a missing terminal boundary treats EOF as the close.
fn split_multipart(body: &[u8], boundary: &str) -> Vec<Vec<u8>> {
    let mut needle = Vec::with_capacity(boundary.len() + 2);
    needle.extend_from_slice(b"--");
    needle.extend_from_slice(boundary.as_bytes());

    let n = body.len();
    let mut parts: Vec<Vec<u8>> = Vec::new();
    let mut content_start: Option<usize> = None;
    let mut i = 0; // always at a line start

    while i <= n {
        if let Some((closing, after)) = delim_at(body, i, &needle) {
            if let Some(cs) = content_start {
                let end = strip_one_eol_before(body, i, cs);
                parts.push(body[cs..end].to_vec());
            }
            if closing {
                content_start = None;
                break;
            }
            content_start = Some(after);
            i = after;
            continue;
        }
        // advance to the next line start
        let mut j = i;
        while j < n && body[j] != b'\n' {
            j += 1;
        }
        if j < n {
            i = j + 1;
        } else {
            break;
        }
    }
    // missing terminal boundary: close a still-open part at EOF
    if let Some(cs) = content_start {
        let end = strip_one_eol_before(body, n, cs);
        parts.push(body[cs..end].to_vec());
    }
    parts
}

/// If a boundary delimiter begins at `pos` (a line start), return
/// (is_closing, index just past the delimiter line's newline).
fn delim_at(body: &[u8], pos: usize, needle: &[u8]) -> Option<(bool, usize)> {
    if pos + needle.len() > body.len() || &body[pos..pos + needle.len()] != needle {
        return None;
    }
    let mut j = pos + needle.len();
    let mut closing = false;
    if body.get(j) == Some(&b'-') && body.get(j + 1) == Some(&b'-') {
        closing = true;
        j += 2;
    }
    // remainder of the line may only be transport padding (whitespace)
    while j < body.len() && body[j] != b'\n' {
        if body[j] != b' ' && body[j] != b'\t' && body[j] != b'\r' {
            return None;
        }
        j += 1;
    }
    let after = if j < body.len() { j + 1 } else { j };
    Some((closing, after))
}

/// Content is body[start..delim_pos]; strip exactly one trailing line ending
/// (the CRLF/LF that belongs to the following delimiter).
fn strip_one_eol_before(body: &[u8], delim_pos: usize, start: usize) -> usize {
    let mut end = delim_pos;
    if end > start && body[end - 1] == b'\n' {
        end -= 1;
        if end > start && body[end - 1] == b'\r' {
            end -= 1;
        }
    }
    end
}

// ---- transfer + charset decoding --------------------------------------------

pub fn decode_body(encoding: &str, raw: &[u8]) -> Vec<u8> {
    match encoding.trim().to_ascii_lowercase().as_str() {
        "base64" => base64::decode(&String::from_utf8_lossy(raw)).unwrap_or_default(),
        "quoted-printable" => qp::decode(raw),
        _ => raw.to_vec(), // 7bit / 8bit / binary / unknown -> passthrough
    }
}

/// Transcode to UTF-8. Explicit: us-ascii, utf-8, iso-8859-1, iso-8859-15,
/// windows-1252. Unknown -> String::from_utf8_lossy (documented lossy fallback).
pub fn charset_to_utf8(bytes: &[u8], charset: &str) -> String {
    match charset.trim().to_ascii_lowercase().as_str() {
        "utf-8" | "utf8" | "" => String::from_utf8_lossy(bytes).into_owned(),
        "us-ascii" | "ascii" | "ansi_x3.4-1968" | "ansi_x3.4" | "iso646-us" => bytes
            .iter()
            .map(|&b| if b < 0x80 { b as char } else { '\u{FFFD}' })
            .collect(),
        "iso-8859-1" | "iso_8859-1" | "iso8859-1" | "latin1" | "latin-1" | "l1" | "cp819"
        | "8859-1" => bytes.iter().map(|&b| b as char).collect(),
        "iso-8859-15" | "iso_8859-15" | "iso8859-15" | "latin9" | "latin-9" | "l9" => {
            bytes.iter().map(|&b| iso8859_15(b)).collect()
        }
        "windows-1252" | "cp1252" | "windows1252" | "1252" | "ansi" => {
            bytes.iter().map(|&b| cp1252(b)).collect()
        }
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn iso8859_15(b: u8) -> char {
    match b {
        0xA4 => '\u{20AC}',
        0xA6 => '\u{0160}',
        0xA8 => '\u{0161}',
        0xB4 => '\u{017D}',
        0xB8 => '\u{017E}',
        0xBC => '\u{0152}',
        0xBD => '\u{0153}',
        0xBE => '\u{0178}',
        _ => b as char, // identical to ISO-8859-1 elsewhere
    }
}

fn cp1252(b: u8) -> char {
    // Only the 0x80-0x9F block differs from ISO-8859-1. Undefined slots
    // (0x81/0x8D/0x8F/0x90/0x9D) map to their C1 control code point.
    match b {
        0x80 => '\u{20AC}',
        0x82 => '\u{201A}',
        0x83 => '\u{0192}',
        0x84 => '\u{201E}',
        0x85 => '\u{2026}',
        0x86 => '\u{2020}',
        0x87 => '\u{2021}',
        0x88 => '\u{02C6}',
        0x89 => '\u{2030}',
        0x8A => '\u{0160}',
        0x8B => '\u{2039}',
        0x8C => '\u{0152}',
        0x8E => '\u{017D}',
        0x91 => '\u{2018}',
        0x92 => '\u{2019}',
        0x93 => '\u{201C}',
        0x94 => '\u{201D}',
        0x95 => '\u{2022}',
        0x96 => '\u{2013}',
        0x97 => '\u{2014}',
        0x98 => '\u{02DC}',
        0x99 => '\u{2122}',
        0x9A => '\u{0161}',
        0x9B => '\u{203A}',
        0x9C => '\u{0153}',
        0x9E => '\u{017E}',
        0x9F => '\u{0178}',
        _ => b as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(headers: &str, body: &str) -> Vec<u8> {
        format!("{headers}\r\n\r\n{body}").into_bytes()
    }

    #[test]
    fn unfold_joins_continuation_lines() {
        let raw = b"Subject: a\r\n b\r\n\tc\r\nX: y\r\n\r\nbody";
        let (h, off) = unfold_headers(raw);
        assert_eq!(header_value(&h, "subject"), Some("a b c"));
        assert_eq!(header_value(&h, "X"), Some("y"));
        assert_eq!(&raw[off..], b"body");
    }

    #[test]
    fn unfold_accepts_bare_lf() {
        let raw = b"A: 1\nB: 2\n\nbody";
        let (h, off) = unfold_headers(raw);
        assert_eq!(header_value(&h, "a"), Some("1"));
        assert_eq!(header_value(&h, "b"), Some("2"));
        assert_eq!(&raw[off..], b"body");
    }

    #[test]
    fn encoded_word_b_and_q() {
        assert_eq!(decode_encoded_words("=?utf-8?B?SGVsbG8=?="), "Hello");
        assert_eq!(
            decode_encoded_words("=?utf-8?Q?Hello=20World?="),
            "Hello World"
        );
        // Q underscore -> space
        assert_eq!(decode_encoded_words("=?utf-8?Q?a_b?="), "a b");
    }

    #[test]
    fn encoded_word_adjacent_bytes_concatenate() {
        // U+2600 (â\x98\x80) split across two words, joined BEFORE charset decode.
        let s = "=?utf-8?B?4pg=?= =?utf-8?B?gA==?=";
        assert_eq!(decode_encoded_words(s), "\u{2600}");
    }

    #[test]
    fn encoded_word_whitespace_elided_between_words_but_kept_before_text() {
        assert_eq!(
            decode_encoded_words("=?utf-8?B?SGVsbG8=?= =?utf-8?B?V29ybGQ=?="),
            "HelloWorld"
        );
        assert_eq!(
            decode_encoded_words("plain =?utf-8?B?SGk=?= tail"),
            "plain Hi tail"
        );
    }

    #[test]
    fn encoded_word_latin1_q() {
        // =E9 in iso-8859-1 is 'é'
        assert_eq!(decode_encoded_words("=?ISO-8859-1?Q?caf=E9?="), "café");
    }

    #[test]
    fn content_type_params_and_boundary() {
        let (ct, params) = parse_content_type("multipart/mixed; boundary=\"a=b;c\"; charset=UTF-8");
        assert_eq!(ct, "multipart/mixed");
        assert_eq!(param_get(&params, "boundary"), Some("a=b;c"));
        assert_eq!(param_get(&params, "charset"), Some("UTF-8"));
    }

    #[test]
    fn rfc2231_extended_filename() {
        let (_ct, p) = parse_content_type("attachment; filename*=utf-8''%E2%98%83.txt");
        assert_eq!(param_get(&p, "filename"), Some("\u{2603}.txt"));
    }

    #[test]
    fn rfc2231_continued_filename() {
        let (_ct, p) =
            parse_content_type("attachment; filename*0*=utf-8''%E2%98%83; filename*1=snow.txt");
        assert_eq!(param_get(&p, "filename"), Some("\u{2603}snow.txt"));
    }

    #[test]
    fn charset_windows1252_and_latin1() {
        assert_eq!(
            charset_to_utf8(&[0x93, 0x94], "windows-1252"),
            "\u{201C}\u{201D}"
        );
        assert_eq!(charset_to_utf8(&[0x80], "cp1252"), "\u{20AC}");
        assert_eq!(charset_to_utf8(&[0xE9], "iso-8859-1"), "é");
        assert_eq!(charset_to_utf8(&[0xA4], "iso-8859-15"), "\u{20AC}");
        // unknown charset -> lossy utf-8
        assert_eq!(charset_to_utf8(b"hi", "made-up-9000"), "hi");
    }

    #[test]
    fn decode_body_qp_and_base64() {
        assert_eq!(decode_body("quoted-printable", b"a=3Db"), b"a=b");
        assert_eq!(decode_body("base64", b"SGVsbG8="), b"Hello");
        assert_eq!(decode_body("8bit", b"\x00\xff"), &[0x00, 0xff]);
    }

    #[test]
    fn simple_text_message() {
        let raw = plain(
            "From: a@b\r\nSubject: hi\r\nContent-Type: text/plain; charset=us-ascii",
            "hello",
        );
        let m = parse(&raw);
        assert_eq!(m.subject, "hi");
        assert_eq!(m.from, "a@b");
        assert_eq!(m.text_body().as_deref(), Some("hello"));
        assert!(m.html_body().is_none());
        assert_eq!(m.root.id, "0");
    }

    #[test]
    fn nested_multipart_tree_and_ids() {
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=OUT\r\n",
            "\r\n",
            "preamble\r\n",
            "--OUT\r\n",
            "Content-Type: multipart/alternative; boundary=IN\r\n",
            "\r\n",
            "--IN\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "text part\r\n",
            "--IN\r\n",
            "Content-Type: text/html\r\n",
            "\r\n",
            "<b>html</b>\r\n",
            "--IN--\r\n",
            "--OUT\r\n",
            "Content-Type: text/plain\r\n",
            "Content-Disposition: attachment; filename=notes.txt\r\n",
            "\r\n",
            "attached\r\n",
            "--OUT--\r\n",
        )
        .as_bytes();
        let m = parse(raw);
        let ids: Vec<&str> = m.flat_parts().iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["0", "1", "1.1", "1.2", "2"]);
        assert_eq!(m.text_body().as_deref(), Some("text part"));
        assert_eq!(m.html_body().as_deref(), Some("<b>html</b>"));
        let atts = m.attachments();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].id, "2");
        assert_eq!(atts[0].filename.as_deref(), Some("notes.txt"));
        // part_by_id
        assert_eq!(m.part_by_id("1.1").unwrap().content_type, "text/plain");
    }

    #[test]
    fn boundary_preceding_crlf_is_stripped_binary_exact() {
        // A binary part whose content ends right before the delimiter CRLF must
        // NOT gain a trailing CRLF.
        let mut raw = Vec::new();
        raw.extend_from_slice(b"Content-Type: multipart/mixed; boundary=B\r\n\r\n");
        raw.extend_from_slice(b"--B\r\n");
        raw.extend_from_slice(b"Content-Type: application/octet-stream\r\n");
        raw.extend_from_slice(b"Content-Transfer-Encoding: 8bit\r\n\r\n");
        raw.extend_from_slice(&[0x00, 0xFF, 0x89, 0x50]); // exact payload
        raw.extend_from_slice(b"\r\n--B--\r\n");
        let m = parse(&raw);
        let att = &m.root.children[0];
        assert_eq!(att.body, vec![0x00, 0xFF, 0x89, 0x50]);
        assert!(att.is_attachment);
    }

    #[test]
    fn base64_attachment_roundtrip() {
        let payload = base64::encode(&[1u8, 2, 3, 4, 5]);
        let raw = format!(
            "Content-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\nContent-Type: application/pdf; name=doc.pdf\r\nContent-Transfer-Encoding: base64\r\n\r\n{payload}\r\n--B--\r\n"
        );
        let m = parse(raw.as_bytes());
        let att = m.attachments();
        assert_eq!(att.len(), 1);
        assert_eq!(att[0].body, vec![1, 2, 3, 4, 5]);
        assert_eq!(att[0].filename.as_deref(), Some("doc.pdf"));
    }

    #[test]
    fn inline_cid_image_not_attachment() {
        let raw = concat!(
            "Content-Type: multipart/related; boundary=R\r\n\r\n",
            "--R\r\n",
            "Content-Type: text/html\r\n\r\n",
            "<img src=\"cid:img1\">\r\n",
            "--R\r\n",
            "Content-Type: image/png\r\n",
            "Content-ID: <img1>\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\n",
            "AAAA\r\n",
            "--R--\r\n",
        )
        .as_bytes();
        let m = parse(raw);
        // the image is referenced by cid: -> inline, not an attachment
        assert!(m.attachments().is_empty());
        assert_eq!(
            m.part_by_id("2").unwrap().content_id.as_deref(),
            Some("img1")
        );
    }

    #[test]
    fn cid_reference_is_token_anchored() {
        // The HTML references cid:img10, so a leaf with Content-ID <img1> is NOT
        // referenced and must be classified as an attachment (no substring match).
        let raw = concat!(
            "Content-Type: multipart/related; boundary=R\r\n\r\n",
            "--R\r\n",
            "Content-Type: text/html\r\n\r\n",
            "<img src=\"cid:img10\">\r\n",
            "--R\r\n",
            "Content-Type: image/png\r\n",
            "Content-ID: <img1>\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\n",
            "AAAA\r\n",
            "--R--\r\n",
        )
        .as_bytes();
        let m = parse(raw);
        assert_eq!(m.attachments().len(), 1);
    }

    #[test]
    fn malformed_missing_terminal_boundary_no_panic() {
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=B\r\n\r\n",
            "--B\r\n",
            "Content-Type: text/plain\r\n\r\n",
            "unterminated body with no closing boundary\r\n",
        )
        .as_bytes();
        let m = parse(raw);
        assert_eq!(m.root.children.len(), 1);
        assert_eq!(
            m.text_body().as_deref(),
            Some("unterminated body with no closing boundary")
        );
    }

    #[test]
    fn truncated_and_empty_inputs_do_not_panic() {
        let _ = parse(b"");
        let _ = parse(b"=?utf-8?B?");
        let _ = parse(b"Content-Type: multipart/mixed; boundary=");
        let _ = parse(b"Subject:");
        let _ = parse(&[0xff, 0xfe, 0x00, b'\n']);
        let _ = decode_encoded_words("=?=?=?B?=?=");
        let _ = parse_content_type(";;;=;");
    }

    #[test]
    fn header_original_case_and_order_preserved() {
        let raw = plain("From: A\r\nX-Custom: v\r\nSubject: s", "b");
        let m = parse(&raw);
        let names: Vec<&str> = m.headers.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["From", "X-Custom", "Subject"]);
    }
}
