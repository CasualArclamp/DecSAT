// made by CasualArclamp
//! Recognisable artefacts carved out of a decoded payload: X.509 certificates,
//! DNS messages, HTTP request/status lines, TLS handshakes and URLs.
//!
//! Port of the findings half of `bgan/findings.py` from DecBGAN (Python, same
//! author, GPL-3.0-or-later). The HTTP body / file reconstruction half lives in
//! [`super::files`].
//!
//! This is carving, not demultiplexing: the payload has silent gaps wherever a
//! FEC block failed. Every extractor validates *structurally* (a candidate must
//! parse to its own declared length and end where it said it would) rather
//! than by pattern alone; scanning a megabyte for a two-byte tag finds a hit
//! every few hundred bytes in pure noise. The URL extractor is the loose one.
//!
//! # Streaming
//!
//! [`FindingsScanner`] is the incremental version of [`scan`]: `push` bytes as
//! they arrive and get the findings that became final. Each extractor keeps a
//! cursor; a candidate is decided once its look-ahead window (`LOOK_*`) is
//! fully received, on exactly the window a one-shot scan would use, so
//! `push`ing a buffer in any chunking and then `finish()`ing yields the same
//! findings as `scan` on the whole buffer. Memory is bounded: bytes more than
//! [`KEEP_BACK`] behind every cursor are dropped. Offsets are absolute.
//!
//! Look-ahead windows cap how far a parser may read from a candidate. Python
//! reads to the end of the buffer; the caps (e.g. 8 KiB certificate plus 64 KiB
//! of trailing TLV) only differ from Python for absurd malformed lengths.

use std::collections::{BTreeMap, HashSet, VecDeque};

use super::stream::{
    Rolling, ascii_lossy, be16, be24, find, find_within, py_prefix, py_split_ws, py_strip,
    rfind_byte, strip_bytes,
};

/// Bytes kept behind the slowest cursor (HTTP request start-line walk-back is
/// 8192, the SIP one is ~1 KiB).
pub const KEEP_BACK: usize = 8200;

/// Look-ahead in bytes after a candidate's first byte, per extractor.
const LOOK_CERT: usize = 8192 + 65_536 + 16;
const LOOK_FRAG: usize = 3000 + 64 + 65_536;
const LOOK_DNS: usize = 1520;
const LOOK_HTTP: usize = 2300;
// Record (<=16 KiB), then ClientHello fields whose own lengths can exceed the
// record: 2+32+1+255+2+65535+1+255+2+65535 bytes.
const LOOK_TLS: usize = 140_000;
const LOOK_URL: usize = 215;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Cert,
    Dns,
    Http,
    Tls,
    Url,
}

impl Kind {
    /// The Python `Finding.kind` string.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Cert => "cert",
            Kind::Dns => "dns",
            Kind::Http => "http",
            Kind::Tls => "tls",
            Kind::Url => "url",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub kind: Kind,
    /// Absolute offset in the payload.
    pub offset: usize,
    pub summary: String,
    pub detail: Vec<String>,
}

// --- minimal DER ------------------------------------------------------------

/// `(tag, content_start, content_end)`.
pub(crate) type Tlv = (u8, usize, usize);

/// `(length, index_after_length)`. Rejects indefinite/oversized/non-minimal.
fn der_len(b: &[u8], i: usize) -> Option<(usize, usize)> {
    let n = *b.get(i)?;
    if n < 0x80 {
        return Some((n as usize, i + 1));
    }
    let k = (n & 0x7f) as usize;
    if k == 0 || k > 4 || i + 1 + k > b.len() {
        return None; // indefinite form is illegal in DER
    }
    let v = b[i + 1..i + 1 + k]
        .iter()
        .fold(0usize, |a, &x| (a << 8) | x as usize);
    if v < 0x80 || (k > 1 && b[i + 1] == 0) {
        return None; // non-minimal encoding: not real DER
    }
    Some((v, i + 1 + k))
}

pub(crate) fn tlv(b: &[u8], i: usize) -> Option<Tlv> {
    let tag = *b.get(i)?;
    let (ln, j) = der_len(b, i + 1)?;
    if j + ln > b.len() {
        return None;
    }
    Some((tag, j, j + ln))
}

/// Children of a constructed TLV; `None` unless they tile `[start, end)` exactly.
pub(crate) fn children(b: &[u8], start: usize, end: usize) -> Option<Vec<Tlv>> {
    let mut out = Vec::new();
    let mut i = start;
    while i < end {
        let t = tlv(b, i)?;
        if t.2 > end {
            return None; // a child overran its parent
        }
        out.push(t);
        i = t.2;
    }
    if i == end { Some(out) } else { None }
}

fn oid_name(oid: &[u8]) -> Option<&'static str> {
    // id-at-*, X.520: 2.5.4.x
    match oid {
        [0x55, 0x04, 0x03] => Some("CN"),
        [0x55, 0x04, 0x06] => Some("C"),
        [0x55, 0x04, 0x07] => Some("L"),
        [0x55, 0x04, 0x08] => Some("ST"),
        [0x55, 0x04, 0x0a] => Some("O"),
        [0x55, 0x04, 0x0b] => Some("OU"),
        _ => None,
    }
}
/// id-ce-subjectAltName, 2.5.29.17.
const OID_SAN: [u8; 3] = [0x55, 0x1d, 0x11];

/// RDNSequence -> `CN=x, O=y`. Best effort; unknown OIDs are skipped.
fn dn_name(b: &[u8], start: usize, end: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (_t, s, e) in children(b, start, end).unwrap_or_default() {
        for (_t2, s2, e2) in children(b, s, e).unwrap_or_default() {
            let kids = children(b, s2, e2).unwrap_or_default();
            if kids.len() != 2 || kids[0].0 != 0x06 {
                continue;
            }
            if let Some(key) = oid_name(&b[kids[0].1..kids[0].2]) {
                // `String::from_utf8_lossy` replaces bad sequences with U+FFFD
                // exactly like Python's decode("utf-8", "replace").
                parts.push(format!(
                    "{key}={}",
                    String::from_utf8_lossy(&b[kids[1].1..kids[1].2])
                ));
            }
        }
    }
    parts.join(", ")
}

/// dNSName entries of a subjectAltName extension found in `b[tbs_start..tbs_end]`.
/// (The Python has a first tree-walking loop whose results are never used; it
/// is dropped here.)
fn san(b: &[u8], tbs_start: usize, tbs_end: usize) -> Vec<String> {
    let mut names = Vec::new();
    let mut i = tbs_start;
    while let Some(j) = find_within(b, &OID_SAN, i, tbs_end) {
        i = j + 1;
        let Some(mut t) = tlv(b, j + OID_SAN.len()) else {
            continue;
        };
        if t.0 == 0x01 {
            // optional critical BOOLEAN
            match tlv(b, t.2) {
                Some(n) => t = n,
                None => continue,
            }
        }
        if t.0 != 0x04 {
            continue; // extnValue OCTET STRING
        }
        let Some(inner) = tlv(b, t.1) else { continue };
        if inner.0 != 0x30 {
            continue;
        }
        for (tag, cs, ce) in children(b, inner.1, inner.2).unwrap_or_default() {
            if tag == 0x82 {
                names.push(ascii_lossy(&b[cs..ce])); // [2] dNSName
            }
        }
    }
    names
}

/// A DER X.509 certificate starting at `start`, validated by exact length
/// consistency. Returns the finding (relative offset) and the end index.
///
/// `Certificate ::= SEQUENCE { tbsCertificate SEQUENCE, signatureAlgorithm
/// SEQUENCE, signatureValue BIT STRING }` — exactly three children consuming
/// the outer SEQUENCE to the byte.
fn x509_at(b: &[u8], start: usize) -> Option<(Finding, usize)> {
    let t = tlv(b, start)?;
    if t.2 - start > 8192 {
        return None;
    }
    let kids = children(b, t.1, t.2)?;
    if kids.len() != 3 || kids[0].0 != 0x30 || kids[1].0 != 0x30 || kids[2].0 != 0x03 {
        return None;
    }
    let tbs = children(b, kids[0].1, kids[0].2)?;
    if tbs.len() < 6 {
        return None;
    }
    // TBSCertificate: [0] version?, serial, sigalg, issuer, validity, subject, ...
    let k = usize::from(tbs[0].0 == 0xa0);
    if tbs.len() < k + 6 || tbs[k].0 != 0x02 {
        return None; // serialNumber INTEGER
    }
    let (issuer, validity, subject) = (tbs[k + 2], tbs[k + 3], tbs[k + 4]);
    if issuer.0 != 0x30 || validity.0 != 0x30 || subject.0 != 0x30 {
        return None;
    }
    let va = children(b, validity.1, validity.2)?;
    if va.len() != 2 || !matches!(va[0].0, 0x17 | 0x18) {
        return None;
    }
    let sub = dn_name(b, subject.1, subject.2);
    let iss = dn_name(b, issuer.1, issuer.2);
    if sub.is_empty() && iss.is_empty() {
        return None;
    }
    let nb = ascii_lossy(&b[va[0].1..va[0].2]);
    let na = ascii_lossy(&b[va[1].1..va[1].2]);
    let serial: String = b[tbs[k].1..tbs[k].2]
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect();
    let mut det = vec![
        format!("issuer   {iss}"),
        format!("validity {nb} .. {na}"),
        format!("serial   {serial}"),
        format!("{} bytes DER", t.2 - start),
    ];
    for d in san(b, kids[0].1, kids[0].2).into_iter().take(12) {
        det.push(format!("dNSName  {d}"));
    }
    let summary = if sub.is_empty() { iss } else { sub };
    Some((
        Finding {
            kind: Kind::Cert,
            offset: start,
            summary,
            detail: det,
        },
        t.2,
    ))
}

/// UTCTime `YYMMDDHHMMSSZ` or GeneralizedTime `YYYYMMDDHHMMSSZ` -> ISO date.
fn isotime(v: &[u8]) -> Option<String> {
    let s = ascii_lossy(v);
    let d = s.strip_suffix('Z')?;
    if d.is_empty() || !d.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let d = match d.len() {
        12 => {
            let yy: u32 = d[..2].parse().ok()?;
            format!("{}{}", if yy >= 50 { "19" } else { "20" }, d)
        }
        14 => d.to_string(),
        _ => return None,
    };
    Some(format!("{}-{}-{}", &d[0..4], &d[4..6], &d[6..8]))
}

/// A certificate recognised by its Validity block when the whole DER will not
/// parse (the payload is FEC blocks concatenated with no Bearer Connection
/// reassembly, so almost no certificate parses end to end).
/// `Validity ::= SEQUENCE { notBefore Time, notAfter Time }`.
fn frag_at(b: &[u8], start: usize) -> Option<Finding> {
    const WINDOW: usize = 3000;
    let t = tlv(b, start)?;
    if t.0 != 0x30 {
        return None;
    }
    let kids = children(b, t.1, t.2)?;
    if kids.len() != 2 || kids.iter().any(|k| !matches!(k.0, 0x17 | 0x18)) {
        return None;
    }
    let nb = isotime(&b[kids[0].1..kids[0].2])?;
    let na = isotime(&b[kids[1].1..kids[1].2])?;
    if na <= nb {
        return None; // string compare of ISO dates, like Python
    }
    let sub = match tlv(b, t.2) {
        Some(n) if n.0 == 0x30 => dn_name(b, n.1, n.2),
        _ => String::new(),
    };
    let mut det = vec![format!("valid {nb} .. {na}")];
    for d in san(b, t.2, b.len().min(t.2 + WINDOW)).into_iter().take(10) {
        det.push(format!("dNSName  {d}"));
    }
    let summary = if sub.is_empty() {
        "(subject not recoverable)".to_string()
    } else {
        sub
    };
    Some(Finding {
        kind: Kind::Cert,
        offset: start,
        summary,
        detail: det,
    })
}

// --- DNS --------------------------------------------------------------------

fn qtype_name(t: usize) -> Option<&'static str> {
    Some(match t {
        1 => "A",
        2 => "NS",
        5 => "CNAME",
        6 => "SOA",
        12 => "PTR",
        15 => "MX",
        16 => "TXT",
        28 => "AAAA",
        33 => "SRV",
        35 => "NAPTR",
        41 => "OPT",
        43 => "DS",
        48 => "DNSKEY",
        65 => "HTTPS",
        255 => "ANY",
        _ => return None,
    })
}

/// Python's `^[A-Za-z0-9_-]+$`. Note `$` also matches before one trailing
/// newline, so a label of host chars followed by `\n` is accepted too.
fn hostchars(lab: &[u8]) -> bool {
    let ok = |c: &u8| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-';
    let body = match lab.split_last() {
        Some((b'\n', rest)) if !rest.is_empty() => rest,
        _ => lab,
    };
    !body.is_empty() && body.iter().all(ok)
}

/// Uncompressed or pointer-terminated name: `(name, next_index)`.
fn dns_name(b: &[u8], mut i: usize, end: usize) -> (Option<String>, usize) {
    let mut labels: Vec<String> = Vec::new();
    let mut total = 0;
    while i < end {
        let n = b[i] as usize;
        if n == 0 {
            return (Some(labels.join(".")), i + 1);
        }
        if n & 0xc0 == 0xc0 {
            labels.push("*".into()); // compression pointer
            return (Some(labels.join(".")), i + 2);
        }
        if n > 63 || i + 1 + n > end {
            return (None, i);
        }
        let lab = &b[i + 1..i + 1 + n];
        if !hostchars(lab) {
            return (None, i);
        }
        labels.push(ascii_lossy(lab));
        total += n + 1;
        if total > 253 {
            return (None, i);
        }
        i += 1 + n;
    }
    (None, i)
}

/// Decide the DNS candidate at `i` of view `b` (whose length plays the role of
/// Python's `n`). Requires `i + 16 < b.len()`.
fn dns_at(b: &[u8], i: usize) -> Option<Finding> {
    let n = b.len();
    if be16(b, i + 4) != 1 {
        return None;
    }
    let fl = be16(b, i + 2);
    if (fl >> 11) & 0xf != 0 || (fl >> 4) & 0x7 != 0 || (fl & 0xf) > 10 {
        return None; // opcode QUERY, Z reserved zero, RCODE <= 10
    }
    let (an, ns, ar) = (be16(b, i + 6), be16(b, i + 8), be16(b, i + 10));
    if an > 64 || ns > 64 || ar > 64 {
        return None;
    }
    let (name, j) = dns_name(b, i + 12, n.min(i + 1500));
    let name = name?;
    if name.is_empty() || !name.contains('.') || j + 4 > n {
        return None;
    }
    let qt = be16(b, j);
    let qc = be16(b, j + 2);
    let qn = qtype_name(qt)?;
    if qc != 1 {
        return None;
    }
    let kind = if fl & 0x8000 != 0 {
        "response"
    } else {
        "query"
    };
    Some(Finding {
        kind: Kind::Dns,
        offset: i,
        summary: format!("{name}  {qn}  ({kind})"),
        detail: vec![format!(
            "id 0x{:04x}  answers {an}, authority {ns}, additional {ar}",
            be16(b, i)
        )],
    })
}

// --- HTTP -------------------------------------------------------------------

const METHODS: [&[u8]; 9] = [
    b"GET ",
    b"POST ",
    b"HEAD ",
    b"PUT ",
    b"DELETE ",
    b"OPTIONS ",
    b"CONNECT ",
    b"TRACE ",
    b"PATCH ",
];
const HDRS: [&str; 7] = [
    "host",
    "user-agent",
    "content-type",
    "server",
    "location",
    "content-length",
    "cache-control",
];

/// `bytes.split(b"\r\n")`.
pub(crate) fn split_crlf(s: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(p) = find(s, b"\r\n", i) {
        out.push(&s[i..p]);
        i = p + 2;
    }
    out.push(&s[i..]);
    out
}

/// Interesting header lines following a start line, until a blank line.
/// Values are cut at the first non-printable byte: a header near the end of a
/// decoded block is followed by unrelated traffic, not its own CRLF.
fn headers(b: &[u8], i: usize) -> Vec<String> {
    const LIMIT: usize = 2048;
    let end = find_within(b, b"\r\n\r\n", i, i + LIMIT);
    let seg = &b[i..end.unwrap_or_else(|| b.len().min(i + LIMIT))];
    let mut out = Vec::new();
    for line in split_crlf(seg) {
        let Some(c) = line.iter().position(|&x| x == b':') else {
            continue;
        };
        let (k, v) = (&line[..c], &line[c + 1..]);
        let kraw = ascii_lossy(k);
        let ks = py_strip(&kraw);
        if !HDRS.contains(&ks.to_lowercase().as_str()) {
            continue;
        }
        let clean: Vec<u8> = strip_bytes(v)
            .iter()
            .copied()
            .take_while(|c| (0x20..=0x7e).contains(c))
            .collect();
        if !clean.is_empty() {
            let cs = ascii_lossy(&clean);
            out.push(format!("{ks}: {}", py_prefix(&cs, 120)));
        }
    }
    out
}

/// Decide an `HTTP/1.[01]` match at `p`. `base` is the absolute offset of
/// `b[0]` (needed only for the quirky start-of-line search, see below).
fn http_at(b: &[u8], p: usize, base: usize) -> Option<Finding> {
    let line_end = find_within(b, b"\r\n", p, p + 202)?; // line_end - p <= 200
    let line = &b[p..line_end];
    // response: the version starts the line: HTTP/1.x SP 3*DIGIT
    if line.len() >= 12 && line[8] == b' ' && line[9..12].iter().all(u8::is_ascii_digit) {
        return Some(Finding {
            kind: Kind::Http,
            offset: p,
            summary: ascii_lossy(line),
            detail: headers(b, line_end + 2),
        });
    }
    // request: method and URI precede it on the same line.
    // Python: ls = rfind(b"\n", max(0, p-8192), p) + 1, so when there is no
    // newline in the window ls is 0 (start of the whole payload), and the
    // `len(head) < 2048` test below then only passes near the payload start.
    let lo = (base + p).saturating_sub(8192).saturating_sub(base);
    let ls = match rfind_byte(b, b'\n', lo, p) {
        Some(r) => r + 1,
        None if base == 0 => 0,
        None => return None,
    };
    let head = &b[ls..line_end];
    if METHODS.iter().any(|m| head.starts_with(m)) && head.len() < 2048 {
        return Some(Finding {
            kind: Kind::Http,
            offset: ls,
            summary: ascii_lossy(head),
            detail: headers(b, line_end + 2),
        });
    }
    None
}

// --- TLS --------------------------------------------------------------------

/// TLS ClientHello (with SNI) / ServerHello at `i`, validated by nested
/// lengths: the 16-bit record length must contain the 24-bit handshake length
/// exactly.
fn tls_at(b: &[u8], i: usize) -> Option<Finding> {
    let n = b.len();
    if b[i + 1] != 0x03 || b[i + 2] > 0x04 {
        return None;
    }
    let rec = be16(b, i + 3);
    if !(4..=16384).contains(&rec) || i + 5 + rec > n {
        return None;
    }
    let ht = b[i + 5];
    if ht != 0x01 && ht != 0x02 {
        return None;
    }
    if be24(b, i + 6) + 4 != rec {
        return None; // handshake must fill record
    }
    let mut p = i + 9;
    if p + 34 > n {
        return None;
    }
    p += 2 + 32; // version, random
    let sl = *b.get(p)? as usize; // Python would raise IndexError here
    p += 1 + sl;
    if ht == 0x02 {
        return Some(Finding {
            kind: Kind::Tls,
            offset: i,
            summary: "ServerHello".into(),
            detail: vec![],
        });
    }
    if p + 2 > n {
        return None;
    }
    p += 2 + be16(b, p);
    if p >= n {
        return None;
    }
    p += 1 + b[p] as usize;
    if p + 2 > n {
        return None;
    }
    let ext_total = be16(b, p);
    p += 2;
    let stop = n.min(p + ext_total);
    let mut sni = String::new();
    while p + 4 <= stop {
        let et = be16(b, p);
        let el = be16(b, p + 2);
        p += 4;
        if et == 0 && p + 5 <= stop {
            let ln = be16(b, p + 3);
            if p + 5 + ln <= stop {
                sni = ascii_lossy(&b[p + 5..p + 5 + ln]);
            }
        }
        p += el;
    }
    let summary = if sni.is_empty() {
        "ClientHello".to_string()
    } else {
        format!("ClientHello  SNI {sni}")
    };
    Some(Finding {
        kind: Kind::Tls,
        offset: i,
        summary,
        detail: vec![],
    })
}

// --- URLs -------------------------------------------------------------------

fn url_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"._~:/?#[]@!$&'()*+,;=%-".contains(&c)
}

/// Regex `https?://[class]{4,200}` anchored at `p`: end of the match.
fn url_match(b: &[u8], p: usize) -> Option<usize> {
    let mut j = p + 4;
    if b.get(j) == Some(&b's') {
        j += 1;
    }
    if b.get(j..j + 3)? != b"://" {
        return None;
    }
    let k = j + 3;
    let run = b[k..]
        .iter()
        .take(200)
        .take_while(|&&c| url_char(c))
        .count();
    if run >= 4 { Some(k + run) } else { None }
}

// --- the incremental scanner ------------------------------------------------

/// One extractor's cursor.
#[derive(Debug, Default)]
struct Cur(usize);

/// Incremental version of [`scan`]. See the module docs.
#[derive(Debug, Default)]
pub struct FindingsScanner {
    win: Rolling,
    fin: bool,
    cert: Cur,
    cert_spans: VecDeque<(usize, usize)>,
    frag: [Cur; 2],
    frag_seen: HashSet<usize>,
    frag_pending: [VecDeque<Finding>; 2],
    dns: Cur,
    http: Cur,
    tls: Cur,
    url: Cur,
    url_seen: HashSet<String>,
}

const FRAG_TAGS: [&[u8]; 2] = [b"\x17\x0d", b"\x18\x0f"];

impl FindingsScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; returns the findings that became final, ordered by
    /// `(offset, kind)` within this batch. (Batches are not globally ordered:
    /// different extractors have different look-ahead.)
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Finding> {
        if self.fin {
            return Vec::new();
        }
        self.win.push(bytes);
        self.run()
    }

    /// No more bytes: decide everything left.
    pub fn finish(&mut self) -> Vec<Finding> {
        self.fin = true;
        self.run()
    }

    fn run(&mut self) -> Vec<Finding> {
        let fin = self.fin;
        let base = self.win.base();
        let buf = self.win.data();
        let n = buf.len();

        // Each extractor's findings are collected as a group, in the same
        // order Python concatenates them, so ties sort identically.
        let mut full = Vec::new();
        let mut dnsf = Vec::new();
        let mut httpf = Vec::new();
        let mut tlsf = Vec::new();
        let mut urlf = Vec::new();

        // X.509: greedy, cursor jumps over an accepted certificate.
        {
            let mut i = self.cert.0 - base;
            loop {
                let Some(p) = find(buf, b"\x30\x82", i) else {
                    i = if fin { n } else { i.max(n.saturating_sub(1)) };
                    break;
                };
                if !fin && p + LOOK_CERT > n {
                    i = p;
                    break;
                }
                match x509_at(&buf[..n.min(p + LOOK_CERT)], p) {
                    Some((mut f, end)) => {
                        f.offset += base;
                        self.cert_spans.push_back((f.offset, f.offset + 8192));
                        full.push(f);
                        i = end;
                    }
                    None => i = p + 1,
                }
            }
            self.cert.0 = base + i;
        }

        // Certificate fragments: one pass per time tag.
        for (pass, tt) in FRAG_TAGS.iter().enumerate() {
            let mut i = self.frag[pass].0 - base;
            loop {
                let Some(idx) = find(buf, tt, i) else {
                    i = if fin { n } else { i.max(n.saturating_sub(1)) };
                    break;
                };
                if !fin && idx + LOOK_FRAG > n {
                    i = idx;
                    break;
                }
                i = idx + 1;
                if base + idx < 2 {
                    continue;
                }
                let start = idx - 2;
                if self.frag_seen.contains(&(base + start)) {
                    continue;
                }
                if let Some(mut f) = frag_at(&buf[..n.min(start + LOOK_FRAG)], start) {
                    f.offset += base;
                    self.frag_seen.insert(f.offset);
                    self.frag_pending[pass].push_back(f);
                }
            }
            self.frag[pass].0 = base + i;
        }
        // A fragment inside a certificate that parsed whole is the same
        // certificate seen twice. It can only be judged once every full
        // certificate starting at or before it has been decided.
        let mut fragf = Vec::new();
        for pass in 0..2 {
            while let Some(f) = self.frag_pending[pass].front() {
                if f.offset >= self.cert.0 {
                    break;
                }
                let f = self.frag_pending[pass].pop_front().unwrap();
                if !self
                    .cert_spans
                    .iter()
                    .any(|&(a, b)| a <= f.offset && f.offset < b)
                {
                    fragf.push(f);
                }
            }
        }
        let fmin = self
            .frag_pending
            .iter()
            .filter_map(|q| q.front().map(|f| f.offset))
            .chain(self.frag.iter().map(|c| c.0))
            .min()
            .unwrap_or(0);
        while self.cert_spans.front().is_some_and(|&(_, b)| b <= fmin) {
            self.cert_spans.pop_front();
        }

        // DNS: position-local, so scan every position below the limit.
        {
            let lim = if fin {
                n.saturating_sub(16)
            } else {
                n.saturating_sub(LOOK_DNS)
            };
            let mut i = self.dns.0 - base;
            while i < lim {
                if buf[i + 5] == 1
                    && buf[i + 4] == 0
                    && let Some(mut f) = dns_at(&buf[..n.min(i + LOOK_DNS)], i)
                {
                    f.offset += base;
                    dnsf.push(f);
                }
                i += 1;
            }
            self.dns.0 = base + i.max(self.dns.0 - base);
        }

        // HTTP lines: finditer over `HTTP/1.[01]`, non-overlapping.
        {
            let mut i = self.http.0 - base;
            loop {
                let Some(p) = find(buf, b"HTTP/1.", i) else {
                    i = if fin { n } else { i.max(n.saturating_sub(6)) };
                    break;
                };
                if !fin && p + LOOK_HTTP > n {
                    i = p;
                    break;
                }
                if !matches!(buf.get(p + 7), Some(b'0' | b'1')) {
                    i = p + 1;
                    continue;
                }
                if let Some(mut f) = http_at(&buf[..n.min(p + LOOK_HTTP)], p, base) {
                    f.offset += base;
                    httpf.push(f);
                }
                i = p + 8;
            }
            self.http.0 = base + i;
        }

        // TLS: position-local.
        {
            let lim = if fin {
                n.saturating_sub(9)
            } else {
                n.saturating_sub(LOOK_TLS)
            };
            let mut i = self.tls.0 - base;
            while i < lim {
                match buf[i..lim].iter().position(|&c| c == 0x16) {
                    None => {
                        i = lim;
                    }
                    Some(d) => {
                        i += d;
                        if let Some(mut f) = tls_at(&buf[..n.min(i + LOOK_TLS)], i) {
                            f.offset += base;
                            tlsf.push(f);
                        }
                        i += 1;
                    }
                }
            }
            self.tls.0 = base + i.max(self.tls.0 - base);
        }

        // URLs: first occurrence of each distinct URL wins.
        {
            let mut i = self.url.0 - base;
            loop {
                let Some(p) = find(buf, b"http", i) else {
                    i = if fin { n } else { i.max(n.saturating_sub(3)) };
                    break;
                };
                if !fin && p + LOOK_URL > n {
                    i = p;
                    break;
                }
                match url_match(&buf[..n.min(p + LOOK_URL)], p) {
                    Some(end) => {
                        let m = ascii_lossy(&buf[p..end]);
                        let u = m
                            .trim_end_matches(['.', ',', ')', ';', '\'', '"'])
                            .to_string();
                        if self.url_seen.insert(u.clone()) {
                            urlf.push(Finding {
                                kind: Kind::Url,
                                offset: base + p,
                                summary: u,
                                detail: vec![],
                            });
                        }
                        i = end;
                    }
                    None => i = p + 1,
                }
            }
            self.url.0 = base + i;
        }

        let mut out = full;
        out.extend(fragf);
        out.extend(dnsf);
        out.extend(httpf);
        out.extend(tlsf);
        out.extend(urlf);
        out.sort_by_key(|f| (f.offset, f.kind)); // stable, like Python's sort

        let keep = [
            self.cert.0,
            self.frag[0].0,
            self.frag[1].0,
            self.dns.0,
            self.http.0,
            self.tls.0,
            self.url.0,
        ]
        .into_iter()
        .min()
        .unwrap_or(0)
        .saturating_sub(KEEP_BACK);
        self.win.trim_before(keep);
        out
    }
}

// --- one-shot API -----------------------------------------------------------

fn one_shot(blob: &[u8]) -> Vec<Finding> {
    let mut s = FindingsScanner::new();
    let mut out = s.push(blob);
    out.extend(s.finish());
    out.sort_by_key(|f| (f.offset, f.kind));
    out
}

/// All extractors, findings sorted by `(offset, kind)`.
pub fn scan(blob: &[u8]) -> Vec<Finding> {
    one_shot(blob)
}

fn only(blob: &[u8], k: Kind) -> Vec<Finding> {
    one_shot(blob).into_iter().filter(|f| f.kind == k).collect()
}

/// DER X.509 certificates that parse whole (note: `scan` also drops
/// fragments overlapping these).
pub fn x509_certs(blob: &[u8]) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(p) = find(blob, b"\x30\x82", i) {
        match x509_at(&blob[..blob.len().min(p + LOOK_CERT)], p) {
            Some((f, end)) => {
                out.push(f);
                i = end;
            }
            None => i = p + 1,
        }
    }
    out
}

/// Certificates recognised by their validity block, fragments included.
/// Unlike `scan`, does not drop fragments inside whole certificates.
pub fn cert_fragments(blob: &[u8]) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for tt in FRAG_TAGS {
        let mut i = 0;
        while let Some(idx) = find(blob, tt, i) {
            i = idx + 1;
            if idx < 2 || seen.contains(&(idx - 2)) {
                continue;
            }
            if let Some(f) = frag_at(&blob[..blob.len().min(idx - 2 + LOOK_FRAG)], idx - 2) {
                seen.insert(f.offset);
                out.push(f);
            }
        }
    }
    out
}

pub fn dns_messages(blob: &[u8]) -> Vec<Finding> {
    only(blob, Kind::Dns)
}
pub fn http_messages(blob: &[u8]) -> Vec<Finding> {
    only(blob, Kind::Http)
}
pub fn tls_hellos(blob: &[u8]) -> Vec<Finding> {
    only(blob, Kind::Tls)
}
pub fn urls(blob: &[u8]) -> Vec<Finding> {
    only(blob, Kind::Url)
}

// --- streaming DNS (used by the terminals module) ---------------------------

/// Streaming DNS-message finder that also hands the caller the message bytes.
#[derive(Debug, Default)]
pub struct DnsScanner {
    win: Rolling,
    cur: usize,
    fin: bool,
}

impl DnsScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Absolute offset below which every position has been decided.
    pub fn cursor(&self) -> usize {
        self.cur
    }

    /// Feed bytes. `on_hit(finding, bytes)` is called for each message, where
    /// `bytes` runs from the message start to `min(end of data, start + 1500)`
    /// (so it is exactly what Python's `dns_answers` sees).
    pub fn push(&mut self, bytes: &[u8], on_hit: impl FnMut(Finding, &[u8])) {
        if !self.fin {
            self.win.push(bytes);
            self.run(on_hit);
        }
    }

    pub fn finish(&mut self, on_hit: impl FnMut(Finding, &[u8])) {
        self.fin = true;
        self.run(on_hit);
    }

    fn run(&mut self, mut on_hit: impl FnMut(Finding, &[u8])) {
        let base = self.win.base();
        let buf = self.win.data();
        let n = buf.len();
        let lim = if self.fin {
            n.saturating_sub(16)
        } else {
            n.saturating_sub(LOOK_DNS)
        };
        let mut i = self.cur - base;
        while i < lim {
            if buf[i + 4] == 0
                && buf[i + 5] == 1
                && let Some(mut f) = dns_at(&buf[..n.min(i + LOOK_DNS)], i)
            {
                f.offset += base;
                on_hit(f, &buf[i..n.min(i + 1500)]);
            }
            i += 1;
        }
        self.cur = base + i.max(self.cur - base);
        self.win.trim_before(self.cur);
    }
}

// --- hostnames --------------------------------------------------------------

/// Running tally of hostnames implicated by findings: "who was this terminal
/// talking to", which no single extractor gives on its own.
#[derive(Debug, Default, Clone)]
pub struct HostTally {
    counts: BTreeMap<usize, (String, usize)>,
    index: std::collections::HashMap<String, usize>,
}

impl HostTally {
    pub fn new() -> Self {
        Self::default()
    }

    fn bump(&mut self, h: &str) {
        let next = self.index.len();
        let k = *self.index.entry(h.to_string()).or_insert(next);
        self.counts.entry(k).or_insert_with(|| (h.to_string(), 0)).1 += 1;
    }

    /// Account for one finding (same rules as Python `hosts`).
    pub fn add(&mut self, f: &Finding) {
        match f.kind {
            Kind::Dns => {
                if let Some(h) = py_split_ws(&f.summary).next() {
                    self.bump(h);
                }
            }
            Kind::Tls => {
                if let Some((_, rest)) = f.summary.split_once("SNI ") {
                    self.bump(py_strip(rest));
                }
            }
            Kind::Cert => {
                for d in &f.detail {
                    if let Some(rest) = d.strip_prefix("dNSName  ") {
                        self.bump(py_strip(rest));
                    }
                }
                for part in f.summary.split(',') {
                    if let Some(cn) = py_strip(part).strip_prefix("CN=") {
                        self.bump(cn);
                    }
                }
            }
            Kind::Http => {
                for d in &f.detail {
                    if d.to_lowercase().starts_with("host:")
                        && let Some((_, v)) = d.split_once(':')
                    {
                        self.bump(py_strip(v));
                    }
                }
            }
            Kind::Url => {
                let s = &f.summary;
                let rest = s
                    .strip_prefix("https://")
                    .or_else(|| s.strip_prefix("http://"));
                if let Some(rest) = rest {
                    let h: &str = rest.split(['/', ':']).next().unwrap_or("");
                    if !h.is_empty() {
                        self.bump(h);
                    }
                }
            }
        }
    }

    /// `(host, count)` by descending count, ties in first-seen order
    /// (Python `Counter.most_common()`).
    pub fn most_common(&self) -> Vec<(String, usize)> {
        let mut v: Vec<(String, usize)> = self.counts.values().cloned().collect();
        v.sort_by_key(|e| std::cmp::Reverse(e.1)); // stable
        v
    }
}

/// Hostnames implicated by `findings`, most common first.
pub fn hosts(findings: &[Finding]) -> Vec<(String, usize)> {
    let mut t = HostTally::new();
    for f in findings {
        t.add(f);
    }
    t.most_common()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tlv_bytes(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![tag];
        match body.len() {
            n if n < 0x80 => v.push(n as u8),
            n if n < 0x100 => v.extend_from_slice(&[0x81, n as u8]),
            n => v.extend_from_slice(&[0x82, (n >> 8) as u8, n as u8]),
        }
        v.extend_from_slice(body);
        v
    }

    fn rdn(oid_last: u8, val: &str) -> Vec<u8> {
        let oid = tlv_bytes(0x06, &[0x55, 0x04, oid_last]);
        let v = tlv_bytes(0x0c, val.as_bytes());
        tlv_bytes(0x31, &tlv_bytes(0x30, &[oid, v].concat()))
    }

    pub(crate) fn make_cert(cn: &str) -> Vec<u8> {
        let name = tlv_bytes(0x30, &[rdn(0x06, "US"), rdn(0x03, cn)].concat());
        let validity = tlv_bytes(
            0x30,
            &[
                tlv_bytes(0x17, b"230101000000Z"),
                tlv_bytes(0x17, b"250101000000Z"),
            ]
            .concat(),
        );
        let sigalg = tlv_bytes(
            0x30,
            &tlv_bytes(
                0x06,
                &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b],
            ),
        );
        let spki = tlv_bytes(
            0x30,
            &[sigalg.clone(), tlv_bytes(0x03, &[7u8; 300])].concat(),
        );
        // subjectAltName extension with one dNSName
        let san_val = tlv_bytes(0x30, &tlv_bytes(0x82, b"alt.example.net"));
        let ext = tlv_bytes(
            0x30,
            &[tlv_bytes(0x06, &OID_SAN), tlv_bytes(0x04, &san_val)].concat(),
        );
        let exts = tlv_bytes(0xa3, &tlv_bytes(0x30, &ext));
        let tbs = tlv_bytes(
            0x30,
            &[
                tlv_bytes(0xa0, &tlv_bytes(0x02, &[2])),
                tlv_bytes(0x02, &[0x12, 0x34]),
                sigalg.clone(),
                name.clone(),
                validity,
                name,
                spki,
                exts,
            ]
            .concat(),
        );
        tlv_bytes(0x30, &[tbs, sigalg, tlv_bytes(0x03, &[0; 40])].concat())
    }

    fn sample() -> Vec<u8> {
        let mut b = vec![0u8; 100];
        b.extend(make_cert("host.example.com"));
        b.extend([0xAAu8; 50]);
        // DNS response for myip.opendns.com A
        let mut d = vec![0x12, 0x34, 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0];
        for l in ["myip", "opendns", "com"] {
            d.push(l.len() as u8);
            d.extend(l.bytes());
        }
        d.extend([0, 0, 1, 0, 1]);
        b.extend(&d);
        b.extend([0x55u8; 20]);
        b.extend(
            b"
GET /index.html HTTP/1.1\r\nHost: www.example.org\r\nUser-Agent: t\r\n\r\n",
        );
        b.extend([0xEEu8; 20]);
        b.extend(b"visit https://docs.example.com/a/b. now");
        b.extend([0u8; 300]);
        b
    }

    #[test]
    fn extracts_each_kind() {
        let b = sample();
        let f = scan(&b);
        let kinds: Vec<_> = f.iter().map(|x| x.kind).collect();
        assert!(kinds.contains(&Kind::Cert));
        let cert = f.iter().find(|x| x.kind == Kind::Cert).unwrap();
        assert_eq!(cert.summary, "C=US, CN=host.example.com");
        assert_eq!(cert.offset, 100);
        assert!(cert.detail.iter().any(|d| d == "dNSName  alt.example.net"));
        assert!(
            cert.detail
                .iter()
                .any(|d| d == "validity 230101000000Z .. 250101000000Z")
        );
        // fragment inside the whole certificate is suppressed
        assert_eq!(f.iter().filter(|x| x.kind == Kind::Cert).count(), 1);
        let dns = f.iter().find(|x| x.kind == Kind::Dns).unwrap();
        assert_eq!(dns.summary, "myip.opendns.com  A  (response)");
        assert_eq!(
            dns.detail[0],
            "id 0x1234  answers 1, authority 0, additional 0"
        );
        let http = f.iter().find(|x| x.kind == Kind::Http).unwrap();
        assert_eq!(http.summary, "GET /index.html HTTP/1.1");
        assert_eq!(http.offset, b.iter().position(|&c| c == b'G').unwrap());
        assert_eq!(http.detail, ["Host: www.example.org", "User-Agent: t"]);
        let url = f.iter().find(|x| x.kind == Kind::Url).unwrap();
        assert_eq!(url.summary, "https://docs.example.com/a/b");
        let h = hosts(&f);
        let names: Vec<_> = h.iter().map(|x| x.0.as_str()).collect();
        for want in [
            "host.example.com",
            "alt.example.net",
            "myip.opendns.com",
            "www.example.org",
            "docs.example.com",
        ] {
            assert!(names.contains(&want), "{want} in {names:?}");
        }
    }

    #[test]
    fn fragment_reported_when_cert_is_damaged() {
        let mut b = vec![0u8; 50];
        let mut c = make_cert("frag.example.com");
        c[5] ^= 0xff; // break the outer structure but not the Validity block
        b.extend(c);
        b.extend([0u8; 100]);
        let f = scan(&b);
        let certs: Vec<_> = f.iter().filter(|x| x.kind == Kind::Cert).collect();
        assert_eq!(certs.len(), 1);
        assert!(certs[0].detail[0].starts_with("valid 2023-01-01 .. 2025-01-01"));
    }

    #[test]
    fn tls_client_hello_sni() {
        let sni = b"tls.example.com";
        let mut ext = vec![
            0,
            0,
            0,
            (sni.len() + 5) as u8,
            0,
            (sni.len() + 3) as u8,
            0,
            0,
            sni.len() as u8,
        ];
        ext.extend(sni);
        let mut body = vec![3, 3];
        body.extend([0u8; 32]);
        body.push(0); // session id
        body.extend([0, 2, 0xc0, 0x2f]); // cipher suites
        body.extend([1, 0]); // compression
        body.extend([(ext.len() >> 8) as u8, ext.len() as u8]);
        body.extend(ext);
        let mut hs = vec![1, 0, (body.len() >> 8) as u8, body.len() as u8];
        hs.extend(body);
        let mut rec = vec![0x16, 3, 1, (hs.len() >> 8) as u8, hs.len() as u8];
        rec.extend(hs);
        let mut b = vec![0u8; 20];
        b.extend(rec);
        b.extend([0u8; 40]);
        let f = scan(&b);
        let t = f.iter().find(|x| x.kind == Kind::Tls).unwrap();
        assert_eq!(t.summary, "ClientHello  SNI tls.example.com");
    }

    #[test]
    fn streaming_matches_one_shot() {
        let mut b = sample();
        // make it long enough that the rolling window trims
        let tail = sample();
        for _ in 0..3 {
            b.extend(vec![0x99u8; 200_000]);
            b.extend(&tail);
        }
        let want = scan(&b);
        assert!(want.len() > 10);
        for chunk in [997usize, 70_000] {
            let mut s = FindingsScanner::new();
            let mut got = Vec::new();
            for ch in b.chunks(chunk) {
                got.extend(s.push(ch));
            }
            got.extend(s.finish());
            got.sort_by_key(|f| (f.offset, f.kind));
            assert_eq!(got, want, "chunk {chunk}");
        }
    }

    #[test]
    fn dns_label_newline_quirk() {
        // Python's `$` accepts one trailing newline in a label
        assert!(hostchars(b"abc\n"));
        assert!(!hostchars(b"\n"));
        assert!(!hostchars(b"a b"));
    }

    #[test]
    fn random_noise_has_no_findings() {
        let mut x = 0x1234_5678u32;
        let b: Vec<u8> = (0..1_000_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x >> 8) as u8
            })
            .collect();
        assert!(scan(&b).is_empty());
    }
}
