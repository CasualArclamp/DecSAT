// made by CasualArclamp
//! Convolutional codes found blind: any rate-½ code of constraint length up
//! to [`MAX_K`], recovered from the received bits' own parity checks, with a
//! soft-decision Viterbi decoder for it and a receiver that finds the
//! carrier's orientation and follows it when it turns.
//!
//! The K = 7 code of DVB-S and Intelsat modems (171, 133) has its own
//! decoder ([`crate::conv`], with the punctured rates); this is for the
//! rest — other generators, other constraint lengths (K = 9 (561, 753) of
//! IS-95/UMTS, K = 5 of GSM, K = 3 …), and K = 7 itself on BPSK.
//!
//! **Finding the code.** A rate-½ code with generators g₁(D), g₂(D) (bit j:
//! the input j steps ago) sends c₁ = g₁·u and c₂ = g₂·u, so every coded
//! stream satisfies g₂·c₁ + g₁·c₂ = 0: a parity check spanning K trellis
//! steps, its taps the generators themselves, reversed. In a window of K
//! steps (2K coded bits) the code leaves 2K − 1 dimensions, so the windows
//! of the received stream have exactly one null vector — that check — and
//! none at all for a shorter K. Bit errors break the rank, so the null
//! space is taken from random handfuls of windows (some handful is
//! error-free) and each vector it gives is judged on thousands of windows:
//! a true check of weight w holds in (1 + (1 − 2p)^w)/2 of them at bit
//! error rate p (0.98 at p = 10⁻³, still 0.66 for K = 9 at p = 3 %), any
//! other in half. The shortest check that holds is the code. (The method
//! is the dual-code reconstruction of the blind-recognition literature,
//! e.g. Marazin, Gautier & Burel, "Dual code method for blind
//! identification of convolutional encoder", 2009.)

use decsat_core::Iq;

/// The longest constraint length looked for (and decoded: 2^(K−1) states).
pub const MAX_K: usize = 10;

/// A rate-½ convolutional code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Code {
    /// Constraint length: the input bit and K − 1 before it.
    pub k: usize,
    /// Generators, bit j = the tap on the input j steps ago.
    pub g: [u32; 2],
    /// Outputs sent inverted (CCSDS inverts its second); found on BPSK,
    /// where no orientation of the carrier can undo it.
    pub inv: [bool; 2],
}

/// `x`'s lowest `n` bits, reversed.
fn reverse(x: u32, n: usize) -> u32 {
    (0..n).fold(0, |a, j| a | (((x >> j) & 1) << (n - 1 - j)))
}

fn parity(x: u32) -> u8 {
    (x.count_ones() & 1) as u8
}

impl Code {
    /// The generators as modems and textbooks write them, in octal: the
    /// tap on the newest bit most significant (171, 133 for K = 7).
    pub fn octal(&self) -> [u32; 2] {
        [reverse(self.g[0], self.k), reverse(self.g[1], self.k)]
    }

    /// "K = 9, rate 1/2 (561, 753)", and any output sent inverted.
    pub fn name(&self) -> String {
        let o = self.octal();
        format!(
            "K = {}, rate 1/2 ({:o}, {:o}){}",
            self.k,
            o[0],
            o[1],
            match self.inv {
                [false, false] => "",
                [true, false] => ", first output inverted",
                [false, true] => ", second output inverted",
                [true, true] => ", both outputs inverted",
            }
        )
    }

    /// The standard K = 7 code (171, 133), the one [`crate::conv`] decodes.
    pub fn is_k7_standard(&self) -> bool {
        self.k == 7 && {
            let o = self.octal();
            o == [0o171, 0o133] || o == [0o133, 0o171]
        }
    }

    /// Encode `bits` from the zero state: c₁, c₂ per input bit.
    pub fn encode(&self, bits: &[u8]) -> Vec<u8> {
        let mut reg = 0u32;
        let mask = (1u32 << self.k) - 1;
        let mut out = Vec::with_capacity(2 * bits.len());
        for &b in bits {
            reg = ((reg << 1) | (b & 1) as u32) & mask;
            for i in 0..2 {
                out.push(parity(reg & self.g[i]) ^ self.inv[i] as u8);
            }
        }
        out
    }
}

/// Trellis steps kept before a decision (≥ 5·K for every K here) and bits
/// decided per traceback.
const DEPTH: usize = 96;
const CHUNK: usize = 64;

/// Soft-decision Viterbi decoder for any [`Code`], streaming. Soft inputs:
/// positive for a 0, negative for a 1, 0 for an erasure.
pub struct Decoder {
    m: usize,
    metric: Vec<f32>,
    next: Vec<f32>,
    /// Per step, `words` u64s: bit s = which predecessor state s came from.
    decisions: Vec<u64>,
    words: usize,
    pending: usize,
    /// Branch outputs per (state, input) as ±1.
    branch: Vec<[(f32, f32); 2]>,
}

impl Decoder {
    pub fn new(code: &Code) -> Self {
        let m = code.k - 1;
        let n = 1usize << m;
        // The register holds the input at bit m and the state below it
        // (newest at bit m − 1); taps on the register are the generators
        // reversed — the octal form.
        let taps = code.octal();
        let pm = |p: u8, inv: bool| if (p ^ inv as u8) == 0 { 1.0 } else { -1.0 };
        let branch = (0..n)
            .map(|ps| {
                let mut b = [(0.0, 0.0); 2];
                for (input, o) in b.iter_mut().enumerate() {
                    let reg = ((input as u32) << m) | ps as u32;
                    *o = (
                        pm(parity(reg & taps[0]), code.inv[0]),
                        pm(parity(reg & taps[1]), code.inv[1]),
                    );
                }
                b
            })
            .collect();
        Decoder {
            m,
            metric: vec![0.0; n],
            next: vec![0.0; n],
            decisions: Vec::new(),
            words: n.div_ceil(64),
            pending: 0,
            branch,
        }
    }

    /// One trellis step on a soft (c₁, c₂) pair; decided bits go to `out`
    /// once enough steps follow them.
    pub fn push(&mut self, s1: f32, s2: f32, out: &mut Vec<u8>) {
        let n = self.metric.len();
        let mask = n - 1;
        let base = self.decisions.len();
        self.decisions.resize(base + self.words, 0);
        // Next state ns = (input << (m−1)) | (ps >> 1); its predecessors are
        // (ns << 1) & mask with the dropped bit 0 or 1.
        for ns in 0..n {
            let input = ns >> (self.m - 1);
            let p0 = (ns << 1) & mask;
            let p1 = p0 | 1;
            let (x0, y0) = self.branch[p0][input];
            let (x1, y1) = self.branch[p1][input];
            let m0 = self.metric[p0] + s1 * x0 + s2 * y0;
            let m1 = self.metric[p1] + s1 * x1 + s2 * y1;
            if m1 > m0 {
                self.next[ns] = m1;
                self.decisions[base + ns / 64] |= 1 << (ns % 64);
            } else {
                self.next[ns] = m0;
            }
        }
        std::mem::swap(&mut self.metric, &mut self.next);
        let top = self.metric.iter().fold(f32::MIN, |a, &b| a.max(b));
        if top > 1e6 {
            for m in &mut self.metric {
                *m -= top;
            }
        }
        self.pending += 1;
        if self.steps() >= DEPTH + CHUNK && self.pending >= CHUNK {
            self.traceback(CHUNK, out);
        }
    }

    fn steps(&self) -> usize {
        self.decisions.len() / self.words
    }

    fn traceback(&mut self, n: usize, out: &mut Vec<u8>) {
        let len = self.steps();
        let mask = self.metric.len() - 1;
        let mut s = self
            .metric
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, _)| i)
            .unwrap_or(0);
        let mut bits = vec![0u8; len];
        for t in (0..len).rev() {
            bits[t] = (s >> (self.m - 1)) as u8 & 1;
            let w = self.decisions[t * self.words + s / 64];
            let d = ((w >> (s % 64)) & 1) as usize;
            s = ((s << 1) & mask) | d;
        }
        let start = len - self.pending;
        out.extend_from_slice(&bits[start..start + n]);
        self.pending -= n;
        self.decisions.drain(..(start + n) * self.words);
    }

    /// Decide everything still pending.
    pub fn flush(&mut self, out: &mut Vec<u8>) {
        if self.pending > 0 {
            let n = self.pending;
            self.traceback(n, out);
        }
    }
}

/// Decode a whole block of soft coded bits (c₁, c₂ pairs) with `code`.
pub fn decode_block(code: &Code, soft: &[f32]) -> Vec<u8> {
    let mut d = Decoder::new(code);
    let mut out = Vec::with_capacity(soft.len() / 2);
    for [a, b] in soft.as_chunks::<2>().0 {
        d.push(*a, *b, &mut out);
    }
    d.flush(&mut out);
    out
}

/// A code found in hard bits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Found {
    pub code: Code,
    /// The coded pairs start this many bits into the stream (0 or 1).
    pub phase: usize,
    /// The share of windows the parity check holds in.
    pub fit: f32,
    /// The check (2K bits, window order c₁(t), c₂(t), c₁(t+1) …), and the
    /// value it holds as: 1 when an odd number of its taps fall on an
    /// inverted output.
    pub check: u32,
    pub value: u8,
}

/// A check holding in at least this share of windows is a code (any other
/// holds in half; the best of thousands of chance vectors stays under
/// ~0.56 on the windows judged).
const FIT_MIN: f32 = 0.62;
/// Windows a check is judged on, and handfuls of windows tried per K.
const JUDGE: usize = 6000;
const TRIES: usize = 1500;

/// xorshift64*, for picking windows (deterministic: no `rand` here).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// The windows of `bits` for constraint length `k` from `phase`: 2K bits
/// each (bit i of the word = the i-th bit of the window), one per step.
fn windows(bits: &[u8], k: usize, phase: usize, most: usize) -> Vec<u32> {
    let span = 2 * k;
    if bits.len() < phase + span {
        return Vec::new();
    }
    let n = ((bits.len() - phase - span) / 2 + 1).min(most);
    (0..n)
        .map(|t| {
            let at = phase + 2 * t;
            bits[at..at + span]
                .iter()
                .enumerate()
                .fold(0u32, |w, (i, &b)| w | (((b & 1) as u32) << i))
        })
        .collect()
}

/// GF(2) row reduction of `rows` (`cols` bits each); the null space's
/// basis (vectors h with h·row = 0 for every row).
fn null_space(rows: &[u32], cols: usize) -> Vec<u32> {
    let mut m: Vec<u32> = rows.to_vec();
    let mut pivots: Vec<(usize, usize)> = Vec::new(); // (row, column)
    let mut r = 0;
    for c in 0..cols {
        let Some(p) = (r..m.len()).find(|&i| (m[i] >> c) & 1 == 1) else {
            continue;
        };
        m.swap(r, p);
        for i in 0..m.len() {
            if i != r && (m[i] >> c) & 1 == 1 {
                m[i] ^= m[r];
            }
        }
        pivots.push((r, c));
        r += 1;
        if r == m.len() {
            break;
        }
    }
    let pivot_cols: u32 = pivots.iter().fold(0, |a, &(_, c)| a | (1 << c));
    (0..cols)
        .filter(|&f| (pivot_cols >> f) & 1 == 0)
        .map(|f| {
            // The free column set, each pivot column solved for.
            let mut h = 1u32 << f;
            for &(row, c) in &pivots {
                if (m[row] >> f) & 1 == 1 {
                    h |= 1 << c;
                }
            }
            h
        })
        .collect()
}

/// The share of `rows` check `h` holds in as `value`.
fn fit(rows: &[u32], h: u32, value: u8) -> f32 {
    let ok = rows.iter().filter(|&&w| parity(w & h) == value).count();
    ok as f32 / rows.len().max(1) as f32
}

/// The code a check of span `k` implies: the check's taps on c₁ are g₂
/// reversed, on c₂ g₁ reversed (g₂·c₁ + g₁·c₂ = 0).
fn code_of(check: u32, k: usize) -> Code {
    let (mut a, mut b) = (0u32, 0u32);
    for j in 0..k {
        a |= ((check >> (2 * j)) & 1) << j;
        b |= ((check >> (2 * j + 1)) & 1) << j;
    }
    Code {
        k,
        g: [reverse(b, k), reverse(a, k)],
        inv: [false; 2],
    }
}

/// Whether the windows vary enough to show a code: a stream repeating a
/// short pattern (an idle line sent uncoded, or coded unscrambled) fits
/// many checks and proves none. A code fed random data fills its 2^(2K−1)
/// window patterns about evenly; ask for most of them, or of as many as
/// the windows could show.
fn varied(rows: &[u32], k: usize) -> bool {
    let mut seen = rows.to_vec();
    seen.sort_unstable();
    seen.dedup();
    let patterns = 1usize << (2 * k - 1);
    let expect = patterns.min(rows.len() / 8);
    seen.len() * 10 >= expect * 9
}

/// Look for a rate-½ convolutional code in hard bits: the shortest parity
/// check (K = 2…[`MAX_K`]) most windows satisfy, at either alignment of the
/// coded pairs. `None`: no such code (a block code, an uncoded stream, one
/// too noisy, or too regular to tell).
pub fn find(bits: &[u8]) -> Option<Found> {
    find_pieces(&[bits])
}

/// As [`find`], over separate stretches of one stream (a TDMA channel's
/// bursts, each coded on its own): no window crosses from one into the
/// next, and the coded pairs start `phase` bits into every stretch.
pub fn find_pieces(pieces: &[&[u8]]) -> Option<Found> {
    let total: usize = pieces.iter().map(|p| p.len()).sum();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ total as u64);
    for k in 2..=MAX_K {
        let mut best: Option<Found> = None;
        for phase in 0..2 {
            let mut rows = Vec::new();
            for p in pieces {
                if rows.len() >= JUDGE {
                    break;
                }
                rows.extend(windows(p, k, phase, JUDGE - rows.len()));
            }
            if rows.len() < 200 || !varied(&rows, k) {
                continue;
            }
            let span = 2 * k;
            let take = span + 7;
            let one = 1u32 << span;
            let mut tried: Vec<u32> = Vec::new();
            for _ in 0..TRIES {
                // Each window with a constant 1 after it: the null space
                // then holds affine checks too (h·w = 1 everywhere, an
                // inverted output), as h with that bit set.
                let pick: Vec<u32> = (0..take)
                    .map(|_| rows[rng.below(rows.len())] | one)
                    .collect();
                let null = null_space(&pick, span + 1);
                if null.len() > 3 {
                    // The handful happened to be degenerate.
                    continue;
                }
                for h in null.into_iter().map(|h| h & (one - 1)) {
                    // Span exactly k steps: a tap in the first and last step
                    // (a shorter check would have been found at a smaller K).
                    let first = h & 0b11 != 0;
                    let last = (h >> (span - 2)) & 0b11 != 0;
                    // Both outputs involved (a check on one rail alone is no
                    // rate-½ code).
                    let even = (0..k).any(|j| (h >> (2 * j)) & 1 == 1);
                    let odd = (0..k).any(|j| (h >> (2 * j + 1)) & 1 == 1);
                    if !(first && last && even && odd) || tried.contains(&h) {
                        continue;
                    }
                    tried.push(h);
                    // Holding as 1 everywhere is as good: an inverted output.
                    let f0 = fit(&rows, h, 0);
                    let (f, value) = if f0 >= 0.5 { (f0, 0) } else { (1.0 - f0, 1) };
                    if f >= FIT_MIN && best.is_none_or(|b| f > b.fit) {
                        best = Some(Found {
                            code: code_of(h, k),
                            phase,
                            fit: f,
                            check: h,
                            value,
                        });
                    }
                }
                if best.is_some_and(|b| b.fit > 0.9) {
                    break;
                }
            }
        }
        if best.is_some() {
            return best;
        }
    }
    None
}

/// The raw bit error rate a check of weight `w` failing in `fail` of the
/// windows implies: fail = (1 − (1 − 2p)^w)/2.
pub fn ber_from_fit(fit: f32, w: u32) -> f32 {
    let f = (1.0 - fit).clamp(0.0, 0.4999);
    (1.0 - (1.0 - 2.0 * f).powf(1.0 / w.max(1) as f32)) / 2.0
}

/// How the receiver reads the carrier: the turn (×90°, ×180° on BPSK),
/// whether mirrored, and where the coded pairs start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Orientation {
    pub turn: u8,
    pub mirrored: bool,
    pub phase: usize,
}

impl Orientation {
    pub fn describe(&self, bpsk: bool) -> String {
        let deg = self.turn as u32 * if bpsk { 180 } else { 90 };
        format!(
            "{}{}{}",
            if deg == 0 {
                "not turned".to_string()
            } else {
                format!("turned {deg}°")
            },
            if self.mirrored { ", mirrored" } else { "" },
            if self.phase == 1 {
                ", pairs from the second bit"
            } else {
                ""
            }
        )
    }
}

/// Soft coded bits from symbols under an orientation: BPSK one per symbol
/// (its real part), QPSK two (I then Q); positive for 0. The phase is left
/// to the caller.
fn soft_bits(symbols: &[Iq], bpsk: bool, turn: u8, mirrored: bool, out: &mut Vec<f32>) {
    let rot = if bpsk {
        if turn & 1 == 1 {
            Iq::new(-1.0, 0.0)
        } else {
            Iq::new(1.0, 0.0)
        }
    } else {
        [
            Iq::new(1.0, 0.0),
            Iq::new(0.0, 1.0),
            Iq::new(-1.0, 0.0),
            Iq::new(0.0, -1.0),
        ][turn as usize & 3]
    };
    for &s in symbols {
        let mut y = s * rot;
        if mirrored {
            y = y.conj();
        }
        out.push(y.re);
        if !bpsk {
            out.push(y.im);
        }
    }
}

fn hard(soft: &[f32]) -> Vec<u8> {
    soft.iter().map(|&s| (s < 0.0) as u8).collect()
}

/// Hard-decided coded bits of symbols as they come: BPSK one a symbol (its
/// real part), QPSK two (I then Q); 1 for a negative.
pub fn hard_bits(symbols: &[Iq], bpsk: bool) -> Vec<u8> {
    let mut soft = Vec::with_capacity(2 * symbols.len());
    soft_bits(symbols, bpsk, 0, false, &mut soft);
    hard(&soft)
}

/// What the receiver has made of the carrier.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConvStats {
    /// The code found, its name, and how it is read.
    pub code: Option<Code>,
    pub orientation: Option<String>,
    /// The raw bit error rate (from how often the parity check fails).
    pub channel_ber: f32,
    /// Data bits decoded.
    pub bits: u64,
    /// Searches made, and whether it has stopped looking.
    pub searches: u64,
    pub gave_up: bool,
    /// Times the code stopped fitting and was looked for again; turns of
    /// the carrier followed.
    pub losses: u64,
    pub turns: u64,
}

/// Symbols gathered for a search, searches before giving up, and the
/// cadence of the lock check (symbols, over the last `CHECK_SPAN`).
const SEARCH: usize = 6000;
const MAX_SEARCHES: u64 = 8;
const CHECK_EVERY: usize = 512;
const CHECK_SPAN: usize = 1024;
/// Below this fit the code is taken as lost (a 90° turn, a slip).
const FIT_LOST: f32 = 0.6;
/// Decoded data handed on in blocks of this many bits.
pub const CONV_BLOCK: usize = 4096;

struct Lock {
    code: Code,
    /// The parity check and the value it holds as under `orient`.
    check: u32,
    value: u8,
    orient: Orientation,
    decoder: Decoder,
    /// Coded bits still to skip before the first pair (the phase).
    skip: usize,
    /// Symbols since the lock and since the last lock check; the newest
    /// symbols, for the check.
    total: u64,
    since: usize,
    recent: Vec<Iq>,
    /// A soft bit carried over when a pair straddles two blocks.
    carry: Option<f32>,
}

/// A receiver for a convolutionally coded BPSK or QPSK carrier whose code
/// is not known: finds it, decodes, follows turns.
pub struct ConvRx {
    bpsk: bool,
    held: Vec<Iq>,
    lock: Option<Lock>,
    data: Vec<u8>,
    pub stats: ConvStats,
}

impl ConvRx {
    pub fn new(bpsk: bool) -> Self {
        ConvRx {
            bpsk,
            held: Vec::new(),
            lock: None,
            data: Vec::new(),
            stats: ConvStats::default(),
        }
    }

    /// Feed locked soft symbols; decoded data come out in blocks of
    /// [`CONV_BLOCK`] bits (one bit a byte).
    pub fn push(&mut self, symbols: &[Iq], out: &mut Vec<Vec<u8>>) {
        if self.stats.gave_up {
            return;
        }
        if self.lock.is_none() {
            self.held.extend_from_slice(symbols);
            if self.held.len() < SEARCH {
                return;
            }
            self.search();
            if self.lock.is_none() {
                // Keep the newest half: the next search overlaps this one.
                let keep = SEARCH / 2;
                let drop = self.held.len() - keep;
                self.held.drain(..drop);
                if self.stats.searches >= MAX_SEARCHES {
                    self.stats.gave_up = true;
                    self.held.clear();
                }
                return;
            }
            let held = std::mem::take(&mut self.held);
            self.decode(&held, out);
            return;
        }
        self.decode(symbols, out);
    }

    /// Look for the code; lock on it. One orientation is enough: turned or
    /// mirrored, the stream is the same code with its outputs swapped or
    /// inverted (its check then holding as 1), and the inversion is found
    /// below.
    fn search(&mut self) {
        self.stats.searches += 1;
        let (turn, mirrored) = (0, false);
        let mut soft = Vec::new();
        soft_bits(&self.held, self.bpsk, turn, mirrored, &mut soft);
        let Some(found) = find(&hard(&soft)) else {
            return;
        };
        let orient = Orientation {
            turn,
            mirrored,
            phase: found.phase,
        };
        // Which outputs are sent inverted: the decode that re-encodes best.
        let mut soft = Vec::new();
        soft_bits(&self.held, self.bpsk, turn, mirrored, &mut soft);
        let soft = &soft[found.phase..];
        let soft = &soft[..soft.len() / 2 * 2];
        let code = [[false, false], [false, true], [true, false], [true, true]]
            .into_iter()
            .map(|inv| Code { inv, ..found.code })
            .min_by_key(|c| {
                let dec = decode_block(c, soft);
                let re = c.encode(&dec);
                re.iter()
                    .zip(soft)
                    .filter(|&(&b, &s)| b != (s < 0.0) as u8)
                    .count()
            })
            .unwrap_or(found.code);
        // QPSK: read the carrier the way up that needs no inverted output —
        // a rail turned over is the carrier's orientation, not the code's.
        // Conjugating turns Q over, a half turn both, the two together I.
        let (mut code, mut orient, mut value) = (code, orient, found.value);
        if !self.bpsk && code.inv != [false, false] {
            // c₁ rides I when the pairs start on a symbol, else Q.
            let (inv_i, inv_q) = if orient.phase == 0 {
                (code.inv[0], code.inv[1])
            } else {
                (code.inv[1], code.inv[0])
            };
            (orient.turn, orient.mirrored) = match (inv_i, inv_q) {
                (true, true) => (2, false),
                (false, true) => (0, true),
                _ => (2, true),
            };
            code.inv = [false, false];
            // A plain codeword stream now: the check holds as 0.
            value = 0;
        }
        self.stats.code = Some(code);
        self.stats.orientation = Some(orient.describe(self.bpsk));
        self.stats.channel_ber = ber_from_fit(found.fit, found.check.count_ones());
        self.lock = Some(Lock {
            code,
            check: found.check,
            value,
            orient,
            decoder: Decoder::new(&code),
            skip: found.phase,
            total: 0,
            since: 0,
            recent: Vec::new(),
            carry: None,
        });
    }

    fn decode(&mut self, symbols: &[Iq], out: &mut Vec<Vec<u8>>) {
        let bpsk = self.bpsk;
        let Some(lock) = &mut self.lock else {
            return;
        };
        let mut soft = Vec::with_capacity(2 * symbols.len() + 1);
        soft.extend(lock.carry.take());
        soft_bits(
            symbols,
            bpsk,
            lock.orient.turn,
            lock.orient.mirrored,
            &mut soft,
        );
        let skip = lock.skip.min(soft.len());
        lock.skip -= skip;
        let mut bits = Vec::new();
        let (pairs, rest) = soft[skip..].as_chunks::<2>();
        for [a, b] in pairs {
            lock.decoder.push(*a, *b, &mut bits);
        }
        lock.carry = rest.first().copied();
        self.stats.bits += bits.len() as u64;
        self.data.extend_from_slice(&bits);
        while self.data.len() >= CONV_BLOCK {
            out.push(self.data.drain(..CONV_BLOCK).collect());
        }
        // The lock check: the code's parity check on the newest hard bits,
        // under this orientation and, if it fails, the others.
        lock.recent.extend_from_slice(symbols);
        let over = lock.recent.len().saturating_sub(CHECK_SPAN);
        lock.recent.drain(..over);
        lock.total += symbols.len() as u64;
        lock.since += symbols.len();
        if lock.since < CHECK_EVERY || lock.recent.len() < CHECK_SPAN / 2 {
            return;
        }
        lock.since = 0;
        // Where the pairs start in the recent symbols' bits: QPSK symbols
        // are whole pairs; BPSK symbols single bits, so it follows the
        // count.
        let first = lock.total - lock.recent.len() as u64;
        let phase = if bpsk {
            ((lock.orient.phase as u64 + first) % 2) as usize
        } else {
            lock.orient.phase
        };
        let (k, check, value) = (lock.code.k, lock.check, lock.value);
        let fit_under = |turn: u8, mirrored: bool| {
            let mut s = Vec::new();
            soft_bits(&lock.recent, bpsk, turn, mirrored, &mut s);
            fit(&windows(&hard(&s), k, phase, usize::MAX), check, value)
        };
        let now = fit_under(lock.orient.turn, lock.orient.mirrored);
        self.stats.channel_ber = ber_from_fit(now, check.count_ones());
        if now >= FIT_LOST {
            return;
        }
        // Turned: the orientation that fits now (the trellis kept, so no
        // bit is lost or added). A slip turns the carrier, never mirrors
        // it; ±90° before 180°. When the check has even weight a 180° turn
        // fits as well (every bit inverted), so a QPSK slip of +90° and one
        // of −90° cannot be told apart, and the data after it may come out
        // inverted when the generators' weights are odd — as with the K = 7
        // code; differential decoding, or the payload search's inverted
        // line, takes it up.
        let (old, mirrored) = (lock.orient.turn, lock.orient.mirrored);
        let turns: &[u8] = if bpsk { &[1] } else { &[1, 3, 2] };
        let modulo = if bpsk { 2 } else { 4 };
        let turned = turns
            .iter()
            .map(|&d| ((old + d) % modulo, mirrored))
            .find(|&(turn, mirrored)| fit_under(turn, mirrored) >= 0.8);
        match turned {
            Some((turn, mirrored)) => {
                lock.orient.turn = turn;
                lock.orient.mirrored = mirrored;
                self.stats.turns += 1;
                self.stats.orientation = Some(lock.orient.describe(bpsk));
            }
            None => {
                // Lost: look again from the newest symbols.
                self.stats.losses += 1;
                self.held = std::mem::take(&mut lock.recent);
                self.lock = None;
            }
        }
    }

    /// Whether a code has been found and is being decoded.
    pub fn locked(&self) -> bool {
        self.lock.is_some()
    }
}

/// The parity check (window order) a code satisfies: the inverse of
/// [`code_of`].
pub fn check_of(code: &Code, k: usize) -> u32 {
    let (a, b) = (reverse(code.g[1], k), reverse(code.g[0], k));
    (0..k).fold(0u32, |h, j| {
        h | (((a >> j) & 1) << (2 * j)) | (((b >> j) & 1) << (2 * j + 1))
    })
}

/// How well hard `bits` (a coded stream as sent: c₁ c₂ c₁ c₂ …, its
/// pairs from bit 1 when `odd`) carry `code`: of its parity checks
/// g₂·c₁ + g₁·c₂ = 0 (each output sent inverted adds the other
/// generator's weight), the share that fail — near 0 when they do, ½ when
/// not — and how many were judged. Windows where both outputs hold still
/// are left out: fill (1010…, or a code's output for a run of zeros)
/// would pass any check of the right weight.
pub fn check_fails(code: &Code, bits: &[u8], odd: bool) -> (f64, usize) {
    let k = code.k;
    let pairs: Vec<(u8, u8)> = bits[usize::from(odd).min(bits.len())..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| (p[0] & 1, p[1] & 1))
        .collect();
    let mask = (1u32 << k) - 1;
    let want =
        (u8::from(code.inv[0]) & parity(code.g[1])) ^ (u8::from(code.inv[1]) & parity(code.g[0]));
    // Each output's last k bits, newest in bit 0 (bit j: j steps ago, as
    // the generators are written).
    let (mut r1, mut r2) = (0u32, 0u32);
    let (mut judged, mut failed) = (0usize, 0usize);
    for (n, &(c1, c2)) in pairs.iter().enumerate() {
        r1 = ((r1 << 1) | u32::from(c1)) & mask;
        r2 = ((r2 << 1) | u32::from(c2)) & mask;
        if n + 1 < k {
            continue;
        }
        let still = |r: u32| r == 0 || r == mask;
        if still(r1) && still(r2) {
            continue;
        }
        judged += 1;
        if parity(r1 & code.g[1]) ^ parity(r2 & code.g[0]) != want {
            failed += 1;
        }
    }
    (failed as f64 / judged.max(1) as f64, judged)
}

/// Identify's look: whether locked symbols carry a rate-½ convolutional
/// code (as they come: any orientation shows it). Cheap enough for a first
/// look — hard bits and small GF(2) matrices only.
pub fn detect(symbols: &[Iq], bpsk: bool) -> Option<Code> {
    let mut soft = Vec::new();
    soft_bits(symbols, bpsk, 0, false, &mut soft);
    find(&hard(&soft)).map(|f| f.code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(n: usize, seed: u64) -> Vec<u8> {
        let mut r = Rng(seed | 1);
        (0..n).map(|_| (r.next() >> 63) as u8).collect()
    }

    /// Gaussian noise (Box–Muller) from the same generator.
    fn gauss(r: &mut Rng) -> f32 {
        let u1 = ((r.next() >> 11) as f64 + 1.0) / (1u64 << 53) as f64;
        let u2 = (r.next() >> 11) as f64 / (1u64 << 53) as f64;
        ((-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()) as f32
    }

    fn code(k: usize, octal: [u32; 2]) -> Code {
        Code {
            k,
            g: [reverse(octal[0], k), reverse(octal[1], k)],
            inv: [false; 2],
        }
    }

    #[test]
    fn k7_matches_the_dvb_s_coder() {
        // The same stream as `conv::Encoder` for K = 7 (171, 133).
        let c = code(7, [0o171, 0o133]);
        let u = bits(500, 3);
        let ours = c.encode(&u);
        let mut enc = crate::conv::Encoder::new(crate::conv::Rate::R1_2);
        let mut theirs = Vec::new();
        for &b in &u {
            enc.push(b, &mut theirs);
        }
        assert_eq!(ours, theirs);
        assert!(c.is_k7_standard());
        assert_eq!(c.name(), "K = 7, rate 1/2 (171, 133)");
    }

    #[test]
    fn decodes_through_noise() {
        // Eb/N0 ≈ 4.4 dB: K = 9 and K = 5 nearly clean, K = 3 (free
        // distance 5) a few errors in 3000.
        for (c, most) in [
            (code(9, [0o561, 0o753]), 2),
            (code(5, [0o23, 0o35]), 4),
            (code(3, [0o5, 0o7]), 15),
        ] {
            let u = bits(3000, 11);
            let mut r = Rng(5);
            let soft: Vec<f32> = c
                .encode(&u)
                .iter()
                .map(|&b| (1.0 - 2.0 * b as f32) + 0.6 * gauss(&mut r))
                .collect();
            let d = decode_block(&c, &soft);
            let errs = d.iter().zip(&u).filter(|(a, b)| a != b).count();
            assert!(errs <= most, "{}: {errs} errors", c.name());
        }
    }

    #[test]
    fn finds_codes_from_their_parity_checks() {
        for (k, oct) in [
            (9, [0o561, 0o753]),
            (7, [0o171, 0o133]),
            (5, [0o23, 0o35]),
            (3, [0o5, 0o7]),
        ] {
            let c = code(k, oct);
            let mut coded = c.encode(&bits(8000, k as u64));
            // 0.5 % raw errors, and the pairs starting on the second bit.
            let mut r = Rng(7);
            for b in &mut coded {
                if r.next().is_multiple_of(200) {
                    *b ^= 1;
                }
            }
            let stream: Vec<u8> = std::iter::once(1).chain(coded).collect();
            let f = find(&stream).unwrap_or_else(|| panic!("K = {k} not found"));
            assert_eq!(f.phase, 1);
            assert_eq!(f.code.k, k);
            assert_eq!(f.code.octal(), oct, "K = {k}");
            assert!(f.fit > 0.85, "fit {}", f.fit);
            let p = ber_from_fit(f.fit, f.check.count_ones());
            assert!((0.002..0.01).contains(&p), "BER {p}");
        }
    }

    #[test]
    fn finds_a_code_in_separate_bursts() {
        // 60 bursts of 200 coded bits, each coded from the zero state: no
        // window spans two bursts, and the code is found across them.
        let c = code(7, [0o171, 0o133]);
        let bursts: Vec<Vec<u8>> = (0..60).map(|i| c.encode(&bits(100, 100 + i))).collect();
        let refs: Vec<&[u8]> = bursts.iter().map(|b| b.as_slice()).collect();
        let f = find_pieces(&refs).expect("found");
        assert_eq!(f.code.k, 7);
        assert!(f.code.is_k7_standard());
        assert!(f.fit > 0.99);
    }

    #[test]
    fn finds_nothing_in_random_or_regular_bits() {
        assert_eq!(find(&bits(12000, 99)), None);
        // An idle pattern repeating every 12 bits fits many checks and
        // proves none.
        let idle: Vec<u8> = (0..12000)
            .map(|i| [1, 0, 0, 1, 1, 1, 0, 1, 0, 0, 0, 1][i % 12])
            .collect();
        assert_eq!(find(&idle), None);
    }

    /// QPSK symbols of `c`'s stream, turned and mirrored, with noise.
    fn qpsk(c: &Code, u: &[u8], turn: u8, mirrored: bool, sigma: f32) -> Vec<Iq> {
        let coded = c.encode(u);
        let mut r = Rng(17);
        let rot = [
            Iq::new(1.0, 0.0),
            Iq::new(0.0, 1.0),
            Iq::new(-1.0, 0.0),
            Iq::new(0.0, -1.0),
        ][turn as usize];
        coded
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| {
                let mut s = Iq::new(1.0 - 2.0 * p[0] as f32, 1.0 - 2.0 * p[1] as f32);
                if mirrored {
                    s = s.conj();
                }
                s * rot + Iq::new(sigma * gauss(&mut r), sigma * gauss(&mut r))
            })
            .collect()
    }

    #[test]
    fn receiver_finds_decodes_and_follows_a_turn() {
        let c = code(9, [0o561, 0o753]);
        let u = bits(40_000, 21);
        let mut syms = qpsk(&c, &u, 3, true, 0.45);
        // The carrier turns 90° a third of the way in.
        let turn_at = syms.len() / 3;
        for s in &mut syms[turn_at..] {
            *s *= Iq::new(0.0, 1.0);
        }
        let mut rx = ConvRx::new(false);
        let mut out = Vec::new();
        for ch in syms.chunks(700) {
            rx.push(ch, &mut out);
        }
        let st = &rx.stats;
        // Either output first: the orientation decides which rail is c₁.
        let mut oct = st.code.expect("found").octal();
        oct.sort_unstable();
        assert_eq!(oct, [0o561, 0o753]);
        assert_eq!(st.turns, 1, "{st:?}");
        assert_eq!(st.losses, 0);
        // The decoded bits are the sent ones (up to where decoding began),
        // apart from a few around the turn.
        let got: Vec<u8> = out.concat();
        let start = (0..2000)
            .find(|&o| got[..500].iter().zip(&u[o..]).all(|(a, b)| a == b))
            .expect("aligned");
        // Either way up after the slip (see `decode`): per stretch, the
        // errors or their complement.
        let errs: usize = got
            .chunks(2000)
            .enumerate()
            .map(|(i, ch)| {
                let e = ch
                    .iter()
                    .zip(&u[start + i * 2000..])
                    .filter(|(a, b)| a != b)
                    .count();
                e.min(ch.len() - e)
            })
            .sum();
        assert!(errs < 1500, "{errs} errors");
    }

    #[test]
    fn qpsk_read_the_way_up_that_needs_no_inversion() {
        // Every orientation, both pair alignments: the code comes out
        // without an inverted output, nothing turns, the data decode.
        let c = code(5, [0o23, 0o35]);
        let u = bits(12_000, 51);
        for (turn, mirrored) in (0..4).flat_map(|t| [(t, false), (t, true)]) {
            for lead in [false, true] {
                let mut syms = qpsk(&c, &u, turn, mirrored, 0.35);
                if lead {
                    // Pairs straddling symbols: one coded bit ahead.
                    let mut coded = c.encode(&u);
                    coded.insert(0, 0);
                    coded.pop();
                    let rot = [
                        Iq::new(1.0, 0.0),
                        Iq::new(0.0, 1.0),
                        Iq::new(-1.0, 0.0),
                        Iq::new(0.0, -1.0),
                    ][turn as usize];
                    syms = coded
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|p| {
                            let mut s = Iq::new(1.0 - 2.0 * p[0] as f32, 1.0 - 2.0 * p[1] as f32);
                            if mirrored {
                                s = s.conj();
                            }
                            s * rot
                        })
                        .collect();
                }
                let mut rx = ConvRx::new(false);
                let mut out = Vec::new();
                for ch in syms.chunks(800) {
                    rx.push(ch, &mut out);
                }
                let st = &rx.stats;
                let code = st.code.expect("found");
                assert_eq!(code.inv, [false, false], "{turn} {mirrored} {lead}: {st:?}");
                assert_eq!(
                    (st.turns, st.losses),
                    (0, 0),
                    "{turn} {mirrored} {lead}: {st:?}"
                );
                let got: Vec<u8> = out.concat();
                let ok = (0..3000).any(|o| {
                    let e = got.iter().zip(&u[o..]).filter(|(a, b)| a != b).count();
                    e < 20 || e > got.len().min(u.len() - o) - 20
                });
                assert!(ok, "{turn} {mirrored} {lead}: data not decoded");
            }
        }
    }

    #[test]
    fn bpsk_with_an_inverted_output() {
        // CCSDS-style: K = 7, the second output inverted, on BPSK.
        let c = Code {
            inv: [false, true],
            ..code(7, [0o171, 0o133])
        };
        let u = bits(20_000, 33);
        let mut r = Rng(9);
        let syms: Vec<Iq> = c
            .encode(&u)
            .iter()
            .map(|&b| {
                Iq::new(
                    -(1.0 - 2.0 * b as f32) + 0.4 * gauss(&mut r),
                    0.4 * gauss(&mut r),
                )
            })
            .collect();
        let mut rx = ConvRx::new(true);
        let mut out = Vec::new();
        for ch in syms.chunks(1000) {
            rx.push(ch, &mut out);
        }
        let code = rx.stats.code.expect("found");
        assert_eq!(code.k, 7);
        let got: Vec<u8> = out.concat();
        assert!(got.len() > 10_000);
        let start = (0..4000)
            .find(|&o| got[..300].iter().zip(&u[o..]).all(|(a, b)| a == b))
            .expect("decodes the sent bits");
        let errs = got.iter().zip(&u[start..]).filter(|(a, b)| a != b).count();
        assert!(errs < 5, "{errs}");
    }

    #[test]
    fn gives_up_on_an_uncoded_carrier() {
        let u = bits(2 * 60_000, 41);
        let syms: Vec<Iq> = u
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| Iq::new(1.0 - 2.0 * p[0] as f32, 1.0 - 2.0 * p[1] as f32))
            .collect();
        let mut rx = ConvRx::new(false);
        let mut out = Vec::new();
        for ch in syms.chunks(1000) {
            rx.push(ch, &mut out);
        }
        assert!(rx.stats.gave_up && rx.stats.code.is_none());
        assert_eq!(rx.stats.searches, MAX_SEARCHES);
        assert!(out.is_empty());
    }

    #[test]
    fn the_parity_check_names_the_coded_reading() {
        // CCSDS's code (second output inverted) over data with runs of
        // zeros, as a beacon's frames have: the fill is 0101…, which
        // passes any check and is left out.
        let ccsds = Code {
            k: 7,
            g: [reverse(0o171, 7), reverse(0o133, 7)],
            inv: [false, true],
        };
        let mut u = bits(3000, 77);
        u[500..1500].fill(0);
        let mut c = vec![1u8];
        c.extend(ccsds.encode(&u));
        let (f, n) = check_fails(&ccsds, &c, true);
        assert!(f < 0.01 && n > 1500, "{f} over {n}");
        // The pairs from the wrong bit, the outputs swapped or the
        // inversion missed: about half fail.
        let swapped = Code {
            g: [ccsds.g[1], ccsds.g[0]],
            inv: [true, false],
            ..ccsds
        };
        let plain = Code {
            inv: [false, false],
            ..ccsds
        };
        for (code, odd) in [(ccsds, false), (swapped, true), (plain, true)] {
            let (f, _) = check_fails(&code, &c, odd);
            if code == plain {
                assert!(f > 0.99, "{f}");
            } else {
                assert!((f - 0.5).abs() < 0.1, "{code:?} {odd}: {f}");
            }
        }
        // Uncoded bits pass no reading.
        let (f, _) = check_fails(&ccsds, &bits(4000, 5), false);
        assert!((f - 0.5).abs() < 0.1, "{f}");
    }
}
