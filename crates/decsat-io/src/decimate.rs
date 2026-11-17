// made by CasualArclamp
//! Decimation after a radio: the source's rate divided by 2, 4 … 64, for a
//! carrier much narrower than the radio's lowest rate (an Airspy's 2.5 or
//! 3 MS/s, a HackRF's 2 MS/s). Everything downstream — the FFT, the VFOs'
//! mixers and the demodulators — then runs on the smaller rate.
//!
//! A cascade of half-band FIR decimators, one per factor of two. A half-band
//! filter's every other tap is zero, so a stage costs a quarter of its
//! length in multiplies per output sample. Only the last stage has to be
//! sharp: an earlier one needs to reject just the bands that will fold onto
//! the final passband, which are far from its own passband, so it is short.
//! The first stage, at the full rate, is therefore the cheapest per sample.
//!
//! The taps are a Kaiser-windowed sinc (Kaiser's own length and β formulas,
//! "Nonrecursive digital filter design using the I0-sinh window function",
//! 1974). The last stage keeps the middle 80 % of the output span flat and
//! alias-free to better than 70 dB; the outer 10 % each side rolls off, as at
//! the edge of any radio's span.

use decsat_core::{Iq, Result};

use crate::IqSource;

/// The factors offered: none, then 2× to 64×.
pub const DECIMATIONS: [u32; 7] = [1, 2, 4, 8, 16, 32, 64];

/// The nearest factor offered at or below `n` (0 and 1 are "none").
pub fn snap_decimation(n: u32) -> u32 {
    DECIMATIONS
        .iter()
        .copied()
        .rev()
        .find(|&d| d <= n)
        .unwrap_or(1)
}

/// Taps of the last stage: transition 0.4–0.6 of its output Nyquist band.
const SHARP_TAPS: usize = 47;
/// Taps of the earlier stages: their passband is at most a quarter as wide.
const WIDE_TAPS: usize = 19;
/// Kaiser β for about 70 dB of stopband.
const BETA: f64 = 6.76;

/// Modified Bessel function of the first kind, order zero (power series).
fn bessel_i0(x: f64) -> f64 {
    let q = x * x / 4.0;
    let (mut sum, mut term) = (1.0, 1.0);
    for k in 1..40 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// The non-zero taps of an `n`-tap half-band low-pass, `n = 4k + 3`: the
/// outer taps at even distances from the end (one side; the filter is
/// symmetric), scaled so the whole filter has unity gain at DC with the
/// centre tap at exactly one half.
fn halfband(n: usize) -> Vec<f32> {
    debug_assert!(n % 4 == 3);
    let mid = (n - 1) / 2;
    let side: Vec<f64> = (0..mid)
        .step_by(2)
        .map(|i| {
            let k = i as f64 - mid as f64; // odd distance from the centre
            let sinc = (std::f64::consts::FRAC_PI_2 * k).sin() / (std::f64::consts::PI * k);
            let r = k / mid as f64;
            sinc * bessel_i0(BETA * (1.0 - r * r).sqrt()) / bessel_i0(BETA)
        })
        .collect();
    // The two sides together must sum to the other half.
    let sum: f64 = side.iter().sum::<f64>() * 2.0;
    side.iter().map(|&h| (h * 0.5 / sum) as f32).collect()
}

/// One divide-by-two stage.
struct Stage {
    /// One side's non-zero taps (see [`halfband`]).
    side: Vec<f32>,
    taps: usize,
    /// Samples not yet used up: the filter's history, then the new block.
    buf: Vec<Iq>,
}

impl Stage {
    fn new(taps: usize) -> Self {
        Stage {
            side: halfband(taps),
            taps,
            buf: Vec::new(),
        }
    }

    /// Filter and halve `input`, appending to `out`.
    fn run(&mut self, input: &[Iq], out: &mut Vec<Iq>) {
        self.buf.extend_from_slice(input);
        let n = self.taps;
        let mid = (n - 1) / 2;
        let mut used = 0;
        // Rust note: `windows`/`chunks` would do, but slicing each window
        // once lets the compiler drop the bounds checks in the tap loop.
        while used + n <= self.buf.len() {
            let w = &self.buf[used..used + n];
            let mut acc = w[mid] * 0.5;
            for (k, &h) in self.side.iter().enumerate() {
                acc += (w[2 * k] + w[n - 1 - 2 * k]) * h;
            }
            out.push(acc);
            used += 2;
        }
        // Keep the tail as the next block's history. Dropping an even count
        // keeps the output phase across blocks.
        self.buf.drain(..used);
    }
}

/// A source with its rate divided by a power of two.
pub struct Decimate {
    inner: Box<dyn IqSource>,
    factor: u32,
    stages: Vec<Stage>,
    raw: Vec<Iq>,
    tmp: Vec<Iq>,
}

impl Decimate {
    /// Divide `inner`'s rate by `factor` (snapped to one of [`DECIMATIONS`]).
    pub fn new(inner: Box<dyn IqSource>, factor: u32) -> Self {
        let factor = snap_decimation(factor);
        let count = factor.trailing_zeros() as usize;
        let stages = (0..count)
            .map(|i| {
                Stage::new(if i + 1 == count {
                    SHARP_TAPS
                } else {
                    WIDE_TAPS
                })
            })
            .collect();
        Decimate {
            inner,
            factor,
            stages,
            raw: Vec::new(),
            tmp: Vec::new(),
        }
    }

    /// `source` as it is for a factor of 1 (or 0), decimated otherwise: no
    /// copy and no extra call per block when there is nothing to do.
    pub fn wrap(source: Box<dyn IqSource>, factor: u32) -> Box<dyn IqSource> {
        if snap_decimation(factor) > 1 {
            Box::new(Decimate::new(source, factor))
        } else {
            source
        }
    }

    pub fn factor(&self) -> u32 {
        self.factor
    }
}

impl IqSource for Decimate {
    fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize> {
        out.clear();
        // A short block at 64× can yield nothing: read until something does.
        while out.is_empty() {
            if self.inner.read(&mut self.raw)? == 0 {
                return Ok(0);
            }
            // Ping-pong between two buffers down the cascade; the last stage
            // writes into the caller's.
            let Some(last) = self.stages.len().checked_sub(1) else {
                // A factor of one: the block as it came.
                std::mem::swap(out, &mut self.raw);
                break;
            };
            for (i, st) in self.stages.iter_mut().enumerate() {
                if i == last {
                    st.run(&self.raw, out);
                } else {
                    self.tmp.clear();
                    st.run(&self.raw, &mut self.tmp);
                    std::mem::swap(&mut self.raw, &mut self.tmp);
                }
            }
        }
        Ok(out.len())
    }

    fn sample_rate(&self) -> f64 {
        self.inner.sample_rate() / self.factor as f64
    }

    fn center_freq(&self) -> f64 {
        self.inner.center_freq()
    }

    fn describe(&self) -> String {
        format!(
            "{}, decimated {}× to {:.4} MS/s",
            self.inner.describe(),
            self.factor,
            self.sample_rate() / 1e6
        )
    }

    fn rewind(&mut self) -> Result<bool> {
        let done = self.inner.rewind()?;
        if done {
            for st in &mut self.stages {
                st.buf.clear();
            }
        }
        Ok(done)
    }

    fn is_live(&self) -> bool {
        self.inner.is_live()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A complex tone at `f` (cycles per input sample), in odd-sized blocks
    /// so the stages' carry-over between blocks is exercised.
    struct Tone {
        f: f64,
        n: usize,
        left: usize,
    }

    impl IqSource for Tone {
        fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize> {
            out.clear();
            let take = self.left.min(1001);
            for _ in 0..take {
                let ph = std::f64::consts::TAU * self.f * self.n as f64;
                out.push(Iq::new(ph.cos() as f32, ph.sin() as f32));
                self.n += 1;
            }
            self.left -= take;
            Ok(take)
        }
        fn sample_rate(&self) -> f64 {
            6e6
        }
        fn describe(&self) -> String {
            "tone".into()
        }
    }

    /// Output level in dB of a unit tone at `f` after decimating by `factor`,
    /// and the sample count that came out.
    fn level(f: f64, factor: u32) -> (f64, usize) {
        let total = 400_000;
        let mut d = Decimate::new(
            Box::new(Tone {
                f,
                n: 0,
                left: total,
            }),
            factor,
        );
        let mut all = Vec::new();
        let mut buf = Vec::new();
        while d.read(&mut buf).unwrap() > 0 {
            all.extend_from_slice(&buf);
        }
        // Skip the filters' start-up.
        let settled = &all[all.len() / 4..];
        let p: f64 =
            settled.iter().map(|s| s.norm_sqr() as f64).sum::<f64>() / settled.len() as f64;
        (10.0 * p.log10(), all.len())
    }

    #[test]
    fn snaps_to_the_factors_offered() {
        assert_eq!(snap_decimation(0), 1);
        assert_eq!(snap_decimation(1), 1);
        assert_eq!(snap_decimation(2), 2);
        assert_eq!(snap_decimation(3), 2);
        assert_eq!(snap_decimation(64), 64);
        assert_eq!(snap_decimation(1000), 64);
    }

    #[test]
    fn half_band_taps_have_unity_gain() {
        for n in [WIDE_TAPS, SHARP_TAPS] {
            let side = halfband(n);
            assert_eq!(side.len(), n.div_ceil(4));
            let dc: f32 = 0.5 + 2.0 * side.iter().sum::<f32>();
            assert!((dc - 1.0).abs() < 1e-6, "{n} taps: DC gain {dc}");
        }
    }

    #[test]
    fn rate_and_sample_count_divide() {
        for factor in DECIMATIONS {
            let (_, n) = level(0.0, factor);
            let d = Decimate::new(
                Box::new(Tone {
                    f: 0.0,
                    n: 0,
                    left: 0,
                }),
                factor,
            );
            assert_eq!(d.sample_rate(), 6e6 / factor as f64);
            let want = 400_000 / factor as usize;
            // Short only by the filters' fill.
            assert!(n <= want && n + 64 > want, "{factor}×: {n} of {want}");
        }
    }

    #[test]
    fn passband_is_flat() {
        for factor in DECIMATIONS.into_iter().skip(1) {
            // DC, and ±40 % of the output rate: the edge of the flat part.
            for frac in [0.0, 0.2, 0.4, -0.4] {
                let (db, _) = level(frac / factor as f64, factor);
                assert!(
                    db.abs() < 0.1,
                    "{factor}× at {frac} of the span: {db:.3} dB"
                );
            }
        }
    }

    #[test]
    fn what_would_alias_into_the_passband_is_rejected() {
        for factor in DECIMATIONS.into_iter().skip(1) {
            let out = 1.0 / factor as f64; // output rate, cycles per input sample
            // Every image of the flat ±40 %: tones that land at 0, ±0.2 and
            // ±0.4 of the output span after folding.
            for image in 1..factor {
                for frac in [-0.4, -0.2, 0.0, 0.2, 0.4] {
                    let f = (image as f64 + frac) * out;
                    let f = if f > 0.5 { f - 1.0 } else { f };
                    let (db, _) = level(f, factor);
                    assert!(db < -65.0, "{factor}×, image {image} {frac:+}: {db:.1} dB");
                }
            }
        }
    }

    #[test]
    fn a_factor_of_one_is_the_source_itself() {
        let src = Decimate::wrap(
            Box::new(Tone {
                f: 0.0,
                n: 0,
                left: 10,
            }),
            1,
        );
        assert_eq!(src.describe(), "tone");
        assert_eq!(src.sample_rate(), 6e6);
    }
}
