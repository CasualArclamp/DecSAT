// made by CasualArclamp
//! Block automatic gain control.
//!
//! Normalises a VFO's output to unit average power, which the timing loop's
//! gains, the frame correlator's threshold and the demapper all assume. The
//! gain is updated once per block from the block's mean power, smoothed so a
//! fade or a burst does not make it jump — per-sample AGC would chase the
//! modulation itself on APSK, whose rings differ in amplitude by design.

use decsat_core::Iq;

pub struct Agc {
    target: f32,
    /// Smoothing factor per block, 0 < a ≤ 1 (1 = no smoothing).
    alpha: f32,
    power: Option<f32>,
}

impl Agc {
    /// `alpha` sets how fast the gain follows: 0.2 settles in a handful of
    /// blocks.
    pub fn new(target_power: f32, alpha: f32) -> Self {
        Agc {
            target: target_power,
            alpha: alpha.clamp(1e-4, 1.0),
            power: None,
        }
    }

    /// Current linear gain applied to the amplitude.
    pub fn gain(&self) -> f32 {
        match self.power {
            Some(p) if p > 0.0 => (self.target / p).sqrt(),
            _ => 1.0,
        }
    }

    /// Scale `block` in place.
    pub fn process(&mut self, block: &mut [Iq]) {
        if block.is_empty() {
            return;
        }
        let p = block.iter().map(|s| s.norm_sqr()).sum::<f32>() / block.len() as f32;
        self.power = Some(match self.power {
            // The first block sets the level outright, so there is no slow
            // ramp from an arbitrary starting gain.
            None => p,
            Some(prev) => prev + self.alpha * (p - prev),
        });
        let g = self.gain();
        for s in block {
            *s *= g;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_to_target() {
        let mut agc = Agc::new(1.0, 0.5);
        let mut blk = vec![Iq::new(0.03, -0.04); 1000]; // power 0.0025
        agc.process(&mut blk);
        let p = blk.iter().map(|s| s.norm_sqr()).sum::<f32>() / blk.len() as f32;
        assert!((p - 1.0).abs() < 1e-4, "power {p}");
    }

    #[test]
    fn follows_a_level_change_smoothly() {
        let mut agc = Agc::new(1.0, 0.25);
        let mut quiet = vec![Iq::new(0.1, 0.0); 100];
        agc.process(&mut quiet);
        let g_quiet = agc.gain();
        // 20 dB louder: the gain should move towards the new level, not jump.
        let mut loud = vec![Iq::new(1.0, 0.0); 100];
        agc.process(&mut loud);
        let g_after_one = agc.gain();
        assert!(g_after_one < g_quiet && g_after_one > 1.0);
        for _ in 0..60 {
            let mut b = vec![Iq::new(1.0, 0.0); 100];
            agc.process(&mut b);
        }
        assert!((agc.gain() - 1.0).abs() < 1e-3, "gain {}", agc.gain());
    }
}
