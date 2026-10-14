//! One burst demodulated on its own. A TDMA burst is too short for tracking
//! loops to settle in, and each comes from a different terminal with its own
//! clock and oscillator, so every estimate here is made from the burst's own
//! samples, all at once (feedforward):
//!
//! - **timing** from the symbol-rate line of the squared envelope — Oerder &
//!   Meyr, "Digital filter and square timing recovery", IEEE Trans. Commun.
//!   36(5), 1988;
//! - **carrier** from the M-th power of the symbols, which strips QPSK's
//!   (M = 4) or BPSK's (M = 2) modulation — Viterbi & Viterbi, "Nonlinear
//!   estimation of PSK-modulated carrier phase", IEEE Trans. Inf. Theory
//!   29(4), 1983: a coarse frequency from an FFT of `s^M`, then a straight
//!   line fitted through block-wise phases for the rest.

use std::f64::consts::{PI, TAU};

use decsat_core::{Iq, Modulation};
use decsat_dsp::timing::cubic;
use rustfft::FftPlanner;

/// A burst's symbols and what was measured on the way.
#[derive(Debug, Clone)]
pub struct BurstSymbols {
    /// When the first symbol is, channel samples since the stream began.
    pub t0: f64,
    /// Unit mean power, carrier removed — up to the constellation's M-fold
    /// phase ambiguity, which only known symbols can settle.
    pub symbols: Vec<Iq>,
    /// Mean power of the burst's samples, dB (relative: no calibration).
    pub power_db: f32,
    /// Residual carrier, cycles a symbol.
    pub freq: f64,
    pub mer_db: f32,
    pub modulation: Modulation,
}

/// The ideal points of the modulations a burst is demodulated as.
pub fn points(m: Modulation) -> &'static [Iq] {
    use std::sync::OnceLock;
    static BPSK: OnceLock<Vec<Iq>> = OnceLock::new();
    static QPSK: OnceLock<Vec<Iq>> = OnceLock::new();
    static PSK8: OnceLock<Vec<Iq>> = OnceLock::new();
    // Rust note: `OnceLock` builds each table the first time it is asked
    // for and hands out a shared reference ever after.
    match m {
        Modulation::Bpsk => BPSK.get_or_init(|| vec![Iq::new(1.0, 0.0), Iq::new(-1.0, 0.0)]),
        Modulation::Psk8 => PSK8.get_or_init(|| {
            (0..8)
                .map(|k| Iq::from_polar(1.0, k as f32 * std::f32::consts::FRAC_PI_4))
                .collect()
        }),
        _ => QPSK.get_or_init(|| {
            let a = std::f32::consts::FRAC_1_SQRT_2;
            // Numbered counter-clockwise from the first quadrant.
            vec![
                Iq::new(a, a),
                Iq::new(-a, a),
                Iq::new(-a, -a),
                Iq::new(a, -a),
            ]
        }),
    }
}

/// The order of the M-th power that strips `m`'s modulation.
fn power_order(m: Modulation) -> u32 {
    match m {
        Modulation::Bpsk => 2,
        Modulation::Psk8 => 8,
        _ => 4,
    }
}

/// The index of the point of `pts` nearest `s`.
pub fn nearest(s: Iq, pts: &[Iq]) -> usize {
    let mut best = 0;
    let mut d = f32::INFINITY;
    for (k, p) in pts.iter().enumerate() {
        let e = (s - p).norm_sqr();
        if e < d {
            d = e;
            best = k;
        }
    }
    best
}

/// MER of `sym` against `pts`, dB.
pub fn mer_db(sym: &[Iq], pts: &[Iq]) -> f32 {
    let (mut sig, mut err) = (0f64, 0f64);
    for &s in sym {
        let p = pts[nearest(s, pts)];
        sig += p.norm_sqr() as f64;
        err += (s - p).norm_sqr() as f64;
    }
    (10.0 * (sig / err.max(1e-12)).log10()) as f32
}

/// Symbol instants from the envelope's symbol-rate line: the offset (in
/// samples, `0..sps`) of the symbol centres from the start of `on`.
fn timing_offset(x: &[Iq], sps: f64) -> f64 {
    let w = -TAU / sps;
    let step = (w.cos(), w.sin());
    // The oscillator by recursion, put back on the unit circle now and then.
    let mut osc = (1f64, 0f64);
    let (mut re, mut im) = (0f64, 0f64);
    for (n, s) in x.iter().enumerate() {
        let p = s.norm_sqr() as f64;
        re += p * osc.0;
        im += p * osc.1;
        osc = (
            osc.0 * step.0 - osc.1 * step.1,
            osc.0 * step.1 + osc.1 * step.0,
        );
        if n % 1024 == 1023 {
            let a = w * (n + 1) as f64;
            osc = (a.cos(), a.sin());
        }
    }
    // |y|² peaks at the symbol centres, so its line at the symbol rate has
    // phase −2π·τ/sps there.
    (-im.atan2(re) / TAU * sps).rem_euclid(sps)
}

/// The `m`-th power line's frequency in `s` (cycles a symbol, within
/// ±1/(2m)) and its strength, |Σ s^m| / Σ |s|^m (1 = a clean line).
pub fn power_line(s: &[Iq], m: u32) -> (f64, f32) {
    let n = s.len();
    if n < 4 {
        return (0.0, 0.0);
    }
    let size = (2 * n).next_power_of_two().clamp(256, 1 << 15);
    let mut buf: Vec<Iq> = vec![Iq::new(0.0, 0.0); size];
    let mut total = 0f32;
    for (b, &x) in buf.iter_mut().zip(s) {
        let p = x.powu(m);
        total += p.norm();
        *b = p;
    }
    // Rust note: `thread_local!` gives each thread its own planner, which
    // keeps the plans it has made — planning afresh for every burst cost
    // more than the transform.
    thread_local! {
        static PLANNER: std::cell::RefCell<FftPlanner<f32>> =
            std::cell::RefCell::new(FftPlanner::new());
    }
    let fft = PLANNER.with(|p| p.borrow_mut().plan_fft_forward(size));
    fft.process(&mut buf);
    let mags: Vec<f32> = buf.iter().map(|z| z.norm()).collect();
    let (k, &peak) = mags
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .unwrap_or((0, &0.0));
    // Parabolic interpolation between the neighbouring bins.
    let l = mags[(k + size - 1) % size];
    let r = mags[(k + 1) % size];
    let den = l - 2.0 * peak + r;
    let frac = if den.abs() > 1e-12 {
        (0.5 * (l - r) / den).clamp(-0.5, 0.5)
    } else {
        0.0
    };
    let bin = if k > size / 2 {
        k as f64 - size as f64
    } else {
        k as f64
    } + frac as f64;
    (bin / size as f64 / m as f64, peak / total.max(1e-20))
}

/// The modulation whose M-th power line stands out: the lowest order with a
/// clean line (BPSK's square is a line, and so is its fourth power, so the
/// order is tried from the lowest).
pub fn guess_modulation(s: &[Iq]) -> Modulation {
    let bar = (4.0 / (s.len().max(1) as f32).sqrt()).max(0.12);
    for (m, kind) in [
        (2, Modulation::Bpsk),
        (4, Modulation::Qpsk),
        (8, Modulation::Psk8),
    ] {
        if power_line(s, m).1 > bar {
            return kind;
        }
    }
    Modulation::Qpsk
}

/// Demodulate one burst: `x` matched-filtered samples at `sps` samples a
/// symbol, the burst itself within `on` (the rest is margin), the first
/// sample `start` samples into the stream. `modulation` `None` guesses it.
/// `freq`: where the carrier is known to be (cycles a symbol), which saves
/// the frequency search.
pub fn demod_burst(
    x: &[Iq],
    on: std::ops::Range<usize>,
    sps: f64,
    start: u64,
    modulation: Option<Modulation>,
    freq: Option<f64>,
) -> Option<BurstSymbols> {
    let on = on.start.min(x.len())..on.end.min(x.len());
    if (on.len() as f64) < 8.0 * sps {
        return None;
    }
    let power = x[on.clone()]
        .iter()
        .map(|s| s.norm_sqr() as f64)
        .sum::<f64>()
        / on.len() as f64;
    let tau = timing_offset(&x[on.clone()], sps);
    // The first instant at or after the burst's start (half a symbol of
    // leeway, the edges being only as good as the envelope), and the last
    // that the cubic interpolator can reach.
    let first = on.start as f64 + tau - sps * ((tau / sps + 0.5).floor());
    let first = if first < 1.0 { first + sps } else { first };
    let last = (on.end as f64).min(x.len() as f64 - 3.0);
    let mut sym = Vec::with_capacity((on.len() as f64 / sps) as usize + 2);
    let mut t = first;
    while t < last {
        let i = t.floor() as usize;
        let mu = (t - i as f64) as f32;
        sym.push(cubic(x[i - 1], x[i], x[i + 1], x[i + 2], mu));
        t += sps;
    }
    if sym.len() < 8 {
        return None;
    }
    let modulation = modulation.unwrap_or_else(|| guess_modulation(&sym));
    let m = power_order(modulation);
    // Coarse carrier from the M-th power's FFT (or as known), then block
    // phases.
    let f0 = freq.unwrap_or_else(|| power_line(&sym, m).0);
    rotate(&mut sym, f0, 0.0);
    let (f1, phase) = phase_line(&sym, m);
    let offset = match modulation {
        Modulation::Qpsk => PI / 4.0,
        _ => 0.0,
    };
    rotate(&mut sym, f1, phase - offset);
    let p = sym.iter().map(|s| s.norm_sqr() as f64).sum::<f64>() / sym.len() as f64;
    let g = (1.0 / p.max(1e-20).sqrt()) as f32;
    for s in &mut sym {
        *s *= g;
    }
    let pts = points(modulation);
    Some(BurstSymbols {
        t0: start as f64 + first,
        mer_db: mer_db(&sym, pts),
        symbols: sym,
        power_db: (10.0 * power.max(1e-20).log10()) as f32,
        freq: f0 + f1,
        modulation,
    })
}

/// Multiply `s` by exp(−j(2π·f·k + phase)).
pub fn rotate(s: &mut [Iq], f: f64, phase: f64) {
    for (k, x) in s.iter_mut().enumerate() {
        let a = -(TAU * f * k as f64 + phase);
        *x *= Iq::new(a.cos() as f32, a.sin() as f32);
    }
}

/// A straight line through the M-th power phases of blocks of `s`: the
/// frequency left (cycles a symbol) and the phase at symbol 0. Phases are
/// unwrapped modulo 2π/M from block to block.
fn phase_line(s: &[Iq], m: u32) -> (f64, f64) {
    let block = 32.min(s.len());
    let mut pts: Vec<(f64, f64, f64)> = Vec::new(); // (centre, phase, weight)
    let mut prev: Option<f64> = None;
    for (b, c) in s.chunks(block).enumerate() {
        if c.len() < block / 2 {
            break;
        }
        let sum: Iq = c.iter().map(|x| x.powu(m)).sum();
        let mut ph = (sum.im as f64).atan2(sum.re as f64) / m as f64;
        if let Some(p) = prev {
            let span = TAU / m as f64;
            ph -= span * ((ph - p) / span).round();
        }
        prev = Some(ph);
        let centre = (b * block) as f64 + (c.len() as f64 - 1.0) / 2.0;
        pts.push((centre, ph, sum.norm() as f64));
    }
    if pts.len() < 2 {
        return (0.0, pts.first().map_or(0.0, |p| p.1));
    }
    // Weighted least squares.
    let w: f64 = pts.iter().map(|p| p.2).sum::<f64>().max(1e-20);
    let mx = pts.iter().map(|p| p.2 * p.0).sum::<f64>() / w;
    let my = pts.iter().map(|p| p.2 * p.1).sum::<f64>() / w;
    let sxx = pts.iter().map(|p| p.2 * (p.0 - mx).powi(2)).sum::<f64>();
    let sxy = pts
        .iter()
        .map(|p| p.2 * (p.0 - mx) * (p.1 - my))
        .sum::<f64>();
    let slope = if sxx > 0.0 { sxy / sxx } else { 0.0 };
    (slope / TAU, my - slope * mx)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A tiny deterministic noise source for tests.
    pub struct Lcg(pub u64);
    impl Lcg {
        pub fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        pub fn uniform(&mut self) -> f64 {
            (self.next() as f64 + 0.5) / (1u64 << 31) as f64
        }
        pub fn gauss(&mut self) -> f32 {
            let (u, v) = (self.uniform(), self.uniform());
            ((-2.0 * u.ln()).sqrt() * (TAU * v).cos()) as f32
        }
    }

    /// RRC-shaped symbols at `sps`, through the matched filter (so a raised
    /// cosine), with a carrier offset of `f` cycles a symbol.
    pub fn shaped(sym: &[Iq], sps: f64, f: f64, esn0_db: f64, r: &mut Lcg) -> Vec<Iq> {
        let n = ((sym.len() as f64 + 8.0) * sps) as usize;
        let alpha = 0.2;
        let rc = |t: f64| -> f64 {
            // Raised cosine, t in symbols.
            if t.abs() < 1e-9 {
                return 1.0;
            }
            let d = 1.0 - (2.0 * alpha * t).powi(2);
            let sinc = (PI * t).sin() / (PI * t);
            if d.abs() < 1e-6 {
                PI / 4.0 * sinc
            } else {
                sinc * (PI * alpha * t).cos() / d
            }
        };
        let sigma = (0.5 / 10f64.powf(esn0_db / 10.0)).sqrt() as f32;
        (0..n)
            .map(|i| {
                let t = i as f64 / sps - 4.0;
                let k0 = (t.floor() as i64 - 6).max(0) as usize;
                let k1 = ((t.ceil() as i64 + 6).max(0) as usize).min(sym.len());
                let mut v = Iq::new(0.0, 0.0);
                for (k, s) in sym.iter().enumerate().take(k1).skip(k0) {
                    v += s * rc(t - k as f64) as f32;
                }
                let a = TAU * f * t;
                v * Iq::new(a.cos() as f32, a.sin() as f32)
                    + Iq::new(r.gauss() * sigma, r.gauss() * sigma)
            })
            .collect()
    }

    pub fn qpsk(n: usize, r: &mut Lcg) -> Vec<Iq> {
        let p = points(Modulation::Qpsk);
        (0..n).map(|_| p[(r.next() % 4) as usize]).collect()
    }

    #[test]
    fn demodulates_a_burst_with_offset_and_fractional_sps() {
        let mut r = Lcg(7);
        let sym = qpsk(600, &mut r);
        let sps = 3.37;
        let x = shaped(&sym, sps, 0.004, 14.0, &mut r);
        let on = (4.0 * sps) as usize..((4.0 + 600.0) * sps) as usize;
        let b = demod_burst(&x, on.clone(), sps, 0, None, None).unwrap();
        // Told where the carrier is, the same.
        let k = demod_burst(&x, on, sps, 0, Some(Modulation::Qpsk), Some(0.0039)).unwrap();
        assert!(
            (k.freq - 0.004).abs() < 2e-4 && k.mer_db > 11.0,
            "{} {}",
            k.freq,
            k.mer_db
        );
        assert_eq!(b.modulation, Modulation::Qpsk);
        assert!((b.freq - 0.004).abs() < 2e-4, "freq {}", b.freq);
        assert!(b.mer_db > 11.0, "MER {}", b.mer_db);
        // Symbols agree with what was sent up to a quarter turn.
        let pts = points(Modulation::Qpsk);
        let got: Vec<usize> = b.symbols.iter().map(|&s| nearest(s, pts)).collect();
        let sent: Vec<usize> = sym.iter().map(|&s| nearest(s, pts)).collect();
        let lag = (0..3)
            .max_by_key(|&l| {
                (0..4)
                    .map(|rot| {
                        got.iter()
                            .skip(l)
                            .zip(&sent)
                            .filter(|(g, s)| **g == (**s + rot) % 4)
                            .count()
                    })
                    .max()
                    .unwrap()
            })
            .unwrap();
        let agree = (0..4)
            .map(|rot| {
                got.iter()
                    .skip(lag)
                    .zip(&sent)
                    .filter(|(g, s)| **g == (**s + rot) % 4)
                    .count()
            })
            .max()
            .unwrap();
        assert!(
            agree as f64 > 0.98 * (got.len() - lag) as f64,
            "{agree} of {}",
            got.len()
        );
    }

    #[test]
    fn tells_bpsk_from_qpsk() {
        let mut r = Lcg(3);
        let bp: Vec<Iq> = (0..400)
            .map(|_| Iq::new(if r.next() & 1 == 0 { 1.0 } else { -1.0 }, 0.0))
            .collect();
        let x = shaped(&bp, 4.0, -0.01, 12.0, &mut r);
        let b = demod_burst(&x, 16..(404 * 4), 4.0, 0, None, None).unwrap();
        assert_eq!(b.modulation, Modulation::Bpsk);
        assert!(b.mer_db > 9.0);
    }
}
