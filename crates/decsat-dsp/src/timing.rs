// made by CasualArclamp
//! Symbol timing recovery: Gardner timing-error detector driving a cubic
//! (Farrow) interpolator.
//!
//! The DDC leaves a fractional number of samples per symbol (≥ ~2.5), so the
//! symbol instants never land on samples; a cubic Lagrange interpolator
//! evaluates the signal between samples at whatever fraction the loop asks for.
//!
//! Gardner's detector needs two samples per symbol — the strobe and the point
//! half a symbol earlier — and is non-data-aided and insensitive to carrier
//! phase, so it can run before carrier recovery. That ordering matters: frame
//! sync needs symbol-spaced samples, and carrier recovery needs frame sync.
//!
//! Known limitation: Gardner's S-curve slope scales with the excess bandwidth,
//! so it loses gain at the tight S2X roll-offs (α = 0.05–0.10). It is the right
//! detector for M1b and the common 0.20–0.35 roll-offs; a pilot-aided refinement
//! for tight roll-offs is planned with the ACM polish in M5.

use decsat_core::Iq;

/// Cubic Lagrange interpolation between `x0` and `x1` at fraction `mu` ∈ [0, 1),
/// using the neighbours `xm1` (before) and `x2` (after). Farrow form.
#[inline]
pub fn cubic(xm1: Iq, x0: Iq, x1: Iq, x2: Iq, mu: f32) -> Iq {
    let c3 = (x2 - xm1) * (1.0 / 6.0) + (x0 - x1) * 0.5;
    let c2 = (xm1 + x1) * 0.5 - x0;
    let c1 = x1 - xm1 * (1.0 / 3.0) - x0 * 0.5 - x2 * (1.0 / 6.0);
    ((c3 * mu + c2) * mu + c1) * mu + x0
}

/// Proportional-integral gains for a second-order loop with noise bandwidth
/// `bn_t` (normalised to the symbol rate) and damping `zeta`, for a detector of
/// gain `kp`. Standard result, e.g. Rice, *Digital Communications*, App. C.
fn loop_gains(bn_t: f64, zeta: f64, kp: f64) -> (f64, f64) {
    let theta = bn_t / (zeta + 1.0 / (4.0 * zeta));
    let d = 1.0 + 2.0 * zeta * theta + theta * theta;
    let k1 = 4.0 * zeta * theta / d / kp;
    let k2 = 4.0 * theta * theta / d / kp;
    (k1, k2)
}

/// Gardner symbol synchroniser.
pub struct SymbolSync {
    /// Nominal samples per symbol, and the loop's current estimate of it.
    sps_nominal: f64,
    sps: f64,
    /// How far the period estimate may wander from nominal (fraction).
    max_dev: f64,
    k1: f64,
    k2: f64,
    /// Buffered input; `t_next` indexes into it (fractional).
    buf: Vec<Iq>,
    t_next: f64,
    prev_strobe: Iq,
    have_prev: bool,
    /// Smoothed |error| — a rough lock indicator.
    err_avg: f32,
    last_err: f32,
}

impl SymbolSync {
    /// `sps` is the nominal samples per symbol (≥ 2). `bn_t` is the loop noise
    /// bandwidth relative to the symbol rate: ~0.01 pulls in fast, ~0.002
    /// tracks quietly. `max_dev` bounds the period estimate, e.g. 0.02 for ±2 %.
    ///
    /// # Panics
    /// If `sps < 2`.
    pub fn new(sps: f64, bn_t: f64, max_dev: f64) -> Self {
        assert!(sps >= 2.0, "Gardner needs at least 2 samples per symbol");
        // Detector gain for unit-power raised-cosine-filtered PSK at moderate
        // roll-off; folded in so `bn_t` means what it says.
        let (k1, k2) = loop_gains(bn_t, std::f64::consts::FRAC_1_SQRT_2, 1.0);
        SymbolSync {
            sps_nominal: sps,
            sps,
            max_dev,
            k1,
            k2,
            buf: Vec::new(),
            // Start far enough in that the mid-point (half a symbol earlier)
            // and the interpolator's left neighbour both exist. The mid-point
            // lands at 2.0, a full sample clear of the edge: with `+ 1.0` it
            // was exactly 1.0 only in exact arithmetic, and a non-round sps
            // (3.99999996) rounded it to 0.9999999999999998 and indexed buf[-1].
            t_next: sps / 2.0 + 2.0,
            prev_strobe: Iq::new(0.0, 0.0),
            have_prev: false,
            err_avg: 0.0,
            last_err: 0.0,
        }
    }

    /// Change the loop's noise bandwidth (relative to the symbol rate),
    /// keeping its state: wide to pull in, narrow to track quietly.
    pub fn set_bandwidth(&mut self, bn_t: f64) {
        (self.k1, self.k2) = loop_gains(bn_t, std::f64::consts::FRAC_1_SQRT_2, 1.0);
    }

    /// Current samples-per-symbol estimate.
    pub fn sps(&self) -> f64 {
        self.sps
    }

    /// Symbol-rate correction relative to nominal, as a fraction: +0.001 means
    /// the true symbol rate is 0.1 % *lower* than nominal (more samples per
    /// symbol). Useful for refining a symbol-rate estimate.
    pub fn period_ratio(&self) -> f64 {
        self.sps / self.sps_nominal
    }

    /// Smoothed absolute timing error (small once locked).
    pub fn error_level(&self) -> f32 {
        self.err_avg
    }

    pub fn last_error(&self) -> f32 {
        self.last_err
    }

    /// Interpolate the buffer at fractional index `t`.
    #[inline]
    fn at(&self, t: f64) -> Iq {
        debug_assert!(
            t.is_finite() && t >= 1.0,
            "interpolation point {t} out of range (sps {}, buffer {})",
            self.sps,
            self.buf.len()
        );
        // Rust note: `as usize` truncates, which is the floor for t ≥ 1 and
        // one instruction; `t.floor()` is a library call on plain x86-64.
        let i = t as usize;
        let mu = (t - i as f64) as f32;
        cubic(
            self.buf[i - 1],
            self.buf[i],
            self.buf[i + 1],
            self.buf[i + 2],
            mu,
        )
    }

    /// Feed samples; appends one output per recovered symbol to `out`.
    pub fn process(&mut self, input: &[Iq], out: &mut Vec<Iq>) {
        self.buf.extend_from_slice(input);

        // Need the strobe's right-hand neighbours (i + 2) in the buffer.
        while self.t_next + 2.0 < self.buf.len() as f64 {
            let strobe = self.at(self.t_next);
            let mid = self.at(self.t_next - self.sps / 2.0);

            if self.have_prev {
                // Gardner: Re{ conj(mid) · (previous − current) }.
                let e = ((self.prev_strobe - strobe) * mid.conj()).re;
                let e = e.clamp(-1.0, 1.0);
                self.last_err = e;
                self.err_avg = 0.99 * self.err_avg + 0.01 * e.abs();

                // PI loop: the integrator tracks the period, the proportional
                // path nudges the phase. Gardner measures the error in symbol
                // periods, but both corrections act in samples, so they scale
                // by samples-per-symbol — without that the loop bandwidth fell
                // as 1/sps (84× too slow at sps 84).
                let e_samples = e as f64 * self.sps_nominal;
                self.sps += self.k2 * e_samples;
                let lo = self.sps_nominal * (1.0 - self.max_dev);
                let hi = self.sps_nominal * (1.0 + self.max_dev);
                self.sps = self.sps.clamp(lo, hi);
                self.t_next += self.sps + self.k1 * e_samples;
            } else {
                self.have_prev = true;
                self.t_next += self.sps;
            }

            self.prev_strobe = strobe;
            out.push(strobe);
        }

        // Drop consumed samples, keeping the mid-point's and interpolator's
        // look-back.
        // The last step can leave `t_next` up to about sps/2 past the end of
        // the buffer, so clamp: at high samples-per-symbol the unclamped figure
        // ran past the end (found at sps = 84, not caught by tests at ≤ 4).
        // (Truncating a non-negative value is its floor.)
        let keep_from =
            ((self.t_next - self.sps / 2.0 - 2.0).max(0.0) as usize).min(self.buf.len());
        if keep_from > 0 {
            self.buf.drain(..keep_from);
            self.t_next -= keep_from as f64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    /// Raised-cosine pulse at time `t` (symbol periods): what a QPSK symbol
    /// looks like after TX RRC + RX matched RRC.
    fn rc(t: f64, alpha: f64) -> f64 {
        let sinc = if t.abs() < 1e-12 {
            1.0
        } else {
            (PI * t).sin() / (PI * t)
        };
        let d = 1.0 - (2.0 * alpha * t).powi(2);
        if d.abs() < 1e-9 {
            (PI / 4.0) * {
                let x = 1.0 / (2.0 * alpha);
                (PI * x).sin() / (PI * x)
            }
        } else {
            sinc * (PI * alpha * t).cos() / d
        }
    }

    /// Matched-filtered QPSK sampled at `sps` per symbol (fractional allowed),
    /// with a timing offset `tau` (symbols) and a symbol-rate error `rs_err`.
    fn qpsk_rc(
        n_sym: usize,
        sps: f64,
        alpha: f64,
        tau: f64,
        rs_err: f64,
        seed: u64,
    ) -> (Vec<Iq>, Vec<Iq>) {
        let mut s = seed | 1;
        let mut rand2 = || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 62) as u8
        };
        let k = std::f32::consts::FRAC_1_SQRT_2;
        let syms: Vec<Iq> = (0..n_sym)
            .map(|_| match rand2() {
                0 => Iq::new(k, k),
                1 => Iq::new(-k, k),
                2 => Iq::new(-k, -k),
                _ => Iq::new(k, -k),
            })
            .collect();

        // True symbol period in samples, including the rate error.
        let period = sps * (1.0 + rs_err);
        let n_samp = (n_sym as f64 * period) as usize;
        let span = 12i64;
        let x: Vec<Iq> = (0..n_samp)
            .map(|n| {
                let t = n as f64 / period - tau; // time in symbols
                let k0 = t.floor() as i64;
                let mut acc = Iq::new(0.0, 0.0);
                for kk in (k0 - span).max(0)..=(k0 + span).min(n_sym as i64 - 1) {
                    acc += syms[kk as usize] * rc(t - kk as f64, alpha) as f32;
                }
                acc
            })
            .collect();
        (x, syms)
    }

    /// Error vector magnitude (dB) of recovered symbols against the nearest
    /// QPSK point, ignoring the first `skip` (pull-in).
    fn evm_db(out: &[Iq], skip: usize) -> f64 {
        let k = std::f32::consts::FRAC_1_SQRT_2;
        let tail = &out[skip..];
        let err: f64 = tail
            .iter()
            .map(|s| {
                let ideal = Iq::new(k.copysign(s.re), k.copysign(s.im));
                (s - ideal).norm_sqr() as f64
            })
            .sum::<f64>()
            / tail.len() as f64;
        10.0 * err.max(1e-30).log10()
    }

    #[test]
    fn cubic_interpolates_a_cubic_exactly() {
        // Lagrange through 4 points reproduces any cubic exactly.
        let f = |t: f32| Iq::new(0.5 * t * t * t - t + 2.0, -0.25 * t * t + 3.0 * t);
        for &mu in &[0.0f32, 0.25, 0.5, 0.9] {
            let got = cubic(f(-1.0), f(0.0), f(1.0), f(2.0), mu);
            assert!((got - f(mu)).norm() < 1e-4, "mu {mu}");
        }
    }

    #[test]
    fn locks_at_integer_sps() {
        let (x, _) = qpsk_rc(6000, 4.0, 0.35, 0.37, 0.0, 11);
        let mut ss = SymbolSync::new(4.0, 0.01, 0.02);
        let mut out = Vec::new();
        ss.process(&x, &mut out);
        let evm = evm_db(&out, 2000);
        assert!(evm < -25.0, "EVM {evm:.1} dB");
    }

    #[test]
    fn locks_at_fractional_sps_with_a_rate_error() {
        // The DDC's real situation: 2.7 samples per symbol, the symbol-rate
        // estimate 0.3 % out, an arbitrary timing phase.
        let (x, _) = qpsk_rc(12_000, 2.7, 0.25, 0.61, 0.003, 23);
        let mut ss = SymbolSync::new(2.7, 0.01, 0.02);
        let mut out = Vec::new();
        ss.process(&x, &mut out);
        let evm = evm_db(&out, 5000);
        assert!(evm < -22.0, "EVM {evm:.1} dB");
        // The loop's period estimate should have found the true period.
        assert!(
            (ss.period_ratio() - 1.003).abs() < 0.0005,
            "period ratio {}",
            ss.period_ratio()
        );
    }

    #[test]
    fn block_size_does_not_matter() {
        let (x, _) = qpsk_rc(3000, 3.3, 0.35, 0.2, 0.0, 5);
        let mut a = SymbolSync::new(3.3, 0.01, 0.02);
        let mut one = Vec::new();
        a.process(&x, &mut one);

        let mut b = SymbolSync::new(3.3, 0.01, 0.02);
        let mut many = Vec::new();
        for c in x.chunks(101) {
            b.process(c, &mut many);
        }
        assert_eq!(one.len(), many.len());
        for (p, q) in one.iter().zip(&many) {
            assert!((p - q).norm() < 1e-4);
        }
    }

    #[test]
    fn survives_a_non_round_sps() {
        // Regression: sps a hair under an integer used to put the very first
        // mid-symbol point at 1.0 - ε and index before the buffer.
        let sps = 3.999_999_956_180_684;
        let (x, _) = qpsk_rc(2000, 4.0, 0.2, 0.1, 0.0, 3);
        let mut ss = SymbolSync::new(sps, 0.01, 0.02);
        let mut out = Vec::new();
        ss.process(&x, &mut out);
        assert!(out.len() > 1900);
    }

    #[test]
    fn works_at_high_samples_per_symbol() {
        // Regression: a narrow carrier in a wide VFO has many samples per
        // symbol, and the buffer trim used to run off the end above sps ≈ 8.
        for sps in [9.5f64, 20.0, 84.2] {
            let (x, _) = qpsk_rc(1500, sps, 0.35, 0.3, 0.0, 9);
            let mut ss = SymbolSync::new(sps, 0.01, 0.02);
            let mut out = Vec::new();
            for c in x.chunks(1000) {
                ss.process(c, &mut out);
            }
            assert!(out.len() > 1400, "sps {sps}: only {} symbols", out.len());
            let evm = evm_db(&out, 700);
            assert!(evm < -20.0, "sps {sps}: EVM {evm:.1} dB");
        }
    }

    #[test]
    fn is_insensitive_to_carrier_phase() {
        // Gardner runs before carrier recovery, so a constant phase rotation
        // must not stop it locking.
        let (x, _) = qpsk_rc(6000, 4.0, 0.35, 0.4, 0.0, 77);
        let rot = Iq::new(0.6f32.cos(), 0.6f32.sin());
        let rotated: Vec<Iq> = x.iter().map(|s| s * rot).collect();
        let mut ss = SymbolSync::new(4.0, 0.01, 0.02);
        let mut out = Vec::new();
        ss.process(&rotated, &mut out);
        // De-rotate before measuring EVM against the QPSK grid.
        let back: Vec<Iq> = out.iter().map(|s| s * rot.conj()).collect();
        let evm = evm_db(&back, 2000);
        assert!(evm < -25.0, "EVM {evm:.1} dB");
    }
}
