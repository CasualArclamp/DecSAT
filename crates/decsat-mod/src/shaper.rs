// made by CasualArclamp
//! Root-raised-cosine pulse shaping at an integer number of samples per symbol.
//!
//! Zero-stuffs each symbol by `sps` and filters with RRC taps scaled so the
//! output has the same average power as the symbols. Integer `sps` is enough
//! for a test-signal generator: the receiver's DDC produces fractional
//! samples-per-symbol on its own whenever the VFO bandwidth does not divide the
//! sample rate evenly, which is the case the timing loop has to handle.

use decsat_core::Iq;
use decsat_dsp::{Fir, rrc_taps};

pub struct Shaper {
    sps: usize,
    fir: Fir,
    gain: f32,
}

impl Shaper {
    /// # Panics
    /// If `sps < 2`.
    pub fn new(sps: usize, alpha: f64, span_symbols: usize) -> Self {
        assert!(sps >= 2, "need at least 2 samples per symbol");
        Shaper {
            sps,
            fir: Fir::new(rrc_taps(sps as f64, alpha, span_symbols)),
            // Unit-energy taps on a zero-stuffed stream divide the power by sps.
            gain: (sps as f32).sqrt(),
        }
    }

    /// Group delay in output samples.
    pub fn delay(&self) -> usize {
        self.fir.delay()
    }

    /// Shape `symbols`, appending `symbols.len() * sps` samples to `out`.
    pub fn process(&mut self, symbols: &[Iq], out: &mut Vec<Iq>) {
        // Zero-stuff a chunk at a time, then filter it as one block.
        let mut stuffed = Vec::with_capacity(4096 * self.sps);
        for chunk in symbols.chunks(4096) {
            stuffed.clear();
            for &s in chunk {
                stuffed.push(s * self.gain);
                stuffed.extend(std::iter::repeat_n(Iq::new(0.0, 0.0), self.sps - 1));
            }
            self.fir.process(&stuffed, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_average_power() {
        let mut sh = Shaper::new(4, 0.2, 16);
        let k = std::f32::consts::FRAC_1_SQRT_2;
        let syms: Vec<Iq> = (0..20_000)
            .map(|i| match i % 4 {
                0 => Iq::new(k, k),
                1 => Iq::new(-k, k),
                2 => Iq::new(-k, -k),
                _ => Iq::new(k, -k),
            })
            .collect();
        let mut out = Vec::new();
        sh.process(&syms, &mut out);
        let p: f32 =
            out[1000..].iter().map(|s| s.norm_sqr()).sum::<f32>() / (out.len() - 1000) as f32;
        assert!((p - 1.0).abs() < 0.05, "power {p}");
        assert_eq!(out.len(), syms.len() * 4);
    }
}
