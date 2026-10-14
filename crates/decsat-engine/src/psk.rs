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

/// Symbols each side of the one judged in the burst finder's power window.
const BURST_HALF: usize = 2;
/// Symbols coasted through each side of a burst found (its ramps).
const BURST_GUARD: usize = 2;
/// How far the window's mean power must stand above the carrier's own, in
/// standard deviations of that mean — and at least this ratio (2 dB).
const BURST_Z: f32 = 5.0;
const BURST_RATIO: f32 = 1.6;
/// The power statistics' smoothing, a symbol.
const BURST_RATE: f32 = 0.001;
/// Symbols in a row under a "burst" after which the level is taken to have
/// really changed (a fade ending, a stronger carrier): the new normal.
const BURST_LONGEST: usize = 512;
/// A symbol's interference is read from 3-symbol windows, less this many
/// standard deviations of such a window's own spread.
const WEIGHT_SPAN: usize = 3;
const WEIGHT_Z: f32 = 3.0;

/// Short bursts of interference landing on a carrier, wider than it and
/// stronger: each lifts the carrier's in-band power for a few symbols and
/// sends wild decisions into the carrier loop. Found live, and not on the
/// satellite: below 2.17 GHz the HackRF mixes up to a 2.34–2.65 GHz IF
/// (2650 − f/7 MHz), and tuned to 1732.9 MHz that IF sits on Bluetooth LE
/// advertising channel 37 (2402 MHz). Nearby devices' adverts (1 Msym/s,
/// ~0.4 ms, one shared access address, two of them every ~100 ms) leaked
/// in ~0.5 MHz from the centre, onto five 10.24 kBd QPSK carriers: their
/// power rose 3–20× for ~5 symbols at a time, and the two at Es/N0 ≈ 8 dB
/// slipped 90° every ~4 s (29 and 32 times in 120 s; with the bursts zeroed
/// in the IQ, once and not at all). Tuned 2.6 MHz higher the leak moved
/// off them: the weakest read 14.7 dB, and none slipped — retuning is the cure;
/// this is for when the leak and a carrier cannot be kept apart.
///
/// The finder compares each symbol's power, averaged over a short centred
/// window, with the carrier's own mean and spread (tracked on the symbols
/// not under bursts), before the AGC so a burst's effect on the gain does
/// not hide it. A strong carrier's power varies little, so a burst only a
/// little above it shows; a weak one's (or noise's) varies a lot, so only a
/// big burst counts there — false alarms stay rare at any SNR.
///
/// Symbols under a burst coast the carrier loop, and go on to the decoders
/// weighed by N / (N + I), their noise against noise plus interference:
/// soft decisions scale as 1/σ², and the burst raised σ². How I is read
/// matters as much as the formula. Each symbol's is the quietest 3-symbol
/// window holding it, less 3σ of such a window's own spread: a wider or
/// centred window smears a burst onto clean neighbours, and on a strong
/// carrier (N tiny) a clean symbol only 1σ loud already weighs 0.08. Those
/// near-erasures add up to runs longer than the ~6 symbols a K = 7 rate ½
/// code bridges, and the decoder did worse than trusting the hit symbols.
/// Error events in 100 s of the five carriers' idle data (Viterbi K = 7 ½;
/// Es/N0 ≈ 8.6, 8.2, 10, 16.5, 16.6 dB in turn): before 1455 / 1409
/// / 1214 / 43 / 17; all erased (weight 0) 1943 / 1858 / 1860 / 1698 / 1436;
/// N / (N + I) over the 5-symbol window 451 / 698 / 702 / 483 / 168; as here
/// 258 / 109 / 127 / 53 / 6.
#[derive(Default)]
struct Blanker {
    /// Mean and mean square of a symbol's power; `None` until the first block.
    stats: Option<(f32, f32)>,
    /// The previous block's last powers: the windows' look-back.
    tail: Vec<f32>,
    /// Guard symbols still to coast after a burst found near a block's end.
    guard: usize,
    /// Symbols in a row under a burst.
    run: usize,
    symbols: u64,
    bursts: u64,
}

impl Blanker {
    /// Mark the symbols of a block to coast through, given each one's
    /// power, and weigh each symbol (1 when not under a burst).
    fn mark(&mut self, power: &[f32], blank: &mut Vec<bool>, weight: &mut Vec<f32>) {
        blank.clear();
        blank.resize(power.len(), false);
        weight.clear();
        weight.resize(power.len(), 1.0);
        if power.is_empty() {
            return;
        }
        let (mut mean, mut square) = match self.stats {
            Some(s) => s,
            None => Self::first(power),
        };
        // The windows run over the previous block's tail then this block:
        // `ext[i + back]` is `power[i]`.
        let back = self.tail.len();
        let ext: Vec<f32> = self.tail.iter().chain(power).copied().collect();
        for i in 0..power.len() {
            let c = i + back;
            let lo = c.saturating_sub(BURST_HALF);
            let hi = (c + BURST_HALF).min(ext.len() - 1);
            let n = (hi - lo + 1) as f32;
            let avg = ext[lo..=hi].iter().sum::<f32>() / n;
            let sd = (square - mean * mean).max(0.0).sqrt();
            let over = (mean + BURST_Z * sd / n.sqrt()).max(BURST_RATIO * mean);
            if avg > over {
                // A new burst unless the last symbol was under one already.
                let was = if i == 0 { self.run > 0 } else { blank[i - 1] };
                if !was {
                    self.bursts += 1;
                }
                // Its leading ramp, in this block.
                for b in &mut blank[i.saturating_sub(BURST_GUARD)..i] {
                    *b = true;
                }
                blank[i] = true;
                self.guard = BURST_GUARD;
            } else if self.guard > 0 {
                blank[i] = true;
                self.guard -= 1;
            }
            if blank[i] {
                self.run += 1;
                if self.run > BURST_LONGEST {
                    // Not a burst: the level has changed. Take it, keeping
                    // the spread relative to the mean.
                    let rel = square / (mean * mean).max(1e-30);
                    mean = avg;
                    square = rel * avg * avg;
                    self.run = 0;
                    self.guard = 0;
                }
            } else {
                self.run = 0;
                mean += BURST_RATE * (power[i] - mean);
                square += BURST_RATE * (power[i] * power[i] - square);
            }
        }
        // The weights. For a constant-envelope signal S in noise N the power
        // has mean m = S + N and variance v = 2SN + N², so N = m − √(m² − v)
        // (noise alone: v = m², N = m). Multi-ring constellations add their
        // own spread and read N high, which only weighs bursts less.
        let v = (square - mean * mean).max(0.0);
        let noise = (mean - (mean * mean - v).max(0.0).sqrt()).max(1e-3 * mean);
        let spread = WEIGHT_Z * v.sqrt() / (WEIGHT_SPAN as f32).sqrt();
        for (i, w) in weight.iter_mut().enumerate() {
            if !blank[i] {
                continue;
            }
            let c = i + back;
            let quietest = (c.saturating_sub(WEIGHT_SPAN - 1)..=c)
                .filter(|&st| st + WEIGHT_SPAN <= ext.len())
                .map(|st| ext[st..st + WEIGHT_SPAN].iter().sum::<f32>() / WEIGHT_SPAN as f32)
                .fold(f32::INFINITY, f32::min);
            if quietest.is_finite() {
                let interference = (quietest - spread - mean).max(0.0);
                *w = noise / (noise + interference);
            }
        }
        self.symbols += blank.iter().filter(|&&b| b).count() as u64;
        self.stats = Some((mean, square));
        self.tail.clear();
        self.tail
            .extend_from_slice(&power[power.len().saturating_sub(BURST_HALF)..]);
    }

    /// Statistics from a first block, which may hold bursts: from the
    /// powers up to four times the median.
    fn first(power: &[f32]) -> (f32, f32) {
        let mut sorted = power.to_vec();
        sorted.sort_by(f32::total_cmp);
        let median = sorted[sorted.len() / 2];
        let kept: Vec<f32> = sorted.into_iter().filter(|&p| p <= 4.0 * median).collect();
        let n = kept.len().max(1) as f32;
        (
            kept.iter().sum::<f32>() / n,
            kept.iter().map(|p| p * p).sum::<f32>() / n,
        )
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
    /// Bursts of interference to coast through, and per symbol of the
    /// block: its power before the AGC, whether it is under a burst, and
    /// its weight.
    blanker: Blanker,
    power_in: Vec<f32>,
    blank: Vec<bool>,
    weight: Vec<f32>,
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
            blanker: Blanker::default(),
            power_in: Vec::new(),
            blank: Vec::new(),
            weight: Vec::new(),
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

        // Each symbol's power before the AGC (undoing this block's gain), for
        // the burst finder.
        let g2 = (self.agc.gain() * self.agc.gain()).max(1e-30);
        self.power_in.clear();
        self.power_in
            .extend(self.raw.iter().map(|s| s.norm_sqr() / g2));
        self.blanker
            .mark(&self.power_in, &mut self.blank, &mut self.weight);

        let start = out.len();
        if self.power.is_none() && !self.raw.is_empty() {
            let p = self.raw.iter().map(|s| s.norm_sqr()).sum::<f32>() / self.raw.len() as f32;
            self.power = Some(p.max(1e-12));
        }
        for ((&s, &blank), &w) in self.raw.iter().zip(&self.blank).zip(&self.weight) {
            let p = self.power.get_or_insert(1.0);
            if blank {
                // Under a burst: the loop coasts, the decoders get the symbol
                // weighed down, and the level and display leave it out.
                let y = self.pll.coast(s / p.sqrt().max(1e-6));
                out.push((self.cst.nearest(y) as u8, y * w));
                continue;
            }
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
            // The quality readouts from the symbols not blanked.
            let soft: Vec<Iq> = new
                .iter()
                .zip(&self.blank)
                .filter(|(_, b)| !**b)
                .map(|(&(_, y), _)| y)
                .collect();
            if soft.is_empty() {
                return;
            }
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
                1.0 - (-(soft.len() as f32) / SMOOTH_SYMBOLS).exp()
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

    /// Bursts of interference coasted through, and the symbols under them
    /// (handed on weighed down).
    pub fn blanked(&self) -> (u64, u64) {
        (self.blanker.bursts, self.blanker.symbols)
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

    /// QPSK at `esn0_db` under a slow phase wander (an LNB's), and if
    /// `bursts`, every 400 symbols a 5-symbol burst of wideband noise 40
    /// times the carrier's sample power (a Bluetooth packet leaking in).
    /// Returns the demodulator, the symbols sent and what it made of them.
    fn bursty_qpsk(esn0_db: f64, bursts: bool) -> (PskDemod, Vec<usize>, Vec<(u8, Iq)>) {
        let cst = Constellation::generic(Modulation::Qpsk);
        let mut s = 0x5eedu64;
        let mut next = move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            s.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        // Unit-variance complex Gaussian (Box–Muller).
        let mut gauss = move || {
            let u1 = ((next() >> 11) as f64 + 1.0) / (1u64 << 53) as f64;
            let u2 = (next() >> 11) as f64 / (1u64 << 53) as f64;
            let r = (-u1.ln()).sqrt();
            let a = std::f64::consts::TAU * u2;
            Iq::new((r * a.cos()) as f32, (r * a.sin()) as f32)
        };
        let mut t = 0x1234u64;
        let sent: Vec<usize> = (0..60_000)
            .map(|_| {
                t ^= t >> 12;
                t ^= t << 25;
                t ^= t >> 27;
                (t.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 62) as usize
            })
            .collect();
        let syms: Vec<Iq> = sent.iter().map(|&i| cst.map(i)).collect();
        let mut sh = Shaper::new(4, 0.35, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        let ps = x.iter().map(|v| v.norm_sqr()).sum::<f32>() / x.len() as f32;
        // Es/N0 = Ps·sps / σ².
        let sigma = (ps * 4.0 / 10f32.powf(esn0_db as f32 / 10.0)).sqrt();
        let mut phase = 0.3f64;
        for (k, v) in x.iter_mut().enumerate() {
            phase += 0.004 * gauss().re as f64;
            *v = *v * Iq::new(phase.cos() as f32, phase.sin() as f32) + gauss() * sigma;
            if bursts && k % 1600 >= 800 && k % 1600 < 820 {
                *v += gauss() * (40.0 * ps).sqrt();
            }
        }
        let mut d = PskDemod::new(4.0, 1.0, 0.35, Modulation::Qpsk, 0.0);
        let mut out = Vec::new();
        for c in x.chunks(4_096) {
            d.process(c, &mut out);
        }
        (d, sent, out)
    }

    /// The carrier loop's slips: the rotation mapping sent onto decided
    /// symbols, block by block after pull-in, and how often it changes.
    fn slips(sent: &[usize], out: &[(u8, Iq)]) -> usize {
        let cst = Constellation::generic(Modulation::Qpsk);
        let turn = |i: usize, k: usize| {
            let a = k as f32 * std::f32::consts::FRAC_PI_2;
            cst.nearest(cst.points[i] * Iq::new(a.cos(), a.sin()))
        };
        let agree = |from: usize, lag: usize, k: usize| {
            (from..from + 500)
                .filter(|&m| turn(sent[m - lag], k) == out[m].0 as usize)
                .count()
        };
        // The filters' lag, from a block after pull-in.
        let (lag, _) = (0..40)
            .flat_map(|lag| (0..4).map(move |k| (lag, k)))
            .max_by_key(|&(lag, k)| agree(5_000, lag, k))
            .unwrap();
        let mut last = None;
        let mut changes = 0;
        for from in (5_000..out.len() - 600).step_by(500) {
            let k = (0..4).max_by_key(|&k| agree(from, lag, k)).unwrap();
            if last.is_some_and(|l| l != k) {
                changes += 1;
            }
            last = Some(k);
        }
        changes
    }

    #[test]
    fn coasts_through_bursts_without_slipping() {
        // Without the burst finder this slips 14 times.
        let (d, sent, out) = bursty_qpsk(8.0, true);
        assert_eq!(slips(&sent, &out), 0);
        let (bursts, symbols) = d.blanked();
        // 150 bursts sent; each costs its 5 symbols and a few either side.
        assert!((140..=152).contains(&bursts), "{bursts} bursts");
        assert!(symbols < 150 * 14, "{symbols} symbols under bursts");
        assert!(d.mer_db() > 7.0, "MER {}", d.mer_db());
    }

    #[test]
    fn finds_no_bursts_on_a_clean_carrier() {
        // False alarms: none on a strong carrier, a handful in noise.
        for (esn0, most) in [(25.0, 0), (8.0, 5), (0.0, 30)] {
            let (d, _, _) = bursty_qpsk(esn0, false);
            let (bursts, _) = d.blanked();
            assert!(
                bursts <= most,
                "Es/N0 {esn0} dB: {bursts} bursts in 60 000 symbols"
            );
        }
    }
}
