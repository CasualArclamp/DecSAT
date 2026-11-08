// made by CasualArclamp
//! A Comtech 8QAM frame at rate 0.8, as a live carrier sends it: 427.28 kS/s
//! at 24.5°W, 1 Mbit/s of data, idle (Rory's capture, 2026-11-04). Every
//! part of it was found from the signal; no document was used.
//!
//! A frame is 2092 symbols: a 52-symbol unique word, then 2040 symbols of
//! a systematic block code (6120 bits, three a symbol): 1632 symbols of
//! data (4896 bits), then 408 of parity (1224 bits). The UW is BPSK on two
//! opposite corners of the 8QAM, (3, 3) for a 0 and (−3, −3) for a 1. The
//! data are 4896/2092 × 3 bits a symbol = 2.34 × the symbol rate, i.e.
//! 1.000 Mbit/s at 427.28 kS/s.
//!
//! How it was found: the symbols' hard labels agree from frame to frame
//! only in a 52-symbol run every 2092 (the UW). Under the RCV-20x 8QAM
//! labels ([`POINTS`]), each symbol's bits sent least significant first,
//! the data region obeys a linear recurrence of degree 27 —
//! (x + 1)⁷ · (1 + x³ + x²⁰): a period-8 pattern under the
//! self-synchronising ITU-T V.35 scrambler — and the last 408 symbols of
//! every frame do not. So the data are scrambled ahead of the encoder and
//! the code bits are not. [`crate::payload`] finds the V.35 descrambler as
//! it does for other modems, and the pattern is HDLC idle flags on an
//! inverted (differentially read) line.
//!
//! **The parity is not decoded.** With idle data the parity bits' relation
//! to the data cannot be told apart from many others (the data span only a
//! few dozen dimensions), so the code — LDPC, Reed–Solomon or a product
//! code — is not known yet. [`Cdm8QamRx`] passes the data bits on as
//! received; a capture with traffic would let the code be found blind.
//!
//! The same frame was found carrying **16QAM** at 301.6 kS/s (1962–1964 MHz
//! capture, 2026-11-08): sixteen clean clusters, a 52-symbol UW on the
//! grid's corners every 2092 symbols, with a UW of its own ([`UW_16`], half
//! its bits away from the 8QAM one). Its bit labels and
//! its code are not known, so that frame is only found, counted and turned
//! ([`Cdm8QamRx::new_16qam`]); no data bits come out of it.
//!
//! Not handled: a mirrored (spectrum-inverted) carrier. The UW lies on one
//! axis and reads the same mirrored; the data would come out with labels
//! 1 ↔ 2 and 5 ↔ 6 swapped.

use decsat_core::Iq;

/// Symbols a frame, UW included.
pub const FRAME_SYMBOLS: usize = 2092;
/// The unique word's symbols.
pub const UW_SYMBOLS: usize = 52;
/// The UW, one bit a symbol, first symbol most significant: 0 → (3, 3),
/// 1 → (−3, −3).
pub const UW: u64 = 0x1_D0AF_1492_C05A;
/// The UW of the frame carrying 16QAM, read off the carrier (the first
/// symbol taken as 0: the overall sign is the carrier's phase).
pub const UW_16: u64 = 0x7_A85C_157A_B12C;
/// Data symbols a frame (after the UW), then parity symbols.
pub const DATA_SYMBOLS: usize = 1632;
pub const PARITY_SYMBOLS: usize = 408;
/// Bits a symbol.
pub const BITS: usize = 3;
/// The user's data bits a frame.
pub const DATA: usize = DATA_SYMBOLS * BITS;
/// The codeword: data and parity bits.
pub const CODEWORD: usize = (DATA_SYMBOLS + PARITY_SYMBOLS) * BITS;
/// Bits a frame counting the UW's symbols at three bits each (so that
/// data ÷ frame × 3 is the data rate over the symbol rate).
pub const FRAME_BITS: usize = FRAME_SYMBOLS * BITS;

/// The 8 points by label on the ±1, ±3 grid: the CTCOM RCV-20x manual's
/// Figure 3.2 8QAM, as `decsat_fec::Constellation::qam8` has it.
pub const POINTS: [(f32, f32); 8] = [
    (3.0, 3.0),
    (3.0, -1.0),
    (-1.0, 3.0),
    (1.0, 1.0),
    (-3.0, -3.0),
    (1.0, -3.0),
    (-3.0, 1.0),
    (-1.0, -1.0),
];

/// A 16QAM frame's bits, as far as they are known: 1632 + 408 symbols of
/// four bits (the split taken from the 8QAM frame's).
pub const DATA_16: usize = DATA_SYMBOLS * 4;
pub const FRAME_BITS_16: usize = FRAME_SYMBOLS * 4;

/// The 16 points of the ±1, ±3 grid. Their order is this receiver's own (the
/// carrier's bit labels are not known); the UW's two are the first and last.
pub const POINTS_16: [(f32, f32); 16] = [
    (3.0, 3.0),
    (3.0, 1.0),
    (3.0, -1.0),
    (3.0, -3.0),
    (1.0, 3.0),
    (1.0, 1.0),
    (1.0, -1.0),
    (1.0, -3.0),
    (-1.0, 3.0),
    (-1.0, 1.0),
    (-1.0, -1.0),
    (-1.0, -3.0),
    (-3.0, 3.0),
    (-3.0, 1.0),
    (-3.0, -1.0),
    (-3.0, -3.0),
];

/// The UW's symbol `k` as ±1 (+1 for a 0 bit).
fn uw_sign(word: u64, k: usize) -> f32 {
    if (word >> (UW_SYMBOLS - 1 - k)) & 1 == 0 {
        1.0
    } else {
        -1.0
    }
}

/// The UW as ±1, and the products of neighbours the differential search
/// weighs each symbol pair with.
struct UwTables {
    sign: [f32; UW_SYMBOLS],
    pair: [f32; UW_SYMBOLS - 1],
}

impl UwTables {
    fn new(word: u64) -> Self {
        let sign: [f32; UW_SYMBOLS] = std::array::from_fn(|k| uw_sign(word, k));
        let pair = std::array::from_fn(|k| sign[k] * sign[k + 1]);
        UwTables { sign, pair }
    }
}

/// A UW this well matched (coherently, 0..1) counts as found where one is
/// due; random data reach ~0.15.
const UW_HOLD: f32 = 0.5;
/// The differential search's bar, per frame (noisier than the coherent
/// match: two symbols' noise in every term).
const UW_FIND: f32 = 0.45;
/// Frames running the UW must be found in before locking.
const UW_FRAMES: usize = 3;
/// UW misses in a row before searching again.
const UW_LOST: u32 = 4;
/// Symbols either way a slip is looked for.
const SLIP: usize = 2;

/// Differential UW match at every start in `held` that has `UW_FRAMES`
/// frames after it: products of neighbouring symbols, so neither the
/// carrier's phase nor a small frequency error matters.
fn differential(held: &[Iq], t: &UwTables) -> Vec<f32> {
    // d[i] = s[i+1]·conj(s[i]); its magnitude normalises.
    let d: Vec<Iq> = held.windows(2).map(|w| w[1] * w[0].conj()).collect();
    let starts = held.len().saturating_sub(UW_SYMBOLS - 1);
    (0..starts)
        .map(|o| {
            let (mut acc, mut mag) = (Iq::new(0.0, 0.0), 0.0f32);
            for (k, &w) in t.pair.iter().enumerate() {
                acc += d[o + k] * w;
                mag += d[o + k].norm();
            }
            acc.norm() / mag.max(1e-30)
        })
        .collect()
}

/// Where in `held` a UW starts that recurs every frame `UW_FRAMES` times,
/// found differentially: the first such start.
fn find_uw(held: &[Iq], t: &UwTables) -> Option<usize> {
    let need = (UW_FRAMES - 1) * FRAME_SYMBOLS + UW_SYMBOLS;
    if held.len() < need {
        return None;
    }
    let dm = differential(held, t);
    let last = held.len() - need;
    (0..=last.min(FRAME_SYMBOLS - 1))
        .filter(|&o| (0..UW_FRAMES).all(|f| dm[o + f * FRAME_SYMBOLS] >= UW_FIND))
        .max_by(|&a, &b| {
            let score = |o: usize| {
                (0..UW_FRAMES)
                    .map(|f| dm[o + f * FRAME_SYMBOLS])
                    .sum::<f32>()
            };
            score(a).total_cmp(&score(b))
        })
}

/// The coherent UW match at `at`: the correlation (whose angle is the
/// (3, 3) point's) and its magnitude over the symbols' (0..1).
fn coherent(held: &[Iq], at: usize, t: &UwTables) -> (Iq, f32) {
    let (mut acc, mut mag) = (Iq::new(0.0, 0.0), 0.0f32);
    for (k, &s) in held[at..at + UW_SYMBOLS].iter().enumerate() {
        acc += s * t.sign[k];
        mag += s.norm();
    }
    (acc, acc.norm() / mag.max(1e-30))
}

/// Which UWs carrier-locked (or merely symbol-timed) `symbols` carry: a
/// Comtech 8QAM rate-0.8 frame's, every 2092 symbols. For Identify; looked
/// for differentially, so the carrier need not be locked.
pub fn detect(symbols: &[Iq]) -> bool {
    detect_word(symbols, UW)
}

/// The same for the frame that carries 16QAM, by its own UW.
pub fn detect_16(symbols: &[Iq]) -> bool {
    detect_word(symbols, UW_16)
}

fn detect_word(symbols: &[Iq], word: u64) -> bool {
    let t = UwTables::new(word);
    // Through the listen a few frames at a time: the timing may slip.
    let span = (UW_FRAMES + 1) * FRAME_SYMBOLS;
    symbols.chunks(span).any(|c| find_uw(c, &t).is_some())
}

/// What the receiver has found and done (shown as the TPC chain's stats).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cdm8QamStats {
    pub uw_locked: bool,
    /// How far the UW found the carrier turned, degrees.
    pub turned_deg: f32,
    pub frames: u64,
    /// UW symbols decided wrong, and UW symbols: a symbol error estimate
    /// for the constellation's corners.
    pub uw_errors: u64,
    pub uw_symbols: u64,
    pub uw_misses: u64,
    /// Symbol slips followed.
    pub slips: u64,
    /// The frame carries 16QAM (framed only).
    pub sixteen: bool,
}

impl Cdm8QamStats {
    /// The share of UW symbols decided right (1 less the corner points'
    /// symbol error rate).
    pub fn uw_fit(&self) -> f32 {
        1.0 - self.uw_errors as f32 / self.uw_symbols.max(1) as f32
    }
}

/// Comtech 8QAM rate-0.8 receiver: carrier-locked 8QAM symbols in, each
/// frame's 4896 data bits (0/1, in the order sent) out. The UW resolves the
/// carrier's phase (any turn, measured, not only the constellation's 180°
/// symmetry) and the amplitude to decide against.
pub struct Cdm8QamRx {
    tables: UwTables,
    /// The constellation decided against, and the labels of the UW's 0 and 1.
    points: &'static [(f32, f32)],
    uw_labels: [usize; 2],
    held: Vec<Iq>,
    /// The next UW's start in `held`, once locked.
    next: Option<usize>,
    /// Held symbols looked through since the last search.
    since: usize,
    misses: u32,
    /// The last UW found's correlation: a missed UW's frame is decided
    /// with it.
    last: Option<Iq>,
    pub stats: Cdm8QamStats,
}

impl Default for Cdm8QamRx {
    fn default() -> Self {
        Self::new()
    }
}

impl Cdm8QamRx {
    pub fn new() -> Self {
        Cdm8QamRx {
            tables: UwTables::new(UW),
            points: &POINTS,
            uw_labels: [0, 4],
            held: Vec::new(),
            next: None,
            since: usize::MAX,
            misses: 0,
            last: None,
            stats: Cdm8QamStats::default(),
        }
    }

    /// The same frame carrying 16QAM: UW found and counted, no bits out.
    pub fn new_16qam() -> Self {
        let mut rx = Self::new();
        rx.tables = UwTables::new(UW_16);
        rx.points = &POINTS_16;
        rx.uw_labels = [0, 15];
        rx.stats.sixteen = true;
        rx
    }

    /// Feed symbols; each frame's data bits ([`DATA`] of them) go to `out`.
    pub fn push(&mut self, symbols: &[Iq], out: &mut Vec<Vec<u8>>) {
        self.held.extend_from_slice(symbols);
        self.since = self.since.saturating_add(symbols.len());
        loop {
            let Some(at) = self.next else {
                // Search once a frame's worth has come, so a carrier with no
                // UW costs one pass a frame.
                if self.since < FRAME_SYMBOLS {
                    return;
                }
                self.since = 0;
                match find_uw(&self.held, &self.tables) {
                    Some(o) => {
                        self.next = Some(o);
                        self.misses = 0;
                    }
                    None => {
                        // Keep what a UW starting late could still need.
                        let keep = UW_FRAMES * FRAME_SYMBOLS + UW_SYMBOLS;
                        let cut = self.held.len().saturating_sub(keep);
                        self.held.drain(..cut);
                        return;
                    }
                }
                continue;
            };
            // A whole frame and the slip margin either side.
            if self.held.len() < at + FRAME_SYMBOLS + SLIP {
                return;
            }
            let lo = at.saturating_sub(SLIP);
            let (best, c, fit) = (lo..=at + SLIP)
                .map(|p| {
                    let (c, fit) = coherent(&self.held, p, &self.tables);
                    (p, c, fit)
                })
                .max_by(|a, b| a.2.total_cmp(&b.2))
                .expect("a range of five");
            if fit < UW_HOLD {
                self.stats.uw_misses += 1;
                self.misses += 1;
                if self.misses >= UW_LOST {
                    self.stats.uw_locked = false;
                    self.next = None;
                    self.since = usize::MAX;
                    continue;
                }
                // Coast: take the frame where it was due.
                self.frame(at, None, out);
            } else {
                if best != at {
                    self.stats.slips += 1;
                }
                self.misses = 0;
                self.stats.uw_locked = true;
                self.frame(best, Some(c), out);
            }
            let start = if fit < UW_HOLD { at } else { best };
            // Keep SLIP symbols ahead of the next UW, so an early one (a
            // symbol lost) can be found too.
            let next = start + FRAME_SYMBOLS;
            let cut = next - SLIP.min(next);
            self.held.drain(..cut);
            self.next = Some(next - cut);
        }
    }

    /// Decide the frame at `at`; `c` is its UW's correlation (`None`: the
    /// UW was missed, the last one's turn is kept).
    fn frame(&mut self, at: usize, c: Option<Iq>, out: &mut Vec<Vec<u8>>) {
        // `or` keeps `c` when it is `Some`, else falls back to the last.
        let Some(c) = c.or(self.last) else { return };
        self.last = Some(c);
        // The UW's 0 is the (3, 3) corner at 45°: turn it back there and
        // scale its length to the grid's 3√2.
        let corner = Iq::new(3.0, 3.0);
        let scale = corner.norm() * UW_SYMBOLS as f32 / c.norm().max(1e-30);
        let rot = corner / corner.norm() * c.conj() / c.norm().max(1e-30) * scale;
        self.stats.turned_deg = (c.arg() - corner.arg()).to_degrees().rem_euclid(360.0);
        let points = self.points;
        let decide = |s: Iq| -> usize {
            let z = s * rot;
            (0..points.len())
                .min_by(|&a, &b| {
                    let d = |l: usize| (z - Iq::new(points[l].0, points[l].1)).norm_sqr();
                    d(a).total_cmp(&d(b))
                })
                .expect("a constellation has points")
        };
        for k in 0..UW_SYMBOLS {
            let want = self.uw_labels[usize::from(self.tables.sign[k] < 0.0)];
            if decide(self.held[at + k]) != want {
                self.stats.uw_errors += 1;
            }
        }
        self.stats.uw_symbols += UW_SYMBOLS as u64;
        if self.stats.sixteen {
            self.stats.frames += 1;
            return;
        }
        let mut bits = Vec::with_capacity(DATA);
        for &s in &self.held[at + UW_SYMBOLS..at + UW_SYMBOLS + DATA_SYMBOLS] {
            let l = decide(s);
            // Least significant bit first.
            bits.extend((0..BITS).map(|b| ((l >> b) & 1) as u8));
        }
        self.stats.frames += 1;
        out.push(bits);
    }
}

/// Build a frame for tests: the UW, `data` ([`DATA`] bits) and `parity`
/// ([`CODEWORD`] − [`DATA`] bits, the code being unknown) on the grid,
/// unit average power not applied (the receiver measures the scale).
pub fn modulate_frame(data: &[u8], parity: &[u8], out: &mut Vec<Iq>) {
    assert_eq!(data.len(), DATA);
    assert_eq!(parity.len(), CODEWORD - DATA);
    for k in 0..UW_SYMBOLS {
        let p = if uw_sign(UW, k) > 0.0 {
            POINTS[0]
        } else {
            POINTS[4]
        };
        out.push(Iq::new(p.0, p.1));
    }
    for t in data.chunks(BITS).chain(parity.chunks(BITS)) {
        let l = usize::from(t[0]) | usize::from(t[1]) << 1 | usize::from(t[2]) << 2;
        out.push(Iq::new(POINTS[l].0, POINTS[l].1));
    }
}

/// Build a 16QAM frame for tests: the UW, then 2040 labels of [`POINTS_16`].
pub fn modulate_frame_16qam(labels: &[u8], out: &mut Vec<Iq>) {
    assert_eq!(labels.len(), DATA_SYMBOLS + PARITY_SYMBOLS);
    for k in 0..UW_SYMBOLS {
        let p = POINTS_16[if uw_sign(UW_16, k) > 0.0 { 0 } else { 15 }];
        out.push(Iq::new(p.0, p.1));
    }
    for &l in labels {
        let p = POINTS_16[usize::from(l & 15)];
        out.push(Iq::new(p.0, p.1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tpc2964::tests::{gauss, rng};

    #[test]
    fn frames_16qam_and_puts_out_no_bits() {
        let mut next = rng(21);
        let mut syms = Vec::new();
        for _ in 0..12 {
            let labels: Vec<u8> = (0..DATA_SYMBOLS + PARITY_SYMBOLS)
                .map(|_| (next() & 15) as u8)
                .collect();
            modulate_frame_16qam(&labels, &mut syms);
        }
        let rx_in = channel(&syms[500..], 73.0, 22);
        assert!(detect_16(&rx_in));
        let mut rx = Cdm8QamRx::new_16qam();
        let mut out = Vec::new();
        for c in rx_in.chunks(1000) {
            rx.push(c, &mut out);
        }
        assert!(rx.stats.uw_locked);
        assert!(rx.stats.frames >= 9, "{:?}", rx.stats);
        assert_eq!(rx.stats.uw_fit(), 1.0, "{:?}", rx.stats);
        assert!(out.is_empty());
    }

    fn frames(n: usize, seed: u64) -> (Vec<Vec<u8>>, Vec<Iq>) {
        let mut next = rng(seed);
        let mut sent = Vec::new();
        let mut syms = Vec::new();
        for _ in 0..n {
            let data: Vec<u8> = (0..DATA).map(|_| (next() & 1) as u8).collect();
            let parity: Vec<u8> = (0..CODEWORD - DATA).map(|_| (next() & 1) as u8).collect();
            modulate_frame(&data, &parity, &mut syms);
            sent.push(data);
        }
        (sent, syms)
    }

    /// Turned, scaled, noisy (Es/N0 ≈ 22 dB).
    fn channel(syms: &[Iq], turn_deg: f32, seed: u64) -> Vec<Iq> {
        let mut g = gauss(seed);
        let r = Iq::from_polar(0.3, turn_deg.to_radians());
        syms.iter()
            .map(|&s| s * r + Iq::new(g(), g()) * 0.3 * 0.18)
            .collect()
    }

    #[test]
    fn uw_is_52_symbols() {
        assert_eq!(64 - UW.leading_zeros() as usize, UW_SYMBOLS - 3);
        assert_eq!(FRAME_SYMBOLS, UW_SYMBOLS + DATA_SYMBOLS + PARITY_SYMBOLS);
        assert_eq!(CODEWORD, 6120);
    }

    #[test]
    fn receives_frames_at_any_turn() {
        for (turn, seed) in [(0.0, 1), (180.0, 2), (73.0, 3)] {
            let (sent, syms) = frames(12, seed);
            let rx_in = channel(&syms[777..], turn, seed + 10);
            assert!(detect(&rx_in));
            let mut rx = Cdm8QamRx::new();
            let mut out = Vec::new();
            for c in rx_in.chunks(1000) {
                rx.push(c, &mut out);
            }
            assert!(rx.stats.uw_locked, "turn {turn}");
            // The first frame was cut; the rest come out whole.
            assert!(out.len() >= 9, "turn {turn}: {} frames", out.len());
            let first = sent
                .iter()
                .position(|f| *f == out[0])
                .expect("a sent frame");
            assert!(first >= 1);
            for (k, f) in out.iter().enumerate() {
                assert_eq!(*f, sent[first + k], "turn {turn}, frame {k}");
            }
            assert!(
                (rx.stats.turned_deg - turn).abs() < 2.0,
                "{}",
                rx.stats.turned_deg
            );
        }
    }

    #[test]
    fn follows_a_slipped_symbol() {
        let (sent, mut syms) = frames(10, 7);
        // A symbol lost in the fifth frame's parity.
        syms.remove(5 * FRAME_SYMBOLS - 10);
        let rx_in = channel(&syms, 10.0, 8);
        let mut rx = Cdm8QamRx::new();
        let mut out = Vec::new();
        rx.push(&rx_in, &mut out);
        assert_eq!(rx.stats.slips, 1);
        assert_eq!(out.len(), 9);
        assert!(out.iter().zip(&sent).all(|(a, b)| a == b));
    }

    #[test]
    fn random_8qam_has_no_uw() {
        let mut next = rng(3);
        let syms: Vec<Iq> = (0..6 * FRAME_SYMBOLS)
            .map(|_| {
                let p = POINTS[(next() % 8) as usize];
                Iq::new(p.0, p.1)
            })
            .collect();
        assert!(!detect(&syms));
        let mut rx = Cdm8QamRx::new();
        let mut out = Vec::new();
        rx.push(&syms, &mut out);
        assert!(out.is_empty() && !rx.stats.uw_locked);
    }
}
