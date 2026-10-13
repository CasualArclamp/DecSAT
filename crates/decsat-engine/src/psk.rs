//! Generic PSK/APSK demodulator: baseband in, carrier-locked symbols and hard
//! decisions out, with no framing assumed.
//!
//! For carriers that are not DVB-S2 (or not yet decodable): DVB-S, SCPC data,
//! telemetry, anything linearly modulated. The recovered symbols go to a file
//! for offline work — e.g. hunting a frame sync word, or a Viterbi decoder.
//!
//! Without a known preamble the absolute phase is ambiguous by the
//! constellation's rotational symmetry (90° for QPSK, 45° for 8PSK, 180° for
//! BPSK), so the symbol indices written may be a fixed rotation of the
//! transmitted ones. That is inherent, not a fault; a sync word found offline
//! resolves it.

use std::collections::VecDeque;

use decsat_core::{Iq, Modulation};
use decsat_dsp::{Agc, CarrierPll, Fir, SymbolSync, rrc_taps};
use decsat_fec::Constellation;
use decsat_modem::text::{TextFinder, TextView};

/// Locked symbols kept for display.
const RECENT: usize = 3000;
/// The MER and lock readouts' smoothing, symbols.
const SMOOTH_SYMBOLS: f32 = 2000.0;

/// The carrier loop's noise bandwidth (Bn·T) for a symbol rate: 0.008 of
/// the symbol rate, but never under ~250 Hz nor over ~1 kHz. Slower carriers
/// need the loop at least ~250 Hz wide to follow the LNB's phase noise: a
/// 10.24 kBd QPSK carrier off a consumer LNB stayed a ring at 0.008 (MER 8
/// dB, i.e. unlocked) and locked at 0.02–0.04 (MER 15 dB). Faster ones want
/// it no wider than ~1 kHz: a wider loop lets more noise and more wrong
/// decisions through, and the 477.9 kS/s Comtech 8PSK at 9.6 dB slipped 135
/// times in 6 s at 0.008 (3.8 kHz), 31 at 0.005 and none at 0.002; six fast
/// QPSK TPC carriers (299–640 kS/s) decoded the same either way.
fn pll_bandwidth(symbol_rate: f64) -> f64 {
    let hz = (0.008 * symbol_rate).clamp(250.0, 1000.0);
    (hz / symbol_rate).min(0.04)
}

/// How the symbols written to a .bin file are numbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SymbolLabels {
    /// As the modulation's standard labels them (DVB-S2's mappings, the
    /// RCV-20x manual's for its QAMs): the hard index itself.
    #[default]
    Standard,
    /// By position: PSK points in order round the circle from the first
    /// one counter-clockwise of 0°, square QAM by column and row (x index
    /// then y index, each counted from the lowest).
    Natural,
    /// The Gray code of the natural number (per axis for QAM): neighbouring
    /// points differ in one bit, as most modems map them.
    Gray,
}

impl SymbolLabels {
    pub const ALL: [SymbolLabels; 3] = [
        SymbolLabels::Standard,
        SymbolLabels::Natural,
        SymbolLabels::Gray,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            SymbolLabels::Standard => "standard labels",
            SymbolLabels::Natural => "natural (by position)",
            SymbolLabels::Gray => "Gray code",
        }
    }

    /// Label → byte for `cst`. APSK rings and 8QAM have no single natural
    /// order: they keep their standard labels.
    fn table(&self, cst: &Constellation) -> Vec<u8> {
        let n = cst.points.len();
        let standard: Vec<u8> = (0..n).map(|l| l as u8).collect();
        if *self == SymbolLabels::Standard {
            return standard;
        }
        let gray = |k: usize| k ^ (k >> 1);
        let radii: Vec<f32> = cst.points.iter().map(|p| p.norm()).collect();
        let one_ring = radii
            .iter()
            .all(|r| (r - radii[0]).abs() < 0.02 * radii[0].max(1e-6));
        if one_ring {
            // PSK: rank by angle in [0, 2π).
            let angle = |l: usize| {
                let a = cst.points[l].arg();
                if a < -1e-4 {
                    a + std::f32::consts::TAU
                } else {
                    a.max(0.0)
                }
            };
            let mut order: Vec<usize> = (0..n).collect();
            order.sort_by(|&a, &b| angle(a).total_cmp(&angle(b)));
            let mut t = standard.clone();
            for (k, &l) in order.iter().enumerate() {
                t[l] = match self {
                    SymbolLabels::Gray => gray(k) as u8,
                    _ => k as u8,
                };
            }
            return t;
        }
        // Square QAM: distinct x and y levels, as many of each.
        let levels = |f: fn(&Iq) -> f32| {
            let mut v: Vec<f32> = cst.points.iter().map(f).collect();
            v.sort_by(f32::total_cmp);
            v.dedup_by(|a, b| (*a - *b).abs() < 1e-3);
            v
        };
        let (xs, ys) = (levels(|p| p.re), levels(|p| p.im));
        if xs.len() * ys.len() != n || xs.len() != ys.len() {
            return standard;
        }
        let bits = xs.len().trailing_zeros();
        let at = |v: &[f32], x: f32| v.iter().position(|&l| (l - x).abs() < 1e-3).unwrap_or(0);
        (0..n)
            .map(|l| {
                let (xi, yi) = (at(&xs, cst.points[l].re), at(&ys, cst.points[l].im));
                let (xi, yi) = match self {
                    SymbolLabels::Gray => (gray(xi), gray(yi)),
                    _ => (xi, yi),
                };
                ((xi << bits) | yi) as u8
            })
            .collect()
    }
}

/// Streaming generic demodulator.
pub struct PskDemod {
    mf: Fir,
    agc: Agc,
    sync: SymbolSync,
    pll: CarrierPll,
    cst: Constellation,
    rate: f64,
    filtered: Vec<Iq>,
    raw: Vec<Iq>,
    recent: VecDeque<Iq>,
    symbols: u64,
    mer: f32,
    coherence: f32,
    /// Mean symbol power, tracked: the AGC levels samples, not symbols, and
    /// QAM and APSK decisions need the symbols at the constellation's scale.
    power: Option<f32>,
    /// The outermost ring's points, and the radius beyond which a symbol is
    /// taken to be on it: until lock, a multi-ring constellation steers on
    /// those alone (the reduced-constellation algorithm) — full decisions
    /// on 16QAM can settle 26.6° off, where its inner and middle points
    /// trade places.
    outer: Vec<Iq>,
    outer_from: f32,
    /// Steering on full decisions (after the outer ring has locked).
    full: bool,
}

impl PskDemod {
    /// `rate` is the baseband sample rate, `rs` the symbol rate, `alpha` the
    /// roll-off; `offset_cycles` seeds the carrier loop (cycles per symbol).
    pub fn new(rate: f64, rs: f64, alpha: f64, modulation: Modulation, offset_cycles: f64) -> Self {
        let sps = rate / rs;
        PskDemod {
            mf: Fir::new(rrc_taps(sps, alpha, 12)),
            agc: Agc::new(1.0, 0.2),
            sync: SymbolSync::new(sps, 0.005, 0.02),
            pll: CarrierPll::new(pll_bandwidth(rs), offset_cycles),
            cst: Constellation::generic(modulation),
            rate,
            filtered: Vec::new(),
            raw: Vec::new(),
            recent: VecDeque::with_capacity(RECENT),
            symbols: 0,
            mer: 0.0,
            coherence: 0.0,
            power: None,
            outer: Vec::new(),
            outer_from: f32::INFINITY,
            full: false,
        }
        .with_outer_ring()
    }

    fn with_outer_ring(mut self) -> Self {
        let mut radii: Vec<f32> = self.cst.points.iter().map(|p| p.norm()).collect();
        radii.sort_by(|a, b| b.partial_cmp(a).unwrap());
        radii.dedup_by(|a, b| (*a - *b).abs() < 1e-3);
        if radii.len() > 1 {
            self.outer_from = 0.5 * (radii[0] + radii[1]);
            self.outer = self
                .cst
                .points
                .iter()
                .copied()
                .filter(|p| p.norm() > self.outer_from)
                .collect();
        }
        self
    }

    /// Each point's label (the hard index) to the byte written for it,
    /// under `labels`.
    pub fn label_table(&self, labels: SymbolLabels) -> Vec<u8> {
        labels.table(&self.cst)
    }

    pub fn modulation(&self) -> Modulation {
        self.cst.modulation
    }

    /// Feed baseband; append (hard index, locked soft symbol) per symbol.
    pub fn process(&mut self, baseband: &[Iq], out: &mut Vec<(u8, Iq)>) {
        self.filtered.clear();
        self.mf.process(baseband, &mut self.filtered);
        self.agc.process(&mut self.filtered);
        self.raw.clear();
        self.sync.process(&self.filtered, &mut self.raw);

        let start = out.len();
        if self.power.is_none() && !self.raw.is_empty() {
            let p = self.raw.iter().map(|s| s.norm_sqr()).sum::<f32>() / self.raw.len() as f32;
            self.power = Some(p.max(1e-12));
        }
        for &s in &self.raw {
            let p = self.power.get_or_insert(1.0);
            *p += 0.0005 * (s.norm_sqr() - *p);
            let s = s / p.sqrt().max(1e-6);
            let y = if self.outer.is_empty() || self.full {
                self.pll.step(s, &self.cst.points)
            } else if s.norm() > self.outer_from {
                self.pll.step(s, &self.outer)
            } else {
                self.pll.coast(s)
            };
            out.push((self.cst.nearest(y) as u8, y));
            if self.recent.len() == RECENT {
                self.recent.pop_front();
            }
            self.recent.push_back(y);
        }
        let new = &out[start..];
        self.symbols += new.len() as u64;
        if !new.is_empty() {
            let soft: Vec<Iq> = new.iter().map(|&(_, y)| y).collect();
            let (mer, coh) = decsat_dsp::quality(&soft, |_| &self.cst.points);
            // Smooth so the readout does not flicker: over ~SMOOTH_SYMBOLS
            // symbols whatever the block size, from the first block's own
            // values. (A fixed 0.3 a block from zero read 0.3 × the MER and
            // coherence when a short capture's first look came as one
            // block: 2.8 dB and "not locked" for a 9.3 dB carrier.)
            let first = self.symbols == new.len() as u64;
            let k = if first {
                1.0
            } else {
                1.0 - (-(new.len() as f32) / SMOOTH_SYMBOLS).exp()
            };
            self.mer += k * (mer - self.mer);
            self.coherence += k * (coh - self.coherence);
            // Full decisions only once truly locked — a 16QAM false lock
            // still scores ~0.45 — and back to the outer ring if it fades.
            // Not on the first block: its coherence includes the pull-in,
            // and full decisions from there false-lock.
            if !self.full && !first && self.coherence > 0.7 {
                self.full = true;
            } else if self.full && self.coherence < 0.4 {
                self.full = false;
            }
        }
    }

    pub fn recent(&self) -> Vec<Iq> {
        self.recent.iter().copied().collect()
    }

    pub fn symbols(&self) -> u64 {
        self.symbols
    }

    pub fn mer_db(&self) -> f32 {
        self.mer
    }

    pub fn coherence(&self) -> f32 {
        self.coherence
    }

    pub fn locked(&self) -> bool {
        // A multi-ring constellation's false locks score well above what
        // a single ring's lock needs: ask more of it.
        let need = if self.outer.is_empty() {
            decsat_dsp::LOCK_COHERENCE
        } else {
            0.6
        };
        self.coherence > need
    }

    /// Symbol rate as the timing loop tracks it.
    pub fn symbol_rate(&self) -> f64 {
        self.rate / self.sync.sps()
    }

    /// Residual carrier offset the loop is tracking, Hz.
    pub fn carrier_offset_hz(&self) -> f64 {
        self.pll.freq_cycles() * self.symbol_rate()
    }

    /// The input is about to arrive `delta_hz` lower (the down-converter
    /// moved onto the carrier by that much): take it out of the loop's
    /// frequency, so it sees no step.
    pub fn retune_hz(&mut self, delta_hz: f64) {
        let f = self.pll.freq_cycles() - delta_hz / self.symbol_rate();
        self.pll.set_freq_cycles(f);
    }
}

/// Live text search on a generic carrier: the hard decisions read under
/// each of the constellation's phase ambiguities (and their mirror images),
/// each a bit stream for a [`TextFinder`].
pub struct TextSearch {
    finder: TextFinder,
    cst: Constellation,
    /// Turn applied before deciding, and whether to mirror (conjugate) first.
    turns: Vec<(Iq, bool)>,
    bits: Vec<Vec<u8>>,
}

impl TextSearch {
    pub fn new(modulation: Modulation) -> Self {
        let cst = Constellation::generic(modulation);
        let (steps, mirror) = match modulation {
            Modulation::Bpsk | Modulation::Pi2Bpsk => (2, false),
            Modulation::Psk8 => (8, true),
            _ => (4, true),
        };
        let mut turns = Vec::new();
        let mut names = Vec::new();
        for m in [false, true].into_iter().take(if mirror { 2 } else { 1 }) {
            for k in 0..steps {
                let a = std::f32::consts::TAU * k as f32 / steps as f32;
                turns.push((Iq::new(a.cos(), a.sin()), m));
                names.push(format!(
                    "turned {}°{}",
                    360 * k / steps,
                    if m { ", mirrored" } else { "" }
                ));
            }
        }
        TextSearch {
            finder: TextFinder::new(names),
            bits: vec![Vec::new(); turns.len()],
            cst,
            turns,
        }
    }

    pub fn push(&mut self, syms: &[(u8, Iq)]) {
        let nb = self.cst.bits() as usize;
        for b in &mut self.bits {
            b.clear();
        }
        for &(_, y) in syms {
            for (k, &(turn, mirror)) in self.turns.iter().enumerate() {
                let z = if mirror { y.conj() } else { y } * turn;
                let label = self.cst.nearest(z);
                // The label's bits, most significant first.
                self.bits[k].extend((0..nb).rev().map(|i| ((label >> i) & 1) as u8));
            }
        }
        for (k, b) in self.bits.iter().enumerate() {
            self.finder.push(k, b);
        }
    }

    pub fn view(&self) -> TextView {
        self.finder.view()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_labels_number_by_position_and_gray() {
        let d = |m| PskDemod::new(1.0, 0.25, 0.35, m, 0.0);
        // QPSK: points at 45°, 135°, 225°, 315° → 0, 1, 2, 3; Gray 0, 1, 3, 2.
        for (labels, want) in [
            (SymbolLabels::Natural, [0u8, 1, 2, 3]),
            (SymbolLabels::Gray, [0, 1, 3, 2]),
        ] {
            let q = d(Modulation::Qpsk);
            let t = q.label_table(labels);
            let mut by_angle: Vec<(f32, u8)> = q
                .cst
                .points
                .iter()
                .enumerate()
                .map(|(l, p)| (p.arg().rem_euclid(std::f32::consts::TAU), t[l]))
                .collect();
            by_angle.sort_by(|a, b| a.0.total_cmp(&b.0));
            assert_eq!(by_angle.iter().map(|x| x.1).collect::<Vec<_>>(), want);
        }
        // 8PSK under Gray: neighbours round the circle differ in one bit.
        let e = d(Modulation::Psk8);
        let t = e.label_table(SymbolLabels::Gray);
        let mut by_angle: Vec<(f32, u8)> = e
            .cst
            .points
            .iter()
            .enumerate()
            .map(|(l, p)| (p.arg().rem_euclid(std::f32::consts::TAU), t[l]))
            .collect();
        by_angle.sort_by(|a, b| a.0.total_cmp(&b.0));
        for k in 0..8 {
            let (a, b) = (by_angle[k].1, by_angle[(k + 1) % 8].1);
            assert_eq!((a ^ b).count_ones(), 1, "{by_angle:?}");
        }
        // 16QAM under Gray: horizontal and vertical neighbours, one bit.
        let q16 = d(Modulation::Qam16);
        let t = q16.label_table(SymbolLabels::Gray);
        let pts = &q16.cst.points;
        for a in 0..16 {
            for b in 0..16 {
                let dd = (pts[a] - pts[b]).norm();
                let min = pts
                    .iter()
                    .flat_map(|p| pts.iter().map(move |q| (p - q).norm()))
                    .filter(|&x| x > 1e-6)
                    .fold(f32::MAX, f32::min);
                if (dd - min).abs() < 1e-3 {
                    assert_eq!((t[a] ^ t[b]).count_ones(), 1);
                }
            }
        }
        // Standard: the labels themselves.
        let s = d(Modulation::Psk8).label_table(SymbolLabels::Standard);
        assert_eq!(s, (0..8).collect::<Vec<u8>>());
    }
    use decsat_mod::Shaper;

    #[test]
    fn locks_16qam_without_false_lock() {
        // Full decisions on 16QAM settled 27° off (MER 11 dB); acquiring on
        // the corners alone does not.
        let cst = Constellation::qam16();
        let mut s = 0x1234u64;
        let mut next = move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            s.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        let syms: Vec<Iq> = (0..40_000)
            .map(|_| cst.map((next() >> 60) as usize))
            .collect();
        let mut sh = Shaper::new(4, 0.25, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        for (off, seed) in [(0.0, 0.0), (0.004, 0.0036)] {
            let xx: Vec<Iq> = x
                .iter()
                .enumerate()
                .map(|(n, &v)| {
                    let ph = std::f64::consts::TAU * off * n as f64 / 4.0 + 1.1;
                    v * Iq::new(ph.cos() as f32, ph.sin() as f32)
                })
                .collect();
            let mut d = PskDemod::new(4.0, 1.0, 0.25, Modulation::Qam16, seed);
            let mut out = Vec::new();
            for c in xx.chunks(9_999) {
                d.process(c, &mut out);
            }
            assert!(
                d.locked() && d.mer_db() > 25.0,
                "offset {off}: MER {}",
                d.mer_db()
            );
        }
    }

    #[test]
    fn one_big_first_block_reads_its_full_mer() {
        // A 2 s capture's first look reaches the demodulator as one block;
        // the readout must be that block's MER and lock, not 0.3 of them.
        let cst = Constellation::generic(Modulation::Qpsk);
        let mut s = 0x9e37u64;
        let mut next = move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            s.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        let syms: Vec<Iq> = (0..30_000)
            .map(|_| cst.map((next() >> 62) as usize))
            .collect();
        let mut sh = Shaper::new(4, 0.25, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        let mut d = PskDemod::new(4.0, 1.0, 0.25, Modulation::Qpsk, 0.0);
        let mut out = Vec::new();
        d.process(&x, &mut out);
        assert!(d.locked(), "coherence {}", d.coherence());
        assert!(d.mer_db() > 20.0, "MER {}", d.mer_db());
    }

    #[test]
    fn recovers_8psk_symbols_through_offset_and_noise() {
        let cst = Constellation::psk8();
        let mut s = 0x1234u64;
        let mut next = move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            s.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        let idx: Vec<usize> = (0..40_000).map(|_| (next() >> 61) as usize).collect();
        let syms: Vec<Iq> = idx.iter().map(|&i| cst.map(i)).collect();
        let mut sh = Shaper::new(4, 0.25, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        // 0.4 % of the symbol rate off, seeded 10 % wrong.
        let x: Vec<Iq> = x
            .iter()
            .enumerate()
            .map(|(n, &v)| {
                let ph = std::f64::consts::TAU * 0.004 * n as f64 / 4.0 + 1.1;
                v * Iq::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect();

        let mut d = PskDemod::new(4.0, 1.0, 0.25, Modulation::Psk8, 0.0036);
        let mut out = Vec::new();
        for c in x.chunks(9_999) {
            d.process(c, &mut out);
        }
        assert!(d.locked(), "coherence {}", d.coherence());
        assert!(d.mer_db() > 20.0, "MER {}", d.mer_db());
        assert!((d.carrier_offset_hz() - 0.004).abs() < 1e-4);

        // After pull-in the hard decisions equal the sent symbols up to one
        // fixed rotation of the 8PSK constellation (the phase ambiguity).
        let got: Vec<u8> = out.iter().map(|&(i, _)| i).collect();
        let tail = got.len() - 5_000;
        let rotate = |i: usize, k: usize| {
            // Rotate point i by k·45° and find its index.
            let a = cst.points[i]
                * Iq::new(
                    (k as f32 * std::f32::consts::FRAC_PI_4).cos(),
                    (k as f32 * std::f32::consts::FRAC_PI_4).sin(),
                );
            cst.nearest(a) as u8
        };
        // Align by searching the small lag the filters introduce, and the
        // rotation.
        let best = (0..30)
            .flat_map(|lag| (0..8).map(move |k| (lag, k)))
            .map(|(lag, k)| {
                let agree = (tail..got.len())
                    .filter(|&n| n >= lag && rotate(idx[n - lag], k) == got[n])
                    .count();
                agree as f64 / (got.len() - tail) as f64
            })
            .fold(0.0f64, f64::max);
        assert!(best > 0.99, "only {:.3} of symbols agree", best);
    }
}
