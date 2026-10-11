//! Teledyne Paradise Q-Flex **FastLink**: Paradise's own low-latency LDPC
//! family, unpublished. Everything here was measured on a live Q-Flex
//! carrier (FastLink, QPSK, rate 0.710, 95 017 sym/s, closed network plus
//! ESC) — the frame, the code and the scrambling, all from the signal:
//!
//! - **Frame**: 11 538 symbols — an 18-symbol sync word, then 11 520 symbols
//!   = 23 040 code bits = eight 2880-bit codewords (I bit first, a bit is 1
//!   when its rail is negative).
//! - **Code**: a quasi-cyclic irregular repeat–accumulate LDPC code,
//!   (2880, 2048), circulants of 64 — the GF(2) rank of thousands of
//!   received codewords gave the dimension, random information-set
//!   reductions of the dual gave its sparse checks: 640 of weight 10 and 192
//!   of weight 18; every data bit is in 4 checks, every parity bit in 2
//!   (an accumulator through all 832 checks, closing on a bit that is
//!   always 0). 8 × 2048 data bits in 23 076 symbols' worth of bits is the
//!   menu's "0.710".
//! - **Layout as sent**: 416 units of six bits — two parity bits, then four
//!   data bits — and a tail of 384 data bits. The data bits form 64 rows of
//!   32 (rows 0–51 in the units, 52–63 the tail); the bit in row `r`,
//!   column `c` is in checks `X[c][i] + 13·t mod 832` with `t = 37·r mod
//!   64` — a DVB-S2-style address table (EN 302 307-1 §5.3.2 builds its
//!   codes the same way, `x + q·(m mod 360)`), here with 13 = 832 / 64.
//! - **Coset**: every check has odd parity as received.
//! - **Scrambling**: the frame's 16 384 data bits, in the order sent, are
//!   added to a sequence that restarts every frame and obeys
//!   `s[i] = s[i−2] ⊕ s[i−16] ⊕ s[i−18] ⊕ s[i−30] ⊕ s[i−32]`
//!   — two interleaved `x¹⁵ + x⁸ + 1` sequences, the even one inverted.
//!
//! The decoder hands the data bits to the payload search like the TPC 2964
//! decoder does (Paradise closed network + ESC framing: [`crate::paradise`]).
//! Not every Q-Flex hands them over in the same order: the ASA192 MSO-BN
//! site sends each data pair's odd bit two pairs late — a skew between the
//! rails that the FEC cannot see, which the payload search undoes
//! ([`crate::payload::PayloadRx::with_rail_skews`]).

use std::sync::OnceLock;

use decsat_core::Iq;

/// The sync word as QPSK symbols, in one carrier orientation: (I, Q) signs,
/// `true` for negative.
const UW: [(bool, bool); 18] = {
    // Measured labels (I bit, Q bit): 0,1,2,1,1,0,1,1,2,2,2,3,3,3,0,3,0,2.
    const L: [u8; 18] = [0, 1, 2, 1, 1, 0, 1, 1, 2, 2, 2, 3, 3, 3, 0, 3, 0, 2];
    let mut out = [(false, false); 18];
    let mut i = 0;
    while i < 18 {
        out[i] = (L[i] >> 1 == 1, L[i] & 1 == 1);
        i += 1;
    }
    out
};
/// Symbols per frame (QPSK, rate 0.710).
pub const FRAME_SYMBOLS: usize = 11_538;
pub const UW_SYMBOLS: usize = 18;
/// Codewords per frame; code and data bits in each.
pub const CODEWORDS: usize = 8;
pub const CODEWORD_BITS: usize = 2880;
pub const DATA_BITS: usize = 2048;
/// Data bits a frame.
pub const FRAME_DATA: usize = CODEWORDS * DATA_BITS;
/// UW symbols allowed wrong.
const UW_ERRORS: usize = 2;

/// Checks per codeword (2880 − 2048), the circulant size, and the step on
/// the check ring between a data column's consecutive circulant rows
/// (832 / 64).
const CHECKS: usize = 832;
const Z: usize = 64;
const STEP: usize = CHECKS / Z;
/// Six-bit units (two parity bits, four data bits) before the data tail.
const UNITS: usize = 416;
const TAIL: usize = 6 * UNITS;
/// Data rows sent before the tail, and data bits a row.
const UNIT_ROWS: usize = 52;
const ROW: usize = 32;
/// The circulant row of data row `r` is `ROW_STEP · r mod 64`.
const ROW_STEP: usize = 37;

/// Each data column's checks for circulant row 0 (measured).
const X: [[u16; 4]; 32] = [
    [89, 127, 629, 796],
    [158, 446, 629, 796],
    [440, 514, 617, 709],
    [89, 127, 440, 709],
    [103, 230, 528, 737],
    [103, 514, 617, 737],
    [338, 552, 599, 809],
    [230, 338, 528, 552],
    [160, 382, 411, 820],
    [160, 411, 599, 809],
    [12, 45, 182, 204],
    [45, 182, 382, 820],
    [54, 315, 393, 566],
    [12, 204, 315, 566],
    [259, 519, 696, 739],
    [54, 259, 393, 519],
    [35, 191, 231, 279],
    [35, 191, 696, 739],
    [261, 308, 364, 773],
    [231, 261, 279, 773],
    [469, 542, 597, 643],
    [308, 364, 469, 643],
    [336, 472, 675, 714],
    [336, 542, 597, 714],
    [151, 242, 486, 773],
    [242, 472, 486, 675],
    [71, 413, 454, 652],
    [151, 413, 652, 773],
    [106, 291, 576, 663],
    [71, 106, 454, 576],
    [22, 95, 127, 639],
    [22, 127, 291, 663],
];

/// The scrambling sequence's first 32 bits, bit 0 first (measured).
const SCRAMBLER_START: u32 = 0xAAA2_A2A6;

/// Where data bit `k` of a codeword (in the order sent) lies.
fn data_pos(k: usize) -> usize {
    let (row, col) = (k / ROW, k % ROW);
    if row < UNIT_ROWS {
        6 * (8 * row + col / 4) + 2 + col % 4
    } else {
        TAIL + ROW * (row - UNIT_ROWS) + col
    }
}

/// Where accumulator bit `i` lies: the ring runs back through the units,
/// and bit 831 — the one that closes it — is always 0.
fn parity_pos(i: usize) -> usize {
    let m = i + 1;
    // (415 − m/2) mod 416, kept non-negative.
    6 * ((2 * UNITS - 1 - m / 2) % UNITS) + m % 2
}

/// The checks of data bit `k`.
fn data_checks(k: usize) -> [usize; 4] {
    let (row, col) = (k / ROW, k % ROW);
    let t = (ROW_STEP * row) % Z;
    X[col].map(|x| (x as usize + STEP * t) % CHECKS)
}

/// The code as check → bit positions, built once.
struct Code {
    /// Bit positions of each check, flattened, and where each check starts.
    bits: Vec<u16>,
    start: Vec<usize>,
}

fn code() -> &'static Code {
    // `OnceLock`: a value built on first use and shared after, safely
    // across threads (Rust's lazy static).
    static CODE: OnceLock<Code> = OnceLock::new();
    CODE.get_or_init(|| {
        let mut per: Vec<Vec<u16>> = vec![Vec::new(); CHECKS];
        for (i, p) in per.iter_mut().enumerate() {
            p.push(parity_pos((i + CHECKS - 1) % CHECKS) as u16);
            p.push(parity_pos(i) as u16);
        }
        for k in 0..DATA_BITS {
            for c in data_checks(k) {
                per[c].push(data_pos(k) as u16);
            }
        }
        let mut start = Vec::with_capacity(CHECKS + 1);
        let mut bits = Vec::new();
        for p in &per {
            start.push(bits.len());
            bits.extend_from_slice(p);
        }
        start.push(bits.len());
        Code { bits, start }
    })
}

/// The frame's scrambling sequence (0/1), built once.
fn scrambler() -> &'static [u8] {
    static SEQ: OnceLock<Vec<u8>> = OnceLock::new();
    SEQ.get_or_init(|| {
        let mut s: Vec<u8> = (0..32)
            .map(|i| ((SCRAMBLER_START >> (31 - i)) & 1) as u8)
            .collect();
        while s.len() < FRAME_DATA {
            let i = s.len();
            s.push(s[i - 2] ^ s[i - 16] ^ s[i - 18] ^ s[i - 30] ^ s[i - 32]);
        }
        s
    })
}

/// Encode 2048 data bits (0/1, in the order sent) into a codeword in the
/// received coset: every check of odd parity.
pub fn encode(data: &[u8], out: &mut [u8]) {
    assert_eq!(data.len(), DATA_BITS);
    assert_eq!(out.len(), CODEWORD_BITS);
    out.fill(0);
    let mut sum = [0u8; CHECKS];
    for (k, &b) in data.iter().enumerate() {
        out[data_pos(k)] = b;
        for c in data_checks(k) {
            sum[c] ^= b;
        }
    }
    // Check i holds accumulator bits i − 1 and i: p_i = p_{i−1} ⊕ s_i ⊕ 1,
    // from p_{−1} = p_831 = 0 (each data bit is in an even number of checks
    // and 832 is even, so the ring closes on 0 again).
    let mut prev = 0u8;
    for (i, &s) in sum.iter().enumerate().take(CHECKS - 1) {
        let p = prev ^ s ^ 1;
        out[parity_pos(i)] = p;
        prev = p;
    }
}

/// Iterations before giving up, and the normalised min-sum scale.
const MAX_ITER: usize = 40;
const ALPHA: f32 = 0.75;

/// Decode one codeword by layered normalised min-sum: LLRs in (positive
/// means 0), hard bits out. The number of iterations, or `None` if it did
/// not converge (the bits are the last hard decisions).
fn decode(llr: &[f32], bits: &mut [u8], msgs: &mut Vec<f32>, post: &mut Vec<f32>) -> Option<usize> {
    let code = code();
    msgs.clear();
    msgs.resize(code.bits.len(), 0.0);
    post.clear();
    post.extend_from_slice(llr);
    let odd_ok = |post: &[f32]| {
        (0..CHECKS).all(|c| {
            code.bits[code.start[c]..code.start[c + 1]]
                .iter()
                .filter(|&&p| post[p as usize] < 0.0)
                .count()
                % 2
                == 1
        })
    };
    let mut done = None;
    if odd_ok(post) {
        done = Some(0);
    }
    let mut it = 0;
    while done.is_none() && it < MAX_ITER {
        it += 1;
        for c in 0..CHECKS {
            // This check's bits and its messages to them (slices of the
            // flattened tables, walked side by side with `zip`).
            let (a, b) = (code.start[c], code.start[c + 1]);
            let bits = &code.bits[a..b];
            let msg = &mut msgs[a..b];
            // Extrinsic inputs, their sign product and two smallest sizes.
            let (mut min1, mut min2, mut at) = (f32::INFINITY, f32::INFINITY, 0);
            let mut neg = true; // odd parity: the product's sign flips once more
            for (e, (&p, &r)) in bits.iter().zip(msg.iter()).enumerate() {
                let p = p as usize;
                let t = post[p] - r;
                post[p] = t;
                neg ^= t < 0.0;
                let m = t.abs();
                if m < min1 {
                    (min2, min1, at) = (min1, m, e);
                } else if m < min2 {
                    min2 = m;
                }
            }
            for (e, (&p, r)) in bits.iter().zip(msg.iter_mut()).enumerate() {
                let p = p as usize;
                let t = post[p];
                let size = ALPHA * if e == at { min2 } else { min1 };
                // The others' sign: the whole product with this one undone.
                let m = if neg ^ (t < 0.0) { -size } else { size };
                *r = m;
                post[p] = t + m;
            }
        }
        if odd_ok(post) {
            done = Some(it);
        }
    }
    for (b, &l) in bits.iter_mut().zip(post.iter()) {
        *b = u8::from(l < 0.0);
    }
    done
}

/// A carrier orientation: quarter turns, and mirrored first.
type Orientation = (u8, bool);

/// The sync word under orientation `o` as unit-scale points.
fn uw_points((k, conj): Orientation) -> [Iq; UW_SYMBOLS] {
    let mut out = [Iq::new(0.0, 0.0); UW_SYMBOLS];
    for (o, &(i, q)) in out.iter_mut().zip(&UW) {
        let mut z = Iq::new(if i { -1.0 } else { 1.0 }, if q { -1.0 } else { 1.0 });
        if conj {
            z = z.conj();
        }
        for _ in 0..k {
            z *= Iq::new(0.0, 1.0);
        }
        *o = z;
    }
    out
}

/// Undo orientation `o`: received = mirror?(sent) · jᵏ.
fn derotate(z: Iq, (k, conj): Orientation) -> Iq {
    let mut z = z;
    for _ in 0..k {
        z *= Iq::new(0.0, -1.0);
    }
    if conj { z.conj() } else { z }
}

const ORIENTATIONS: [Orientation; 8] = [
    (0, false),
    (1, false),
    (2, false),
    (3, false),
    (0, true),
    (1, true),
    (2, true),
    (3, true),
];

fn quad(z: Iq) -> (bool, bool) {
    (z.re < 0.0, z.im < 0.0)
}

/// Whether the sync word under `o` starts at symbol `p`.
fn uw_at(symbols: &[Iq], p: usize, o: Orientation) -> bool {
    p + UW_SYMBOLS <= symbols.len()
        && symbols[p..p + UW_SYMBOLS]
            .iter()
            .zip(&uw_points(o))
            .filter(|(a, b)| quad(**a) != quad(**b))
            .count()
            <= UW_ERRORS
}

/// The first place where the sync word recurs `min_frames` times running,
/// under whichever orientation shows it. The run may start anywhere: a
/// carrier phase slip ends one orientation's run and starts another's
/// (Identify's one look at the ASA192 MSO-BN Q-Flex: two frames, a half
/// turn, five frames).
fn locate(symbols: &[Iq], min_frames: usize) -> Option<(Orientation, usize)> {
    if symbols.len() < FRAME_SYMBOLS * min_frames + UW_SYMBOLS {
        return None;
    }
    let last_start = symbols.len() - (min_frames - 1) * FRAME_SYMBOLS - UW_SYMBOLS;
    (0..last_start)
        .flat_map(|p| ORIENTATIONS.map(|o| (o, p)))
        .find(|&(o, p)| (0..min_frames).all(|m| uw_at(symbols, p + m * FRAME_SYMBOLS, o)))
}

/// Where the sync word appears in carrier-locked QPSK `symbols`, under
/// whichever orientation fits best; at least `min_frames` running.
pub fn find_frames(symbols: &[Iq], min_frames: usize) -> Option<Vec<usize>> {
    let (o, p0) = locate(symbols, min_frames)?;
    Some(
        (0..)
            .map(|m| p0 + m * FRAME_SYMBOLS)
            .take_while(|&p| p + UW_SYMBOLS <= symbols.len())
            .filter(|&p| uw_at(symbols, p, o))
            .collect(),
    )
}

/// Whether carrier-locked QPSK symbols carry Q-Flex FastLink framing.
pub fn detect(symbols: &[Iq]) -> bool {
    find_frames(symbols, 3).is_some()
}

/// What the receiver has found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FastLinkStats {
    /// The sync word is being found every 11 538 symbols.
    pub locked: bool,
    /// The carrier's phase ambiguity, once the sync word resolved it.
    pub orientation: Option<String>,
    pub frames: u64,
    pub codewords: u64,
    /// Codewords whose decoding converged, and those that did not.
    pub decoded: u64,
    pub failed: u64,
    /// Code bits the decoder changed, and code bits, in codewords that
    /// decoded: a channel bit error estimate.
    pub corrected_bits: u64,
    pub code_bits: u64,
    pub uw_misses: u64,
    /// Symbol slips repaired.
    pub slips: u64,
}

impl FastLinkStats {
    pub fn channel_ber(&self) -> f64 {
        self.corrected_bits as f64 / self.code_bits.max(1) as f64
    }
}

/// Sync words missed in a row before searching again, and how far (in
/// symbols) a slipped one is looked for.
const UW_LOST: u32 = 3;
const SLIP: usize = 2;

/// FastLink receiver: carrier-locked QPSK symbols in, each frame's 16 384
/// descrambled data bits (0/1) out.
#[derive(Default)]
pub struct FastLinkRx {
    held: Vec<Iq>,
    /// Orientation and where the next frame's sync word is due in `held`.
    lock: Option<(Orientation, usize)>,
    misses: u32,
    llr: Vec<f32>,
    bits: Vec<u8>,
    msgs: Vec<f32>,
    post: Vec<f32>,
    pub stats: FastLinkStats,
}

impl FastLinkRx {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, symbols: &[Iq], out: &mut Vec<Vec<u8>>) {
        self.held.extend_from_slice(symbols);
        if self.lock.is_none() {
            match locate(&self.held, 3) {
                Some((o, p)) => {
                    self.lock = Some((o, p));
                    self.misses = 0;
                    self.stats.locked = true;
                    self.stats.orientation = Some(orientation_name(o));
                }
                None => {
                    // Keep enough for three frames from any start.
                    let keep = 4 * FRAME_SYMBOLS + UW_SYMBOLS;
                    if self.held.len() > keep {
                        self.held.drain(..self.held.len() - keep);
                    }
                    return;
                }
            }
        }
        while let Some((o, at)) = self.lock {
            // The frame, plus room to look for a slip of the next sync word.
            if at + FRAME_SYMBOLS + SLIP + UW_SYMBOLS > self.held.len() {
                break;
            }
            let (mut o, mut start) = (o, at);
            if !uw_at(&self.held, at, o) {
                // A timing slip (a symbol or two) or a carrier phase slip (a
                // quarter turn): the sync word and the next one both appear
                // under the new orientation and offset.
                let moved = (0..=SLIP)
                    .flat_map(|d| [at + d, at.wrapping_sub(d)])
                    .filter(|&p| p < self.held.len())
                    .flat_map(|p| ORIENTATIONS.map(|o2| (o2, p)))
                    .find(|&(o2, p)| {
                        uw_at(&self.held, p, o2) && uw_at(&self.held, p + FRAME_SYMBOLS, o2)
                    });
                match moved {
                    Some((o2, p)) => {
                        (o, start) = (o2, p);
                        self.stats.orientation = Some(orientation_name(o));
                        self.stats.slips += 1;
                        self.misses = 0;
                    }
                    None => {
                        self.misses += 1;
                        self.stats.uw_misses += 1;
                        if self.misses >= UW_LOST {
                            self.lock = None;
                            self.stats.locked = false;
                            self.held.drain(..at.min(self.held.len()));
                            return;
                        }
                    }
                }
            } else {
                self.misses = 0;
            }
            self.frame(o, start + UW_SYMBOLS, out);
            let next = start + FRAME_SYMBOLS;
            let drop = next.saturating_sub(SLIP);
            self.held.drain(..drop);
            self.lock = Some((o, next - drop));
        }
    }

    /// Decode the eight codewords from symbol `from` and descramble.
    fn frame(&mut self, o: Orientation, from: usize, out: &mut Vec<Vec<u8>>) {
        self.stats.frames += 1;
        let mut data = Vec::with_capacity(FRAME_DATA);
        let syms = CODEWORD_BITS / 2;
        for j in 0..CODEWORDS {
            self.llr.clear();
            for &z in &self.held[from + j * syms..from + (j + 1) * syms] {
                let z = derotate(z, o);
                self.llr.push(z.re);
                self.llr.push(z.im);
            }
            self.bits.resize(CODEWORD_BITS, 0);
            let ok = decode(&self.llr, &mut self.bits, &mut self.msgs, &mut self.post).is_some();
            self.stats.codewords += 1;
            if ok {
                self.stats.decoded += 1;
                self.stats.code_bits += CODEWORD_BITS as u64;
                self.stats.corrected_bits += self
                    .llr
                    .iter()
                    .zip(&self.bits)
                    .filter(|(l, b)| u8::from(**l < 0.0) != **b)
                    .count() as u64;
            } else {
                self.stats.failed += 1;
            }
            data.extend((0..DATA_BITS).map(|k| self.bits[data_pos(k)]));
        }
        for (d, s) in data.iter_mut().zip(scrambler()) {
            *d ^= s;
        }
        out.push(data);
    }
}

fn orientation_name((k, conj): Orientation) -> String {
    format!(
        "turned {}°{}",
        90 * k as u32,
        if conj { ", mirrored" } else { "" }
    )
}

/// A FastLink transmitter (for tests and test signals): 16 384 data bits a
/// frame in, 11 538 QPSK symbols out.
pub fn modulate(data: &[u8], out: &mut Vec<Iq>) {
    assert_eq!(data.len(), FRAME_DATA);
    let a = std::f32::consts::FRAC_1_SQRT_2;
    let point = |i: u8, q: u8| Iq::new(if i == 1 { -a } else { a }, if q == 1 { -a } else { a });
    out.extend(uw_points((0, false)).iter().map(|&z| z * a));
    let scrambled: Vec<u8> = data.iter().zip(scrambler()).map(|(d, s)| d ^ s).collect();
    let mut cw = vec![0u8; CODEWORD_BITS];
    for chunk in scrambled.chunks(DATA_BITS) {
        encode(chunk, &mut cw);
        out.extend(cw.chunks(2).map(|p| point(p[0], p[1])));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(seed: u64) -> impl FnMut() -> u64 {
        let mut s = seed | 1;
        move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        }
    }

    fn frames(n: usize, k: u8, conj: bool, seed: u64) -> Vec<Iq> {
        let mut s = seed | 1;
        let mut out = vec![Iq::new(0.7, 0.7); 1234];
        let uw = uw_points((k, conj));
        for _ in 0..n {
            out.extend_from_slice(&uw);
            for _ in 0..FRAME_SYMBOLS - UW_SYMBOLS {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                out.push(Iq::new(
                    if s & 1 == 0 { 1.0 } else { -1.0 },
                    if s & 2 == 0 { 1.0 } else { -1.0 },
                ));
            }
        }
        out
    }

    #[test]
    fn finds_the_framing_in_any_orientation() {
        for (k, conj) in [(0, false), (1, false), (3, true), (2, true)] {
            let x = frames(5, k, conj, 3);
            let hits = find_frames(&x, 3).expect("not found");
            assert_eq!(hits[0], 1234);
            assert_eq!(hits.len(), 5);
        }
    }

    /// Two frames, then a half-turn carrier slip, then five: what Identify
    /// saw of the ASA192 MSO-BN Q-Flex. The framing is the second run.
    #[test]
    fn finds_the_framing_after_a_phase_slip() {
        let mut x = frames(2, 2, false, 5);
        x.extend(frames(5, 0, false, 6));
        assert!(detect(&x));
        let hits = find_frames(&x, 3).expect("not found");
        assert_eq!(hits.len(), 5);
        assert_eq!(hits[0], 1234 + 2 * FRAME_SYMBOLS + 1234);
    }

    #[test]
    fn random_qpsk_is_not_fastlink() {
        let mut s = 7u64;
        let x: Vec<Iq> = (0..60_000)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                Iq::new(
                    if s & 1 == 0 { 1.0 } else { -1.0 },
                    if s & 2 == 0 { 1.0 } else { -1.0 },
                )
            })
            .collect();
        assert!(!detect(&x));
    }

    #[test]
    fn the_code_is_as_measured() {
        let c = code();
        let weights: Vec<usize> = (0..CHECKS).map(|i| c.start[i + 1] - c.start[i]).collect();
        assert_eq!(weights.iter().filter(|&&w| w == 10).count(), 640);
        assert_eq!(weights.iter().filter(|&&w| w == 18).count(), 192);
        // Every bit in 2 (parity) or 4 (data) checks; the layout covers
        // every position once.
        let mut deg = vec![0u32; CODEWORD_BITS];
        for &p in &c.bits {
            deg[p as usize] += 1;
        }
        assert_eq!(deg.iter().filter(|&&d| d == 2).count(), 832);
        assert_eq!(deg.iter().filter(|&&d| d == 4).count(), 2048);
        // The ring closes on position 2490, which is always 0.
        assert_eq!(parity_pos(CHECKS - 1), 2490);
    }

    #[test]
    fn encodes_and_decodes_through_noise() {
        let mut r = rng(11);
        let data: Vec<u8> = (0..DATA_BITS).map(|_| (r() & 1) as u8).collect();
        let mut cw = vec![0u8; CODEWORD_BITS];
        encode(&data, &mut cw);
        // Clean: already a codeword (every check odd).
        let llr: Vec<f32> = cw
            .iter()
            .map(|&b| if b == 1 { -1.0 } else { 1.0 })
            .collect();
        let (mut bits, mut m, mut p) = (vec![0u8; CODEWORD_BITS], Vec::new(), Vec::new());
        assert_eq!(decode(&llr, &mut bits, &mut m, &mut p), Some(0));
        // Near-Gaussian noise, σ ≈ 0.5: about 2 % raw bit errors.
        let mut noisy = llr.clone();
        for l in &mut noisy {
            let n: f32 = (0..6)
                .map(|_| (r() % 1000) as f32 / 1000.0 - 0.5)
                .sum::<f32>()
                * 0.7;
            *l += n;
        }
        let raw_errors = noisy
            .iter()
            .zip(&cw)
            .filter(|(l, b)| u8::from(**l < 0.0) != **b)
            .count();
        assert!(raw_errors > 20, "noise too weak: {raw_errors} errors");
        assert!(
            decode(&noisy, &mut bits, &mut m, &mut p).is_some(),
            "{raw_errors} raw errors"
        );
        assert_eq!(bits, cw);
    }

    #[test]
    fn receives_its_own_frames_in_any_orientation() {
        let mut r = rng(5);
        let frames: Vec<Vec<u8>> = (0..5)
            .map(|_| {
                (0..FRAME_DATA)
                    .map(|_| r().is_multiple_of(3) as u8)
                    .collect()
            })
            .collect();
        let mut tx = vec![Iq::new(0.3, -0.2); 777];
        for f in &frames {
            modulate(f, &mut tx);
        }
        for o in [(1u8, false), (2, true)] {
            let rx_syms: Vec<Iq> = tx
                .iter()
                .map(|&z| {
                    let mut z = if o.1 { z.conj() } else { z };
                    for _ in 0..o.0 {
                        z *= Iq::new(0.0, 1.0);
                    }
                    z
                })
                .collect();
            let mut rx = FastLinkRx::new();
            let mut out = Vec::new();
            for chunk in rx_syms.chunks(5000) {
                rx.push(chunk, &mut out);
            }
            // The last frame waits for the next sync word's slip window.
            assert!(out.len() >= 4, "{} frames", out.len());
            for (got, want) in out.iter().zip(&frames) {
                assert_eq!(got, want);
            }
            assert_eq!(rx.stats.failed, 0);
            assert_eq!(
                rx.stats.orientation.as_deref(),
                Some(orientation_name(o).as_str())
            );
        }
    }
}
