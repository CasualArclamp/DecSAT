//! Averaged power spectrum for the GUI.

use decsat_core::Iq;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

/// Welch-style averaged periodogram with a Hann window and 50 % overlap.
pub struct Spectrum {
    fft: Arc<dyn Fft<f32>>,
    size: usize,
    window: Vec<f32>,
    /// Scratch buffers, kept across calls to avoid per-frame allocation.
    scratch: Vec<Iq>,
    accum: Vec<f32>,
}

impl Spectrum {
    pub fn new(size: usize) -> Self {
        assert!(
            size.is_power_of_two() && size >= 64,
            "fft size must be a power of two >= 64"
        );
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(size);
        // Hann window: cheap and good enough to keep sidelobes off the plot.
        let window = (0..size)
            .map(|k| {
                let x = std::f32::consts::TAU * k as f32 / size as f32;
                0.5 - 0.5 * x.cos()
            })
            .collect();
        Spectrum {
            fft,
            size,
            window,
            scratch: vec![Iq::new(0.0, 0.0); size],
            accum: vec![0.0; size],
        }
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// Average the periodogram over `samples` and return it in dB, FFT-shifted
    /// so index 0 is the most negative frequency and the centre is DC.
    pub fn compute(&mut self, samples: &[Iq]) -> Vec<f32> {
        self.compute_max(samples, usize::MAX)
    }

    /// As [`Self::compute`], but average at most `max_segments` FFTs, spread
    /// evenly over the block. A 20 MS/s waterfall row covers ~800 k samples;
    /// 64 well-spread segments give a smooth row at a fraction of the cost of
    /// transforming all of them.
    pub fn compute_max(&mut self, samples: &[Iq], max_segments: usize) -> Vec<f32> {
        self.compute_max_with(samples, max_segments, |_| {}).0
    }

    /// As [`Self::compute_max`], handing each segment's own power spectrum
    /// (linear, FFT-shifted, unnormalised) to `each` on the way — the burst
    /// meter's short frames — and returning the samples between segments
    /// with the row.
    pub fn compute_max_with(
        &mut self,
        samples: &[Iq],
        max_segments: usize,
        mut each: impl FnMut(&[f32]),
    ) -> (Vec<f32>, usize) {
        self.accum.fill(0.0);
        if samples.len() < self.size {
            return (vec![-200.0; self.size], self.size);
        }

        let full = (samples.len() - self.size) / (self.size / 2) + 1;
        let hop = if full > max_segments {
            (samples.len() - self.size) / max_segments.max(1)
        } else {
            self.size / 2
        };
        let half = self.size / 2;
        let mut frame = vec![0f32; self.size];
        let mut segments = 0usize;
        let mut start = 0usize;
        while start + self.size <= samples.len() {
            for k in 0..self.size {
                self.scratch[k] = samples[start + k] * self.window[k];
            }
            self.fft.process(&mut self.scratch);
            for k in 0..self.size {
                let p = self.scratch[k].norm_sqr();
                self.accum[k] += p;
                // FFT-shifted, as the row.
                frame[(k + half) % self.size] = p;
            }
            each(&frame);
            segments += 1;
            start += hop.max(1);
        }

        let norm = 1.0 / (segments.max(1) as f32 * self.size as f32);
        let half = self.size / 2;
        // fftshift while converting to dB: output bin k reads accumulator bin
        // k+half for the lower half (the negative frequencies live up top).
        let (neg, pos) = self.accum.split_at(half);
        let row = pos
            .iter()
            .chain(neg.iter())
            .map(|&p| 10.0 * (p * norm).max(1e-20).log10())
            .collect();
        (row, hop.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    #[test]
    fn tone_lands_in_the_right_bin() {
        let size = 1024;
        let mut sp = Spectrum::new(size);
        // A complex tone at +1/8 of the sample rate.
        let frac = 0.125f32;
        let n = size * 4;
        let samples: Vec<Iq> = (0..n)
            .map(|k| {
                let ph = TAU * frac * k as f32;
                Iq::new(ph.cos(), ph.sin())
            })
            .collect();

        let db = sp.compute(&samples);
        let peak = db
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(i, _)| i)
            .unwrap();

        // After the shift, DC is at size/2, so +0.125 fs sits an eighth above it.
        let expected = size / 2 + (frac * size as f32) as usize;
        assert!(
            peak.abs_diff(expected) <= 1,
            "peak at {peak}, expected ~{expected}"
        );
    }

    #[test]
    fn short_input_is_floor() {
        let mut sp = Spectrum::new(256);
        let db = sp.compute(&[Iq::new(1.0, 0.0); 10]);
        assert_eq!(db.len(), 256);
        assert!(db.iter().all(|&v| v < -100.0));
    }
}
