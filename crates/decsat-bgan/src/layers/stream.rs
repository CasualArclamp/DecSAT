// made by CasualArclamp
//! Small shared helpers for the byte-stream layers: a rolling window for
//! incremental scanning, byte-string searching and the handful of Python
//! string behaviours the ported code relies on.
//!
//! The scanners in this directory are ports of the DecBGAN Python modules
//! (<https://github.com/CasualArclamp/DecBGAN>, GPL-3.0-or-later, same author).
//! Python scans a finished `bytes` object; here every scanner can also be fed a
//! growing stream. The trick used throughout is the same one:
//!
//! * a scanner keeps a *cursor* (an absolute stream offset) below which every
//!   candidate position has been decided;
//! * a candidate is only decided once enough bytes after it have arrived for
//!   the verdict to be final (its "look-ahead"), or once `finish()` says no
//!   more bytes are coming;
//! * the decision is made on exactly the bytes a one-shot scan would see, so
//!   a stream scan and a one-shot scan of the same bytes agree.

/// A byte buffer that remembers how many bytes were dropped from its front, so
/// callers can keep using absolute stream offsets while the memory stays
/// bounded.
#[derive(Debug, Default, Clone)]
pub struct Rolling {
    buf: Vec<u8>,
    base: usize,
}

impl Rolling {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append newly arrived bytes.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The retained bytes. `data()[0]` is at absolute offset [`Self::base`].
    pub fn data(&self) -> &[u8] {
        &self.buf
    }

    /// Absolute offset of `data()[0]`.
    pub fn base(&self) -> usize {
        self.base
    }

    /// Absolute offset one past the last byte received so far.
    pub fn end(&self) -> usize {
        self.base + self.buf.len()
    }

    /// Forget everything before absolute offset `abs`. The memmove is only
    /// done once a worthwhile amount can be dropped, which keeps the cost per
    /// byte constant (amortised).
    pub fn trim_before(&mut self, abs: usize) {
        if abs <= self.base {
            return;
        }
        let drop = (abs - self.base).min(self.buf.len());
        if drop >= 65_536 && drop * 4 >= self.buf.len() {
            self.buf.drain(..drop);
            self.base += drop;
        }
    }
}

/// First occurrence of `needle` in `hay[from..]`.
pub fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    find_within(hay, needle, from, hay.len())
}

/// Python's `hay.find(needle, from, to)`: the match must lie entirely inside
/// `[from, to)`.
pub fn find_within(hay: &[u8], needle: &[u8], from: usize, to: usize) -> Option<usize> {
    let to = to.min(hay.len());
    let m = needle.len();
    if m == 0 || from >= to || to - from < m {
        return None;
    }
    let first = needle[0];
    let last_start = to - m;
    let mut i = from;
    // `position` over a slice of u8 is vectorised by the compiler, which is
    // fast enough for our needles (all begin with a fairly rare byte).
    while i <= last_start {
        let at = i + hay[i..=last_start].iter().position(|&b| b == first)?;
        if &hay[at..at + m] == needle {
            return Some(at);
        }
        i = at + 1;
    }
    None
}

/// Python's `hay.rfind(byte, from, to)`.
pub fn rfind_byte(hay: &[u8], byte: u8, from: usize, to: usize) -> Option<usize> {
    let to = to.min(hay.len());
    if from >= to {
        return None;
    }
    hay[from..to]
        .iter()
        .rposition(|&b| b == byte)
        .map(|p| from + p)
}

/// `bytes.decode("ascii", "replace")`: every byte >= 0x80 becomes U+FFFD.
pub fn ascii_lossy(b: &[u8]) -> String {
    b.iter()
        .map(|&c| if c < 0x80 { c as char } else { '\u{FFFD}' })
        .collect()
}

/// `bytes.strip()`: Python strips ASCII whitespace including vertical tab
/// (Rust's `trim_ascii` does not strip 0x0B).
pub fn strip_bytes(b: &[u8]) -> &[u8] {
    let ws = |c: &u8| matches!(*c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    let s = b.iter().position(|c| !ws(c)).unwrap_or(b.len());
    let e = b.iter().rposition(|c| !ws(c)).map_or(s, |p| p + 1);
    &b[s..e]
}

/// `str.isspace()` for one char (Python also counts 0x1C..=0x1F).
pub fn py_isspace(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

/// `str.strip()`.
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(py_isspace)
}

/// `str.split()` with no argument: split on runs of whitespace, no empties.
pub fn py_split_ws(s: &str) -> impl Iterator<Item = &str> {
    s.split(py_isspace).filter(|p| !p.is_empty())
}

/// Python's `s[:n]` counts characters, not bytes.
pub fn py_prefix(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Big-endian u16 at `i`. Callers guarantee `i + 2 <= b.len()`.
#[inline]
pub fn be16(b: &[u8], i: usize) -> usize {
    ((b[i] as usize) << 8) | b[i + 1] as usize
}

/// Big-endian u24 at `i`.
#[inline]
pub fn be24(b: &[u8], i: usize) -> usize {
    ((b[i] as usize) << 16) | ((b[i + 1] as usize) << 8) | b[i + 2] as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_semantics() {
        let h = b"abcabcabc";
        assert_eq!(find(h, b"abc", 0), Some(0));
        assert_eq!(find(h, b"abc", 1), Some(3));
        assert_eq!(find_within(h, b"abc", 1, 5), None); // would end at 6 > 5
        assert_eq!(find_within(h, b"abc", 1, 6), Some(3));
        assert_eq!(rfind_byte(h, b'a', 0, 8), Some(6));
        assert_eq!(rfind_byte(h, b'a', 0, 6), Some(3));
        assert_eq!(rfind_byte(h, b'z', 0, 9), None);
    }

    #[test]
    fn strip_and_split() {
        assert_eq!(strip_bytes(b"\x0b x \x0c"), b"x");
        assert_eq!(strip_bytes(b"   "), b"");
        assert_eq!(py_strip("\u{1c}a b\u{85}"), "a b");
        assert_eq!(
            py_split_ws(" a  b\nc ").collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(py_prefix("héllo", 2), "hé");
    }

    #[test]
    fn rolling_trim_keeps_absolute_offsets() {
        let mut r = Rolling::new();
        r.push(&vec![7u8; 200_000]);
        r.trim_before(150_000);
        assert_eq!(r.base(), 150_000);
        assert_eq!(r.end(), 200_000);
        r.push(&[1, 2, 3]);
        assert_eq!(r.end(), 200_003);
        assert_eq!(&r.data()[r.data().len() - 3..], &[1, 2, 3]);
    }
}
