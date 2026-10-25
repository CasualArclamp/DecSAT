// made by CasualArclamp
//! Digital down-converter: the front of every VFO.
//!
//! Takes the wideband stream, shifts the VFO's centre frequency to DC with a
//! numerically controlled oscillator, low-pass filters to the VFO's bandwidth
//! and decimates by an integer factor. The output rate is chosen to leave at
//! least ~2.5 samples per symbol for timing recovery; the exact samples per
//! symbol is then fractional, which the symbol synchroniser's interpolator
//! handles, so no fractional resampler is needed here.
//!
//! The filters are Blackman-windowed sincs. The stopband starts just past the
//! VFO's edge (half as far again as its half-width) — so a VFO delivers what
//! its edges show on the waterfall, as in SDR++ — and never later than
//! `out_rate − bw/2`, where aliasing would begin. An earlier version used that
//! aliasing limit alone, which let a strong neighbour up to ~out_rate/2 through
//! and wrecked a symbol-rate estimate; see the regression test.
//!
//! One stage when the decimation is small. Otherwise two: a short filter with a
//! generous transition band does most of the decimation, and the sharp filter
//! that defines the VFO's edges runs at the lower rate. The split is chosen to
//! minimise multiply-adds per input sample; for a narrow VFO on a wide span it
//! costs a third to a quarter of a single sharp stage.

use decsat_core::Iq;

use crate::dot::{dot_cr, fir_cr};

/// Output rate as a multiple of the VFO bandwidth (before rounding the
/// decimation factor down, which only raises it).
const OVERSAMPLE: f64 = 2.5;
/// Blackman window: transition width ≈ 5.5 / N (normalised), ~74 dB stopband.
const BLACKMAN_K: f64 = 5.5;
/// Upper bound on filter length, so a very wide VFO near the full span cannot
/// ask for an absurd filter. Past this the filter just gets softer.
const MAX_TAPS: usize = 2047;
/// Renormalise the NCO phasor this often to stop magnitude drift.
const RENORM_EVERY: u32 = 1024;

/// Design a Blackman-windowed-sinc low-pass filter.
///
/// `cutoff` is the −6 dB point as a fraction of the sample rate (0..0.5).
/// Unity DC gain. Odd length, linear phase.
pub fn lowpass(cutoff: f64, ntaps: usize) -> Vec<f32> {
    let ntaps = ntaps | 1; // odd, so there is a centre tap
    let m = (ntaps - 1) as f64;
    let mut taps: Vec<f64> = (0..ntaps)
        .map(|k| {
            let n = k as f64 - m / 2.0;
            let sinc = if n == 0.0 {
                2.0 * cutoff
            } else {
                (2.0 * std::f64::consts::PI * cutoff * n).sin() / (std::f64::consts::PI * n)
            };
            let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * k as f64 / m).cos()
                + 0.08 * (4.0 * std::f64::consts::PI * k as f64 / m).cos();
            sinc * w
        })
        .collect();
    let sum: f64 = taps.iter().sum();
    for t in &mut taps {
        *t /= sum;
    }
    taps.into_iter().map(|t| t as f32).collect()
}

/// Filter length for a transition from `pass` to `stop` Hz at `rate`.
fn ntaps(rate: f64, pass: f64, stop: f64) -> usize {
    ((BLACKMAN_K * rate / (stop - pass)).ceil() as usize).clamp(15, MAX_TAPS)
}

/// A low-pass for a transition from `pass` to `stop` Hz at `rate`, cut off
/// midway between them.
fn design(rate: f64, pass: f64, stop: f64) -> Vec<f32> {
    lowpass(
        ((pass + stop) / 2.0 / rate).min(0.49),
        ntaps(rate, pass, stop),
    )
}

/// One decimating FIR stage.
struct Stage {
    /// Taps reversed, so the dot product runs over the buffer forwards.
    taps_rev: Vec<f32>,
    decim: usize,
    /// The last `taps - 1` inputs from previous calls, then the new ones.
    buf: Vec<Iq>,
    /// Index in `buf` of the input at which the next output is due.
    next_out: usize,
}

impl Stage {
    fn new(mut taps: Vec<f32>, decim: usize) -> Self {
        taps.reverse();
        let l = taps.len();
        Stage {
            taps_rev: taps,
            decim,
            buf: vec![Iq::new(0.0, 0.0); l - 1],
            next_out: l - 1,
        }
    }

    fn process(&mut self, input: &[Iq], out: &mut Vec<Iq>) {
        let l = self.taps_rev.len();
        self.buf.extend_from_slice(input);
        out.reserve(self.buf.len().saturating_sub(self.next_out) / self.decim + 1);
        if self.decim == 1 && self.next_out < self.buf.len() {
            // A VFO too wide to decimate (over 40 % of the span) filters
            // every sample: the block kernel, several outputs at a time.
            fir_cr(&self.buf[self.next_out + 1 - l..], &self.taps_rev, out);
            self.next_out = self.buf.len();
        }
        while self.next_out < self.buf.len() {
            let window = &self.buf[self.next_out + 1 - l..=self.next_out];
            out.push(dot_cr(window, &self.taps_rev));
            self.next_out += self.decim;
        }
        // Keep only the history the next call needs.
        let drop = self.buf.len() - (l - 1);
        self.buf.drain(..drop);
        self.next_out -= drop;
    }

    fn reset(&mut self) {
        let l = self.taps_rev.len();
        self.buf.clear();
        self.buf.resize(l - 1, Iq::new(0.0, 0.0));
        self.next_out = l - 1;
    }
}

/// Numerically controlled oscillator mixing + one or two decimating FIRs.
pub struct Ddc {
    in_rate: f64,
    decim: usize,
    bandwidth: f64,
    offset_hz: f64,
    /// Current oscillator phasor and its per-sample step (unit magnitude).
    /// f64 so that hours of running do not accumulate phase error.
    phasor: (f64, f64),
    step: (f64, f64),
    since_renorm: u32,
    stages: Vec<Stage>,
    /// Scratch: mixed input, and the output of the first of two stages.
    mixed: Vec<Iq>,
    mid: Vec<Iq>,
}

impl Ddc {
    /// A DDC for a VFO centred `offset_hz` from the wideband centre, `bandwidth`
    /// wide, fed at `in_rate`.
    ///
    /// # Panics
    /// If `bandwidth` is not positive or exceeds `in_rate`.
    pub fn new(in_rate: f64, offset_hz: f64, bandwidth: f64) -> Self {
        assert!(
            bandwidth > 0.0 && bandwidth <= in_rate,
            "VFO bandwidth must be positive and within the sample rate"
        );
        let max_decim = ((in_rate / (OVERSAMPLE * bandwidth)).floor() as usize).max(1);
        let pass = bandwidth / 2.0;

        // Passband to the VFO edge; stopband half as far again, and never past
        // the point where aliasing would begin at the final output rate.
        let final_stop = |out_rate: f64, decim: usize| {
            let alias = if decim > 1 {
                out_rate - pass
            } else {
                in_rate / 2.0
            };
            (pass * 1.5).min(alias).max(pass * 1.02)
        };

        // Single stage, as the baseline.
        let stop = final_stop(in_rate / max_decim as f64, max_decim);
        let mut best_cost = ntaps(in_rate, pass, stop) as f64 / max_decim as f64;
        let mut best: Option<(usize, usize)> = None;

        // Two stages, over the exact factorisations of the decimation.
        for d1 in 2..=max_decim / 2 {
            if !max_decim.is_multiple_of(d1) {
                continue;
            }
            let d2 = max_decim / d1;
            let r1 = in_rate / d1 as f64;
            let stop2 = final_stop(r1 / d2 as f64, d2);
            // Stage 1 need only be flat to the VFO edge, and must keep its
            // aliases out of everything stage 2 does not itself remove.
            let stop1 = r1 - stop2;
            if stop1 - pass < 0.1 * bandwidth {
                continue;
            }
            let cost = ntaps(in_rate, pass, stop1) as f64 / d1 as f64
                + ntaps(r1, pass, stop2) as f64 / max_decim as f64;
            if cost < best_cost {
                best_cost = cost;
                best = Some((d1, d2));
            }
        }

        let stages = match best {
            None => vec![Stage::new(design(in_rate, pass, stop), max_decim)],
            Some((d1, d2)) => {
                let r1 = in_rate / d1 as f64;
                let stop2 = final_stop(r1 / d2 as f64, d2);
                vec![
                    Stage::new(design(in_rate, pass, r1 - stop2), d1),
                    Stage::new(design(r1, pass, stop2), d2),
                ]
            }
        };

        let mut ddc = Ddc {
            in_rate,
            decim: max_decim,
            bandwidth,
            offset_hz: 0.0,
            phasor: (1.0, 0.0),
            step: (1.0, 0.0),
            since_renorm: 0,
            stages,
            mixed: Vec::new(),
            mid: Vec::new(),
        };
        ddc.set_offset(offset_hz);
        ddc
    }

    /// Retune without disturbing the filter state or the oscillator phase, so a
    /// VFO can be dragged across the waterfall smoothly.
    pub fn set_offset(&mut self, offset_hz: f64) {
        self.offset_hz = offset_hz;
        // Mixing *down* by the offset: multiply by exp(-j 2π f t).
        let w = -2.0 * std::f64::consts::PI * offset_hz / self.in_rate;
        self.step = (w.cos(), w.sin());
    }

    pub fn offset_hz(&self) -> f64 {
        self.offset_hz
    }

    pub fn bandwidth(&self) -> f64 {
        self.bandwidth
    }

    pub fn decimation(&self) -> usize {
        self.decim
    }

    pub fn out_rate(&self) -> f64 {
        self.in_rate / self.decim as f64
    }

    /// Filter stages as (taps, decimation), for diagnostics.
    pub fn stage_layout(&self) -> Vec<(usize, usize)> {
        self.stages
            .iter()
            .map(|s| (s.taps_rev.len(), s.decim))
            .collect()
    }

    /// Multiply-adds per input sample.
    pub fn cost(&self) -> f64 {
        let mut d = 1usize;
        let mut c = 0.0;
        for s in &self.stages {
            d *= s.decim;
            c += s.taps_rev.len() as f64 / d as f64;
        }
        c
    }

    /// The filters' combined span in input samples: skipping
    /// `taps() / decimation()` outputs clears the start-up transient.
    pub fn taps(&self) -> usize {
        let mut d = 1usize;
        let mut span = 0usize;
        for s in &self.stages {
            span += s.taps_rev.len() * d;
            d *= s.decim;
        }
        span
    }

    /// Mix, filter and decimate `input`, appending to `out`.
    pub fn process(&mut self, input: &[Iq], out: &mut Vec<Iq>) {
        // The mixer is on the VFO worker's hot path, executed at the full
        // *wideband* rate (for example 30.72 MS/s), before FIR decimation.
        // Keep the f64 oscillator accumulation for long-term frequency
        // accuracy, but carry out the per-sample IQ multiplications in f32.
        // The input and downstream FIR are both f32 already.
        //
        // A VFO at DC needs no mixer at all. In that case the FIR can read
        // the input slice directly, avoiding a full-band temporary copy.
        let mixed: &[Iq] = if self.offset_hz == 0.0 {
            input
        } else {
            self.mixed.clear();
            self.mixed.reserve(input.len());
            let (sr, si) = self.step;
            let mut pr = self.phasor.0;
            let mut pi = self.phasor.1;
            let mut since = self.since_renorm;
            for &x in input {
                let re = pr as f32;
                let im = pi as f32;
                self.mixed.push(Iq::new(
                    x.re * re - x.im * im,
                    x.re * im + x.im * re,
                ));
                let next_re = pr * sr - pi * si;
                pi = pr * si + pi * sr;
                pr = next_re;
                since += 1;
                if since == RENORM_EVERY {
                    let mag = (pr * pr + pi * pi).sqrt();
                    pr /= mag;
                    pi /= mag;
                    since = 0;
                }
            }
            self.phasor = (pr, pi);
            self.since_renorm = since;
            &self.mixed
        };

        match self.stages.as_mut_slice() {
            [only] => only.process(mixed, out),
            [first, second] => {
                self.mid.clear();
                first.process(mixed, &mut self.mid);
                second.process(&self.mid, out);
            }
            _ => unreachable!("a DDC has one or two stages"),
        }
    }

    /// Clear filter history (e.g. after the source restarts).
    pub fn reset(&mut self) {
        for s in &mut self.stages {
            s.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    fn tone(rate: f64, freq: f64, n: usize) -> Vec<Iq> {
        (0..n)
            .map(|k| {
                let ph = TAU * freq * k as f64 / rate;
                Iq::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect()
    }

    fn power_db(x: &[Iq]) -> f64 {
        let p: f64 = x.iter().map(|s| s.norm_sqr() as f64).sum::<f64>() / x.len() as f64;
        10.0 * p.max(1e-30).log10()
    }

    /// Output of a DDC fed `x`, with the start-up transient skipped.
    fn settled(d: &mut Ddc, x: &[Iq]) -> Vec<Iq> {
        let mut out = Vec::new();
        d.process(x, &mut out);
        out[d.taps() / d.decimation() + 10..].to_vec()
    }

    #[test]
    fn lowpass_has_unity_dc_gain() {
        let t = lowpass(0.1, 101);
        let s: f32 = t.iter().sum();
        assert!((s - 1.0).abs() < 1e-5);
        assert_eq!(t.len() % 2, 1);
    }

    #[test]
    fn decimation_targets_the_oversample_factor() {
        let d = Ddc::new(20e6, 1e6, 500e3);
        // 20e6 / (2.5 * 500e3) = 16.
        assert_eq!(d.decimation(), 16);
        assert!((d.out_rate() - 1.25e6).abs() < 1.0);
    }

    #[test]
    fn tone_at_the_vfo_centre_comes_out_at_dc() {
        let rate = 8e6;
        let off = 1.234e6;
        let mut d = Ddc::new(rate, off, 400e3);
        let s = settled(&mut d, &tone(rate, off, 200_000));
        // A constant phasor: tiny sample-to-sample phase change, unity gain.
        let max_step = s
            .windows(2)
            .map(|w| (w[1] * w[0].conj()).arg().abs())
            .fold(0.0f32, f32::max);
        assert!(max_step < 1e-3, "residual rotation {max_step} rad/sample");
        assert!(power_db(&s).abs() < 0.1, "gain {} dB", power_db(&s));
    }

    #[test]
    fn out_of_band_tone_is_rejected() {
        // 1.5 MHz away: far outside the 200 kHz half-bandwidth, and it would
        // alias into the passband if the filter let it through.
        let rate = 8e6;
        let mut d = Ddc::new(rate, 0.0, 400e3);
        let s = settled(&mut d, &tone(rate, 1.5e6, 400_000));
        assert!(power_db(&s) < -60.0, "leak {} dB", power_db(&s));
    }

    #[test]
    fn neighbour_just_outside_the_vfo_is_rejected() {
        // Regression: a 1.5 MHz VFO at 8 MS/s decimates by 2 to 4 MS/s, so a
        // neighbour 1.7 MHz from the VFO centre is inside the output's Nyquist
        // band. The first filter design only stopped at the aliasing limit
        // (~2 MHz) and let it through at full strength.
        let rate = 8e6;
        let mut d = Ddc::new(rate, -2.5e6, 1.516e6);
        assert_eq!(d.decimation(), 2);
        let s = settled(&mut d, &tone(rate, -0.8e6, 400_000));
        assert!(
            power_db(&s) < -60.0,
            "neighbour leaks at {} dB",
            power_db(&s)
        );
    }

    #[test]
    fn vfo_passband_is_flat_to_its_edge() {
        let rate = 8e6;
        let mut d = Ddc::new(rate, 0.0, 1.0e6);
        let s = settled(&mut d, &tone(rate, 0.48e6, 400_000));
        assert!(power_db(&s).abs() < 0.5, "edge gain {} dB", power_db(&s));
    }

    #[test]
    fn narrow_vfo_on_a_wide_span_uses_two_cheap_stages() {
        // The case that loaded the CPU: 380 kHz at 20 MS/s.
        let decim = (20e6 / (OVERSAMPLE * 380e3)).floor();
        let pass = 190e3;
        let stop = (pass * 1.5f64).min(20e6 / decim - pass);
        let one_stage = ntaps(20e6, pass, stop) as f64 / decim;

        let d = Ddc::new(20e6, 0.0, 380e3);
        assert_eq!(d.stage_layout().len(), 2, "{:?}", d.stage_layout());
        assert!(
            d.cost() < one_stage / 2.5,
            "cost {:.1} vs single-stage {:.1} MAC/sample ({:?})",
            d.cost(),
            one_stage,
            d.stage_layout()
        );
    }

    #[test]
    fn two_stages_still_reject_neighbours_and_aliases() {
        let rate = 20e6;
        for f in [0.6e6, 1.3e6, 5.0e6, -3.7e6] {
            let mut d = Ddc::new(rate, 0.0, 380e3);
            assert_eq!(d.stage_layout().len(), 2);
            let s = settled(&mut d, &tone(rate, f, 600_000));
            assert!(
                power_db(&s) < -60.0,
                "tone at {f} leaks at {} dB",
                power_db(&s)
            );
        }
        let mut d = Ddc::new(rate, 0.0, 380e3);
        let s = settled(&mut d, &tone(rate, 150e3, 600_000));
        assert!(power_db(&s).abs() < 0.5, "passband {} dB", power_db(&s));
    }

    #[test]
    fn block_size_does_not_change_the_output() {
        // A VFO is fed whatever block size the source produces, so the result
        // must not depend on how the stream was chopped up — on both paths.
        for (rate, bw) in [(4e6, 200e3), (20e6, 380e3)] {
            let input = tone(rate, 133e3, 120_000);
            let mut a = Ddc::new(rate, 100e3, bw);
            let mut one = Vec::new();
            a.process(&input, &mut one);

            let mut b = Ddc::new(rate, 100e3, bw);
            let mut many = Vec::new();
            for chunk in input.chunks(777) {
                b.process(chunk, &mut many);
            }
            assert_eq!(one.len(), many.len());
            for (x, y) in one.iter().zip(&many) {
                assert!((x - y).norm() < 1e-4, "{x} vs {y}");
            }
        }
    }

    #[test]
    fn retuning_moves_the_passband() {
        let rate = 4e6;
        let mut d = Ddc::new(rate, 0.0, 200e3);
        d.set_offset(1e6);
        let s = settled(&mut d, &tone(rate, 1e6, 100_000));
        assert!(power_db(&s).abs() < 0.2);
        assert_eq!(d.offset_hz(), 1e6);
    }

    /// `cargo test --release -p decsat-dsp ddc_throughput -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark"]
    fn ddc_throughput() {
        let rate = 20e6;
        let input = tone(rate, 1.0e6, 2_000_000);
        for bw in [5e6, 1.5e6, 500e3, 100e3] {
            let mut d = Ddc::new(rate, 1.0e6, bw);
            let mut out = Vec::new();
            let t0 = std::time::Instant::now();
            for _ in 0..5 {
                out.clear();
                d.process(&input, &mut out);
            }
            let secs = t0.elapsed().as_secs_f64();
            let real = 5.0 * input.len() as f64 / rate;
            eprintln!(
                "VFO {:>7.0} kHz at 20 MS/s: {:>5.1} % of a core ({:?}, {:.1} MAC/sample)",
                bw / 1e3,
                100.0 * secs / real,
                d.stage_layout(),
                d.cost()
            );
        }
    }

    #[test]
    fn output_count_matches_decimation() {
        for (rate, bw) in [(10e6, 1e6), (20e6, 380e3)] {
            let mut d = Ddc::new(rate, 0.0, bw);
            let n = 40_000 * d.decimation();
            let mut out = Vec::new();
            d.process(&vec![Iq::new(1.0, 0.0); n], &mut out);
            assert_eq!(out.len(), n / d.decimation(), "{:?}", d.stage_layout());
        }
    }
}
