//! PLHEADER frame synchronisation by differential correlation.
//!
//! Approach follows `gr-dvbs2rx`'s `lib/pl_frame_sync.cc` (GPL-3), which is
//! worth understanding because it is what makes acquisition work before the
//! carrier is recovered.
//!
//! The correlation is **differential**: on `d[n] = x[n] * conj(x[n+1])` rather
//! than on the symbols themselves. A constant frequency offset `f0` contributes
//! `exp(j2πf0 n) * conj(exp(j2πf0 (n+1))) = exp(-j2πf0)` to *every* differential,
//! so it rotates the whole correlation by one common phase and leaves its
//! magnitude alone. Frame sync therefore works at any frequency offset that
//! does not change appreciably across the 90-symbol PLHEADER — which is why it
//! must run *before* carrier recovery, not after.
//!
//! Two correlators are summed:
//!
//! * **SOF** — 25 taps, from the 25 differentials of the known 26-symbol SOF.
//! * **PLSC** — 32 taps, from the *scrambler* alone. This is the neat part. The
//!   interleaved Reed–Muller construction makes each consecutive pair of PLS
//!   codeword bits either equal (when the 7th bit is 0) or opposite (when it is
//!   1). Under pi/2-BPSK the differential of a pair depends only on whether the
//!   two transmitted bits are equal, so for a given scrambler it takes one of
//!   two values — the same for every possible PLS code, give or take a 180°
//!   flip from the 7th bit. So the taps can be derived from the scrambler
//!   without knowing the PLS code that is still to be decoded.
//!
//! The 180° ambiguity is resolved by taking whichever of `SOF + PLSC` and
//! `SOF - PLSC` is larger: the sum peaks when the 7th bit is 0, the difference
//! when it is 1.

use decsat_core::Iq;

use decsat_dsp::dot::fir_cr;

use crate::defs::{PLSC_LEN, PLSC_SCRAMBLER, SOF_BIG_ENDIAN, SOF_LEN};
use crate::pi2bpsk::map_bpsk;

/// Known SOF differentials: `SOF_LEN - 1`.
pub const SOF_CORR_LEN: usize = SOF_LEN - 1;
/// Known PLSC pairwise differentials: one per bit pair.
pub const PLSC_CORR_LEN: usize = PLSC_LEN / 2;
/// Differentials spanned by one PLHEADER.
const WINDOW: usize = SOF_LEN + PLSC_LEN - 1;
/// Peak magnitude of the unnormalised metric with noiseless unit-power input.
const TAPS: usize = SOF_CORR_LEN + PLSC_CORR_LEN;

/// The SOF's differential correlation for 26 symbols starting at an SOF:
/// `Σ x[n]·conj(x[n+1]) · conj(expected[n])` over its 25 differentials.
///
/// A carrier offset of `f0` cycles per symbol multiplies every differential by
/// `exp(-j2πf0)`, so `-arg(result) / 2π` **is** the offset — unambiguous
/// within ±0.5 cycles per symbol, independent of payload and MODCOD, and
/// summable coherently over many headers for precision.
pub fn sof_differential(sof: &[Iq]) -> Iq {
    assert!(sof.len() >= SOF_LEN, "need the 26 SOF symbols");
    let mut expected = [Iq::new(0.0, 0.0); SOF_LEN];
    map_bpsk(SOF_BIG_ENDIAN, &mut expected, SOF_LEN);
    let mut acc = Iq::new(0.0, 0.0);
    for n in 0..SOF_CORR_LEN {
        let d = sof[n] * sof[n + 1].conj();
        let e = expected[n] * expected[n + 1].conj();
        acc += d * e.conj();
    }
    acc
}

/// Differential correlator over the PLHEADER.
///
/// Feed symbols with [`Self::process`] (a block) or [`Self::push`] (one at a
/// time). The metric is normalised so that a noiseless, unit-power PLHEADER
/// gives 1.0, and it peaks on the PLHEADER's **last** symbol — so the next
/// symbol is the first payload symbol.
///
/// Every expected differential is ±j (consecutive pi/2-BPSK symbols are a
/// quarter turn apart), so each correlator is j times a sum of differentials
/// signed ±1, and j drops out of the metric's magnitude: both are real-tap
/// filters over the differential stream, run with `decsat_dsp`'s block FIR.
/// (The first version wrapped a ring index on each of its 57 complex taps per
/// symbol: 55 % of a core for a 6.75 MBd carrier, more than any other stage.)
pub struct PlHeaderCorrelator {
    /// Signs of the expected SOF differentials (`Im` of their conjugates).
    sof_sign: [f32; SOF_CORR_LEN],
    /// Signs of the expected PLSC pairwise differentials, at window offsets
    /// `SOF_LEN + 2i`, zero at the odd offsets between them.
    plsc_sign: [f32; WINDOW - SOF_LEN],
    /// Differentials: the last `WINDOW - 1` from before, then the new ones.
    diffs: Vec<Iq>,
    /// How many differentials have been seen (so we know when a window fills).
    seen: usize,
    /// Previous symbol, for forming the next differential.
    prev: Iq,
    /// Whether `prev` holds a real symbol yet.
    primed: bool,
    // Scratch: the two correlators' outputs, and one metric for `push`.
    sof: Vec<Iq>,
    plsc: Vec<Iq>,
    one: Vec<f32>,
}

impl Default for PlHeaderCorrelator {
    fn default() -> Self {
        Self::new()
    }
}

impl PlHeaderCorrelator {
    pub fn new() -> Self {
        // Expected SOF symbols; each differential `sof[n]·conj(sof[n+1])` is
        // ±j, and correlating with its conjugate means multiplying by ∓j:
        // the sign kept is that of the conjugate's imaginary part.
        let mut sof = [Iq::new(0.0, 0.0); SOF_LEN];
        map_bpsk(SOF_BIG_ENDIAN, &mut sof, SOF_LEN);
        let mut sof_sign = [0f32; SOF_CORR_LEN];
        for (n, s) in sof_sign.iter_mut().enumerate() {
            let tap = (sof[n] * sof[n + 1].conj()).conj();
            debug_assert!(tap.re.abs() < 1e-6 && (tap.im.abs() - 1.0).abs() < 1e-5);
            *s = tap.im.signum();
        }

        // PLSC taps from the scrambler. Under pi/2-BPSK the differential of a
        // (even, odd) symbol pair is -j when the two transmitted bits are equal
        // and +j when they differ; see the module comment. Conjugated, that is
        // +j and -j.
        let mut plsc_sign = [0f32; WINDOW - SOF_LEN];
        for i in 0..PLSC_CORR_LEN {
            let a = (PLSC_SCRAMBLER >> (63 - 2 * i)) & 1;
            let b = (PLSC_SCRAMBLER >> (63 - (2 * i + 1))) & 1;
            plsc_sign[2 * i] = if a == b { 1.0 } else { -1.0 };
        }

        PlHeaderCorrelator {
            sof_sign,
            plsc_sign,
            diffs: Vec::new(),
            seen: 0,
            prev: Iq::new(0.0, 0.0),
            primed: false,
            sof: Vec::new(),
            plsc: Vec::new(),
            one: Vec::with_capacity(1),
        }
    }

    /// Push a block of symbols: one metric each is appended to `out`, 0.0
    /// where [`Self::push`] would say `None` (too little history yet).
    pub fn process(&mut self, symbols: &[Iq], out: &mut Vec<f32>) {
        let mut rest = symbols;
        if !self.primed {
            let Some((&x, tail)) = rest.split_first() else {
                return;
            };
            self.prev = x;
            self.primed = true;
            out.push(0.0);
            rest = tail;
        }
        for &x in rest {
            self.diffs.push(self.prev * x.conj());
            self.prev = x;
        }
        self.seen += rest.len();

        // Windows that fit end on the newest differentials; the new ones
        // before them have no metric yet.
        let full = self.diffs.len().saturating_sub(WINDOW - 1);
        out.extend(std::iter::repeat_n(0.0, rest.len() - full));
        if full > 0 {
            self.sof.clear();
            self.plsc.clear();
            // Window start `w`: SOF over differentials w..w+25, PLSC over
            // w+26..w+89 (every other one counted).
            let n = self.diffs.len();
            fir_cr(
                &self.diffs[..n - (WINDOW - SOF_CORR_LEN)],
                &self.sof_sign,
                &mut self.sof,
            );
            fir_cr(&self.diffs[SOF_LEN..], &self.plsc_sign, &mut self.plsc);
            // Sum peaks when the PLS code's 7th bit is 0, difference when it
            // is 1.
            out.extend(
                self.sof
                    .iter()
                    .zip(&self.plsc)
                    .map(|(&s, &p)| (s + p).norm().max((s - p).norm()) / TAPS as f32),
            );
        }
        // Keep only the history the next window needs.
        let drop = self.diffs.len().saturating_sub(WINDOW - 1);
        self.diffs.drain(..drop);
    }

    /// Push one symbol and get the timing metric, or `None` until enough
    /// history has accumulated to fill the window.
    pub fn push(&mut self, x: Iq) -> Option<f32> {
        let mut one = std::mem::take(&mut self.one);
        one.clear();
        self.process(&[x], &mut one);
        let m = one[0];
        self.one = one;
        (self.seen >= WINDOW).then_some(m)
    }

    /// Forget all history, e.g. after a retune.
    pub fn reset(&mut self) {
        self.diffs.clear();
        self.seen = 0;
        self.primed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::defs::PLHEADER_LEN;
    use crate::plsc::{PlsInfo, PlscEncoder};

    /// A 90-symbol PLHEADER for the given fields.
    fn plheader(modcod: u8, short: bool, pilots: bool) -> Vec<Iq> {
        let mut out = vec![Iq::new(0.0, 0.0); PLHEADER_LEN];
        map_bpsk(SOF_BIG_ENDIAN, &mut out[..SOF_LEN], SOF_LEN);
        PlscEncoder::new().encode_fields(modcod, short, pilots, &mut out[SOF_LEN..]);
        out
    }

    /// Deterministic pseudo-random unit-power filler, standing in for payload.
    struct Filler(u64);
    impl Filler {
        fn next(&mut self) -> Iq {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            let x = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D);
            let th = std::f32::consts::TAU * ((x >> 40) as f32 / (1u32 << 24) as f32);
            Iq::new(th.cos(), th.sin())
        }
    }

    /// Feed a stream and return (index of best metric, best metric, runner-up
    /// metric outside a guard band around the peak).
    fn scan(stream: &[Iq]) -> (usize, f32, f32) {
        let mut c = PlHeaderCorrelator::new();
        let mut metrics = Vec::with_capacity(stream.len());
        for &x in stream {
            metrics.push(c.push(x).unwrap_or(0.0));
        }
        let (best_i, &best) = metrics
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap();
        let runner = metrics
            .iter()
            .enumerate()
            .filter(|(i, _)| i.abs_diff(best_i) > 4)
            .map(|(_, &m)| m)
            .fold(0.0f32, f32::max);
        (best_i, best, runner)
    }

    #[test]
    fn peaks_on_the_last_plheader_symbol() {
        let mut f = Filler(0xACE1);
        let offset = 137usize;
        let mut stream: Vec<Iq> = (0..offset).map(|_| f.next()).collect();
        stream.extend_from_slice(&plheader(4, false, false));
        stream.extend((0..400).map(|_| f.next()));

        let (idx, peak, runner) = scan(&stream);
        // The metric is returned on the push of the PLHEADER's last symbol.
        assert_eq!(idx, offset + PLHEADER_LEN - 1, "peak at {idx}");
        assert!(peak > 0.98, "peak {peak} too low");
        assert!(runner < 0.5, "runner-up {runner} too high");
    }

    #[test]
    fn peaks_for_both_values_of_the_seventh_plsc_bit() {
        // The sum/difference trick has to cover both cases: pilots off (bit 0)
        // and pilots on (bit 1).
        for pilots in [false, true] {
            let seventh = PlsInfo::from_fields(4, false, pilots).plsc & 1;
            assert_eq!(seventh, pilots as u8);

            let mut f = Filler(0xBEEF);
            let offset = 61usize;
            let mut stream: Vec<Iq> = (0..offset).map(|_| f.next()).collect();
            stream.extend_from_slice(&plheader(4, false, pilots));
            stream.extend((0..200).map(|_| f.next()));

            let (idx, peak, _) = scan(&stream);
            assert_eq!(idx, offset + PLHEADER_LEN - 1);
            assert!(peak > 0.98, "pilots={pilots}: peak {peak}");
        }
    }

    #[test]
    fn peak_survives_a_large_frequency_offset() {
        // The whole reason for differential correlation. 1 % of the symbol rate
        // is a huge offset -- far more than carrier recovery would ever see.
        for &frac in &[0.0f32, 0.001, 0.005, 0.01] {
            let mut f = Filler(0xD00D);
            let offset = 95usize;
            let mut stream: Vec<Iq> = (0..offset).map(|_| f.next()).collect();
            stream.extend_from_slice(&plheader(12, false, true));
            stream.extend((0..200).map(|_| f.next()));

            // Apply the offset across the whole stream.
            let rotated: Vec<Iq> = stream
                .iter()
                .enumerate()
                .map(|(n, s)| {
                    let th = std::f32::consts::TAU * frac * n as f32;
                    s * Iq::new(th.cos(), th.sin())
                })
                .collect();

            let (idx, peak, _) = scan(&rotated);
            assert_eq!(idx, offset + PLHEADER_LEN - 1, "frac {frac}");
            assert!(peak > 0.95, "frac {frac}: peak {peak}");
        }
    }

    #[test]
    fn peak_is_independent_of_the_modcod() {
        // The PLSC taps come from the scrambler, so every PLS code must peak
        // equally well.
        for modcod in 0..32u8 {
            let mut f = Filler(0x5EED + modcod as u64);
            let offset = 40usize;
            let mut stream: Vec<Iq> = (0..offset).map(|_| f.next()).collect();
            stream.extend_from_slice(&plheader(modcod, modcod % 2 == 0, modcod % 3 == 0));
            stream.extend((0..150).map(|_| f.next()));

            let (idx, peak, _) = scan(&stream);
            assert_eq!(idx, offset + PLHEADER_LEN - 1, "MODCOD {modcod}");
            assert!(peak > 0.98, "MODCOD {modcod}: peak {peak}");
        }
    }

    #[test]
    fn peak_survives_noise() {
        let mut f = Filler(0xFACE);
        let offset = 70usize;
        let mut stream: Vec<Iq> = (0..offset).map(|_| f.next()).collect();
        stream.extend_from_slice(&plheader(7, false, false));
        stream.extend((0..200).map(|_| f.next()));

        let mut n = Filler(0x1357);
        let noisy: Vec<Iq> = stream.iter().map(|s| s + n.next() * 0.5).collect();

        let (idx, peak, runner) = scan(&noisy);
        assert_eq!(idx, offset + PLHEADER_LEN - 1);
        assert!(peak > 0.7, "peak {peak}");
        assert!(peak > 1.5 * runner, "peak {peak} vs runner-up {runner}");
    }

    #[test]
    fn sof_differential_measures_the_frequency_offset() {
        for &f in &[0.0f64, 0.004, -0.012, 0.21] {
            let h = plheader(4, false, false);
            let rotated: Vec<Iq> = h
                .iter()
                .enumerate()
                .map(|(n, s)| {
                    let th = std::f64::consts::TAU * f * n as f64 + 0.9;
                    s * Iq::new(th.cos() as f32, th.sin() as f32)
                })
                .collect();
            let est = -(sof_differential(&rotated).arg() as f64) / std::f64::consts::TAU;
            assert!((est - f).abs() < 1e-5, "offset {f}: measured {est}");
        }
    }

    #[test]
    fn no_peak_in_pure_filler() {
        let mut f = Filler(0x2468);
        let stream: Vec<Iq> = (0..1000).map(|_| f.next()).collect();
        let (_, peak, _) = scan(&stream);
        assert!(peak < 0.5, "false peak {peak} with no PLHEADER present");
    }

    #[test]
    fn finds_consecutive_frames_at_the_right_spacing() {
        // Two dummy frames back to back: 37 slots of 90 symbols each.
        let info = PlsInfo::from_fields(0, false, false);
        let frame_len = info.plframe_len as usize;
        let mut f = Filler(0x99);

        let mut stream: Vec<Iq> = Vec::new();
        for _ in 0..2 {
            stream.extend_from_slice(&plheader(0, false, false));
            stream.extend((0..frame_len - PLHEADER_LEN).map(|_| f.next()));
        }

        let mut c = PlHeaderCorrelator::new();
        let peaks: Vec<usize> = stream
            .iter()
            .enumerate()
            .filter_map(|(i, &x)| c.push(x).filter(|&m| m > 0.9).map(|_| i))
            .collect();

        assert_eq!(peaks.len(), 2, "expected two peaks, got {peaks:?}");
        assert_eq!(peaks[1] - peaks[0], frame_len);
    }
}
