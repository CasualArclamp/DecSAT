//! Carrier phase/frequency tracking on recovered symbols.
//!
//! A second-order phase-locked loop driven by either decisions (the nearest
//! constellation point) or known reference symbols (headers, pilots). It runs
//! after timing recovery, on one sample per symbol, and is meant to be started
//! close to the right frequency by a coarse estimator — the 4th/8th-power
//! spectral line, or the phase of the PLHEADER's differential correlation —
//! then to hold the residual phase and drift.
//!
//! For DVB-S2, decisions work directly on the PL-scrambled payload: scrambling
//! rotates each symbol by a multiple of 90°, and every S2 constellation (QPSK,
//! 8PSK, 16APSK, 32APSK) maps onto itself under a 90° rotation, so the nearest
//! point is still a constellation point.

use std::f64::consts::{PI, TAU};

use decsat_core::Iq;

/// Proportional-integral gains for a second-order loop with noise bandwidth
/// `bn_t` (relative to the symbol rate), damping 1/√2, detector gain 1.
fn loop_gains(bn_t: f64) -> (f64, f64) {
    let zeta = std::f64::consts::FRAC_1_SQRT_2;
    let theta = bn_t / (zeta + 1.0 / (4.0 * zeta));
    let d = 1.0 + 2.0 * zeta * theta + theta * theta;
    (4.0 * zeta * theta / d, 4.0 * theta * theta / d)
}

/// `e^{jθ}`, to f32 precision, without a call into the C maths library.
///
/// The loops turn a phase into a rotation for every symbol, and on a plain
/// x86-64 build (no SSE4.1/FMA) `f64::sin_cos` is two calls into the C
/// library — millions a second on a fast carrier. Here θ is reduced to
/// the nearest quarter turn, `θ = q·π/2 + r` with |r| ≤ π/4, where the
/// Taylor series to r⁹ (sine) and r¹⁰ (cosine) are within 2·10⁻⁹ — far
/// below f32's resolution — and the quarter turns are swaps and signs.
#[inline]
pub fn cis(theta: f64) -> Iq {
    use std::f64::consts::{FRAC_2_PI, FRAC_PI_2};
    // Rust note: a float-to-int `as` truncates toward zero, one instruction
    // (`floor`/`round` would be library calls here); adding ±0.5 first makes
    // it round to the nearest quarter turn.
    let qf = theta * FRAC_2_PI;
    let q = (qf + 0.5f64.copysign(qf)) as i64;
    let r = theta - q as f64 * FRAC_PI_2;
    let r2 = r * r;
    let s =
        r * (1.0 + r2 * (-1.0 / 6.0 + r2 * (1.0 / 120.0 + r2 * (-1.0 / 5040.0 + r2 / 362_880.0))));
    let c = 1.0
        + r2 * (-0.5
            + r2 * (1.0 / 24.0 + r2 * (-1.0 / 720.0 + r2 * (1.0 / 40_320.0 - r2 / 3_628_800.0))));
    let (s, c) = (s as f32, c as f32);
    // An odd quarter turn swaps cos and sin (with a sign); the half turn in
    // `q & 2` negates both. (`q & 3` is right for negative q too: two's
    // complement.)
    let (re, im) = if q & 1 == 0 { (c, s) } else { (-s, c) };
    let sign = if q & 2 == 0 { 1.0 } else { -1.0 };
    Iq::new(sign * re, sign * im)
}

/// Index of the constellation point nearest `y`.
#[inline]
fn nearest_index(y: Iq, points: &[Iq]) -> usize {
    let mut best = 0;
    let mut best_d = f32::INFINITY;
    for (i, &p) in points.iter().enumerate() {
        let d = (y - p).norm_sqr();
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    best
}

/// The constellation point nearest `y`.
#[inline]
fn nearest(y: Iq, points: &[Iq]) -> Iq {
    points[nearest_index(y, points)]
}

/// `e^{j·order·arg(u)}`: a phase error folded by a ring's rotational
/// symmetry, by repeated squaring of the unit phasor instead of `atan2`,
/// `cos` and `sin` (three library calls a symbol). Zero for `u = 0`.
#[inline]
fn fold(u: Iq, order: u32) -> Iq {
    let m = u.norm();
    if m == 0.0 {
        return Iq::new(0.0, 0.0);
    }
    let mut base = u / m;
    let mut acc = Iq::new(1.0, 0.0);
    let mut n = order;
    while n > 0 {
        if n & 1 == 1 {
            acc *= base;
        }
        base *= base;
        n >>= 1;
    }
    acc
}

/// Second-order carrier PLL.
#[derive(Debug, Clone)]
pub struct CarrierPll {
    /// Current phase estimate, radians.
    phase: f64,
    /// Current frequency estimate, radians per symbol.
    freq: f64,
    k1: f64,
    k2: f64,
}

impl CarrierPll {
    /// A loop of noise bandwidth `bn_t` (0.005–0.02 is sensible), started at
    /// `freq_cycles` cycles per symbol.
    pub fn new(bn_t: f64, freq_cycles: f64) -> Self {
        let (k1, k2) = loop_gains(bn_t);
        CarrierPll {
            phase: 0.0,
            freq: freq_cycles * TAU,
            k1,
            k2,
        }
    }

    /// Frequency estimate, cycles per symbol.
    pub fn freq_cycles(&self) -> f64 {
        self.freq / TAU
    }

    /// Re-seed the frequency (e.g. from a better coarse estimate).
    pub fn set_freq_cycles(&mut self, cycles: f64) {
        self.freq = cycles * TAU;
    }

    pub fn set_phase(&mut self, radians: f64) {
        self.phase = radians;
    }

    /// Change the loop's noise bandwidth, keeping its phase and frequency.
    pub fn set_bandwidth(&mut self, bn_t: f64) {
        (self.k1, self.k2) = loop_gains(bn_t);
    }

    /// Re-anchor the phase on a block of known symbols about to be stepped
    /// through: whatever phase error the loop would carry into the block —
    /// including a whole cycle slip of 90° or more — is removed at once, from
    /// the block's average, instead of being pulled in over ~1/Bn symbols.
    /// `sent(n)` is the reference for `block[n]`. Returns the correction
    /// made, radians: how far the loop had drifted.
    pub fn anchor(&mut self, block: &[Iq], sent: impl Fn(usize) -> Iq) -> f64 {
        let mut acc = Iq::new(0.0, 0.0);
        for (n, &y) in block.iter().enumerate() {
            acc += y * cis(-(self.phase + self.freq * n as f64)) * sent(n).conj();
        }
        if acc.norm() == 0.0 {
            return 0.0;
        }
        let corr = acc.arg() as f64;
        self.phase += corr;
        if self.phase > PI {
            self.phase -= TAU;
        } else if self.phase < -PI {
            self.phase += TAU;
        }
        corr
    }

    /// Frequency estimate, radians per symbol.
    pub fn freq_rad(&self) -> f64 {
        self.freq
    }

    pub fn phase(&self) -> f64 {
        self.phase
    }

    #[inline]
    fn rotate(&self, x: Iq) -> Iq {
        x * cis(-self.phase)
    }

    #[inline]
    fn update(&mut self, err: f64) {
        self.freq += self.k2 * err;
        self.phase += self.freq + self.k1 * err;
        // Keep the phase small so f64 never loses precision on long runs.
        if self.phase > PI {
            self.phase -= TAU;
        } else if self.phase < -PI {
            self.phase += TAU;
        }
    }

    /// De-rotate `x`, steer on the nearest of `points`, return the
    /// de-rotated symbol.
    ///
    /// The error is `Im(y·d*)` — the angle weighted by the amplitudes —
    /// rather than the angle itself: on APSK an inner-ring symbol's angle is
    /// several times noisier than an outer one's, and weighting them equally
    /// let it jitter the loop into cycle slips (16APSK at 12 dB lost one
    /// frame in five). On unit-power constellations the gain averages 1.
    pub fn step(&mut self, x: Iq, points: &[Iq]) -> Iq {
        let y = self.rotate(x);
        let d = nearest(y, points);
        let err = (y * d.conj()).im as f64;
        self.update(err);
        y
    }

    /// De-rotate `x` and advance, without steering (a symbol not trusted to
    /// say anything about the phase).
    pub fn coast(&mut self, x: Iq) -> Iq {
        let y = self.rotate(x);
        self.update(0.0);
        y
    }

    /// De-rotate `x`, steer on a known `reference` symbol (data-aided).
    pub fn step_known(&mut self, x: Iq, reference: Iq) -> Iq {
        let y = self.rotate(x);
        let err = (y * reference.conj()).arg() as f64;
        self.update(err);
        y
    }
}

/// Modulation error ratio, dB: signal power over the power of the error to the
/// nearest constellation point. On locked symbols this is the figure a
/// satellite modem reports; on rotating symbols it collapses towards 0 dB.
pub fn mer_db(sym: &[Iq], points: &[Iq]) -> f32 {
    if sym.is_empty() {
        return 0.0;
    }
    let (mut sig, mut err) = (0.0f64, 0.0f64);
    for &s in sym {
        let d = nearest(s, points);
        sig += d.norm_sqr() as f64;
        err += (s - d).norm_sqr() as f64;
    }
    (10.0 * (sig / err.max(1e-30)).log10()) as f32
}

/// The largest number of points on any one ring: the finest rotational
/// step a decision-directed loop can slip by is 2π over it.
pub fn densest_ring(points: &[Iq]) -> usize {
    ring_orders(points).into_iter().max().unwrap_or(1).max(1) as usize
}

/// How many points share each point's ring: the rotational symmetry the
/// phase error has to be folded by for that point.
fn ring_orders(points: &[Iq]) -> Vec<u32> {
    points
        .iter()
        .map(|p| {
            let r = p.norm();
            points
                .iter()
                .filter(|q| (q.norm() - r).abs() < 1e-3 * r.max(1e-6))
                .count() as u32
        })
        .collect()
}

/// Carrier-lock coherence, 0..1: `|mean(exp(j·S·err))|`, where `err` is each
/// symbol's phase error to its nearest point and `S` the number of points on
/// that point's ring (4 for QPSK, 8 for 8PSK, 12 for 16APSK's outer ring).
///
/// MER cannot tell lock on its own: a uniformly rotating QPSK is never more
/// than 45° from a point and still reads ~7 dB, 8PSK ~13 dB. Folding by `S`
/// spreads a rotating signal's errors evenly round the circle (→ 0) while a
/// locked one's stay bunched (→ 1, e.g. ~0.67 for QPSK at 10 dB Es/N0).
pub fn lock_coherence(sym: &[Iq], points: &[Iq]) -> f32 {
    if sym.is_empty() {
        return 0.0;
    }
    let orders = ring_orders(points);
    let mut acc = Iq::new(0.0, 0.0);
    for &s in sym {
        let i = nearest_index(s, points);
        acc += fold(s * points[i].conj(), orders[i]);
    }
    acc.norm() / sym.len() as f32
}

/// MER (dB) and lock coherence together, with the constellation chosen per
/// symbol — for ACM, where each frame has its own. `points_for(i)` gives the
/// constellation symbol `i` is judged against.
pub fn quality<'a>(sym: &[Iq], points_for: impl Fn(usize) -> &'a [Iq]) -> (f32, f32) {
    if sym.is_empty() {
        return (0.0, 0.0);
    }
    let (mut sig, mut err) = (0.0f64, 0.0f64);
    let mut acc = Iq::new(0.0, 0.0);
    // The constellation's ring orders, worked out again only when it
    // changes (an ACM frame boundary), not for every symbol.
    let mut orders: (*const Iq, usize, Vec<u32>) = (std::ptr::null(), 0, Vec::new());
    for (i, &s) in sym.iter().enumerate() {
        let pts = points_for(i);
        if (pts.as_ptr(), pts.len()) != (orders.0, orders.1) {
            orders = (pts.as_ptr(), pts.len(), ring_orders(pts));
        }
        let k = nearest_index(s, pts);
        let d = pts[k];
        sig += d.norm_sqr() as f64;
        err += (s - d).norm_sqr() as f64;
        acc += fold(s * d.conj(), orders.2[k]);
    }
    (
        (10.0 * (sig / err.max(1e-30)).log10()) as f32,
        acc.norm() / sym.len() as f32,
    )
}

/// Lock is declared above this coherence. Rotating signals sit near 0 (±0.03
/// over a few thousand symbols); locked QPSK stays above it down to ~6 dB.
pub const LOCK_COHERENCE: f32 = 0.15;

#[cfg(test)]
mod tests {
    use super::*;

    const K: f32 = std::f32::consts::FRAC_1_SQRT_2;
    const QPSK: [Iq; 4] = [
        Iq::new(K, K),
        Iq::new(-K, K),
        Iq::new(-K, -K),
        Iq::new(K, -K),
    ];

    /// QPSK symbols with a frequency offset (cycles/symbol), a phase and noise.
    fn qpsk(n: usize, freq: f64, phase: f64, noise: f32, seed: u64) -> Vec<Iq> {
        let mut s = seed | 1;
        let mut next = || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            s.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        (0..n)
            .map(|k| {
                let p = QPSK[(next() >> 62) as usize];
                let ph = TAU * freq * k as f64 + phase;
                let nz = Iq::new(
                    ((next() >> 40) as f32 / (1u32 << 24) as f32 - 0.5) * noise,
                    ((next() >> 40) as f32 / (1u32 << 24) as f32 - 0.5) * noise,
                );
                p * Iq::new(ph.cos() as f32, ph.sin() as f32) + nz
            })
            .collect()
    }

    #[test]
    fn locks_from_a_good_coarse_estimate() {
        let x = qpsk(20_000, 2.0e-3, 0.7, 0.2, 1);
        // Coarse estimate 1 % off; the loop takes up the rest.
        let mut pll = CarrierPll::new(0.01, 1.98e-3);
        let y: Vec<Iq> = x.iter().map(|&s| pll.step(s, &QPSK)).collect();
        let mer = mer_db(&y[10_000..], &QPSK);
        assert!(mer > 15.0, "MER {mer:.1} dB");
        assert!(
            (pll.freq_cycles() - 2.0e-3).abs() < 2e-5,
            "freq {}",
            pll.freq_cycles()
        );
    }

    #[test]
    fn pulls_in_a_small_residual_from_zero() {
        let x = qpsk(20_000, 3.0e-4, -1.2, 0.15, 2);
        let mut pll = CarrierPll::new(0.01, 0.0);
        let y: Vec<Iq> = x.iter().map(|&s| pll.step(s, &QPSK)).collect();
        assert!(mer_db(&y[10_000..], &QPSK) > 15.0);
    }

    #[test]
    fn rotation_is_incoherent_though_its_mer_looks_fine() {
        // A rotating QPSK still reads ~7 dB MER (never more than 45° from a
        // point), which is why lock is judged by coherence instead.
        let x = qpsk(5_000, 1.0e-2, 0.0, 0.0, 3);
        let mer = mer_db(&x, &QPSK);
        assert!((5.0..9.0).contains(&mer), "MER {mer}");
        assert!(
            lock_coherence(&x, &QPSK) < 0.1,
            "{}",
            lock_coherence(&x, &QPSK)
        );

        let locked = qpsk(5_000, 0.0, 0.0, 0.3, 3);
        assert!(
            lock_coherence(&locked, &QPSK) > 0.5,
            "{}",
            lock_coherence(&locked, &QPSK)
        );
    }

    #[test]
    fn coherence_folds_each_ring_by_its_own_order() {
        // A rotating 16APSK (4 + 12) must read incoherent too: the outer ring
        // is folded by 12, not by 4.
        let r1 = 0.4f32;
        let r2 = 1.13f32;
        let mut pts = Vec::new();
        for k in 0..4 {
            let a = std::f32::consts::FRAC_PI_4 + k as f32 * std::f32::consts::FRAC_PI_2;
            pts.push(Iq::new(r1 * a.cos(), r1 * a.sin()));
        }
        for k in 0..12 {
            let a = std::f32::consts::PI / 12.0 + k as f32 * std::f32::consts::PI / 6.0;
            pts.push(Iq::new(r2 * a.cos(), r2 * a.sin()));
        }
        let rotating: Vec<Iq> = (0..12_000)
            .map(|n| {
                let p = pts[n % 16];
                let th = 0.0007 * n as f32;
                p * Iq::new(th.cos(), th.sin())
            })
            .collect();
        assert!(lock_coherence(&rotating, &pts) < 0.1);
        assert!(lock_coherence(&pts.repeat(100), &pts) > 0.99);
    }

    #[test]
    fn cis_matches_sin_cos() {
        let mut worst = 0f64;
        for k in -200_000i64..=200_000 {
            let th = k as f64 * 1.37e-4 * if k % 7 == 0 { 300.0 } else { 1.0 };
            let z = cis(th);
            let (s, c) = th.sin_cos();
            worst = worst
                .max((z.re as f64 - c).abs())
                .max((z.im as f64 - s).abs());
        }
        assert!(worst < 2e-7, "worst error {worst:e}");
    }

    #[test]
    fn fold_matches_the_angle_times_the_order() {
        for k in 0..1000 {
            let th = -3.1 + k as f32 * 0.0062;
            let u = Iq::new(th.cos(), th.sin()) * (0.3 + k as f32 * 0.001);
            for order in [1u32, 4, 8, 12, 16] {
                let e = th * order as f32;
                let want = Iq::new(e.cos(), e.sin());
                assert!(
                    (fold(u, order) - want).norm() < 2e-5,
                    "θ {th} order {order}"
                );
            }
        }
        assert_eq!(fold(Iq::new(0.0, 0.0), 4), Iq::new(0.0, 0.0));
    }

    #[test]
    fn data_aided_steps_track_known_symbols() {
        let x = qpsk(5_000, 5.0e-4, 2.0, 0.1, 4);
        let mut pll = CarrierPll::new(0.02, 0.0);
        // Known references: the noiseless, unrotated symbols.
        let refs = qpsk(5_000, 0.0, 0.0, 0.0, 4);
        let y: Vec<Iq> = x
            .iter()
            .zip(&refs)
            .map(|(&s, &r)| pll.step_known(s, r))
            .collect();
        assert!(mer_db(&y[2_500..], &QPSK) > 15.0);
    }
}
