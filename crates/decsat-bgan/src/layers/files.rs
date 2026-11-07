// made by CasualArclamp
//! Files reconstructed from HTTP response bodies found in a decoded payload.
//!
//! Port of the "HTTP bodies, i.e. documents" half of `bgan/findings.py` from
//! DecBGAN (Python, same author, GPL-3.0-or-later).
//!
//! A body is delimited by `Content-Length` or by chunk framing (a response with
//! neither cannot be bounded and is skipped), decompressed where the headers
//! say gzip/deflate, and labelled [`Check::Intact`] / [`Check::Truncated`] /
//! [`Check::Unverified`] by [`Document::check`]. There is no Bearer Connection
//! reassembly, so the bytes after a header may belong to another flow; only
//! self-consistency can be checked, and only for types carrying their own
//! structure.
//!
//! [`FileScanner`] is the incremental version of [`documents`]. A body is
//! reported as soon as its `Content-Length` bytes (or its chunk framing) have
//! arrived; candidates are decided strictly in stream order.
//!
//! Decompression mirrors Python's `zlib.decompressobj` fed 512 bytes at a time
//! (see [`super::inflate`]): when a chunk fails (corrupt data or a bad gzip/zlib
//! trailer) the output of that chunk is discarded and decoding stops, keeping
//! what came before. Output is capped at 64 MiB.

use std::collections::VecDeque;

use super::findings::{children, tlv};
use super::inflate::{Wrap, inflate};
use super::stream::{Rolling, ascii_lossy, find, find_within, py_strip};

pub const MAX_BODY: usize = 1 << 20;

/// Content types with a file extension (`_EXT` in Python).
fn ext_for(ct: &str) -> &'static str {
    match ct {
        "text/html" => ".html",
        "text/plain" => ".txt",
        "text/css" => ".css",
        "application/json" | "text/x-json" => ".json",
        "application/javascript" | "text/javascript" => ".js",
        "application/xml" | "text/xml" => ".xml",
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/gif" => ".gif",
        "image/svg+xml" => ".svg",
        "application/pkix-crl" => ".crl",
        "application/pkix-cert" => ".cer",
        "application/ocsp-response" => ".ors",
        _ => ".bin",
    }
}

/// How far a body can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    Intact,
    Truncated,
    Unverified,
}

impl Check {
    pub fn as_str(self) -> &'static str {
        match self {
            Check::Intact => "intact",
            Check::Truncated => "truncated",
            Check::Unverified => "unverified",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    /// Absolute offset of the status line (or of the markup for fragments).
    pub offset: usize,
    pub status: String,
    pub ctype: String,
    /// Content-Length, or -1 when chunked/unknown.
    pub declared: i64,
    pub data: Vec<u8>,
    pub note: String,
}

fn media_type(ctype: &str) -> String {
    let first = ctype.split(';').next().unwrap_or("");
    py_strip(first).to_lowercase()
}

impl Document {
    /// `doc_00012345.html`: offset and an extension from the content type.
    pub fn name(&self) -> String {
        format!(
            "doc_{:08}{}",
            self.offset,
            ext_for(&media_type(&self.ctype))
        )
    }

    /// "intact", "truncated" or "unverified".
    ///
    /// * text: entirely printable (>98%), or it was cut off partway;
    /// * DER types: the outer TLV consumes exactly the body *and* its children
    ///   tile it (an inserted Bearer Control header displaces bytes without
    ///   changing the outer length);
    /// * everything else: no check exists, so none is claimed.
    pub fn check(&self) -> Check {
        if self.data.is_empty() {
            return Check::Truncated;
        }
        let ct = media_type(&self.ctype);
        let texty = ct.starts_with("text/")
            || [
                "application/json",
                "application/xml",
                "text/x-json",
                "application/javascript",
            ]
            .iter()
            .any(|p| ct.starts_with(p));
        if texty {
            let ok = self
                .data
                .iter()
                .filter(|&&c| (0x09..=0x7e).contains(&c) || c == 0x0a || c == 0x0d)
                .count();
            return if ok as f64 / self.data.len() as f64 > 0.98 {
                Check::Intact
            } else {
                Check::Truncated
            };
        }
        if matches!(
            ct.as_str(),
            "application/pkix-crl"
                | "application/pkix-cert"
                | "application/x-x509-ca-cert"
                | "application/ocsp-response"
        ) {
            let Some(t) = tlv(&self.data, 0) else {
                return Check::Truncated;
            };
            if t.2 != self.data.len() {
                return Check::Truncated;
            }
            return if children(&self.data, t.1, t.2).is_some_and(|k| !k.is_empty()) {
                Check::Intact
            } else {
                Check::Truncated
            };
        }
        Check::Unverified
    }

    pub fn intact(&self) -> bool {
        self.check() == Check::Intact
    }
}

// --- chunked transfer coding -------------------------------------------------

/// Python `int(x, 16)` on bytes: surrounding whitespace, optional sign and
/// `0x` prefix, single underscores between digits.
fn py_int_hex(s: &[u8]) -> Option<i128> {
    let mut s = super::stream::strip_bytes(s);
    let neg = match s.first() {
        Some(b'-') => {
            s = &s[1..];
            true
        }
        Some(b'+') => {
            s = &s[1..];
            false
        }
        _ => false,
    };
    if s.len() >= 2 && s[0] == b'0' && (s[1] == b'x' || s[1] == b'X') {
        s = &s[2..];
        if s.first() == Some(&b'_') {
            s = &s[1..];
        }
    }
    if s.is_empty() || s[0] == b'_' || *s.last().unwrap() == b'_' {
        return None;
    }
    let mut v: i128 = 0;
    let mut prev_us = false;
    for &c in s {
        if c == b'_' {
            if prev_us {
                return None;
            }
            prev_us = true;
            continue;
        }
        prev_us = false;
        let d = (c as char).to_digit(16)? as i128;
        v = v.saturating_mul(16).saturating_add(d);
    }
    Some(if neg { -v } else { v })
}

enum Dechunk {
    Done(Vec<u8>),
    /// The decision depends on bytes that have not arrived.
    Need,
}

/// `Transfer-Encoding: chunked`; stops at the first malformed size line.
fn dechunk(b: &[u8], mut i: usize, limit: usize, fin: bool) -> Dechunk {
    let mut out = Vec::new();
    let end = b.len().min(i + limit);
    // True when `end` is the end of the received data rather than the limit:
    // more bytes could change the outcome.
    let starved = !fin && b.len() < i + limit;
    let mut guard = 0usize;
    while i < end {
        guard += 1;
        if guard > 1 << 22 {
            break;
        }
        let win_end = end.min(i + 20);
        let Some(nl) = find_within(b, b"\r\n", i, win_end) else {
            if starved && win_end == end {
                return Dechunk::Need;
            }
            break;
        };
        let sz = b[i..nl].split(|&c| c == b';').next().unwrap_or(&[]);
        let Some(n) = py_int_hex(sz) else { break };
        if n == 0 {
            break;
        }
        if n < 0 {
            // Python slices backwards to an empty slice here; keep going.
            i = ((nl + 2) as i128 + n + 2).max(0) as usize;
            continue;
        }
        if (nl + 2) as i128 + n > end as i128 {
            if starved {
                return Dechunk::Need;
            }
            out.extend_from_slice(&b[(nl + 2).min(end)..end]); // truncated final chunk
            break;
        }
        let n = n as usize;
        out.extend_from_slice(&b[nl + 2..nl + 2 + n]);
        i = nl + 2 + n + 2;
    }
    if i >= end && starved {
        return Dechunk::Need;
    }
    Dechunk::Done(out)
}

// --- HTTP header lookup -----------------------------------------------------

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c | 0x1c..=0x1f)
}

/// `re.search(rf"^{name}:\s*([^\r\n]*)", hdrs, re.I | re.M)` then `.strip()`.
/// Quirks kept: `^` matches only after `\n`, and `\s*` may swallow a line end
/// (so an empty value takes the next line).
fn header_value(h: &[u8], name: &str) -> String {
    let nm = name.as_bytes();
    let mut pos = 0;
    loop {
        if h.len() > pos + nm.len()
            && h[pos..pos + nm.len()].eq_ignore_ascii_case(nm)
            && h[pos + nm.len()] == b':'
        {
            let mut j = pos + nm.len() + 1;
            while j < h.len() && is_space(h[j]) {
                j += 1;
            }
            let e = h[j..]
                .iter()
                .position(|&c| c == b'\r' || c == b'\n')
                .map_or(h.len(), |p| j + p);
            return py_strip(&ascii_lossy(&h[j..e])).to_string();
        }
        match h[pos..].iter().position(|&c| c == b'\n') {
            Some(p) => pos += p + 1,
            None => return String::new(),
        }
    }
}

// --- body extraction --------------------------------------------------------

enum Step<T> {
    Need,
    Done(T),
}

/// Decide the `HTTP/1.x` candidate at `m` (relative). On `Done`, the pair is the
/// document (if any) and the relative cursor to continue from.
fn body_at(b: &[u8], m: usize, base: usize, fin: bool) -> Step<(Option<Document>, usize)> {
    let n = b.len();
    // HTTP/1.[01] SP 3DIGIT [^\r\n]{0,80} CRLF
    if !fin && n < m + 95 {
        return Step::Need;
    }
    let fail = Step::Done((None, m + 1));
    if b.get(m + 8) != Some(&b' ')
        || b.get(m + 9..m + 12)
            .is_none_or(|d| !d.iter().all(u8::is_ascii_digit))
    {
        return fail;
    }
    let Some(e) = b[m + 12..]
        .iter()
        .position(|&c| c == b'\r' || c == b'\n')
        .map(|p| m + 12 + p)
    else {
        return fail;
    };
    if e - (m + 12) > 80 || b[e] != b'\r' || b.get(e + 1) != Some(&b'\n') {
        return fail;
    }
    let hs = e + 2; // end of the regex match
    let skip = |_| Step::Done((None, hs));
    let status = py_strip(&format!(
        "{}{}",
        ascii_lossy(&b[m + 9..m + 12]),
        ascii_lossy(&b[m + 12..e])
    ))
    .to_string();

    let Some(he) = find_within(b, b"\r\n\r\n", hs, hs + 4096) else {
        if !fin && n < hs + 4096 {
            return Step::Need;
        }
        return skip(());
    };
    let hdrs = &b[hs..he];
    let ctype = {
        let v = header_value(hdrs, "Content-Type");
        if v.is_empty() {
            "application/octet-stream".to_string()
        } else {
            v
        }
    };
    let enc = header_value(hdrs, "Content-Encoding").to_lowercase();
    let te = header_value(hdrs, "Transfer-Encoding").to_lowercase();
    let cl = header_value(hdrs, "Content-Length");

    let bs = he + 4;
    let declared: i64;
    let mut body: Vec<u8>;
    let mut note: String;
    if te.contains("chunked") {
        match dechunk(b, bs, MAX_BODY, fin) {
            Dechunk::Need => return Step::Need,
            Dechunk::Done(v) => body = v,
        }
        declared = -1;
        note = "chunked".to_string();
    } else if !cl.is_empty() && cl.bytes().all(|c| c.is_ascii_digit()) {
        let d: u128 = cl.parse().unwrap_or(u128::MAX);
        if d == 0 || d > MAX_BODY as u128 {
            return skip(());
        }
        let d = d as usize;
        if !fin && n < bs + d {
            return Step::Need;
        }
        declared = d as i64;
        body = b[bs.min(n)..(bs + d).min(n)].to_vec();
        note = if body.len() < d {
            format!("truncated, {} of {}", body.len(), d)
        } else {
            String::new()
        };
    } else {
        return skip(()); // no way to bound the body
    }
    if body.is_empty() {
        return skip(());
    }
    let sep = |note: &str| {
        if note.is_empty() {
            String::new()
        } else {
            format!("{note}; ")
        }
    };
    let raw = body.len();
    if enc == "gzip" || enc == "x-gzip" {
        body = inflate(&body, Wrap::Gzip);
        note = sep(&note)
            + &if body.is_empty() {
                "gzip did not inflate".to_string()
            } else {
                format!("gunzipped {raw}->{}", body.len())
            };
    } else if enc == "deflate" {
        // zlib-wrapped first, then bare deflate, like the Python `or`
        let comp = std::mem::take(&mut body);
        body = inflate(&comp, Wrap::Zlib);
        if body.is_empty() {
            body = inflate(&comp, Wrap::Raw);
        }
        note = sep(&note)
            + &if body.is_empty() {
                "deflate did not inflate".to_string()
            } else {
                format!("inflated {raw}->{}", body.len())
            };
    } else if enc == "br" {
        note = sep(&note) + "brotli, not decoded";
    }
    if body.is_empty() {
        return skip(());
    }
    Step::Done((
        Some(Document {
            offset: base + m,
            status,
            ctype,
            declared,
            data: body,
            note,
        }),
        hs,
    ))
}

/// Case-insensitive `\s` set used by the markup regex.
fn re_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c | 0x1c..=0x1f)
}

/// `_MARKUP` regex at `p`: returns the end of the match, `Ok(None)` for no
/// match, `Err` when the verdict needs bytes not yet received.
fn markup_match(b: &[u8], p: usize, fin: bool) -> Result<Option<usize>, ()> {
    let n = b.len();
    let rest = &b[p..];
    let starts_ci =
        |lit: &[u8]| rest.len() >= lit.len() && rest[..lit.len()].eq_ignore_ascii_case(lit);
    let more = |len: usize| {
        if !fin && rest.len() < len {
            Err(())
        } else {
            Ok(None)
        }
    };
    // <!DOCTYPE\s+html
    if starts_ci(b"<!doctype") {
        let mut j = p + 9;
        while j < n && re_space(b[j]) {
            j += 1;
        }
        if j == n && !fin {
            return Err(()); // the whitespace run may continue
        }
        if j == p + 9 {
            return Ok(None);
        }
        let avail = (n - j).min(4);
        if !b[j..j + avail].eq_ignore_ascii_case(&b"html"[..avail]) {
            return Ok(None);
        }
        if avail < 4 {
            return if fin { Ok(None) } else { Err(()) };
        }
        return Ok(Some(j + 4));
    }
    // The other four: literal then one character from a small class.
    for (lit, class) in [
        (&b"<html"[..], &b" >"[..]),
        (b"<?xml", b" ?"),
        (b"<svg", b" >"),
        (b"<rss", b" >"),
    ] {
        if starts_ci(lit) {
            if rest.len() == lit.len() {
                return more(lit.len() + 1);
            }
            let c = rest[lit.len()];
            let hit = if class[0] == b' ' {
                re_space(c) || c == class[1]
            } else {
                c == class[1]
            };
            return Ok(if hit { Some(p + lit.len() + 1) } else { None });
        }
    }
    // partial literal at the end of the data
    if !fin {
        for lit in [&b"<!doctype"[..], b"<html", b"<?xml", b"<svg", b"<rss"] {
            if rest.len() < lit.len() && lit[..rest.len()].eq_ignore_ascii_case(rest) {
                return Err(());
            }
        }
    }
    Ok(None)
}

/// A markup fragment at `p`, or `Need`.
fn markup_at(b: &[u8], p: usize, base: usize, fin: bool) -> Step<(Option<Document>, usize)> {
    const SPAN: usize = 4096;
    let end_of_match = match markup_match(b, p, fin) {
        Err(()) => return Step::Need,
        Ok(None) => return Step::Done((None, p + 1)),
        Ok(Some(e)) => e,
    };
    let n = b.len();
    let end = n.min(p + SPAN);
    let mut j = p;
    while j < end && ((0x09..=0x7e).contains(&b[j]) || b[j] == 0x0a || b[j] == 0x0d) {
        j += 1;
    }
    if j == end && !fin && n < p + SPAN {
        return Step::Need;
    }
    if j - p < 64 {
        return Step::Done((None, end_of_match));
    }
    Step::Done((
        Some(Document {
            offset: base + p,
            status: "no header".into(),
            ctype: "text/html".into(),
            declared: -1,
            data: b[p..j].to_vec(),
            note: "fragment, no Content-Length".into(),
        }),
        end_of_match,
    ))
}

// --- scanner ----------------------------------------------------------------

/// Incremental [`documents`]. `push` bytes, get newly finished files.
#[derive(Debug, Default)]
pub struct FileScanner {
    win: Rolling,
    fin: bool,
    doc_cur: usize,
    mk_cur: usize,
    spans: VecDeque<(usize, usize)>,
    frags: VecDeque<Document>,
}

impl FileScanner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<Document> {
        if self.fin {
            return Vec::new();
        }
        self.win.push(bytes);
        self.run()
    }

    pub fn finish(&mut self) -> Vec<Document> {
        self.fin = true;
        self.run()
    }

    fn run(&mut self) -> Vec<Document> {
        let fin = self.fin;
        let base = self.win.base();
        let buf = self.win.data();
        let n = buf.len();
        let mut out: Vec<Document> = Vec::new();

        // HTTP bodies
        let mut i = self.doc_cur - base;
        loop {
            let Some(p) = find(buf, b"HTTP/1.", i) else {
                i = if fin { n } else { i.max(n.saturating_sub(6)) };
                break;
            };
            if !matches!(buf.get(p + 7), Some(b'0' | b'1')) {
                if p + 8 > n && !fin {
                    i = p;
                    break;
                }
                i = p + 1;
                continue;
            }
            match body_at(buf, p, base, fin) {
                Step::Need => {
                    i = p;
                    break;
                }
                Step::Done((d, next)) => {
                    if let Some(d) = d {
                        self.spans
                            .push_back((d.offset, d.offset + d.data.len() + 4096));
                        out.push(d);
                    }
                    i = next;
                }
            }
        }
        self.doc_cur = base + i;

        // markup fragments
        let mut i = self.mk_cur - base;
        loop {
            let Some(p) = buf[i..].iter().position(|&c| c == b'<').map(|d| i + d) else {
                i = n;
                break;
            };
            match markup_at(buf, p, base, fin) {
                Step::Need => {
                    i = p;
                    break;
                }
                Step::Done((d, next)) => {
                    if let Some(d) = d {
                        self.frags.push_back(d);
                    }
                    i = next;
                }
            }
        }
        self.mk_cur = base + i;

        // A fragment inside the span of a body (plus slack) is that body's
        // own markup. Judge it once every body starting at or before it is known.
        let mut released = Vec::new();
        while let Some(f) = self.frags.front() {
            if f.offset >= self.doc_cur {
                break;
            }
            let f = self.frags.pop_front().unwrap();
            if !self
                .spans
                .iter()
                .any(|&(a, b)| a <= f.offset && f.offset < b)
            {
                released.push(f);
            }
        }
        let low = self
            .frags
            .front()
            .map_or(self.mk_cur, |f| f.offset.min(self.mk_cur));
        while self.spans.front().is_some_and(|&(_, e)| e <= low) {
            self.spans.pop_front();
        }
        // spans are pushed in offset order but `e` is not monotone; drop only
        // from the front, which is safe (it merely keeps a few extra spans).

        out.extend(released);
        out.sort_by_key(|d| d.offset);
        self.win.trim_before(self.doc_cur.min(self.mk_cur));
        out
    }
}

/// Everything reconstructable as a file: bodies, then markup fragments not
/// already inside a body, sorted by offset.
pub fn documents(blob: &[u8]) -> Vec<Document> {
    let mut s = FileScanner::new();
    let mut v = s.push(blob);
    v.extend(s.finish());
    v.sort_by_key(|d| d.offset);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder, write::ZlibEncoder};
    use std::io::Write;

    fn gz(data: &[u8]) -> Vec<u8> {
        let mut e = GzEncoder::new(Vec::new(), Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn resp(headers: &str, body: &[u8]) -> Vec<u8> {
        let mut v = format!("HTTP/1.1 200 OK\r\n{headers}\r\n\r\n").into_bytes();
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn content_length_text_and_name() {
        let body = b"<html><body>hello hello hello hello hello hello hello hello hello hello</body></html>";
        let mut blob = vec![0u8; 30];
        blob.extend(resp(
            &format!(
                "Content-Type: text/html; charset=utf-8\r\nContent-Length: {}",
                body.len()
            ),
            body,
        ));
        blob.extend([0u8; 50]);
        let d = documents(&blob);
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].offset, 30);
        assert_eq!(d[0].data, body);
        assert_eq!(d[0].status, "200 OK");
        assert_eq!(d[0].check(), Check::Intact);
        assert_eq!(d[0].name(), "doc_00000030.html");
    }

    #[test]
    fn gzip_and_truncation() {
        let text: Vec<u8> = (0..4000).map(|i| b"abcdefghij"[i % 10]).collect();
        let z = gz(&text);
        let mut blob = resp(
            &format!(
                "Content-Type: text/plain\r\nContent-Encoding: gzip\r\nContent-Length: {}",
                z.len()
            ),
            &z,
        );
        blob.extend([0u8; 20]);
        let d = documents(&blob);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].data, text);
        assert_eq!(d[0].note, format!("gunzipped {}->{}", z.len(), text.len()));

        // body cut short: whole 512-byte input chunks still inflate
        let mut cut = resp(
            &format!(
                "Content-Type: text/plain\r\nContent-Encoding: gzip\r\nContent-Length: {}",
                z.len() + 100
            ),
            &z,
        );
        cut.truncate(cut.len()); // everything present is short of the declared length
        let d = documents(&cut);
        assert_eq!(d.len(), 1);
        assert!(
            d[0].note
                .starts_with(&format!("truncated, {} of {}; ", z.len(), z.len() + 100))
        );
    }

    #[test]
    fn deflate_zlib_and_raw() {
        let text = b"hello deflate world, hello deflate world, hello deflate world".to_vec();
        let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
        e.write_all(&text).unwrap();
        let z = e.finish().unwrap();
        let blob = resp(
            &format!(
                "Content-Type: text/plain\r\nContent-Encoding: deflate\r\nContent-Length: {}",
                z.len()
            ),
            &z,
        );
        assert_eq!(documents(&blob)[0].data, text);
        // raw deflate = zlib without its 2-byte header and 4-byte trailer
        let raw = &z[2..z.len() - 4];
        let blob = resp(
            &format!(
                "Content-Type: text/plain\r\nContent-Encoding: deflate\r\nContent-Length: {}",
                raw.len()
            ),
            raw,
        );
        assert_eq!(documents(&blob)[0].data, text);
    }

    #[test]
    fn chunked_body() {
        let body = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        let blob = resp(
            "Content-Type: text/plain\r\nTransfer-Encoding: chunked",
            body,
        );
        let d = documents(&blob);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].data, b"Wikipedia");
        assert_eq!(d[0].note, "chunked");
        assert_eq!(d[0].declared, -1);
    }

    #[test]
    fn unbounded_body_skipped_and_der_check() {
        let blob = resp("Content-Type: text/plain", b"no length here");
        assert!(documents(&blob).is_empty());

        let der = [0x30, 0x03, 0x02, 0x01, 0x05];
        let d = Document {
            offset: 0,
            status: "200".into(),
            ctype: "application/pkix-crl".into(),
            declared: 5,
            data: der.to_vec(),
            note: String::new(),
        };
        assert_eq!(d.check(), Check::Intact);
        let bad = Document {
            data: vec![0x30, 0x03, 0x02, 0x01],
            ..d.clone()
        };
        assert_eq!(bad.check(), Check::Truncated);
        let bin = Document {
            ctype: "image/png".into(),
            ..d
        };
        assert_eq!(bin.check(), Check::Unverified);
    }

    #[test]
    fn markup_fragment_without_header() {
        let mut blob = vec![0u8; 10];
        blob.extend(b"<!DOCTYPE html><html><head><title>t</title></head><body>some text that goes on a while</body>");
        blob.push(0xff);
        let d = documents(&blob);
        // the DOCTYPE and the <html> inside it overlap, as in finditer
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].status, "no header");
        assert_eq!(d[0].offset, 10);
        assert_eq!(d[1].offset, 25);
    }

    #[test]
    fn streaming_matches_one_shot() {
        let text: Vec<u8> = (0..3000).map(|i| b"hello world "[i % 12]).collect();
        let z = gz(&text);
        let one = resp(
            &format!(
                "Content-Type: text/plain\r\nContent-Encoding: gzip\r\nContent-Length: {}",
                z.len()
            ),
            &z,
        );
        let two = resp(
            "Content-Type: text/plain\r\nTransfer-Encoding: chunked",
            b"4\r\nWiki\r\n0\r\n\r\n",
        );
        let mut blob = Vec::new();
        for k in 0..6 {
            blob.extend(vec![0x11u8; 70_000 + k]);
            blob.extend(&one);
            blob.extend(vec![0x22u8; 100]);
            blob.extend(&two);
            blob.extend(b"<html><body>fragment fragment fragment fragment fragment fragment fragment</body>");
        }
        let want = documents(&blob);
        // 12 bodies; the markup after each sits inside the chunked body's span and is dropped
        assert_eq!(want.len(), 12);
        for chunk in [333usize, 50_000] {
            let mut s = FileScanner::new();
            let mut got = Vec::new();
            for ch in blob.chunks(chunk) {
                got.extend(s.push(ch));
            }
            got.extend(s.finish());
            got.sort_by_key(|d| d.offset);
            assert_eq!(got, want, "chunk {chunk}");
        }
    }

    #[test]
    fn hex_parse_quirks() {
        assert_eq!(py_int_hex(b" 1a "), Some(26));
        assert_eq!(py_int_hex(b"0x10"), Some(16));
        assert_eq!(py_int_hex(b"1_0"), Some(16));
        assert_eq!(py_int_hex(b"1__0"), None);
        assert_eq!(py_int_hex(b""), None);
        assert_eq!(py_int_hex(b"zz"), None);
        assert_eq!(py_int_hex(b"-5"), Some(-5));
    }

    #[test]
    fn header_value_swallows_blank_value() {
        let h = b"Content-Length:\r\nX: 7\r\nContent-Type: a/b";
        assert_eq!(header_value(h, "Content-Length"), "X: 7");
        assert_eq!(header_value(h, "content-type"), "a/b");
        assert_eq!(header_value(h, "Server"), "");
    }
}
