// made by CasualArclamp
//! The beacon's carrier: found as the strongest narrow line, tracked by a
//! phase-locked loop, measured (frequency, level, C/N0), and its phase
//! handed on — for a phase-modulated beacon, the telemetry itself.

use decsat_core::Iq;
use rustfft::{FftPlanner, num_complex::Complex32};

/// One-sided-or-not power spectrum, linear, Welch (Hann, 50 % overlap),
/// each bin normalised so white noise of variance σ² reads σ² — the noise
/// floor in bins is then the noise per sample directly. Complex input,
/// FFT-shifted (bin n/2 is DC).
pub(crate) fn psd_complex(x: &[Iq], n: usize, max_segments: usize) -> (Vec<f64>, usize) {
    let mut acc = vec![0f64; n];
    if x.len() < n {
        return (acc, 0);
    }
    let w: Vec<f32> = (0..n)
        .map(|k| 0.5 - 0.5 * (std::f32::consts::TAU * k as f32 / n as f32).cos())
        .collect();
    let w2: f64 = w.iter().map(|&v| f64::from(v * v)).sum();
    let fft = FftPlanner::<f32>::new().plan_fft_forward(n);
    let full = (x.len() - n) / (n / 2) + 1;
    let hop = if full > max_segments {
        (x.len() - n) / max_segments.max(1)
    } else {
        n / 2
    }
    .max(1);
    let mut buf = vec![Complex32::new(0.0, 0.0); n];
    let mut segs = 0usize;
    let mut start = 0;
    while start + n <= x.len() {
        for k in 0..n {
            buf[k] = x[start + k] * w[k];
        }
        fft.process(&mut buf);
        for k in 0..n {
            acc[(k + n / 2) % n] += f64::from(buf[k].norm_sqr());
        }
        segs += 1;
        start += hop;
    }
    let norm = 1.0 / (segs.max(1) as f64 * w2);
    for a in &mut acc {
        *a *= norm;
    }
    (acc, segs)
}

/// The noise per sample (σ², the floor of a [`psd_complex`]) from the bins
/// within `half_band` Hz of the centre: their 10th percentile, put right for
/// how far an average of `segments` periodograms sits below its mean there.
/// Bins over 20 dB under the upper quartile are left out first: a
/// down-converter's stopband inside the band (a VFO recording replayed in
/// a wider VFO, seen live) is no noise floor.
pub(crate) fn noise_floor(p: &[f64], rate: f64, half_band: f64, segments: usize) -> Option<f64> {
    let n = p.len();
    let mut inside: Vec<f64> = (0..n)
        .filter(|&k| ((k as f64 - (n / 2) as f64) * rate / n as f64).abs() <= half_band)
        .map(|k| p[k])
        .collect();
    if inside.len() < 16 || segments == 0 {
        return None;
    }
    inside.sort_unstable_by(f64::total_cmp);
    let q3 = inside[inside.len() * 3 / 4];
    inside.retain(|&v| v >= 0.01 * q3);
    if inside.len() < 16 {
        return None;
    }
    let p10 = inside[inside.len() / 10];
    // Overlapped Hann segments are worth about 0.7 of an independent one.
    let m = (segments as f64 * 0.7).max(1.0);
    Some(p10 / (1.0 - 1.28 / m.sqrt()).max(0.3))
}

/// A carrier line found in a first look.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Line {
    /// Offset from the centre of the baseband, Hz.
    pub offset_hz: f64,
    /// Its power, and the noise per sample, in the baseband's units.
    pub power: f64,
    pub noise: f64,
    /// Share of the signal's power (noise left out) in the line.
    pub share: f64,
}

impl Line {
    /// C/N0, dB-Hz.
    pub fn cn0_dbhz(&self, rate: f64) -> f64 {
        10.0 * (self.power * rate / self.noise.max(1e-30)).log10()
    }
}

/// The strongest narrow line in `x` (at `rate`, a VFO `band` Hz wide): a
/// residual carrier, if it stands at least 20 dB over the noise in its
/// bins and holds a twentieth of the power above the noise. `None`: no
/// line (nothing there, or a suppressed carrier).
pub fn find_line(x: &[Iq], rate: f64, band: f64) -> Option<Line> {
    let half = (0.45 * band).min(0.48 * rate);
    // The floor from a coarse spectrum averaged many times (bins of any
    // width read σ² for white noise, so it serves the fine one too).
    let nc = 4096usize
        .min((x.len() / 16).next_power_of_two() / 2)
        .max(256);
    let (pc, segs) = psd_complex(x, nc, 512);
    let noise = noise_floor(&pc, rate, half, segs)?;
    // The line in bins of a few hertz: a carrier with LNB phase noise
    // still sits in a handful, while the modulation spreads over
    // thousands.
    let n = ((rate / 4.0) as usize)
        .next_power_of_two()
        .clamp(1024, 1 << 17)
        .min(x.len().next_power_of_two() / 4)
        .max(256);
    let (p, _) = psd_complex(x, n, 64);
    let bin = rate / n as f64;
    let inside = |k: usize| ((k as f64 - (n / 2) as f64) * bin).abs() <= half;
    let strongest = (0..n)
        .filter(|&k| inside(k))
        .max_by(|&a, &b| p[a].total_cmp(&p[b]))?;
    // A line at the VFO's centre (where a claimed carrier puts it) is taken
    // over a stronger neighbour further out.
    let near = (2000.0f64).max(0.05 * band) / bin;
    let central = (0..n)
        .filter(|&k| (k as f64 - (n / 2) as f64).abs() <= near && inside(k))
        .max_by(|&a, &b| p[a].total_cmp(&p[b]))
        .filter(|&k| p[k] >= 100.0 * noise);
    let k0 = central.unwrap_or(strongest);
    if p[k0] < 100.0 * noise {
        return None;
    }
    // Power: a bin sum over n (Parseval, with bins normalised to σ²) — the
    // bins round the peak above the floor (a Hann window spreads a tone
    // over three; phase noise a few more).
    // ±30 Hz at least: an LNB's phase noise spreads its carrier that far.
    let reach = ((30.0 / bin).ceil() as usize).max(6);
    let lo = k0.saturating_sub(reach);
    let hi = (k0 + reach).min(n - 1);
    let power: f64 = (lo..=hi).map(|k| (p[k] - noise).max(0.0)).sum::<f64>() / n as f64;
    let inside_c = |k: usize| ((k as f64 - (nc / 2) as f64) * rate / nc as f64).abs() <= half;
    let total: f64 = (0..nc)
        .filter(|&k| inside_c(k))
        .map(|k| (pc[k] - noise).max(0.0))
        .sum::<f64>()
        / nc as f64;
    let share = power / total.max(1e-30);
    if share < 0.05 {
        return None;
    }
    // Where between bins: a parabola through the peak's dB.
    let db = |k: usize| 10.0 * p[k].max(1e-30).log10();
    let frac = if k0 > 0 && k0 + 1 < n {
        let (a, b, c) = (db(k0 - 1), db(k0), db(k0 + 1));
        let d = a - 2.0 * b + c;
        if d.abs() > 1e-12 {
            (0.5 * (a - c) / d).clamp(-0.5, 0.5)
        } else {
            0.0
        }
    } else {
        0.0
    };
    Some(Line {
        offset_hz: (k0 as f64 + frac - (n / 2) as f64) * bin,
        power,
        noise,
        share: share.min(1.0),
    })
}

/// A measurement over one window of the carrier loop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    /// The carrier's power (coherent: the loop's mean output), and all the
    /// power in the baseband.
    pub carrier: f64,
    pub total: f64,
    /// The loop's frequency, Hz from the baseband's centre.
    pub offset_hz: f64,
    /// 0..1: how much of the loop's output lies along the carrier (1 =
    /// locked, ~0 = the phase wandering).
    pub lock: f64,
    /// Samples it covers.
    pub samples: usize,
}

/// Second-order phase-locked loop on a residual carrier. Its output phase
/// (the input's phase against the carrier's) is the phase modulation.
pub struct CarrierLoop {
    rate: f64,
    /// NCO phase and frequency, radians and radians a sample.
    phase: f64,
    freq: f64,
    /// Loop gains (proportional, integral), per sample.
    kp: f64,
    ki: f64,
    /// The carrier's amplitude, smoothed (normalises the phase detector).
    amp: f64,
    /// Window accumulators.
    win: usize,
    acc: (f64, f64),
    acc_p: f64,
    n: usize,
    windows: Vec<Window>,
}

/// Measurement windows, seconds.
const WINDOW_S: f64 = 0.1;

impl CarrierLoop {
    /// A loop on a carrier `offset_hz` from the centre of baseband at
    /// `rate`, noise bandwidth `bn_hz`, its amplitude about `amp`.
    pub fn new(rate: f64, offset_hz: f64, bn_hz: f64, amp: f64) -> Self {
        let mut l = CarrierLoop {
            rate,
            phase: 0.0,
            freq: std::f64::consts::TAU * offset_hz / rate,
            kp: 0.0,
            ki: 0.0,
            amp: amp.max(1e-9),
            win: ((rate * WINDOW_S) as usize).max(64),
            acc: (0.0, 0.0),
            acc_p: 0.0,
            n: 0,
            windows: Vec::new(),
        };
        l.set_bandwidth(bn_hz);
        l
    }

    /// Loop noise bandwidth, Hz (damping 0.707).
    pub fn set_bandwidth(&mut self, bn_hz: f64) {
        let zeta = std::f64::consts::FRAC_1_SQRT_2;
        let wn = 2.0 * bn_hz / (zeta + 1.0 / (4.0 * zeta));
        let t = 1.0 / self.rate;
        self.kp = 2.0 * zeta * wn * t;
        self.ki = (wn * t) * (wn * t);
    }

    pub fn offset_hz(&self) -> f64 {
        self.freq * self.rate / std::f64::consts::TAU
    }

    /// The baseband moved by `hz` (the VFO followed the carrier): the loop
    /// moves with it, so it sees no step.
    pub fn retune_hz(&mut self, hz: f64) {
        self.freq -= std::f64::consts::TAU * hz / self.rate;
    }

    /// Track `x`, pushing its phase against the carrier (radians, −π..π)
    /// for each sample into `phase`.
    pub fn process(&mut self, x: &[Iq], phase: &mut Vec<f32>) {
        phase.reserve(x.len());
        for &s in x {
            let (sn, cs) = self.phase.sin_cos();
            // y = x·e^(−jθ).
            let yr = f64::from(s.re) * cs + f64::from(s.im) * sn;
            let yi = f64::from(s.im) * cs - f64::from(s.re) * sn;
            phase.push(yi.atan2(yr) as f32);
            // Phase detector: the quadrature part against the carrier's
            // amplitude (the modulation averages out in the loop).
            let e = (yi / self.amp).clamp(-1.0, 1.0);
            self.freq += self.ki * e;
            self.phase += self.freq + self.kp * e;
            if self.phase > std::f64::consts::PI {
                self.phase -= std::f64::consts::TAU;
            } else if self.phase < -std::f64::consts::PI {
                self.phase += std::f64::consts::TAU;
            }
            self.acc.0 += yr;
            self.acc.1 += yi;
            self.acc_p += yr * yr + yi * yi;
            self.n += 1;
            if self.n == self.win {
                let n = self.n as f64;
                let (mr, mi) = (self.acc.0 / n, self.acc.1 / n);
                let c = mr * mr + mi * mi;
                // The amplitude for the detector: the in-phase mean, held
                // above a floor while acquiring.
                self.amp = 0.8 * self.amp + 0.2 * mr.abs().max(c.sqrt() * 0.5).max(1e-9);
                self.windows.push(Window {
                    carrier: c,
                    total: self.acc_p / n,
                    offset_hz: self.offset_hz(),
                    lock: if c > 0.0 {
                        (mr * mr - mi * mi) / c
                    } else {
                        0.0
                    },
                    samples: self.n,
                });
                self.acc = (0.0, 0.0);
                self.acc_p = 0.0;
                self.n = 0;
            }
        }
    }

    /// Windows measured since the last call.
    pub fn take_windows(&mut self) -> Vec<Window> {
        std::mem::take(&mut self.windows)
    }
}

/// J₀⁻¹: the modulation index whose carrier keeps `j0` of its amplitude
/// (0 < j0 ≤ 1) under sinusoidal phase modulation — a sine subcarrier.
pub fn index_from_j0(j0: f64) -> Option<f64> {
    if !(0.0..=1.0).contains(&j0) || j0 < 0.02 {
        return None;
    }
    // J₀ falls monotonically from 1 to 0 on [0, 2.405].
    let (mut lo, mut hi) = (0.0f64, 2.4048);
    for _ in 0..50 {
        let mid = 0.5 * (lo + hi);
        if bessel_j0(mid) > j0 {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(0.5 * (lo + hi))
}

/// J₀(x) by its power series (enough for |x| ≤ 3).
pub fn bessel_j0(x: f64) -> f64 {
    let q = x * x / 4.0;
    let mut term = 1.0;
    let mut sum = 1.0;
    for k in 1..30 {
        term *= -q / (k as f64 * k as f64);
        sum += term;
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, sigma: f32, seed: u64) -> Vec<Iq> {
        let mut s = seed | 1;
        let mut u = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            ((s >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        };
        (0..n)
            .map(|_| {
                let r = (-2.0 * u().ln()).sqrt() * sigma as f64 / std::f64::consts::SQRT_2;
                let a = std::f64::consts::TAU * u();
                Iq::new((r * a.cos()) as f32, (r * a.sin()) as f32)
            })
            .collect()
    }

    #[test]
    fn finds_a_carrier_and_its_c_over_n0() {
        let rate = 100_000.0;
        let f0 = 1234.5;
        let mut x = noise(400_000, 0.1, 7);
        // C = 0.01 (−20 dB), σ² = 0.01 → C/N0 = 10·log10(0.01·1e5/0.01) = 50.
        for (k, s) in x.iter_mut().enumerate() {
            let ph = std::f64::consts::TAU * f0 * k as f64 / rate;
            *s += Iq::new((0.1 * ph.cos()) as f32, (0.1 * ph.sin()) as f32);
        }
        let l = find_line(&x, rate, 80_000.0).expect("line");
        assert!((l.offset_hz - f0).abs() < 1.0, "{}", l.offset_hz);
        assert!(
            (l.cn0_dbhz(rate) - 50.0).abs() < 1.0,
            "{}",
            l.cn0_dbhz(rate)
        );
        assert!(l.share > 0.9);
    }

    #[test]
    fn the_loop_locks_and_hands_on_the_phase_modulation() {
        let rate = 200_000.0;
        let f0 = -3210.0;
        let fsc = 20_000.0;
        let m = 1.0;
        let x: Vec<Iq> = (0..200_000)
            .map(|k| {
                let t = k as f64 / rate;
                let ph = std::f64::consts::TAU * f0 * t
                    + m * (std::f64::consts::TAU * fsc * t).sin()
                    + 0.7;
                Iq::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect();
        let mut l = CarrierLoop::new(rate, f0 + 3.0, 50.0, bessel_j0(m));
        let mut phase = Vec::new();
        l.process(&x, &mut phase);
        let w = l.take_windows();
        let last = w.last().unwrap();
        assert!(last.lock > 0.99, "lock {}", last.lock);
        assert!((last.offset_hz - f0).abs() < 0.5);
        // Carrier power J₀(m)²; the phase out is the modulation.
        assert!((last.carrier - bessel_j0(m).powi(2)).abs() < 0.01);
        let k = 190_000;
        let want = m * (std::f64::consts::TAU * fsc * k as f64 / rate).sin();
        assert!((f64::from(phase[k]) - want).abs() < 0.05);
    }

    #[test]
    fn modulation_index_from_the_carrier() {
        for m in [0.3, 1.0, 1.3, 2.0] {
            let got = index_from_j0(bessel_j0(m)).unwrap();
            assert!((got - m).abs() < 1e-6);
        }
    }
}
