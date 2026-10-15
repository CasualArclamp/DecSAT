//! From the carrier's phase to soft bits: the subcarrier taken to
//! baseband by a Costas loop, the bit synchroniser, and bi-phase chips
//! paired into bits.

use decsat_core::Iq;
use decsat_dsp::Ddc;

/// A data subcarrier (or the phase itself, `freq_hz` 0: PCM/PM) brought to
/// a real data waveform: a down-converter on the phase, then — for a
/// subcarrier — a Costas loop for its phase.
pub struct SubDemod {
    ddc: Ddc,
    costas: bool,
    phase: f64,
    freq: f64,
    kp: f64,
    ki: f64,
    /// The arms' power, smoothed (normalises the loop's error).
    power: f64,
    /// I² and Q² accumulated: the loop's lock.
    arms: (f64, f64),
    input: Vec<Iq>,
    out: Vec<Iq>,
}

impl SubDemod {
    /// At `rate`, a subcarrier at `freq_hz` (0: none) whose data reach
    /// `half_width` Hz either side.
    pub fn new(rate: f64, freq_hz: f64, half_width: f64) -> Self {
        let mut bw = 2.0 * 1.25 * half_width;
        if freq_hz > 0.0 {
            bw = bw.min(2.0 * freq_hz).min(2.0 * (rate / 2.0 - freq_hz));
        }
        let bw = bw.clamp(200.0, rate);
        let ddc = Ddc::new(rate, freq_hz, bw);
        let out_rate = ddc.out_rate();
        // Loop bandwidth: a hundredth of the data's width (the subcarrier is
        // derived from a clean oscillator; its phase only wanders slowly).
        let bn = (half_width / 100.0).clamp(5.0, 500.0);
        let zeta = std::f64::consts::FRAC_1_SQRT_2;
        let wn = 2.0 * bn / (zeta + 1.0 / (4.0 * zeta));
        let t = 1.0 / out_rate;
        SubDemod {
            ddc,
            costas: freq_hz > 0.0,
            phase: 0.0,
            freq: 0.0,
            kp: 2.0 * zeta * wn * t,
            ki: (wn * t) * (wn * t),
            power: 0.0,
            arms: (0.0, 0.0),
            input: Vec::new(),
            out: Vec::new(),
        }
    }

    pub fn out_rate(&self) -> f64 {
        self.ddc.out_rate()
    }

    /// Output samples the down-converter's filters take to fill.
    pub fn settle(&self) -> usize {
        self.ddc.taps() / self.ddc.decimation() + 1
    }

    /// The subcarrier's frequency as now tracked, Hz.
    pub fn freq_hz(&self) -> f64 {
        self.ddc.offset_hz() + self.freq * self.out_rate() / std::f64::consts::TAU
    }

    /// Lock, 0..1: the share of the output's power in phase (the data) over
    /// what is out of phase — since the last call.
    pub fn take_lock(&mut self) -> f64 {
        let (i, q) = std::mem::take(&mut self.arms);
        if i + q > 0.0 { (i - q) / (i + q) } else { 0.0 }
    }

    /// Take phase samples; push the data waveform.
    pub fn process(&mut self, phi: &[f32], out: &mut Vec<f32>) {
        self.input.clear();
        self.input.extend(phi.iter().map(|&p| Iq::new(p, 0.0)));
        self.out.clear();
        self.ddc.process(&self.input, &mut self.out);
        out.reserve(self.out.len());
        for &z in &self.out {
            if !self.costas {
                out.push(z.re);
                self.arms.0 += f64::from(z.re * z.re);
                self.arms.1 += f64::from(z.im * z.im);
                continue;
            }
            let (sn, cs) = self.phase.sin_cos();
            let i = f64::from(z.re) * cs + f64::from(z.im) * sn;
            let q = f64::from(z.im) * cs - f64::from(z.re) * sn;
            self.power = 0.999 * self.power + 0.001 * (i * i + q * q);
            let e = (i * q / self.power.max(1e-12)).clamp(-1.0, 1.0);
            self.freq += self.ki * e;
            self.phase = (self.phase + self.freq + self.kp * e) % std::f64::consts::TAU;
            self.arms.0 += i * i;
            self.arms.1 += q * q;
            out.push(i as f32);
        }
    }
}

/// Bit (chip) synchroniser on a real data waveform: integrate-and-dump
/// chip by chip, its timing kept by a data-transition tracking loop (the
/// classic DTTL: a second integrator straddling each chip boundary, its
/// sign set by the transition — Simon, 1970; Lindsey & Simon,
/// "Telecommunication Systems Engineering", 1973, ch. 9).
pub struct BitSync {
    rate: f64,
    nominal: f64,
    period: f64,
    /// The next chip's start, samples from `buf[0]`.
    t: f64,
    buf: Vec<f32>,
    prefix: Vec<f64>,
    prev: Option<f32>,
    /// Mean |chip|, for the error's scale.
    amp: f64,
    chips: u64,
    kp: f64,
    ki: f64,
}

/// Loop bandwidths (per chip): pulling in, then held.
const BN_PULL: f64 = 0.01;
const BN_HOLD: f64 = 0.002;
const PULL_CHIPS: u64 = 2000;

fn loop_gains(bn_t: f64) -> (f64, f64) {
    let zeta = std::f64::consts::FRAC_1_SQRT_2;
    let th = bn_t / (zeta + 1.0 / (4.0 * zeta));
    let d = 1.0 + 2.0 * zeta * th + th * th;
    // The detector's gain is about 1.5 (2τ on each transition; three
    // chips in four have one on bi-phase, one in two on NRZ).
    (4.0 * zeta * th / d / 1.5, 4.0 * th * th / d / 1.5)
}

impl BitSync {
    /// Chips at `chip_rate` in a waveform at `rate`, the first starting
    /// `phase` samples in.
    pub fn new(rate: f64, chip_rate: f64, phase: f64) -> Self {
        let period = rate / chip_rate;
        let (kp, ki) = loop_gains(BN_PULL);
        BitSync {
            rate,
            nominal: period,
            period,
            t: phase,
            buf: Vec::new(),
            prefix: vec![0.0],
            prev: None,
            amp: 0.0,
            chips: 0,
            kp,
            ki,
        }
    }

    /// The chip rate as tracked, chips a second.
    pub fn chip_rate(&self) -> f64 {
        self.rate / self.period
    }

    /// ∫ of the buffer from `a` to `b` (samples, fractional).
    fn integral(&self, a: f64, b: f64) -> f64 {
        let n = self.buf.len();
        let f = |t: f64| {
            let t = t.clamp(0.0, n as f64 - 1e-9);
            let i = t.floor() as usize;
            self.prefix[i] + (t - i as f64) * f64::from(self.buf[i])
        };
        f(b) - f(a)
    }

    /// Take waveform samples; push each chip's integral (its mean level).
    pub fn push(&mut self, s: &[f32], out: &mut Vec<f32>) {
        let mut acc = *self.prefix.last().unwrap_or(&0.0);
        for &v in s {
            self.buf.push(v);
            acc += f64::from(v);
            self.prefix.push(acc);
        }
        loop {
            let p = self.period;
            if self.t + 1.5 * p + 2.0 >= self.buf.len() as f64 {
                break;
            }
            let i = self.integral(self.t, self.t + p);
            let chip = (i / p) as f32;
            self.amp = if self.chips == 0 {
                f64::from(chip.abs())
            } else {
                0.99 * self.amp + 0.01 * f64::from(chip.abs())
            };
            let mut e = 0.0;
            if let Some(prev) = self.prev
                && self.t - 0.5 * p >= 0.0
                && prev.signum() != chip.signum()
            {
                // Across the boundary at t: early, it holds more of the
                // chip before.
                let m = self.integral(self.t - 0.5 * p, self.t + 0.5 * p);
                let sign = if prev > 0.0 { 1.0 } else { -1.0 };
                e = (sign * m / (self.amp * p).max(1e-12)).clamp(-1.0, 1.0);
            }
            out.push(chip);
            self.prev = Some(chip);
            self.chips += 1;
            if self.chips == PULL_CHIPS {
                (self.kp, self.ki) = loop_gains(BN_HOLD);
            }
            self.period =
                (self.period * (1.0 + self.ki * e)).clamp(0.99 * self.nominal, 1.01 * self.nominal);
            self.t += self.period + self.kp * e * self.period;
        }
        // Keep from a chip before the next start (the boundary integrator
        // looks half a chip back).
        let keep = (self.t - self.period - 2.0).floor().max(0.0) as usize;
        if keep > 4096 {
            self.buf.drain(..keep);
            let base = self.prefix[keep];
            self.prefix.drain(..keep);
            for v in &mut self.prefix {
                *v -= base;
            }
            self.t -= keep as f64;
        }
    }
}

/// Bi-phase chips paired into bits: the pairing (from an even or an odd
/// chip) in which a bit's two chips differ, followed as the chips come —
/// a slip of the bit synchroniser moves it.
#[derive(Debug, Clone)]
pub struct Biphase {
    align: usize,
    score: [f32; 2],
    last: Option<f32>,
    index: u64,
    /// Times the pairing moved.
    pub moves: u64,
}

impl Default for Biphase {
    fn default() -> Self {
        Biphase {
            align: 0,
            score: [0.5, 0.5],
            last: None,
            index: 0,
            moves: 0,
        }
    }
}

impl Biphase {
    /// Take chips; push soft bits (first chip less second, halved: positive
    /// for a high-then-low bit, Bi-φ-L's "one").
    pub fn push(&mut self, chips: &[f32], bits: &mut Vec<f32>) {
        for &c in chips {
            if let Some(l) = self.last {
                let a = ((self.index - 1) % 2) as usize;
                let opposite = if l * c < 0.0 { 1.0 } else { 0.0 };
                self.score[a] = 0.98 * self.score[a] + 0.02 * opposite;
                if a == self.align {
                    bits.push((l - c) / 2.0);
                }
                let other = 1 - self.align;
                if self.score[other] > self.score[self.align] + 0.15 {
                    self.align = other;
                    self.moves += 1;
                }
            }
            self.last = Some(c);
            self.index += 1;
        }
    }
}

/// Eb/N0 from soft bits: (mean |b|)² over the variance of |b| — the bit's
/// SNR after the matched filter, 2·Eb/N0.
#[derive(Debug, Clone, Default)]
pub struct SnrMeter {
    s1: f64,
    s2: f64,
    n: u64,
    last_db: Option<f32>,
}

impl SnrMeter {
    pub fn push(&mut self, bits: &[f32]) {
        for &b in bits {
            let a = f64::from(b.abs());
            self.s1 += a;
            self.s2 += a * a;
            self.n += 1;
        }
        if self.n >= 4096 {
            let n = self.n as f64;
            let m = self.s1 / n;
            let var = (self.s2 / n - m * m).max(1e-12);
            let snr = m * m / var;
            let db = (10.0 * (snr / 2.0).log10()) as f32;
            self.last_db = Some(match self.last_db {
                Some(p) => 0.7 * p + 0.3 * db,
                None => db,
            });
            *self = SnrMeter {
                last_db: self.last_db,
                ..Default::default()
            };
        }
    }

    /// Eb/N0, dB (high SNRs read low: |b| folds the noise).
    pub fn eb_n0_db(&self) -> Option<f32> {
        self.last_db
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_sync_follows_a_clock_off_nominal_and_biphase_pairs_up() {
        let rate = 50_000.0;
        let rb = 2400.0 * (1.0 + 3e-4);
        let mut s = 7u32;
        let bits: Vec<u8> = (0..4000)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s & 1) as u8
            })
            .collect();
        let n = (bits.len() as f64 * rate / rb) as usize;
        let wave: Vec<f32> = (0..n)
            .map(|i| {
                let t = (i as f64 + 0.37) * rb / rate;
                let k = (t as usize).min(bits.len() - 1);
                let v = if bits[k] == 1 { 1.0 } else { -1.0 };
                if t.fract() >= 0.5 { -v } else { v }
            })
            .collect();
        // Started at the nominal rate, a quarter chip off.
        let mut sync = BitSync::new(rate, 4800.0, 2.6);
        let mut chips = Vec::new();
        for c in wave.chunks(1000) {
            sync.push(c, &mut chips);
        }
        assert!(
            (sync.chip_rate() - 2.0 * rb).abs() < 1.0,
            "{}",
            sync.chip_rate()
        );
        let mut bp = Biphase::default();
        let mut soft = Vec::new();
        bp.push(&chips, &mut soft);
        let got: Vec<u8> = soft.iter().map(|&b| u8::from(b > 0.0)).collect();
        // The last 3000 bits match, at some offset.
        let tail = &got[got.len() - 3000..];
        let found = (0..got.len() - 2000).any(|o| {
            o + 3000 <= bits.len()
                && tail
                    .iter()
                    .zip(&bits[o..o + 3000])
                    .filter(|(a, b)| a != b)
                    .count()
                    < 5
        });
        assert!(found);
    }
}
