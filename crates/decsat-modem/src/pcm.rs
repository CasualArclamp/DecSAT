//! PCM telemetry frames: frame synchronisation and the frames' structure.
//!
//! A beacon's telemetry is a bit stream cut into fixed-length minor frames,
//! each starting with a sync word (IRIG 106 Chapter 4 §4.3; CCSDS
//! 131.0-B's attached sync marker). [`Framer`] finds the frames in any
//! reading of the bits — as sent or inverted (a phase modulator's or a
//! subcarrier's sign is not known), and differentially decoded (NRZ-M/S,
//! bi-phase-M/S: see [`Reading`]) — on a known word ([`KNOWN_WORDS`]), or,
//! failing those, on one found blind: the frame length from the bits'
//! autocorrelation, the word from the run of bits that never change between
//! frames.
//!
//! What the frames show is read by [`find_counter`]: the minor-frame
//! counter (IRIG's subframe ID, SFID) that numbers frames within a major
//! frame, so subcommutated words can be shown each in its own place.

use rustfft::{FftPlanner, num_complex::Complex32};

/// The sync words looked for first, longest first. IRIG 106-22 Chapter 4
/// Appendix 4-C (Table 4C-1, the recommended frame synchronisation
/// patterns for 16, 24 and 32 bits) and CCSDS 131.0-B §9.3.2's marker.
pub const KNOWN_WORDS: &[(u64, u8, &str)] = &[
    (0x1ACF_FC1D, 32, "CCSDS ASM 1ACFFC1D"),
    (0xFE6B_2840, 32, "IRIG 32-bit FE6B2840"),
    (0x00FA_F320, 24, "IRIG 24-bit FAF320"),
    (0xEB90, 16, "IRIG 16-bit EB90"),
];

/// The longest minor frame looked for, bits: a CCSDS codeblock at
/// interleave 8 with its marker (8 × 255 × 8 + 32 = 16 352) fits.
pub const MAX_FRAME_BITS: usize = 16_384;
/// The shortest.
pub const MIN_FRAME_BITS: usize = 64;

/// How the bits are read before the sync word is looked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reading {
    /// As decided (NRZ-L, bi-phase-L), either polarity.
    Direct,
    /// Each bit the XOR of two consecutive decisions: NRZ-M/S, and
    /// bi-phase-M/S taken as bi-phase-L at the other half-bit (IRIG 106
    /// Chapter 4 §4.2.1's codes). Either polarity.
    Differential,
}

impl Reading {
    pub fn name(self) -> &'static str {
        match self {
            Reading::Direct => "NRZ-L / bi-phase-L",
            Reading::Differential => "NRZ-M/S or bi-phase-M/S (differential)",
        }
    }
}

/// A sync word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncWord {
    /// The word, its first bit sent the most significant of `len`.
    pub bits: u64,
    pub len: u8,
    /// "IRIG 24-bit FAF320", or "found blind".
    pub name: String,
}

impl SyncWord {
    fn known(i: usize) -> Self {
        let (bits, len, name) = KNOWN_WORDS[i];
        SyncWord {
            bits,
            len,
            name: name.into(),
        }
    }

    /// Bit errors allowed in a word judged present: one in twelve.
    pub fn tolerance(&self) -> u32 {
        u32::from(self.len) / 12
    }

    /// The word in hex, as long as it is ("FAF320").
    pub fn hex(&self) -> String {
        let digits = usize::from(self.len).div_ceil(4);
        format!("{:0digits$X}", self.bits)
    }

    pub fn is_ccsds(&self) -> bool {
        self.len == 32 && self.bits == 0x1ACF_FC1D
    }

    /// Its bits, first sent first.
    fn bit(&self, i: usize) -> u8 {
        ((self.bits >> (usize::from(self.len) - 1 - i)) & 1) as u8
    }
}

/// The lock: what was found and where the next frame starts.
#[derive(Debug, Clone, PartialEq)]
pub struct Lock {
    pub word: SyncWord,
    pub frame_bits: usize,
    pub reading: Reading,
    /// The word was found blind (no known one fitted).
    pub blind: bool,
    /// Stream bit index (in the reading) where the next frame starts.
    next: u64,
    misses: u32,
}

/// A minor frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    /// The frame's bits from its sync word on, packed MSB first (the last
    /// byte padded with zeros when the length is not whole bytes), the
    /// polarity put right (the sync word reads as itself).
    pub bytes: Vec<u8>,
    pub bits: usize,
    /// Bit errors in its sync word (`None`: no word there — kept by the
    /// flywheel).
    pub sync_errors: Option<u32>,
    /// The bits were inverted to make the word read right.
    pub inverted: bool,
    /// Stream bit index of the frame's first bit.
    pub start: u64,
}

/// What the framer has done.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FramerStats {
    pub frames: u64,
    /// Frames kept with their word missing (the flywheel).
    pub flywheel: u64,
    /// Bit errors counted in the sync words of found frames.
    pub sync_bit_errors: u64,
    /// Locks lost, and words found a bit or two off (a bit slip).
    pub losses: u64,
    pub slips: u64,
    pub searches: u64,
}

impl FramerStats {
    /// Bit error rate in the sync words.
    pub fn sync_ber(&self, word_len: u8) -> f64 {
        let words = self.frames - self.flywheel;
        if words == 0 {
            return 0.0;
        }
        self.sync_bit_errors as f64 / (words as f64 * f64::from(word_len))
    }
}

/// Frames missing their word in a row before the lock is given up.
const FLYWHEEL: u32 = 4;
/// Bits a word may sit early or late and still be taken (a bit slip).
const SLIP: usize = 2;
/// Bits kept for a search: four of the longest frames and some.
const KEEP_BITS: usize = 4 * MAX_FRAME_BITS + 4096;

/// Frame synchroniser over a hard-decided bit stream.
pub struct Framer {
    /// Bits as decided, and differentially decoded; `base` is the stream
    /// index of their first.
    direct: Vec<u8>,
    diff: Vec<u8>,
    base: u64,
    last_bit: u8,
    lock: Option<Lock>,
    /// Bits taken since the last search.
    since_search: usize,
    /// Don't look for blind words (only known ones).
    known_only: bool,
    pub stats: FramerStats,
}

impl Default for Framer {
    fn default() -> Self {
        Self::new()
    }
}

impl Framer {
    pub fn new() -> Self {
        Framer {
            direct: Vec::new(),
            diff: Vec::new(),
            base: 0,
            last_bit: 0,
            lock: None,
            since_search: 0,
            known_only: false,
            stats: FramerStats::default(),
        }
    }

    /// Only the known words, no blind search (for a stream where a
    /// structure found blind would mislead — tests, mostly).
    pub fn known_only(mut self) -> Self {
        self.known_only = true;
        self
    }

    pub fn lock(&self) -> Option<&Lock> {
        self.lock.as_ref()
    }

    /// Bits taken so far.
    pub fn bits_seen(&self) -> u64 {
        self.base + self.direct.len() as u64
    }

    /// Take decided bits (0/1); frames found go to `out`.
    pub fn push(&mut self, bits: &[u8], out: &mut Vec<Frame>) {
        for &b in bits {
            let b = b & 1;
            self.direct.push(b);
            self.diff.push(b ^ self.last_bit);
            self.last_bit = b;
        }
        self.since_search += bits.len();
        if self.lock.is_none() {
            let wanted = (self.direct.len() / 4).clamp(2048, 16_384);
            if self.since_search >= wanted && self.direct.len() >= 3 * MIN_FRAME_BITS {
                self.since_search = 0;
                self.stats.searches += 1;
                self.lock = self.search();
            }
        }
        if self.lock.is_some() {
            self.track(out);
        }
        self.trim();
    }

    fn reading(&self, r: Reading) -> &[u8] {
        match r {
            Reading::Direct => &self.direct,
            Reading::Differential => &self.diff,
        }
    }

    /// Follow the lock: one frame per word found where it should be.
    fn track(&mut self, out: &mut Vec<Frame>) {
        while let Some(lock) = self.lock.as_mut() {
            let bits = match lock.reading {
                Reading::Direct => &self.direct,
                Reading::Differential => &self.diff,
            };
            let at = lock.next.saturating_sub(self.base) as usize;
            if at + lock.frame_bits + SLIP + usize::from(lock.word.len) > bits.len() {
                return;
            }
            let tol = lock.word.tolerance();
            let check = |i: usize| word_errors(bits, i, &lock.word);
            let (mut pos, (mut errs, mut inv)) = (at, check(at));
            if errs > tol {
                // A slip: the word a bit or two off.
                let best = (1..=SLIP)
                    .flat_map(|d| [at + d, at.wrapping_sub(d)])
                    .filter(|&i| i < bits.len())
                    .map(|i| (i, check(i)))
                    .min_by_key(|(_, (e, _))| *e);
                if let Some((i, (e, v))) = best.filter(|(_, (e, _))| *e <= tol) {
                    pos = i;
                    errs = e;
                    inv = v;
                    self.stats.slips += 1;
                }
            }
            let found = errs <= tol;
            if found {
                lock.misses = 0;
                self.stats.sync_bit_errors += u64::from(errs);
            } else {
                lock.misses += 1;
                self.stats.flywheel += 1;
                if lock.misses >= FLYWHEEL {
                    self.stats.losses += 1;
                    self.lock = None;
                    self.since_search = usize::MAX / 2;
                    return;
                }
                // Keep the polarity of the frames before.
                inv = out.last().is_some_and(|f| f.inverted);
            }
            let frame = &bits[pos..pos + lock.frame_bits];
            out.push(Frame {
                bytes: pack(frame, inv),
                bits: lock.frame_bits,
                sync_errors: found.then_some(errs),
                inverted: inv,
                start: self.base + pos as u64,
            });
            self.stats.frames += 1;
            lock.next = self.base + (pos + lock.frame_bits) as u64;
        }
    }

    /// Drop bits no longer needed: all but a search's worth behind the next
    /// frame (locked) or the newest (searching).
    fn trim(&mut self) {
        let keep_from = match &self.lock {
            Some(l) => (l.next.saturating_sub(self.base) as usize).saturating_sub(KEEP_BITS / 2),
            None => self.direct.len().saturating_sub(KEEP_BITS),
        };
        if keep_from > KEEP_BITS / 4 {
            self.direct.drain(..keep_from);
            self.diff.drain(..keep_from);
            self.base += keep_from as u64;
        }
    }

    /// Look for frames in the bits held: the known words, in both readings;
    /// then a word found blind. The lock starts at the earliest frame whose
    /// whole length is held, so the frames from it on come out. The bits as
    /// decided are preferred: a differential reading of a fixed pattern is
    /// fixed too, so it wins only with clearly more frames found.
    fn search(&self) -> Option<Lock> {
        let lock = |reading: Reading, word: SyncWord, blind: bool| {
            let (hits, len, first) = grid(self.reading(reading), &word)?;
            // More hits on the grid, then longer words, win.
            let score = hits * usize::from(word.len);
            Some((
                score,
                Lock {
                    word,
                    frame_bits: len,
                    reading,
                    blind,
                    next: self.base + first as u64,
                    misses: 0,
                },
            ))
        };
        let best_of = |reading: Reading| {
            let known = (0..KNOWN_WORDS.len())
                .filter_map(|i| lock(reading, SyncWord::known(i), false))
                .max_by_key(|(s, _)| *s);
            match known {
                Some(k) => Some(k),
                None if !self.known_only => {
                    let word = blind_word(self.reading(reading))?;
                    lock(reading, word, true)
                }
                None => None,
            }
        };
        let known_first = |a: &Option<(usize, Lock)>| a.as_ref().is_some_and(|(_, l)| !l.blind);
        let direct = best_of(Reading::Direct);
        let diff = best_of(Reading::Differential);
        let pick = match (&direct, &diff) {
            (Some(_), None) => direct,
            (None, Some(_)) => diff,
            (Some((a, _)), Some((b, _))) => {
                // A known word beats one found blind; else the clearly
                // better score.
                if known_first(&direct) != known_first(&diff) {
                    if known_first(&direct) { direct } else { diff }
                } else if *b * 4 > *a * 5 {
                    diff
                } else {
                    direct
                }
            }
            (None, None) => None,
        };
        pick.map(|(_, l)| l)
    }
}

/// Bit errors of `word` at `i` in `bits`, and whether it matched inverted
/// (the fewer errors of the two readings).
fn word_errors(bits: &[u8], i: usize, word: &SyncWord) -> (u32, bool) {
    let n = usize::from(word.len);
    if i + n > bits.len() {
        return (u32::MAX, false);
    }
    let e = (0..n).filter(|&k| bits[i + k] != word.bit(k)).count() as u32;
    let inv = word.len as u32 - e;
    if inv < e { (inv, true) } else { (e, false) }
}

/// Where `word` sits on a regular grid in `bits`: the grid's hits, the
/// frame length, and the first position from which a whole frame is held
/// (the earliest grid hit). `None` unless three hits (two when the word is
/// 24 bits or longer and the bits hold under three frames) share one
/// spacing.
fn grid(bits: &[u8], word: &SyncWord) -> Option<(usize, usize, usize)> {
    let n = usize::from(word.len);
    if bits.len() < n + MIN_FRAME_BITS {
        return None;
    }
    let tol = word.tolerance();
    let hits: Vec<usize> = (0..=bits.len() - n)
        .filter(|&i| word_errors(bits, i, word).0 <= tol)
        .collect();
    if hits.len() < 2 || hits.len() > bits.len() / MIN_FRAME_BITS + 8 {
        return None;
    }
    // Spacings between each hit and the next few: the frame length is the
    // commonest (false hits between real ones break only some of them).
    let mut votes: std::collections::HashMap<usize, usize> = Default::default();
    for (a, &p) in hits.iter().enumerate() {
        for &q in hits.iter().skip(a + 1).take(3) {
            let d = q - p;
            if (MIN_FRAME_BITS..=MAX_FRAME_BITS).contains(&d) {
                *votes.entry(d).or_default() += 1;
            }
        }
    }
    // The smallest spacing among the best voted (multiples of the frame
    // vote too, from frames whose word was hit by errors).
    let top = *votes.values().max()?;
    let len = votes
        .iter()
        .filter(|(_, v)| **v * 2 >= top && **v >= 1)
        .map(|(d, _)| *d)
        .filter(|&d| {
            // Not a multiple of a smaller well-voted spacing.
            !votes
                .iter()
                .any(|(e, v)| *e < d && d % *e == 0 && *v * 2 >= top)
        })
        .min()?;
    // The grid: the hit with most others a whole number of frames away.
    let (anchor, on) = hits
        .iter()
        .map(|&p| {
            let on = hits
                .iter()
                .filter(|&&q| q.abs_diff(p).is_multiple_of(len))
                .count();
            (p, on)
        })
        .max_by_key(|&(p, on)| (on, usize::MAX - p))?;
    let frames_held = bits.len() / len;
    let needed = if frames_held < 3 && n >= 24 { 2 } else { 3 };
    if on < needed {
        return None;
    }
    let first = anchor % len;
    Some((on, len, first))
}

/// A sync word found blind: the frame length from the autocorrelation of
/// the bits (as ±1), then, folding the bits on it, the longest run of
/// positions that hold the same value in every frame — up to 32 bits of it
/// from its start. `None` without at least four frames held, a clear
/// period, or a run of 16.
fn blind_word(bits: &[u8]) -> Option<SyncWord> {
    let n = bits.len();
    if n < 4 * MIN_FRAME_BITS {
        return None;
    }
    let max_lag = (n / 4).min(MAX_FRAME_BITS);
    let size = (2 * n).next_power_of_two();
    let mut fft = FftPlanner::<f32>::new();
    let fwd = fft.plan_fft_forward(size);
    let inv = fft.plan_fft_inverse(size);
    // Mean removed: idle fill (long runs of one value) would make every lag
    // look alike.
    let mean = bits.iter().map(|&b| f32::from(b)).sum::<f32>() / n as f32;
    let mut x: Vec<Complex32> = bits
        .iter()
        .map(|&b| Complex32::new(f32::from(b) - mean, 0.0))
        .chain(std::iter::repeat_n(Complex32::new(0.0, 0.0), size - n))
        .collect();
    fwd.process(&mut x);
    for v in &mut x {
        *v = Complex32::new(v.norm_sqr(), 0.0);
    }
    inv.process(&mut x);
    let ac0 = x[0].re.max(1e-9);
    let ac = |lag: usize| x[lag].re / ac0 * n as f32 / (n - lag) as f32;
    let (best, peak) = (MIN_FRAME_BITS..=max_lag)
        .map(|l| (l, ac(l)))
        .max_by(|a, b| a.1.total_cmp(&b.1))?;
    // The fundamental: the smallest lag nearly as strong.
    let len = (MIN_FRAME_BITS..best)
        .filter(|l| best.is_multiple_of(*l) && ac(*l) >= 0.85 * peak)
        .min()
        .unwrap_or(best);
    // Far above what a random stream's lags reach (~1/√n).
    if peak < 8.0 / (n as f32).sqrt() || peak < 0.08 {
        return None;
    }
    let frames = n / len;
    if frames < 4 {
        return None;
    }
    let constant: Vec<Option<u8>> = (0..len)
        .map(|p| {
            let v = bits[p];
            (1..frames).all(|f| bits[f * len + p] == v).then_some(v)
        })
        .collect();
    // The longest run of constant positions, round the frame's end.
    let mut best_run = (0usize, 0usize);
    let mut start = None;
    for i in 0..2 * len {
        let c = constant[i % len].is_some();
        match (c, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                if i - s > best_run.1 {
                    best_run = (s, i - s);
                }
                start = None;
            }
            _ => {}
        }
        if i >= len && start.is_none() {
            break;
        }
    }
    if let Some(s) = start
        && (2 * len - s).min(len) > best_run.1
    {
        best_run = (s, (2 * len - s).min(len));
    }
    let (s, run) = best_run;
    if run < 16 || run >= len {
        return None;
    }
    let take = run.min(32);
    let mut w = 0u64;
    for k in 0..take {
        w = (w << 1) | u64::from(constant[(s + k) % len].unwrap_or(0));
    }
    // A word of one value only (a long run of idle fill) is no sync word.
    if w == 0 || w == (1u64 << take) - 1 {
        return None;
    }
    Some(SyncWord {
        bits: w,
        len: take as u8,
        name: "found blind".into(),
    })
}

/// Bits (0/1) packed MSB first, inverted if asked; the last byte padded.
pub fn pack(bits: &[u8], invert: bool) -> Vec<u8> {
    let inv = u8::from(invert);
    bits.chunks(8)
        .map(|c| {
            let mut b = 0u8;
            for (k, &x) in c.iter().enumerate() {
                b |= ((x ^ inv) & 1) << (7 - k);
            }
            b
        })
        .collect()
}

/// A minor-frame counter: the word (byte) numbering frames within a major
/// frame (IRIG 106 Chapter 4 §4.3.2's subframe ID).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counter {
    /// Byte position in the frame.
    pub byte: usize,
    /// Bits of the byte that count.
    pub mask: u8,
    /// The values it runs through, `first` to `first + modulus − 1`; the
    /// modulus once a wrap has been seen.
    pub first: u8,
    pub modulus: Option<u32>,
    pub down: bool,
}

impl Counter {
    /// Where frame `bytes` sits in its major frame (0 = the first).
    pub fn index(&self, bytes: &[u8]) -> Option<u32> {
        let v = bytes.get(self.byte)? & self.mask;
        // Until a wrap shows where the count starts, the lowest value seen
        // may not be it: the value as it is (counters mostly start at 0).
        let first = if self.modulus.is_some() {
            self.first
        } else {
            0
        };
        let i = u32::from(v.wrapping_sub(first) & self.mask);
        let i = if self.down {
            self.modulus.map_or(i, |m| (m - 1).saturating_sub(i))
        } else {
            i
        };
        match self.modulus {
            Some(m) if i >= m => None,
            _ => Some(i),
        }
    }
}

/// Find the minor-frame counter in consecutive frames (bytes, the sync
/// word's `skip` bytes left out): a byte (or its low bits) that steps by
/// one from frame to frame, up or down, wrapping round. Needs eight frames
/// or more; nine in ten steps must fit.
pub fn find_counter(frames: &[&[u8]], skip: usize) -> Option<Counter> {
    if frames.len() < 8 {
        return None;
    }
    let len = frames.iter().map(|f| f.len()).min()?;
    let mut best: Option<(f64, Counter)> = None;
    for byte in skip..len {
        for mask in [0xFFu8, 0x7F, 0x3F, 0x1F, 0x0F, 0x07] {
            let v: Vec<u8> = frames.iter().map(|f| f[byte] & mask).collect();
            let lo = *v.iter().min()?;
            let hi = *v.iter().max()?;
            if hi - lo < 2 {
                continue;
            }
            for down in [false, true] {
                let mut fit = 0usize;
                let mut wrapped = false;
                for w in v.windows(2) {
                    let (a, b) = if down { (w[1], w[0]) } else { (w[0], w[1]) };
                    if b == a.wrapping_add(1) & mask && b <= hi {
                        fit += 1;
                    } else if a == hi && b == lo {
                        fit += 1;
                        wrapped = true;
                    }
                }
                let share = fit as f64 / (v.len() - 1) as f64;
                if share < 0.9 {
                    continue;
                }
                // Prefer the widest mask that fits (the whole byte when it
                // is all counter), then the best fit.
                let key = share + f64::from(mask.count_ones()) * 1e-3;
                if best.as_ref().is_none_or(|(k, _)| key > *k) {
                    best = Some((
                        key,
                        Counter {
                            byte,
                            mask,
                            first: lo,
                            modulus: wrapped.then(|| u32::from(hi - lo) + 1),
                            down,
                        },
                    ));
                }
            }
        }
    }
    best.map(|(_, c)| c)
}

/// A spacecraft clock in the frames: a big-endian word counting up at a
/// steady rate (the IS-19 beacon's bytes 12–15: seconds, since
/// 2012-05-31 17:30:48 UTC by the wall clock — 13 h before launch).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClockWord {
    pub byte: usize,
    /// Bytes in the word (2–4).
    pub len: usize,
    /// Counts a second (measured: 1.0 for seconds).
    pub per_second: f64,
    /// Its last reading.
    pub value: u64,
}

impl ClockWord {
    /// The word's value in `bytes`.
    pub fn read(&self, bytes: &[u8]) -> Option<u64> {
        let w = bytes.get(self.byte..self.byte + self.len)?;
        Some(w.iter().fold(0u64, |a, &b| (a << 8) | u64::from(b)))
    }

    /// Its reading in seconds.
    pub fn seconds(&self) -> f64 {
        self.value as f64 / self.per_second
    }
}

/// A check byte at a frame's end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checksum {
    pub kind: CheckKind,
    /// The bytes it covers start here and run up to it (the last byte).
    pub from: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckKind {
    /// Sum mod 256 (the IS-19 beacon's: bytes 0–254 into byte 255).
    Sum,
    /// The sum's two's complement (the whole frame then sums to 0).
    NegSum,
    Xor,
}

impl Checksum {
    /// The frame's check byte holds.
    pub fn check(&self, bytes: &[u8]) -> bool {
        let Some((&last, body)) = bytes.split_last() else {
            return false;
        };
        let Some(body) = body.get(self.from..) else {
            return false;
        };
        let sum = body.iter().fold(0u8, |a, &b| a.wrapping_add(b));
        last == match self.kind {
            CheckKind::Sum => sum,
            CheckKind::NegSum => sum.wrapping_neg(),
            CheckKind::Xor => body.iter().fold(0u8, |a, &b| a ^ b),
        }
    }

    /// "sum of bytes 0–254 in byte 255".
    pub fn describe(&self, frame_bytes: usize) -> String {
        let what = match self.kind {
            CheckKind::Sum => "sum",
            CheckKind::NegSum => "negated sum",
            CheckKind::Xor => "XOR",
        };
        format!(
            "{what} of bytes {}–{} in byte {}",
            self.from,
            frame_bytes.saturating_sub(2),
            frame_bytes.saturating_sub(1)
        )
    }
}

/// Find a check byte at the end of the frames: a sum, negated sum or XOR
/// of the bytes before it (from byte 0 to 4 on) that holds in four frames
/// in five or more (bit errors break the rest) of at least sixteen. A
/// random last byte matches one in 256.
pub fn find_checksum(frames: &[&[u8]]) -> Option<Checksum> {
    if frames.len() < 16 {
        return None;
    }
    let mut best: Option<(usize, Checksum)> = None;
    for kind in [CheckKind::Sum, CheckKind::NegSum, CheckKind::Xor] {
        for from in 0..=4 {
            let c = Checksum { kind, from };
            let ok = frames.iter().filter(|f| c.check(f)).count();
            if 5 * ok >= 4 * frames.len() && best.as_ref().is_none_or(|(n, _)| ok > *n) {
                best = Some((ok, c));
            }
        }
    }
    best.map(|(_, c)| c)
}

/// Find a clock word in frames as (time s, bytes), in time order: a 4-,
/// 3- or 2-byte big-endian word (same place in every frame) that never
/// goes down, has moved by at least 4 counts, and whose rate over the
/// frames' span is within 2 % (and a count's worth) of a power of two
/// counts a second (1/16 to 1024) — the longest such word, earliest in the
/// frame. The minor-frame counter wraps within seconds and so is never
/// taken. `skip`: leading bytes left out (the sync word).
pub fn find_clock(frames: &[(f64, &[u8])], skip: usize) -> Option<ClockWord> {
    if frames.len() < 12 {
        return None;
    }
    let span = frames.last()?.0 - frames.first()?.0;
    if span < 5.0 {
        return None;
    }
    let len = frames.iter().map(|f| f.1.len()).min()?;
    for wlen in [4usize, 3, 2] {
        for byte in skip..len.saturating_sub(wlen - 1) {
            let read = |b: &[u8]| {
                b[byte..byte + wlen]
                    .iter()
                    .fold(0u64, |a, &x| (a << 8) | u64::from(x))
            };
            // Frames that go back, or leap further than a fast clock
            // could, are outliers (a bit error, or another word in this
            // place once a major frame): one in twenty may be.
            let first = read(frames[0].1);
            let (mut prev, mut t_prev) = (first, frames[0].0);
            let mut bad = 0usize;
            for (t, b) in &frames[1..] {
                let v = read(b);
                let most = (1024.0 * (t - t_prev).max(0.0) + 2.0) as u64;
                if v < prev || v - prev > most {
                    bad += 1;
                    continue;
                }
                (prev, t_prev) = (v, *t);
            }
            if 20 * bad > frames.len() || prev - first < 4 {
                continue;
            }
            let rate = (prev - first) as f64 / span;
            let p2 = rate.log2().round();
            if !(-4.0..=10.0).contains(&p2) {
                continue;
            }
            let want = 2f64.powf(p2);
            // 2 %, and a count either way over the span.
            if (rate - want).abs() > 0.02 * want + 1.5 / span {
                continue;
            }
            return Some(ClockWord {
                byte,
                len: wlen,
                per_second: want,
                value: prev,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_sum_check_byte_through_bit_errors() {
        let mut frames: Vec<Vec<u8>> = (0..40u32)
            .map(|i| {
                let mut f: Vec<u8> = (0..64).map(|j| (j * 31 + i * 7) as u8).collect();
                let s = f[..63].iter().fold(0u8, |a, &b| a.wrapping_add(b));
                f[63] = s;
                f
            })
            .collect();
        // A few frames with a bit error.
        for f in frames.iter_mut().step_by(9) {
            f[20] ^= 0x10;
        }
        let refs: Vec<&[u8]> = frames.iter().map(|f| f.as_slice()).collect();
        let c = find_checksum(&refs).expect("checksum");
        assert_eq!(
            c,
            Checksum {
                kind: CheckKind::Sum,
                from: 0
            }
        );
        assert!(!c.check(&frames[0]) && c.check(&frames[1]));
        assert_eq!(c.describe(64), "sum of bytes 0–62 in byte 63");
    }

    #[test]
    fn finds_a_seconds_clock() {
        // 0.427 s frames; bytes 12–15 count seconds, byte 4 a counter.
        let frames: Vec<(f64, Vec<u8>)> = (0..60)
            .map(|i| {
                let t = i as f64 * 0.4267;
                let mut f = vec![0u8; 64];
                f[..3].copy_from_slice(&[0xFA, 0xF3, 0x20]);
                f[4] = (i % 32) as u8;
                let c = 0x1B08_AA00u32 + t as u32;
                f[12..16].copy_from_slice(&c.to_be_bytes());
                f[20] = (i * 7 % 13) as u8;
                // Once, another word in bytes 11–13.
                if i == 17 {
                    f[11..14].copy_from_slice(&[0x28, 0xFB, 0x48]);
                }
                (t, f)
            })
            .collect();
        let refs: Vec<(f64, &[u8])> = frames.iter().map(|(t, f)| (*t, f.as_slice())).collect();
        let c = find_clock(&refs, 3).expect("clock");
        assert_eq!((c.byte, c.len, c.per_second), (12, 4, 1.0));
        assert_eq!(c.value, 0x1B08_AA00 + 25);
    }

    /// IS-19-like frames: FAF320, a fixed byte, a counter 0–31 in byte 4,
    /// then data with some fixed and some changing bytes.
    fn frames(n: usize, len_bytes: usize, start: u8) -> Vec<Vec<u8>> {
        let mut seed = 12345u32;
        let mut rnd = move || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) as u8
        };
        (0..n)
            .map(|i| {
                let mut f = vec![0u8; len_bytes];
                f[..3].copy_from_slice(&[0xFA, 0xF3, 0x20]);
                f[3] = 0x10;
                f[4] = (start as usize + i) as u8 % 32;
                for (j, b) in f.iter_mut().enumerate().skip(5) {
                    *b = if j % 3 == 0 { 0xFF } else { rnd() };
                }
                f
            })
            .collect()
    }

    fn bits_of(frames: &[Vec<u8>]) -> Vec<u8> {
        frames
            .iter()
            .flat_map(|f| {
                f.iter()
                    .flat_map(|b| (0..8).map(move |k| (b >> (7 - k)) & 1))
            })
            .collect()
    }

    #[test]
    fn locks_on_faf320_inverted_with_errors_and_finds_the_counter() {
        let fr = frames(30, 256, 10);
        let mut bits = bits_of(&fr);
        // Some lead-in, inverted throughout, a few bit errors.
        let mut lead: Vec<u8> = (0..777).map(|i| ((i * 7) % 3 == 0) as u8).collect();
        lead.append(&mut bits);
        let mut bits: Vec<u8> = lead.iter().map(|b| b ^ 1).collect();
        for i in (1000..bits.len()).step_by(997) {
            bits[i] ^= 1;
        }
        let mut f = Framer::new();
        let mut out = Vec::new();
        for c in bits.chunks(500) {
            f.push(c, &mut out);
        }
        let l = f.lock().expect("locked");
        assert_eq!(l.word.hex(), "FAF320");
        assert_eq!(l.frame_bits, 2048);
        assert_eq!(l.reading, Reading::Direct);
        assert!(out.len() >= 28, "{} frames", out.len());
        assert!(out.iter().all(|o| o.inverted));
        // The word reads right wherever it had no bit errors.
        assert!(
            out.iter()
                .filter(|o| o.sync_errors == Some(0))
                .all(|o| o.bytes[..3] == [0xFA, 0xF3, 0x20])
        );
        let refs: Vec<&[u8]> = out.iter().map(|o| o.bytes.as_slice()).collect();
        let c = find_counter(&refs, 3).expect("counter");
        assert_eq!(
            (c.byte, c.first, c.modulus, c.down),
            (4, 0, Some(32), false)
        );
        assert_eq!(c.index(&out[0].bytes), Some(u32::from(out[0].bytes[4])));
    }

    #[test]
    fn differential_coding_is_read() {
        let fr = frames(12, 128, 0);
        let bits = bits_of(&fr);
        // NRZ-M: a transition for each 1.
        let mut level = 0u8;
        let coded: Vec<u8> = bits
            .iter()
            .map(|&b| {
                level ^= b;
                level
            })
            .collect();
        let mut f = Framer::new();
        let mut out = Vec::new();
        f.push(&coded, &mut out);
        let l = f.lock().expect("locked");
        assert_eq!(l.reading, Reading::Differential);
        assert_eq!(l.frame_bits, 1024);
        assert!(out.len() >= 10);
    }

    #[test]
    fn a_proprietary_word_is_found_blind() {
        let mut fr = frames(16, 100, 3);
        for f in &mut fr {
            f[..3].copy_from_slice(&[0x5A, 0x0F, 0x36]);
        }
        let bits = bits_of(&fr);
        let mut f = Framer::new();
        let mut out = Vec::new();
        f.push(&bits, &mut out);
        let l = f.lock().expect("locked");
        assert!(l.blind);
        assert_eq!(l.frame_bits, 800);
        // The run holds the word (from the fixed byte before it, on into
        // the fixed byte 3 and the counter's top bits).
        assert!(l.word.hex().contains("5A0F36"), "{}", l.word.hex());
        assert!(out.len() >= 12);
    }

    #[test]
    fn rides_out_a_bit_slip_and_a_lost_word() {
        let fr = frames(16, 64, 0);
        let mut bits = bits_of(&fr);
        // A bit lost in frame 5's middle; frame 9's word wiped (half its
        // bits wrong: inverted, it would still read as the word).
        bits.remove(5 * 512 + 200);
        for (k, b) in bits[9 * 512 - 1..9 * 512 - 1 + 24].iter_mut().enumerate() {
            *b = (k % 2) as u8;
        }
        let mut f = Framer::new().known_only();
        let mut out = Vec::new();
        for c in bits.chunks(300) {
            f.push(c, &mut out);
        }
        assert!(f.lock().is_some());
        assert_eq!(f.stats.losses, 0);
        assert!(f.stats.slips >= 1);
        assert!(f.stats.flywheel >= 1);
        assert!(out.len() >= 14);
    }

    #[test]
    fn noise_locks_nothing() {
        let mut s = 99u32;
        let bits: Vec<u8> = (0..60_000)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s & 1) as u8
            })
            .collect();
        let mut f = Framer::new();
        let mut out = Vec::new();
        for c in bits.chunks(4096) {
            f.push(c, &mut out);
        }
        assert!(f.lock().is_none());
        assert!(out.is_empty());
    }
}
