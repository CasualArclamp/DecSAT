// made by CasualArclamp
//! Live text search in a demodulated bit stream: the `strings` of a carrier
//! nothing else here can decode. Text — callsigns, telemetry, NMEA, idle
//! messages, headers — stands out even in an unknown format, but where the
//! bytes start, which end of a byte goes first, whether the bits are
//! differentially coded and how the constellation is turned are all
//! unknown. So every combination is read at once, each keeping its own
//! runs of printable characters, and the one with far more text than the
//! others is shown (with the longest strings from anywhere as candidates).

use std::collections::{HashMap, VecDeque};

/// Characters a run must hold to count as text (random bytes make a run
/// this long ~1 time in 400).
const MIN_RUN: usize = 6;
/// Strings shown must be at least this long: random bytes make runs of
/// 6–9 printable characters every few dozen bytes, which would bury the
/// text (runs of 6 still count towards finding the reading).
const SHOW_LEN: usize = 10;
/// Strings kept per hypothesis, and candidates overall.
const KEEP: usize = 24;
const CANDIDATES: usize = 8;
/// A candidate must be at least this long.
const CANDIDATE_LEN: usize = 12;
/// The best hypothesis must hold this many times the median's text, and
/// this many characters, to be called text.
const WIN_RATIO: f64 = 4.0;
const WIN_CHARS: u64 = 60;
/// Longest run kept (longer text is cut into pieces).
const MAX_RUN: usize = 200;

fn printable(b: u8) -> bool {
    (0x20..=0x7E).contains(&b) || b == b'\t'
}

/// One way of making bytes out of bits: an alignment and a bit order.
#[derive(Clone, Default)]
struct Reader {
    run: Vec<u8>,
    /// Characters in runs of at least `MIN_RUN`.
    chars: u64,
    found: VecDeque<String>,
}

impl Reader {
    fn byte(&mut self, b: u8) -> Option<String> {
        if printable(b) && self.run.len() < MAX_RUN {
            self.run.push(b);
            return None;
        }
        let s = self.end();
        if printable(b) {
            self.run.push(b);
        }
        s
    }

    fn end(&mut self) -> Option<String> {
        if self.run.len() < MIN_RUN {
            self.run.clear();
            return None;
        }
        self.chars += self.run.len() as u64;
        let s = String::from_utf8_lossy(&self.run).into_owned();
        self.run.clear();
        if s.len() >= SHOW_LEN {
            if self.found.len() == KEEP {
                self.found.pop_front();
            }
            self.found.push_back(s.clone());
        }
        Some(s)
    }
}

/// A bit stream (one carrier orientation), plain or differentially decoded:
/// eight alignments × two bit orders of readers.
#[derive(Clone)]
struct Stream {
    differential: bool,
    prev: u8,
    /// The last eight bits, newest in bit 0.
    shift: u8,
    pos: u64,
    /// [alignment][MSB first, LSB first]
    readers: Vec<[Reader; 2]>,
}

impl Stream {
    fn new(differential: bool) -> Self {
        Stream {
            differential,
            prev: 0,
            shift: 0,
            pos: 0,
            readers: vec![Default::default(); 8],
        }
    }
}

/// What the search has found, for display.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TextView {
    /// The reading that carries text, if one stands out.
    pub best: Option<String>,
    /// Its strings, oldest first.
    pub strings: Vec<String>,
    /// The longest strings from any reading, with the reading.
    pub candidates: Vec<(String, String)>,
    /// Bytes read per reading so far, and the number of readings.
    pub bytes: u64,
    pub readings: usize,
}

/// The search over every reading of one or more bit streams.
pub struct TextFinder {
    /// Names of the bit streams (e.g. the carrier orientations).
    sources: Vec<String>,
    /// Per source: plain, differential.
    streams: Vec<[Stream; 2]>,
    candidates: Vec<(usize, String)>,
}

impl TextFinder {
    pub fn new(sources: Vec<String>) -> Self {
        let streams = sources
            .iter()
            .map(|_| [Stream::new(false), Stream::new(true)])
            .collect();
        TextFinder {
            sources,
            streams,
            candidates: Vec::new(),
        }
    }

    /// Reading number → its description.
    fn describe(&self, h: usize) -> String {
        let (src, rest) = (h / 32, h % 32);
        let (diff, rest) = (rest / 16, rest % 16);
        let (align, lsb) = (rest / 2, rest % 2);
        format!(
            "{}, {}, {} first, bytes from bit {align}",
            self.sources[src],
            if diff == 1 { "differential" } else { "plain" },
            if lsb == 1 { "LSB" } else { "MSB" }
        )
    }

    /// Bits (0/1) of source `src`.
    pub fn push(&mut self, src: usize, bits: &[u8]) {
        for (d, st) in self.streams[src].iter_mut().enumerate() {
            for &b in bits {
                let b = if st.differential {
                    let x = b ^ st.prev;
                    st.prev = b;
                    x
                } else {
                    b
                };
                st.shift = (st.shift << 1) | b;
                st.pos += 1;
                // The byte ending here, for the alignment it completes.
                let align = (st.pos % 8) as usize;
                let msb = st.shift;
                let lsb = msb.reverse_bits();
                for (order, v) in [msb, lsb].into_iter().enumerate() {
                    if let Some(s) = st.readers[align][order].byte(v)
                        && s.len() >= CANDIDATE_LEN
                    {
                        let h = src * 32 + d * 16 + align * 2 + order;
                        self.candidates.push((h, s));
                        self.candidates
                            .sort_by_key(|c| std::cmp::Reverse(c.1.len()));
                        self.candidates.truncate(CANDIDATES);
                    }
                }
            }
        }
    }

    pub fn view(&self) -> TextView {
        let mut chars: Vec<(usize, u64)> = Vec::new();
        for (src, pair) in self.streams.iter().enumerate() {
            for (d, st) in pair.iter().enumerate() {
                for (align, rs) in st.readers.iter().enumerate() {
                    for (order, r) in rs.iter().enumerate() {
                        chars.push((src * 32 + d * 16 + align * 2 + order, r.chars));
                    }
                }
            }
        }
        let mut sorted: Vec<u64> = chars.iter().map(|c| c.1).collect();
        sorted.sort_unstable();
        let median = sorted[sorted.len() / 2].max(1) as f64;
        let &(h, top) = chars.iter().max_by_key(|c| c.1).expect("readings");
        let best = (top >= WIN_CHARS && top as f64 >= WIN_RATIO * median).then_some(h);
        let bytes = self.streams.first().map_or(0, |s| s[0].pos / 8);
        TextView {
            best: best.map(|h| self.describe(h)),
            strings: best
                .map(|h| {
                    let st = &self.streams[h / 32][(h % 32) / 16];
                    st.readers[(h % 16) / 2][h % 2]
                        .found
                        .iter()
                        .cloned()
                        .collect()
                })
                .unwrap_or_default(),
            candidates: self
                .candidates
                .iter()
                .map(|(h, s)| (self.describe(*h), s.clone()))
                .collect(),
            bytes,
            readings: self.sources.len() * 32,
        }
    }
}

/// Text in data already decoded to bytes (IP packets, transport stream
/// payloads): where bytes start is known, so only runs of printable
/// characters are collected. Compressed, encrypted or video payload makes
/// short runs by chance all the time, so strings are ranked by how often
/// they recur — a service name, a URL, a beacon repeats; chance does not —
/// and long ones are kept as they come.
pub struct ByteText {
    /// The string being read in each stream (see [`ByteText::push_keyed`]).
    runs: HashMap<u32, Vec<u8>>,
    counts: HashMap<String, u64>,
    /// The strings seen more than once, with their counts: few, so ranking
    /// them is cheap enough to do on every look.
    repeated: HashMap<String, u64>,
    /// Strings in the order first seen, for forgetting the oldest.
    order: VecDeque<String>,
    recent: VecDeque<String>,
    bytes: u64,
}

/// Shortest string counted.
const BYTE_MIN: usize = 6;
/// Strings this long are listed as they come (chance makes one ~1 time in
/// 10⁷ bytes).
const RECENT_LEN: usize = 16;
/// Distinct strings remembered; past this, ones seen only once are dropped,
/// oldest first.
const MAX_DISTINCT: usize = 65_536;
/// Strings shown in each list.
const SHOW: usize = 16;

/// What [`ByteText`] has found, for display.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ByteTextView {
    /// Strings seen more than once, most frequent first, with their counts.
    pub repeated: Vec<(String, u64)>,
    /// The latest long strings, oldest first.
    pub recent: Vec<String>,
    /// Bytes looked at.
    pub bytes: u64,
}

impl Default for ByteText {
    fn default() -> Self {
        Self::new()
    }
}

/// The key of the unkeyed stream.
const STREAM: u32 = u32::MAX;

impl ByteText {
    pub fn new() -> Self {
        ByteText {
            runs: HashMap::new(),
            counts: HashMap::new(),
            repeated: HashMap::new(),
            order: VecDeque::new(),
            recent: VecDeque::new(),
            bytes: 0,
        }
    }

    /// Bytes of a continuous stream: a string may carry on into the next
    /// call.
    pub fn push(&mut self, bytes: &[u8]) {
        self.push_keyed(STREAM, bytes);
    }

    /// Bytes of one of several interleaved streams (a transport stream's
    /// PIDs): a string carries on into that stream's next bytes, so text
    /// split across packets comes out whole.
    pub fn push_keyed(&mut self, key: u32, bytes: &[u8]) {
        self.bytes += bytes.len() as u64;
        // Taken out of the map while `end` borrows `self`, then put back.
        let mut run = self.runs.remove(&key).unwrap_or_default();
        for &b in bytes {
            if printable(b) && run.len() < MAX_RUN {
                run.push(b);
            } else {
                self.end(&mut run);
                if printable(b) {
                    run.push(b);
                }
            }
        }
        self.runs.insert(key, run);
    }

    /// One packet's bytes: strings end with it.
    pub fn push_unit(&mut self, bytes: &[u8]) {
        self.push(bytes);
        let mut run = self.runs.remove(&STREAM).unwrap_or_default();
        self.end(&mut run);
    }

    fn end(&mut self, run: &mut Vec<u8>) {
        if run.len() < BYTE_MIN {
            run.clear();
            return;
        }
        let s = String::from_utf8_lossy(run).into_owned();
        run.clear();
        if s.len() >= RECENT_LEN && !self.recent.contains(&s) {
            if self.recent.len() == SHOW {
                self.recent.pop_front();
            }
            self.recent.push_back(s.clone());
        }
        // `entry` finds the key's slot or makes one: one hash lookup.
        let n = self.counts.entry(s.clone()).or_insert(0);
        *n += 1;
        if *n > 1 {
            let n = *n;
            self.repeated.insert(s, n);
        } else {
            self.order.push_back(s);
            while self.counts.len() > MAX_DISTINCT {
                let Some(old) = self.order.pop_front() else {
                    break;
                };
                if self.counts.get(&old).is_some_and(|&c| c > 1) {
                    // Seen again: keep it, behind the newer ones.
                    self.order.push_back(old);
                    if self.order.len() <= SHOW {
                        break;
                    }
                } else {
                    self.counts.remove(&old);
                }
            }
        }
    }

    pub fn view(&self) -> ByteTextView {
        let mut repeated: Vec<(String, u64)> =
            self.repeated.iter().map(|(s, &c)| (s.clone(), c)).collect();
        // Most frequent first; equal counts longest first.
        repeated.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.len().cmp(&a.0.len())));
        repeated.truncate(SHOW);
        ByteTextView {
            repeated,
            recent: self.recent.iter().cloned().collect(),
            bytes: self.bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(seed: u64) -> impl FnMut() -> u64 {
        let mut s = seed | 1;
        move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            s.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    /// Random bytes with a message every so often, as bits LSB first,
    /// starting mid-byte.
    fn stream(msg: &str, seed: u64) -> Vec<u8> {
        let mut r = rng(seed);
        let mut bytes = Vec::new();
        for k in 0..60 {
            for _ in 0..(50 + r() % 100) {
                bytes.push(r() as u8);
            }
            bytes.extend_from_slice(format!("{msg} {k:04}\r\n").as_bytes());
        }
        let mut bits: Vec<u8> = bytes
            .iter()
            .flat_map(|&b| (0..8).map(move |i| (b >> i) & 1))
            .collect();
        bits.drain(..3);
        bits
    }

    #[test]
    fn finds_lsb_first_text_at_any_alignment() {
        let mut f = TextFinder::new(vec!["upright".into(), "inverted".into()]);
        let bits = stream("CQ CQ DE VK2XYZ TELEMETRY OK", 1);
        let inverted: Vec<u8> = bits.iter().map(|b| b ^ 1).collect();
        for (a, b) in bits.chunks(1000).zip(inverted.chunks(1000)) {
            f.push(0, a);
            f.push(1, b);
        }
        let v = f.view();
        let best = v.best.clone().expect("no text found");
        assert!(best.starts_with("upright, plain, LSB first"), "{best}");
        assert!(
            v.strings.iter().any(|s| s.starts_with("CQ CQ DE VK2XYZ")),
            "{v:?}"
        );
        assert!(v.candidates[0].1.contains("VK2XYZ"));
    }

    #[test]
    fn finds_differentially_coded_text() {
        let raw = stream("$GPGGA,123519,4807.038,N,01131.000,E", 2);
        // Differential encoding: sent = data ⊕ previous sent.
        let mut prev = 0u8;
        let sent: Vec<u8> = raw
            .iter()
            .map(|&b| {
                prev ^= b;
                prev
            })
            .collect();
        let mut f = TextFinder::new(vec!["0°".into()]);
        f.push(0, &sent);
        let best = f.view().best.expect("no text found");
        assert!(best.contains("differential"), "{best}");
    }

    #[test]
    fn byte_text_ranks_what_recurs() {
        let mut r = rng(9);
        let mut t = ByteText::new();
        for k in 0..2000 {
            let mut pkt: Vec<u8> = (0..180).map(|_| r() as u8).collect();
            if k % 10 == 0 {
                pkt[20..33].copy_from_slice(b"\x00DecSAT News\x00");
            }
            if k == 500 {
                pkt[60..97].copy_from_slice(b"\x00http://example.org/live/stream.m3u8\x00");
            }
            t.push_unit(&pkt);
        }
        let v = t.view();
        assert_eq!(v.repeated[0], ("DecSAT News".into(), 200), "{v:?}");
        assert!(
            v.recent
                .iter()
                .any(|s| s.contains("http://example.org/live")),
            "{v:?}"
        );
        // Chance strings do not recur.
        assert!(
            v.repeated.iter().all(|(_, c)| *c == 200 || *c <= 3),
            "{v:?}"
        );
    }

    #[test]
    fn random_bits_show_no_text() {
        let mut r = rng(3);
        let bits: Vec<u8> = (0..400_000).map(|_| (r() & 1) as u8).collect();
        let mut f = TextFinder::new(vec!["0°".into(), "180°".into()]);
        f.push(0, &bits);
        f.push(1, &bits.iter().map(|b| b ^ 1).collect::<Vec<_>>());
        let v = f.view();
        assert_eq!(v.best, None, "{v:?}");
    }
}
