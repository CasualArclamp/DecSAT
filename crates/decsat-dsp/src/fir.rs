// made by CasualArclamp
//! Streaming FIR filter with real taps and complex samples.

use decsat_core::Iq;

use crate::dot::fir_cr;

/// A complex-in/complex-out FIR with real taps, holding state across calls so
/// it can be fed block by block without edge artefacts.
///
/// The history is a plain `Vec` holding the last `taps − 1` inputs followed by
/// the new block, so the outputs are contiguous dot products (see
/// [`fir_cr`]); a ring buffer would save the copy but wrap its index on every
/// tap, which stops the inner loop vectorising.
pub struct Fir {
    /// Taps reversed, so the dot product runs over the history forwards.
    taps_rev: Vec<f32>,
    buf: Vec<Iq>,
}

impl Fir {
    pub fn new(mut taps: Vec<f32>) -> Self {
        assert!(!taps.is_empty(), "a filter needs at least one tap");
        taps.reverse();
        let n = taps.len();
        Fir {
            taps_rev: taps,
            buf: vec![Iq::new(0.0, 0.0); n - 1],
        }
    }

    pub fn len(&self) -> usize {
        self.taps_rev.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// Group delay in samples (half the tap count for a symmetric filter).
    pub fn delay(&self) -> usize {
        self.taps_rev.len() / 2
    }

    /// Push one sample and get the filtered output. Convenient but slow per
    /// sample; prefer [`Self::process`] for blocks.
    pub fn push(&mut self, x: Iq) -> Iq {
        let mut out = Vec::with_capacity(1);
        self.process(&[x], &mut out);
        out[0]
    }

    /// Filter a whole block, appending to `out`.
    pub fn process(&mut self, input: &[Iq], out: &mut Vec<Iq>) {
        let l = self.taps_rev.len();
        self.buf.extend_from_slice(input);
        fir_cr(&self.buf, &self.taps_rev, out);
        let drop = self.buf.len() - (l - 1);
        self.buf.drain(..drop);
    }

    /// Clear the history (e.g. after a retune).
    pub fn reset(&mut self) {
        self.buf.fill(Iq::new(0.0, 0.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn impulse_response_is_the_taps() {
        let taps = vec![0.5, -0.25, 0.125];
        let mut f = Fir::new(taps.clone());
        let mut out = Vec::new();
        let input = [
            Iq::new(1.0, 0.0),
            Iq::new(0.0, 0.0),
            Iq::new(0.0, 0.0),
            Iq::new(0.0, 0.0),
        ];
        f.process(&input, &mut out);
        for (k, &tap) in taps.iter().enumerate() {
            assert!(
                (out[k].re - tap).abs() < 1e-6,
                "tap {k}: {} != {tap}",
                out[k].re
            );
        }
        assert!(out[3].norm() < 1e-6);
    }

    #[test]
    fn dc_gain_is_the_tap_sum() {
        let mut f = Fir::new(vec![0.25f32; 4]);
        let mut out = Vec::new();
        f.process(&[Iq::new(1.0, 0.0); 16], &mut out);
        assert!((out[15].re - 1.0).abs() < 1e-6, "{}", out[15].re);
    }

    #[test]
    fn imaginary_part_is_filtered_too() {
        let mut f = Fir::new(vec![1.0, 1.0]);
        let mut out = Vec::new();
        f.process(&[Iq::new(0.0, 1.0), Iq::new(0.0, 1.0)], &mut out);
        assert!((out[1].im - 2.0).abs() < 1e-6);
        assert!(out[1].re.abs() < 1e-6);
    }

    #[test]
    fn reset_clears_history() {
        let mut f = Fir::new(vec![1.0, 1.0, 1.0]);
        let mut out = Vec::new();
        f.process(&[Iq::new(1.0, 0.0); 3], &mut out);
        f.reset();
        out.clear();
        f.process(&[Iq::new(1.0, 0.0)], &mut out);
        assert!((out[0].re - 1.0).abs() < 1e-6);
    }

    #[test]
    fn push_and_process_agree_across_blocks() {
        let taps: Vec<f32> = (0..37).map(|k| ((k as f32) * 0.3).sin() / 10.0).collect();
        let input: Vec<Iq> = (0..500)
            .map(|k| Iq::new((k as f32 * 0.1).cos(), (k as f32 * 0.07).sin()))
            .collect();

        let mut a = Fir::new(taps.clone());
        let by_push: Vec<Iq> = input.iter().map(|&x| a.push(x)).collect();

        let mut b = Fir::new(taps);
        let mut by_block = Vec::new();
        for c in input.chunks(61) {
            b.process(c, &mut by_block);
        }
        for (p, q) in by_push.iter().zip(&by_block) {
            assert!((p - q).norm() < 1e-5);
        }
    }
}
