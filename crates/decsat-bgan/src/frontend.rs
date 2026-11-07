// made by CasualArclamp
//! Streaming front end: complex baseband at any rate in, one recovered
//! symbol per 6.6 us out, matched filtered and timing-corrected.
//!
//! ```text
//! IQ (fs_in) -> resampler -> 4 sps -> carrier NCO -> RRC matched filter
//!            -> AGC -> Gardner timing loop (cubic interpolator) -> symbols
//! ```
//!
//! DecBGAN did timing in batch, from the |y|^2 line over a whole capture; a
//! live receiver cannot wait for the capture, so the symbol clock is a
//! Gardner loop. It also absorbs clock error (the loop's integrator) that a
//! fixed-rate resampler cannot.

use crate::frame::C32;
use crate::spec;
use crate::tx::rrc;
use std::f64::consts::PI;

/// Samples per symbol after the resampler.
pub const SPS: usize = 4;
const MF_SPAN: usize = 16;
const SIGN: f64 = -1.0;

/// Windowed-sinc fractional resampler with a fixed output rate. Streams: feed
/// blocks, outputs come out as soon as the kernel has its input. The kernel
/// is scaled by the decimation ratio when downsampling so it also low-passes.
struct Resampler {
    /// Input samples advanced per output sample.
    ratio: f64,
    /// Kernel half-width in input samples.
    half: f64,
    /// Sinc kernel table at `TAB` points per input sample, 0..half.
    table: Vec<f32>,
    /// Input history: `hist[0]` is input sample `base`.
    hist: Vec<C32>,
    base: i64,
    /// Next output position, in input-sample units (absolute).
    pos: f64,
}

const TAB: usize = 128;
const LOBES: f64 = 8.0;

impl Resampler {
    fn new(fs_in: f64, fs_out: f64) -> Self {
        let ratio = fs_in / fs_out;
        // Cutoff at 0.98 of the narrower Nyquist, in cycles per input sample.
        let fc = 0.49 * (fs_out / fs_in).min(1.0);
        let half = LOBES / (2.0 * fc);
        let n = (half * TAB as f64).ceil() as usize + 2;
        let mut table = vec![0.0f32; n];
        for (i, t) in table.iter_mut().enumerate() {
            let x = i as f64 / TAB as f64;
            let s = if x == 0.0 { 2.0 * fc } else { (2.0 * PI * fc * x).sin() / (PI * x) };
            let w = if x >= half { 0.0 } else { 0.5 * (1.0 + (PI * x / half).cos()) }; // Hann
            *t = (s * w) as f32;
        }
        // start far enough in that the first kernel has its history
        Resampler { ratio, half, table, hist: Vec::new(), base: 0, pos: half.ceil() + 1.0 }
    }

    #[inline]
    fn k(&self, x: f64) -> f32 {
        let a = x.abs() * TAB as f64;
        let i = a as usize;
        if i + 1 >= self.table.len() {
            return 0.0;
        }
        let f = (a - i as f64) as f32;
        self.table[i] * (1.0 - f) + self.table[i + 1] * f
    }

    fn push(&mut self, x: &[C32], out: &mut Vec<C32>) {
        self.hist.extend_from_slice(x);
        let end = self.base + self.hist.len() as i64;
        let need = self.half.ceil() as i64 + 1;
        while self.pos + (need as f64) < end as f64 {
            let c = self.pos.floor() as i64;
            let lo = c - need + 1;
            let hi = c + need;
            let mut acc = C32::new(0.0, 0.0);
            let mut gain = 0.0f32;
            for j in lo..=hi {
                let w = self.k(j as f64 - self.pos);
                if w != 0.0 {
                    let idx = (j - self.base) as usize;
                    acc += self.hist[idx] * w;
                    gain += w;
                }
            }
            // normalise by the kernel sum so the passband gain is exactly 1
            out.push(if gain.abs() > 1e-6 { acc / gain } else { acc });
            self.pos += self.ratio;
        }
        // drop history no longer reachable
        let keep_from = (self.pos.floor() as i64 - need).max(self.base);
        let drop = (keep_from - self.base) as usize;
        if drop > 0 {
            self.hist.drain(..drop);
            self.base = keep_from;
        }
    }
}

/// Streaming RRC FIR (real taps, complex data) with a delay line.
struct Fir {
    taps: Vec<f32>,
    line: Vec<C32>,
}

impl Fir {
    fn new(taps: Vec<f32>) -> Self {
        let n = taps.len();
        Fir { taps, line: vec![C32::new(0.0, 0.0); n - 1] }
    }
    /// Filter `x` in place semantics: returns outputs aligned so the group
    /// delay (taps/2) is removed, i.e. output i corresponds to input i - (taps/2).
    fn process(&mut self, x: &[C32], out: &mut Vec<C32>) {
        let n = self.taps.len();
        let mut buf = Vec::with_capacity(self.line.len() + x.len());
        buf.extend_from_slice(&self.line);
        buf.extend_from_slice(x);
        // out[i] = sum_k taps[k] * buf[i + n-1 - k]
        for i in 0..x.len() {
            let w = &buf[i..i + n];
            let mut re = 0.0f32;
            let mut im = 0.0f32;
            for (t, s) in self.taps.iter().rev().zip(w) {
                re += t * s.re;
                im += t * s.im;
            }
            out.push(C32::new(re, im));
        }
        self.line.clear();
        self.line.extend_from_slice(&buf[buf.len() - (n - 1)..]);
    }
}

#[inline]
fn cubic(y: &[C32], t: f64) -> C32 {
    let i = t.floor() as usize;
    let mu = (t - i as f64) as f32;
    let (ym1, y0, y1, y2) = (y[i - 1], y[i], y[i + 1], y[i + 2]);
    let c1 = (y1 - ym1) * 0.5;
    let c2 = ym1 - y0 * 2.5 + y1 * 2.0 - y2 * 0.5;
    let c3 = (y2 - ym1) * 0.5 + (y0 - y1) * 1.5;
    ((c3 * mu + c2) * mu + c1) * mu + y0
}

pub struct FrontEnd {
    fs_in: f64,
    res: Resampler,
    mf: Fir,
    // carrier NCO (applied at 4 sps before the matched filter)
    nco_phase: f64,
    nco_w: f64,
    // AGC
    pwr: f32,
    // Gardner loop state; `t` indexes `y` (absolute sample index of y[0] is y0)
    y: Vec<C32>,
    y0: i64,
    t: f64,
    nu: f64,
    kp: f64,
    ki: f64,
    prev_sym: C32,
    have_prev: bool,
    scratch_a: Vec<C32>,
    scratch_b: Vec<C32>,
    /// Smoothed Gardner integrator = symbol-clock error, ppm.
    pub clock_ppm: f64,
}

impl FrontEnd {
    pub fn new(fs_in: f64) -> Self {
        let fs4 = spec::F80T45X8B.rs * SPS as f64;
        FrontEnd {
            fs_in,
            res: Resampler::new(fs_in, fs4),
            mf: Fir::new(rrc(spec::ROLLOFF, SPS, MF_SPAN)),
            nco_phase: 0.0,
            nco_w: 0.0,
            pwr: 1.0,
            y: Vec::new(),
            y0: 0,
            t: 3.0,
            nu: 0.0,
            kp: 0.004,
            ki: 0.00001,
            prev_sym: C32::new(0.0, 0.0),
            have_prev: false,
            scratch_a: Vec::new(),
            scratch_b: Vec::new(),
            clock_ppm: 0.0,
        }
    }

    pub fn input_rate(&self) -> f64 {
        self.fs_in
    }

    /// Carrier correction in Hz (positive removes a positive offset); the
    /// phase carries on from where it was.
    pub fn set_carrier_hz(&mut self, hz: f64) {
        self.nco_w = 2.0 * PI * hz / (spec::F80T45X8B.rs * SPS as f64);
    }
    pub fn carrier_hz(&self) -> f64 {
        self.nco_w * spec::F80T45X8B.rs * SPS as f64 / (2.0 * PI)
    }

    /// Feed baseband; appends recovered symbols (one per symbol period).
    pub fn push(&mut self, x: &[C32], out: &mut Vec<C32>) {
        self.scratch_a.clear();
        self.res.push(x, &mut self.scratch_a);
        // carrier derotation
        for v in self.scratch_a.iter_mut() {
            let (s, c) = (-self.nco_phase).sin_cos();
            *v *= C32::new(c as f32, s as f32);
            self.nco_phase += self.nco_w;
        }
        self.nco_phase %= 2.0 * PI;
        self.scratch_b.clear();
        self.mf.process(&self.scratch_a, &mut self.scratch_b);

        // AGC to unit power (one-pole on |y|^2, ~0.05 s)
        let a = 1.0 / (0.05 * spec::F80T45X8B.rs as f32 * SPS as f32);
        for v in self.scratch_b.iter_mut() {
            self.pwr += a * (v.norm_sqr() * SPS as f32 - self.pwr);
            let g = 1.0 / self.pwr.max(1e-12).sqrt();
            *v *= g;
        }
        self.y.extend_from_slice(&self.scratch_b);

        // Gardner: symbol at t, midpoint at t - step/2.
        let sps = SPS as f64;
        while self.t + 3.0 < self.y.len() as f64 {
            let cur = cubic(&self.y, self.t);
            let step = sps * (1.0 + self.nu);
            let mid = cubic(&self.y, self.t - step * 0.5);
            if self.have_prev {
                // e = Re{ conj(mid) * (cur - prev) }, normalised by power
                let e = (mid.conj() * (cur - self.prev_sym)).re as f64;
                let e = e.clamp(-4.0, 4.0);
                // Sign: e > 0 means we sampled early (the mid-sample sits on the
                // rising side), so move later. Checked by the lock tests.
                self.nu += SIGN * self.ki * e;
                self.nu = self.nu.clamp(-3e-4, 3e-4); // +-300 ppm
                self.t += step + SIGN * self.kp * e * sps;
            } else {
                self.t += step;
            }
            self.prev_sym = cur;
            self.have_prev = true;
            out.push(cur);
        }
        self.clock_ppm = -self.nu * 1e6;
        // drop consumed samples
        let keep = (self.t as i64 - 4).max(0) as usize;
        if keep > 0 && keep <= self.y.len() {
            self.y.drain(..keep);
            self.t -= keep as f64;
            self.y0 += keep as i64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::qam16;
    use crate::tx::{Impair, Rng, to_iq};

    /// M4 / M2^2 of 16-QAM is 1.32 when symbols are sampled at the right instants
    /// (2.0 for noise); a wrong timing phase pushes it towards 1.5+.
    fn m4(s: &[C32]) -> f64 {
        let m2: f64 = s.iter().map(|v| v.norm_sqr() as f64).sum::<f64>() / s.len() as f64;
        let m4: f64 = s.iter().map(|v| (v.norm_sqr() as f64).powi(2)).sum::<f64>() / s.len() as f64;
        m4 / (m2 * m2)
    }

    fn run(sps_gen: usize, imp: Impair, nsym: usize) -> (f64, f64) {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        let syms: Vec<C32> = (0..nsym).map(|_| qam16()[(rng.next_u32() & 15) as usize]).collect();
        let iq = to_iq(&syms, sps_gen, &imp, &mut rng);
        let mut fe = FrontEnd::new(spec::F80T45X8B.rs * sps_gen as f64);
        let mut out = Vec::new();
        for ch in iq.chunks(4096) {
            fe.push(ch, &mut out);
        }
        assert!(out.len() > nsym / 2, "only {} symbols", out.len());
        let tail = &out[out.len() / 2..];
        (m4(tail), fe.clock_ppm)
    }

    #[test]
    fn timing_loop_locks_clean() {
        for timing in [0.0, 0.3, 0.7] {
            let (m, _) = run(4, Impair { timing, ..Default::default() }, 60_000);
            assert!(m < 1.40, "timing {timing}: m4 = {m}");
        }
    }

    #[test]
    fn timing_loop_tracks_clock_error_and_resamples() {
        // 100 ppm fast, input at 8 samples per symbol (exercises the resampler)
        let (m, ppm) = run(8, Impair { clock_ppm: 100.0, timing: 0.4, ..Default::default() }, 200_000);
        assert!(m < 1.42, "m4 = {m}");
        assert!((ppm - 100.0).abs() < 40.0, "clock estimate {ppm}");
    }

    #[test]
    fn timing_loop_with_noise() {
        let (m, _) = run(4, Impair { esn0_db: Some(12.0), timing: 0.2, ..Default::default() }, 100_000);
        // noise raises M4 a little (calibrated: ~1.45 at 12 dB), nowhere near 2.0
        assert!(m < 1.55, "m4 = {m}");
    }
}
