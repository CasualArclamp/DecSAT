// made by CasualArclamp
//! SIP messages carved out of a decoded payload, and grouped into dialogs.
//!
//! Port of `bgan/sip.py` from DecBGAN (Python, same author,
//! GPL-3.0-or-later). SIP is RFC 3261, a text protocol shaped like HTTP; the
//! safety argument is the same as for HTTP: a start line anchored on the
//! `IP/2.0` literal (see below) plus the grammar of RFC 3261 clause 7.1/7.2.
//!
//! Carving, not demultiplexing: a message straddling a failed FEC block is cut,
//! and [`SipMessage::check`] says which. A body is taken from `Content-Length`
//! only. Headers carrying credentials (Authorization, WWW-Authenticate, ...)
//! are replaced by their length. Messages are grouped into dialogs by Call-ID
//! in wire order ([`dialogs`]).
//!
//! The anchor is `IP/2.0`, one byte short of `SIP/2.0`: on this bearer the
//! leading `S` is often another byte (measured 0x18, 0x24, 0x68, 0xc4; cause
//! not established), which is recorded in [`SipMessage::damaged`].
//!
//! [`SipScanner`] is the incremental version of [`messages`].

use super::stream::{
    Rolling, ascii_lossy, find, find_within, py_isspace, py_prefix, py_strip, strip_bytes,
};

/// RFC 3261 clause 7.1 plus common extension methods.
pub const METHODS: [&str; 14] = [
    "INVITE",
    "ACK",
    "BYE",
    "CANCEL",
    "OPTIONS",
    "REGISTER",
    "PRACK",
    "SUBSCRIBE",
    "NOTIFY",
    "PUBLISH",
    "INFO",
    "REFER",
    "MESSAGE",
    "UPDATE",
];

/// Headers kept, in display order.
pub const KEEP: [&str; 28] = [
    "From",
    "To",
    "Call-ID",
    "CSeq",
    "Contact",
    "Via",
    "Route",
    "Record-Route",
    "P-Asserted-Identity",
    "P-Called-Party-ID",
    "User-Agent",
    "Server",
    "Allow",
    "Supported",
    "Require",
    "Expires",
    "Max-Forwards",
    "Content-Type",
    "Content-Length",
    "Reason",
    "Retry-After",
    "Warning",
    "Subject",
    "Session-Expires",
    "Authorization",
    "WWW-Authenticate",
    "Proxy-Authorization",
    "Proxy-Authenticate",
];

/// Compact forms, RFC 3261 clause 20 and RFC 3265/3515.
const COMPACT: [(&str, &str); 12] = [
    ("f", "From"),
    ("t", "To"),
    ("i", "Call-ID"),
    ("m", "Contact"),
    ("v", "Via"),
    ("c", "Content-Type"),
    ("l", "Content-Length"),
    ("s", "Subject"),
    ("k", "Supported"),
    ("o", "Event"),
    ("r", "Refer-To"),
    ("b", "Referred-By"),
];
// ("e", "Content-Encoding") is the thirteenth; kept below to stay a plain array.
const COMPACT_E: (&str, &str) = ("e", "Content-Encoding");

const SECRET: [&str; 4] = [
    "authorization",
    "www-authenticate",
    "proxy-authorization",
    "proxy-authenticate",
];

const MAX_BODY: usize = 1 << 20;
const HDR_LIMIT: usize = 4096;
/// Bytes of context before an anchor the request walk-back may read.
pub const KEEP_BACK: usize = 1100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SipKind {
    Request,
    Response,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SipMessage {
    pub offset: usize,
    pub kind: SipKind,
    /// The start line, verbatim.
    pub start: String,
    /// Request method, or the CSeq method for a response.
    pub method: String,
    pub uri: String,
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Header block terminated by a blank line.
    pub complete: bool,
    /// Body reached its Content-Length.
    pub body_complete: bool,
    /// The `S` of `SIP/2.0` was some other byte.
    pub damaged: bool,
}

impl SipMessage {
    /// First header called `name` (case-insensitive), or "".
    pub fn get(&self, name: &str) -> &str {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map_or("", |(_, v)| v.as_str())
    }

    pub fn call_id(&self) -> &str {
        self.get("Call-ID")
    }

    /// `(number, METHOD)` from the CSeq header, or `(-1, "")`.
    pub fn cseq(&self) -> (i64, String) {
        let v = self.get("CSeq");
        let s = v.trim_start_matches(py_isspace);
        let digits = s.bytes().take_while(u8::is_ascii_digit).count();
        let rest = &s[digits..];
        let r2 = rest.trim_start_matches(py_isspace);
        if digits == 0 || r2.len() == rest.len() {
            return (-1, String::new());
        }
        let letters = r2.bytes().take_while(u8::is_ascii_alphabetic).count();
        if letters == 0 {
            return (-1, String::new());
        }
        (
            s[..digits].parse().unwrap_or(i64::MAX),
            r2[..letters].to_ascii_uppercase(),
        )
    }

    pub fn summary(&self) -> String {
        match self.kind {
            SipKind::Request => format!("{} {}", self.method, self.uri),
            SipKind::Response => py_strip(&format!("{} {}", self.status, self.reason)).to_string(),
        }
    }

    /// "intact" or "truncated".
    pub fn check(&self) -> &'static str {
        if !self.complete || (!self.get("Content-Length").is_empty() && !self.body_complete) {
            "truncated"
        } else {
            "intact"
        }
    }
}

/// Value up to the first non-printable byte, at most `limit` characters.
/// A header near the end of a decoded block has no CRLF to stop at: what follows
/// is unrelated traffic, so the value must not swallow it.
fn clean(v: &[u8], limit: usize) -> String {
    let t: Vec<u8> = strip_bytes(v)
        .iter()
        .copied()
        .take_while(|c| (0x20..=0x7e).contains(c))
        .collect();
    py_prefix(&ascii_lossy(&t), limit).to_string()
}

/// `[(name, value)]`, the index after the blank line, and whether one was found.
/// Handles RFC 3261 clause 7.3.1 folding and clause 20 compact forms.
/// Returns `None` if the answer needs bytes that have not arrived.
#[allow(clippy::type_complexity)]
fn parse_headers(b: &[u8], i: usize, fin: bool) -> Option<(Vec<(String, String)>, usize, bool)> {
    let end = find_within(b, b"\r\n\r\n", i, i + HDR_LIMIT);
    if end.is_none() && !fin && b.len() < i + HDR_LIMIT {
        return None;
    }
    let complete = end.is_some();
    let seg = &b[i..end.unwrap_or_else(|| b.len().min(i + HDR_LIMIT))];
    let mut lines: Vec<Vec<u8>> = Vec::new();
    for raw in super::findings::split_crlf(seg) {
        if matches!(raw.first(), Some(b' ' | b'\t')) && !lines.is_empty() {
            let last = lines.last_mut().unwrap();
            last.push(b' ');
            last.extend_from_slice(strip_bytes(raw));
        } else {
            lines.push(raw.to_vec());
        }
    }
    let mut out = Vec::new();
    for line in &lines {
        let Some(c) = line.iter().position(|&x| x == b':') else {
            continue;
        };
        let (k, v) = (&line[..c], &line[c + 1..]);
        let ks = ascii_lossy(strip_bytes(k)).to_lowercase();
        let name = KEEP
            .iter()
            .find(|h| h.to_lowercase() == ks)
            .copied()
            .or_else(|| {
                COMPACT
                    .iter()
                    .chain([&COMPACT_E])
                    .find(|(c, _)| *c == ks)
                    .map(|(_, n)| *n)
            });
        let Some(name) = name else { continue };
        if SECRET.contains(&ks.as_str()) || SECRET.contains(&name.to_lowercase().as_str()) {
            out.push((
                name.to_string(),
                format!("<redacted, {} bytes>", strip_bytes(v).len()),
            ));
            continue;
        }
        let val = clean(v, 200);
        if !val.is_empty() {
            out.push((name.to_string(), val));
        }
    }
    Some((
        out,
        if complete { end.unwrap() + 4 } else { b.len() },
        complete,
    ))
}

/// `(body, complete)`; only ever bounded by Content-Length.
/// `None` when the body has not fully arrived (and `fin` is false).
fn body(b: &[u8], i: usize, headers: &[(String, String)], fin: bool) -> Option<(Vec<u8>, bool)> {
    let length = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map_or("", |(_, v)| v.as_str());
    let l = py_strip(length);
    if l.is_empty() || !l.bytes().all(|c| c.is_ascii_digit()) {
        return Some((Vec::new(), false));
    }
    let n: u128 = l.parse().unwrap_or(u128::MAX);
    if n == 0 || n > MAX_BODY as u128 {
        return Some((Vec::new(), n == 0));
    }
    let n = n as usize;
    if !fin && b.len() < i + n {
        return None;
    }
    let lo = i.min(b.len());
    Some((b[lo..(i + n).min(b.len())].to_vec(), i + n <= b.len()))
}

/// RFC 3261 status line check: `^.IP/2\.0 (\d{3})(?: (.{0,120}))?$` with
/// Python's rules (`.` excludes `\n`, `$` also matches before a final `\n`).
/// Returns the code and the reason group.
fn status_line(line: &[u8]) -> Option<(u16, &[u8])> {
    if line.len() < 11
        || line[0] == b'\n'
        || &line[1..8] != b"IP/2.0 "
        || !line[8..11].iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    let code =
        (line[8] - b'0') as u16 * 100 + (line[9] - b'0') as u16 * 10 + (line[10] - b'0') as u16;
    let mut rest = &line[11..];
    if rest.is_empty() || rest == b"\n" {
        return Some((code, b""));
    }
    if rest[0] != b' ' {
        return None;
    }
    rest = &rest[1..];
    if let Some((b'\n', r)) = rest.split_last() {
        rest = r; // `$` before the trailing newline
    }
    if rest.len() > 120 || rest.contains(&b'\n') {
        return None;
    }
    Some((code, rest))
}

/// `^[a-z][a-z0-9+.-]{1,14}:`
fn uri_scheme(uri: &[u8]) -> bool {
    let Some((&f, rest)) = uri.split_first() else {
        return false;
    };
    if !f.is_ascii_lowercase() {
        return false;
    }
    let r = rest
        .iter()
        .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"+.-".contains(c))
        .count();
    (1..=14).contains(&r) && rest.get(r) == Some(&b':')
}

/// Decide the anchor at relative `m` (the position of `IP/2.0`).
/// `Ok(None)`: no message; `Err(())`: need more bytes.
fn message_at(b: &[u8], m: usize, base: usize, fin: bool) -> Result<Option<SipMessage>, ()> {
    if base + m == 0 {
        return Ok(None);
    }
    let i = m - 1; // where the S is, or should have been
    let damaged = b[i] != 0x53;
    let Some(line_end) = find_within(b, b"\r\n", i, i + 202) else {
        return if !fin && b.len() < i + 202 {
            Err(())
        } else {
            Ok(None)
        };
    };
    // Responses: the literal opens the line.
    if let Some((code, reason)) = status_line(&b[i..line_end]) {
        if !(100..=699).contains(&code) {
            return Ok(None);
        }
        let (hdrs, after, complete) = parse_headers(b, line_end + 2, fin).ok_or(())?;
        let (bd, bok) = body(b, after, &hdrs, fin).ok_or(())?;
        let mut msg = SipMessage {
            offset: i,
            kind: SipKind::Response,
            start: ascii_lossy(&b[i..line_end]),
            method: String::new(),
            uri: String::new(),
            status: code,
            reason: clean(reason, 120),
            headers: hdrs,
            body: bd,
            complete,
            body_complete: bok,
            damaged,
        };
        msg.method = msg.cseq().1;
        return Ok(Some(msg));
    }

    // Requests: "<METHOD> <URI> SIP/2.0" ends at the literal, so walk back
    // over the URI and then the method. Not back to the previous newline: what
    // precedes a carved message is the IP/UDP header it arrived in.
    if base + i < 3 || i < 1 {
        return Ok(None);
    }
    let j = i - 1;
    if b[j] != 0x20 {
        return Ok(None);
    }
    let mut k = j as isize - 1;
    while k >= 0 && j as isize - k <= 1024 && !matches!(b[k as usize], 0x20 | 0x0d | 0x0a) {
        k -= 1;
    }
    if k < 0 || b[k as usize] != 0x20 || k as usize == j - 1 {
        return Ok(None);
    }
    let k = k as usize;
    let uri = &b[k + 1..j];
    if !uri_scheme(uri) || !uri.is_ascii() {
        return Ok(None);
    }
    let mut s = k as isize - 1;
    while s >= 0 && k as isize - s <= 12 && b[s as usize].is_ascii_uppercase() {
        s -= 1;
    }
    let run = &b[(s + 1) as usize..k];
    // The run over-reaches when the byte before the method is an uppercase
    // letter; take the longest suffix that is a real method.
    let Some(method) = (3..=run.len())
        .rev()
        .map(|n| &run[run.len() - n..])
        .find_map(|suf| METHODS.iter().find(|m| m.as_bytes() == suf).copied())
    else {
        return Ok(None);
    };
    let ls = k - method.len();
    let (hdrs, after, complete) = parse_headers(b, line_end + 2, fin).ok_or(())?;
    let (bd, bok) = body(b, after, &hdrs, fin).ok_or(())?;
    Ok(Some(SipMessage {
        offset: ls,
        kind: SipKind::Request,
        start: ascii_lossy(&b[ls..line_end]),
        method: method.to_string(),
        uri: ascii_lossy(uri),
        status: 0,
        reason: String::new(),
        headers: hdrs,
        body: bd,
        complete,
        body_complete: bok,
        damaged,
    }))
}

/// Incremental [`messages`].
#[derive(Debug, Default)]
pub struct SipScanner {
    win: Rolling,
    cur: usize,
    fin: bool,
}

impl SipScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; returns messages that became final, in anchor order (a
    /// request's `offset` is its method, a little before its anchor).
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SipMessage> {
        if self.fin {
            return Vec::new();
        }
        self.win.push(bytes);
        self.run()
    }

    pub fn finish(&mut self) -> Vec<SipMessage> {
        self.fin = true;
        self.run()
    }

    fn run(&mut self) -> Vec<SipMessage> {
        let base = self.win.base();
        let buf = self.win.data();
        let n = buf.len();
        let mut out = Vec::new();
        let mut i = self.cur - base;
        loop {
            let Some(p) = find(buf, b"IP/2.0", i) else {
                i = if self.fin {
                    n
                } else {
                    i.max(n.saturating_sub(5))
                };
                break;
            };
            match message_at(buf, p, base, self.fin) {
                Err(()) => {
                    i = p;
                    break;
                }
                Ok(m) => {
                    if let Some(mut m) = m {
                        m.offset += base;
                        out.push(m);
                    }
                    i = p + 6;
                }
            }
        }
        self.cur = base + i;
        self.win.trim_before(self.cur.saturating_sub(KEEP_BACK));
        out
    }
}

/// Every SIP message in `blob`, in offset order.
pub fn messages(blob: &[u8]) -> Vec<SipMessage> {
    let mut s = SipScanner::new();
    let mut v = s.push(blob);
    v.extend(s.finish());
    v.sort_by_key(|m| m.offset);
    v
}

// --- SDP, RFC 4566 ----------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Media {
    /// audio | video | ...
    pub kind: String,
    pub port: u64,
    pub proto: String,
    /// `["8 PCMA/8000", ...]`
    pub formats: Vec<String>,
    pub address: String,
}

/// Media descriptions from an SDP body; empty if it is not SDP. Only the lines
/// that say what the call is: connection address, media type and port, and the
/// rtpmap names of the offered codecs.
pub fn sdp(body: &[u8]) -> Vec<Media> {
    if find(&body[..body.len().min(64)], b"v=0", 0).is_none() {
        return Vec::new();
    }
    let mut session_c = String::new();
    let mut out: Vec<Media> = Vec::new();
    let mut rtpmap: Vec<(String, String)> = Vec::new();
    // re.split(rb"\r\n|\n")
    for raw in body.split(|&c| c == b'\n') {
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        let line = clean(raw, 400);
        if line.chars().nth(1) != Some('=') {
            continue;
        }
        let mut ch = line.chars();
        let tag = ch.next().unwrap();
        let val: String = ch.skip(1).collect();
        match tag {
            'c' => {
                let last = val
                    .split(py_isspace)
                    .rfind(|p| !p.is_empty())
                    .unwrap_or("")
                    .to_string();
                match out.last_mut() {
                    Some(m) => m.address = last,
                    None => session_c = last,
                }
            }
            'm' => {
                let p: Vec<&str> = val.split(py_isspace).filter(|x| !x.is_empty()).collect();
                if p.len() >= 3 && !p[1].is_empty() && p[1].bytes().all(|c| c.is_ascii_digit()) {
                    out.push(Media {
                        kind: p[0].into(),
                        port: p[1].parse().unwrap_or(u64::MAX),
                        proto: p[2].into(),
                        formats: p[3..].iter().map(|s| s.to_string()).collect(),
                        address: session_c.clone(),
                    });
                }
            }
            'a' if val.to_lowercase().starts_with("rtpmap:") => {
                let rest: String = val.chars().skip(7).collect();
                let rest = py_strip(&rest);
                let (pt, name) = rest.split_once(' ').unwrap_or((rest, ""));
                rtpmap.push((py_strip(pt).to_string(), py_strip(name).to_string()));
            }
            _ => {}
        }
    }
    for md in &mut out {
        for f in &mut md.formats {
            // dict semantics: the last rtpmap for a payload type wins
            if let Some((_, nm)) = rtpmap.iter().rfind(|(pt, _)| pt == f) {
                *f = format!("{f} {nm}");
            }
        }
    }
    out
}

// --- dialogs ----------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Dialog<'a> {
    pub call_id: String,
    pub messages: Vec<&'a SipMessage>,
}

/// `sips?:[^>;\s]+` as found in a From/To header; otherwise the header text,
/// truncated to 80 characters.
fn uri_of(header: &str) -> String {
    let b = header.as_bytes();
    let mut i = 0;
    while let Some(p) = find(b, b"sip", i) {
        i = p + 1;
        let mut q = p + 3;
        if b.get(q) == Some(&b's') && b.get(q + 1) == Some(&b':') {
            q += 1;
        }
        if b.get(q) != Some(&b':') {
            continue;
        }
        let rest = &header[q + 1..];
        let run: usize = rest
            .chars()
            .take_while(|&c| c != '>' && c != ';' && !py_isspace(c))
            .map(char::len_utf8)
            .sum();
        if run > 0 {
            return header[p..q + 1 + run].to_string();
        }
    }
    py_prefix(py_strip(header), 80).to_string()
}

impl<'a> Dialog<'a> {
    pub fn offset(&self) -> usize {
        self.messages.iter().map(|m| m.offset).min().unwrap_or(0)
    }

    fn first(&self, name: &str) -> &'a str {
        self.messages
            .iter()
            .map(|m| m.get(name))
            .find(|v| !v.is_empty())
            .unwrap_or("")
    }

    pub fn from_uri(&self) -> String {
        uri_of(self.first("From"))
    }

    pub fn to_uri(&self) -> String {
        uri_of(self.first("To"))
    }

    /// Request methods in order of first appearance.
    pub fn methods(&self) -> Vec<&'a str> {
        let mut seen: Vec<&str> = Vec::new();
        for m in &self.messages {
            if m.kind == SipKind::Request && !seen.contains(&m.method.as_str()) {
                seen.push(&m.method);
            }
        }
        seen
    }

    pub fn media(&self) -> Vec<Media> {
        self.messages.iter().flat_map(|m| sdp(&m.body)).collect()
    }

    /// The most advanced thing that happened, as a short phrase.
    pub fn outcome(&self) -> String {
        let codes: Vec<u16> = self
            .messages
            .iter()
            .filter(|m| m.kind == SipKind::Response)
            .map(|m| m.status)
            .collect();
        let has = |meth: &str| {
            self.messages
                .iter()
                .any(|m| m.kind == SipKind::Request && m.method == meth)
        };
        if has("BYE") {
            return "call ended (BYE)".into();
        }
        if codes.iter().any(|c| (200..300).contains(c)) {
            return "answered (2xx)".into();
        }
        if has("CANCEL") {
            return "cancelled".into();
        }
        if let Some(bad) = codes.iter().filter(|&&c| c >= 400).min() {
            return format!("rejected ({bad})");
        }
        if codes.iter().any(|c| (180..200).contains(c)) {
            return "ringing, no answer seen".into();
        }
        "no response seen".into()
    }
}

/// Group messages by Call-ID, in wire order (not CSeq order: CSeq counts per
/// direction, so a callee's BYE can be numerically lower than the INVITE).
/// Messages with no Call-ID are each their own dialog.
pub fn dialogs(msgs: &[SipMessage]) -> Vec<Dialog<'_>> {
    let mut by_id: Vec<Dialog<'_>> = Vec::new();
    let mut idx: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut loose = Vec::new();
    for m in msgs {
        let cid = m.call_id();
        if cid.is_empty() {
            loose.push(Dialog {
                call_id: String::new(),
                messages: vec![m],
            });
        } else {
            let k = *idx.entry(cid).or_insert_with(|| {
                by_id.push(Dialog {
                    call_id: cid.to_string(),
                    messages: Vec::new(),
                });
                by_id.len() - 1
            });
            by_id[k].messages.push(m);
        }
    }
    for d in &mut by_id {
        d.messages.sort_by_key(|m| m.offset);
    }
    by_id.extend(loose);
    by_id.sort_by_key(|d| d.offset());
    by_id
}

#[cfg(test)]
mod tests {
    use super::*;

    const SDP: &str =
        "v=0\r\nc=IN IP4 10.1.2.3\r\nm=audio 49170 RTP/AVP 0 8\r\na=rtpmap:8 PCMA/8000\r\n";

    fn invite() -> String {
        let head = [
            "INVITE sip:bob@example.com SIP/2.0",
            "Via: SIP/2.0/UDP a.example.com",
            "From: <sip:alice@example.com>;tag=1",
            "To: <sip:bob@example.com>",
            "Call-ID: abc123",
            "CSeq: 1 INVITE",
            "Authorization: Digest secret=1",
            "Content-Type: application/sdp",
        ]
        .join("\r\n");
        format!("{head}\r\nContent-Length: {}\r\n\r\n{SDP}", SDP.len())
    }

    fn sample() -> Vec<u8> {
        let mut b = vec![0x45, 0, 0, 1, 0x13, 0xc4, 0x13, 0xd8, 0, 0];
        b.extend(invite().as_bytes());
        b.extend([0u8; 30]);
        // 200 OK with a damaged leading S, folded header
        b.push(0x18);
        b.extend(
            b"IP/2.0 200 OK\r\nCall-ID: abc123\r\nCSeq: 1 INVITE\r\nTo: <sip:bob@example.com>\r\n ;tag=9\r\nContent-Length: 0\r\n\r\n",
        );
        b.extend([0u8; 30]);
        b.extend(b"BYE sip:alice@example.com SIP/2.0\r\nCall-ID: abc123\r\nCSeq: 2 BYE\r\n\r\n");
        b.extend([0u8; 30]);
        b
    }

    #[test]
    fn parses_requests_responses_and_dialogs() {
        let b = sample();
        let m = messages(&b);
        assert_eq!(m.len(), 3, "{m:#?}");
        assert_eq!(m[0].kind, SipKind::Request);
        assert_eq!(m[0].method, "INVITE");
        assert_eq!(m[0].uri, "sip:bob@example.com");
        assert_eq!(m[0].offset, 10);
        assert_eq!(m[0].get("Authorization"), "<redacted, 15 bytes>");
        assert_eq!(m[0].cseq(), (1, "INVITE".into()));
        assert_eq!(m[0].check(), "intact");
        assert_eq!(m[1].status, 200);
        assert!(m[1].damaged);
        assert_eq!(m[1].method, "INVITE");
        assert_eq!(m[1].get("To"), "<sip:bob@example.com> ;tag=9");
        assert_eq!(m[2].method, "BYE");
        assert_eq!(m[2].check(), "intact");

        let d = dialogs(&m);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].messages.len(), 3);
        assert_eq!(d[0].outcome(), "call ended (BYE)");
        assert_eq!(d[0].from_uri(), "sip:alice@example.com");
        assert_eq!(d[0].to_uri(), "sip:bob@example.com");
        assert_eq!(d[0].methods(), ["INVITE", "BYE"]);
        let media = d[0].media();
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].port, 49170);
        assert_eq!(media[0].address, "10.1.2.3");
        assert_eq!(media[0].formats, ["0", "8 PCMA/8000"]);
    }

    #[test]
    fn truncated_message_is_marked() {
        let mut b = vec![0u8; 5];
        b.extend(b"INVITE sip:x@y.z SIP/2.0\r\nCall-ID: q\r\nContent-Length: 100\r\n\r\nshort");
        let m = messages(&b);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].check(), "truncated");
        assert_eq!(m[0].body, b"short");
    }

    #[test]
    fn streaming_matches_one_shot() {
        let one = sample();
        let mut b = Vec::new();
        for _ in 0..5 {
            b.extend(vec![0x33u8; 9000]);
            b.extend(&one);
        }
        let want = messages(&b);
        assert_eq!(want.len(), 15);
        for chunk in [97usize, 4096] {
            let mut s = SipScanner::new();
            let mut got = Vec::new();
            for ch in b.chunks(chunk) {
                got.extend(s.push(ch));
            }
            got.extend(s.finish());
            got.sort_by_key(|m| m.offset);
            assert_eq!(got, want, "chunk {chunk}");
        }
    }

    #[test]
    fn random_bytes_have_no_messages() {
        let mut x = 0x9e37_79b9u32;
        let b: Vec<u8> = (0..500_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x >> 8) as u8
            })
            .collect();
        assert!(messages(&b).is_empty());
    }

    #[test]
    fn status_line_grammar() {
        assert_eq!(
            status_line(b"SIP/2.0 180 Ringing"),
            Some((180, &b"Ringing"[..]))
        );
        assert_eq!(status_line(b"SIP/2.0 200"), Some((200, &b""[..])));
        assert!(status_line(b"SIP/2.0 2000").is_none());
        assert!(status_line(b"SIP/2.0 20 OK").is_none());
        assert!(uri_scheme(b"sip:a@b"));
        assert!(!uri_scheme(b"x:a"));
        assert!(!uri_scheme(b"1sip:a"));
    }
}
