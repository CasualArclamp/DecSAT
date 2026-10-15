//! DC (zero-frequency spike) removal for a wideband IQ stream.
//!
//! Direct-conversion receivers like the HackRF leave a DC offset — LO leakage
//! and ADC/mixer offsets — that shows as a spike at the centre frequency. It is
//! nearly static, so it is tracked by a one-pole low-pass on every sample (a
//! time constant of a fifth of a second: a notch under a hertz wide) and
//! subtracted. Sample by sample, not block by block: a block's mean taken
//! off whole held a slice of any carrier within a few tens of hertz of the
//! centre (a VFO recording replayed is centred on its carrier) and stepped
//! its phase at every block edge — Eutelsat 172B's beacon, 34 Hz off, lost
//! a frame in three. A carrier sitting exactly on the centre frequency still
//! loses its own DC component.

use decsat_core::Iq;

/// Time constant of the offset's estimate, seconds.
const TAU_S: f64 = 0.2;

/// Running-mean DC remover.
#[derive(Debug, Clone)]
pub struct DcBlocker {
    /// The low-pass's weight on each new sample.
    alpha: f64,
    mean: Option<(f64, f64)>,
}

impl DcBlocker {
    /// For a stream at `rate` samples a second.
    pub fn new(rate: f64) -> Self {
        DcBlocker {
            alpha: (1.0 / (TAU_S * rate)).min(1.0),
            mean: None,
        }
    }

    /// The DC offset currently being removed.
    pub fn offset(&self) -> Iq {
        let (re, im) = self.mean.unwrap_or((0.0, 0.0));
        Iq::new(re as f32, im as f32)
    }

    /// Update the estimate through `block` and subtract it, in place.
    pub fn process(&mut self, block: &mut [Iq]) {
        if block.is_empty() {
            return;
        }
        // Started on the first block's mean, so the offset goes at once.
        // f64: millions of f32 samples would lose the small mean.
        let (mut re, mut im) = *self.mean.get_or_insert_with(|| {
            let (sr, si) = block
                .iter()
                .fold((0f64, 0f64), |(r, i), s| (r + s.re as f64, i + s.im as f64));
            let n = block.len() as f64;
            (sr / n, si / n)
        });
        let a = self.alpha;
        for s in block.iter_mut() {
            re += a * (f64::from(s.re) - re);
            im += a * (f64::from(s.im) - im);
            s.re -= re as f32;
            s.im -= im as f32;
        }
        self.mean = Some((re, im));
    }

    /// Forget the estimate (a retune or a new source moves the offset).
    pub fn reset(&mut self) {
        self.mean = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_a_static_offset_and_leaves_a_tone() {
        let dc = Iq::new(0.03, -0.02);
        let w = std::f64::consts::TAU * 0.01;
        let mut b = DcBlocker::new(1e6);
        let mut last = Vec::new();
        for k in 0..10 {
            let mut block: Vec<Iq> = (0..100_000)
                .map(|n| {
                    let ph = w * (k * 100_000 + n) as f64;
                    Iq::new(ph.cos() as f32, ph.sin() as f32) * 0.1 + dc
                })
                .collect();
            b.process(&mut block);
            last = block;
        }
        assert!((b.offset() - dc).norm() < 1e-4, "offset {}", b.offset());
        // What is left has no mean, and the tone is untouched.
        let mean = last.iter().sum::<Iq>() / last.len() as f32;
        assert!(mean.norm() < 1e-4, "mean {mean}");
        let p = last.iter().map(|s| s.norm_sqr()).sum::<f32>() / last.len() as f32;
        assert!((p - 0.01).abs() < 1e-4, "tone power {p}");
    }

    #[test]
    fn a_carrier_near_the_centre_keeps_its_phase() {
        // A carrier 34 Hz off the centre, in waterfall-sized blocks that
        // hold no whole number of its cycles: its phase stays within a few
        // hundredths of a radian of where it was, with no step at a block
        // edge.
        let rate = 1_230_769.0;
        let w = std::f64::consts::TAU * 34.0 / rate;
        let mut b = DcBlocker::new(rate);
        let block = 65_536;
        let mut worst = 0f64;
        for k in 0..40 {
            let mut x: Vec<Iq> = (0..block)
                .map(|n| {
                    let ph = w * (k * block + n) as f64;
                    Iq::new(ph.cos() as f32, ph.sin() as f32)
                })
                .collect();
            b.process(&mut x);
            if k < 20 {
                continue;
            }
            for (n, s) in x.iter().enumerate() {
                let ph = w * (k * block + n) as f64;
                let err = (f64::from(s.arg()) - ph + std::f64::consts::PI)
                    .rem_euclid(std::f64::consts::TAU)
                    - std::f64::consts::PI;
                worst = worst.max(err.abs());
            }
        }
        assert!(worst < 0.03, "phase off by {worst} rad");
    }
}
