// made by CasualArclamp
//! Spec-compliant F80T4.5X-8B transmitter, for known-good test vectors: every
//! receive stage is validated against this before it meets real IQ (a
//! receiver that only ever sees real signals cannot tell a spec misreading
//! from low SNR). Ported from DecBGAN `bgan/tx.py`. Never keys a radio.

use crate::frame::{self, C32};
use crate::spec::{self, Bearer};
use crate::tables::LevelTables;
use crate::turbo::turbo_encode;

/// One FEC block: scramble, turbo encode, puncture/interleave, map.
pub fn encode_block(data_bits: &[u8], t: &LevelTables) -> Vec<C32> {
    assert_eq!(data_bits.len(), t.d);
    let bits = frame::scramble(data_bits);
    let (df, p, q) = turbo_encode(&bits, &t.perm);
    let streams = [&df, &p, &q];
    (0..t.fecsy)
        .map(|s| {
            let mut slots = [0u8; 4];
            for (k, sl) in slots.iter_mut().enumerate() {
                let (kind, pos) = t.slot(s * 4 + k);
                *sl = streams[kind as usize][pos];
            }
            frame::bits4_to_symbol(slots)
        })
        .collect()
}

/// One frame from `nblocks` payloads, all at `t`'s level.
pub fn make_frame(payloads: &[Vec<u8>], t: &LevelTables, level: &str, b: &Bearer) -> Vec<C32> {
    assert_eq!(payloads.len(), b.nblocks);
    let blocks: Vec<C32> = payloads.iter().flat_map(|p| encode_block(p, t)).collect();
    frame::assemble_frame(&blocks, level, b)
}

/// Root-raised-cosine, unit energy (`span` symbols long at `sps` samples each).
pub fn rrc(beta: f64, sps: usize, span: usize) -> Vec<f32> {
    use std::f64::consts::PI;
    let n = (span * sps) as i64;
    let mut h: Vec<f64> = (-(n / 2)..=(n / 2))
        .map(|i| {
            let t = i as f64 / sps as f64;
            if t.abs() < 1e-9 {
                1.0 - beta + 4.0 * beta / PI
            } else if beta > 0.0 && (t.abs() - 1.0 / (4.0 * beta)).abs() < 1e-9 {
                beta / 2f64.sqrt()
                    * ((1.0 + 2.0 / PI) * (PI / (4.0 * beta)).sin()
                        + (1.0 - 2.0 / PI) * (PI / (4.0 * beta)).cos())
            } else {
                ((PI * t * (1.0 - beta)).sin() + 4.0 * beta * t * (PI * t * (1.0 + beta)).cos())
                    / (PI * t * (1.0 - (4.0 * beta * t).powi(2)))
            }
        })
        .collect();
    let e: f64 = h.iter().map(|x| x * x).sum::<f64>().sqrt();
    for x in &mut h {
        *x /= e;
    }
    h.into_iter().map(|x| x as f32).collect()
}

/// Deterministic xorshift generator for tests (no rand dependency).
pub struct Rng(pub u64);
impl Rng {
    pub fn next_u32(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 16) as u32
    }
    pub fn bit(&mut self) -> u8 {
        (self.next_u32() & 1) as u8
    }
    /// Uniform in (0,1).
    pub fn uniform(&mut self) -> f64 {
        (self.next_u32() as f64 + 0.5) / 4_294_967_296.0
    }
    /// Standard normal (Box-Muller).
    pub fn gauss(&mut self) -> f64 {
        let (u, v) = (self.uniform(), self.uniform());
        (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
    }
}

/// Random payloads for one frame.
pub fn random_payloads(t: &LevelTables, b: &Bearer, rng: &mut Rng) -> Vec<Vec<u8>> {
    (0..b.nblocks).map(|_| (0..t.d).map(|_| rng.bit()).collect()).collect()
}

/// Impairments for [`to_iq`].
#[derive(Debug, Clone, Copy)]
pub struct Impair {
    /// Es/N0 in dB, `None` for a clean signal.
    pub esn0_db: Option<f64>,
    /// Carrier offset, Hz.
    pub cfo_hz: f64,
    /// Symbol-clock error, ppm (positive: the transmitter's clock is fast).
    pub clock_ppm: f64,
    /// Fractional-symbol delay, 0..1.
    pub timing: f64,
}

impl Default for Impair {
    fn default() -> Self {
        Impair { esn0_db: None, cfo_hz: 0.0, clock_ppm: 0.0, timing: 0.0 }
    }
}

/// Pulse-shape `symbols` at `sps` samples per symbol (sample rate `rs*sps`,
/// `rs` the nominal symbol rate), then apply `imp`. The clock error is
/// applied by cubic-resampling the shaped waveform, i.e. as if the whole
/// transmitter ran `clock_ppm` fast.
pub fn to_iq(symbols: &[C32], sps: usize, imp: &Impair, rng: &mut Rng) -> Vec<C32> {
    let h = rrc(spec::ROLLOFF, sps, 16);
    let mut up = vec![C32::new(0.0, 0.0); symbols.len() * sps + h.len()];
    for (i, s) in symbols.iter().enumerate() {
        up[i * sps] = *s;
    }
    // FIR (complex x real taps)
    let n = symbols.len() * sps;
    let delay = h.len() / 2;
    let mut x: Vec<C32> = (0..n)
        .map(|i| {
            let mut acc = C32::new(0.0, 0.0);
            for (k, &hk) in h.iter().enumerate() {
                if let Some(v) = (i + delay).checked_sub(k).and_then(|j| up.get(j)) {
                    acc += *v * hk;
                }
            }
            acc
        })
        .collect();

    // timing delay + clock error by cubic interpolation at t' = (i + d)*(1 + ppm)
    if imp.timing != 0.0 || imp.clock_ppm != 0.0 {
        let step = 1.0 + imp.clock_ppm * 1e-6;
        let d0 = imp.timing * sps as f64;
        let m = ((n as f64 - 4.0 - d0) / step) as usize;
        let mut y = Vec::with_capacity(m);
        for i in 0..m {
            let t = d0 + i as f64 * step;
            let k = t.floor() as usize;
            if k < 1 || k + 2 >= n {
                y.push(C32::new(0.0, 0.0));
                continue;
            }
            let mu = (t - k as f64) as f32;
            let (a, b, c, d) = (x[k - 1], x[k], x[k + 1], x[k + 2]);
            let c1 = (c - a) * 0.5;
            let c2 = a - b * 2.5 + c * 2.0 - d * 0.5;
            let c3 = (d - a) * 0.5 + (b - c) * 1.5;
            y.push(((c3 * mu + c2) * mu + c1) * mu + b);
        }
        x = y;
    }

    let fs = spec::F80T45X8B.rs * sps as f64;
    if imp.cfo_hz != 0.0 {
        let w = 2.0 * std::f64::consts::PI * imp.cfo_hz / fs;
        for (i, v) in x.iter_mut().enumerate() {
            let (s, c) = (w * i as f64).sin_cos();
            *v *= C32::new(c as f32, s as f32);
        }
    }
    if let Some(e) = imp.esn0_db {
        // Es = mean sample power * sps; the unit-energy matched filter turns
        // per-sample variance v into per-symbol variance v, so v must equal
        // N0 (not N0*sps).
        let es = x.iter().map(|v| v.norm_sqr() as f64).sum::<f64>() / x.len() as f64 * sps as f64;
        let n0 = es / 10f64.powf(e / 10.0);
        let sigma = (n0 / 2.0).sqrt();
        for v in &mut x {
            *v += C32::new((sigma * rng.gauss()) as f32, (sigma * rng.gauss()) as f32);
        }
    }
    x
}
