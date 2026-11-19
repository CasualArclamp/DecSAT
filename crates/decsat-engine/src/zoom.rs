// made by CasualArclamp
//! A finer spectrum of the part of the band on screen (from DecAero's
//! `zoom.rs`, same author).
//!
//! The waterfall's own transform has bins of `rate / fft size`. Zoomed in
//! past one bin a pixel, the view is mixed to zero and decimated (the VFOs'
//! [`Ddc`]), and a transform is taken over a stretch of it that grows with
//! the zoom, so the resolution follows the zoom smoothly. The stretch is
//! windowed at its own length and padded with zeros to a power of two, so
//! nothing jumps when the length passes one.
//!
//! The waterfall's own rows carry on beside these: a `Zoom` being started or
//! moved never takes anything off the screen, it only adds detail once it
//! has the samples for it.

use decsat_core::Iq;
use decsat_dsp::Ddc;
use rustfft::FftPlanner;

/// The longest stretch transformed, seconds (so the finest bins are 1 Hz).
pub const LONGEST_S: f64 = 1.0;
/// Most transforms averaged into one row.
const MOST_SEGMENTS: usize = 16;

/// One row (or the average) of the finer spectrum: `db[i]` is the level at
/// `lo_hz + i * step_hz` from the centre of the input.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ZoomRow {
    pub lo_hz: f64,
    pub step_hz: f64,
    pub db: Vec<f32>,
}

impl ZoomRow {
    /// The band the row covers, Hz from the centre of the input: each bin is
    /// `step_hz` wide about its own middle.
    pub fn edges(&self) -> (f64, f64) {
        let left = self.lo_hz - self.step_hz / 2.0;
        (left, left + self.step_hz * self.db.len() as f64)
    }
}

/// The stretch to transform, seconds, for a view `lo_hz..hi_hz` drawn
/// `pixels` wide, where the waterfall's own transform is `base_s` long;
/// `None` while that one is fine enough (a pixel is wider than about a bin).
/// From there the stretch grows as the square root of the zoom at first
/// (finer bins come at the price of slower ones), then, from four times in,
/// with the zoom itself: a bin every two pixels, which is as fine as the
/// window's own width lets the screen show, up to [`LONGEST_S`].
pub fn stretch(base_s: f64, lo_hz: f64, hi_hz: f64, pixels: u32) -> Option<f64> {
    // How far past one bin a pixel the view is.
    let zoom = f64::from(pixels) / ((hi_hz - lo_hz).max(1.0) * base_s);
    let s = base_s * zoom.sqrt().max(zoom / 2.0);
    (s > base_s * 1.25).then_some(s.min(LONGEST_S))
}

pub struct Zoom {
    ddc: Ddc,
    centre_hz: f64,
    /// Width of the band covered, around `centre_hz`.
    cover_hz: f64,
    /// Decimated samples, the newest last, and how many are worth keeping.
    samples: Vec<Iq>,
    keep: usize,
    /// Outputs still to drop: the filters' start-up transient.
    settle: usize,
    planner: FftPlanner<f32>,
    window: Vec<f32>,
    buf: Vec<Iq>,
    power: Vec<f32>,
    /// The running average (linear power), with the transform size it was
    /// taken at.
    average: Option<(usize, Vec<f32>)>,
}

impl Zoom {
    /// A zoom on `lo_hz..hi_hz` (from the centre of the input), covering
    /// twice that so the view can be moved and widened a little without
    /// starting again. `None` when the view is too wide to gain anything by
    /// decimating.
    pub fn new(in_rate: f64, lo_hz: f64, hi_hz: f64) -> Option<Self> {
        let cover_hz = 2.0 * (hi_hz - lo_hz);
        if cover_hz <= 0.0 || cover_hz > in_rate / 5.0 {
            return None;
        }
        let centre_hz = (lo_hz + hi_hz) / 2.0;
        let ddc = Ddc::new(in_rate, centre_hz, cover_hz);
        if ddc.decimation() < 2 {
            return None;
        }
        let keep = (ddc.out_rate() * 0.2) as usize;
        let settle = ddc.taps() / ddc.decimation() + 1;
        Some(Self {
            ddc,
            centre_hz,
            cover_hz,
            samples: Vec::new(),
            keep,
            settle,
            planner: FftPlanner::new(),
            window: Vec::new(),
            buf: Vec::new(),
            power: Vec::new(),
            average: None,
        })
    }

    /// Does this zoom still serve the view `lo_hz..hi_hz`? It must cover it,
    /// and not be so much wider that the transform is mostly off screen.
    pub fn fits(&self, lo_hz: f64, hi_hz: f64) -> bool {
        lo_hz >= self.centre_hz - self.cover_hz / 2.0
            && hi_hz <= self.centre_hz + self.cover_hz / 2.0
            && hi_hz - lo_hz >= self.cover_hz / 4.0
    }

    /// A block of the input.
    pub fn feed(&mut self, block: &[Iq]) {
        self.ddc.process(block, &mut self.samples);
        let drop = self.settle.min(self.samples.len());
        self.samples.drain(..drop);
        self.settle -= drop;
        if self.samples.len() > 2 * self.keep {
            let cut = self.samples.len() - self.keep;
            self.samples.drain(..cut);
        }
    }

    /// The input skipped some samples: what was gathered does not join on.
    pub fn gap(&mut self) {
        self.samples.clear();
        self.ddc.reset();
        self.settle = self.ddc.taps() / self.ddc.decimation() + 1;
    }

    /// The spectrum of the newest `seconds`, or of what has been gathered
    /// since the zoom began once that is an eighth of it (the detail then
    /// fills in as the rest arrives); `None` before. Where the stretch is shorter than a waterfall row lasts
    /// (`row_s`), the half-overlapped stretches of the row are averaged, as
    /// the waterfall's own rows are: the row turns from a smooth one into a
    /// single transform as the zoom deepens.
    pub fn row(&mut self, seconds: f64, row_s: f64) -> Option<ZoomRow> {
        let rate = self.ddc.out_rate();
        let full = ((seconds.min(LONGEST_S) * rate).round() as usize).max(64);
        let have = self.samples.len();
        let len = full.min(have);
        let hop = (len / 2).max(1);
        let wanted = ((2.0 * row_s / seconds) as usize)
            .saturating_sub(1)
            .clamp(1, MOST_SEGMENTS);
        self.keep = full + full / 2 * (wanted - 1) + (rate * row_s) as usize;
        if len < 64.max(full / 8) {
            return None;
        }
        let segments = wanted.min((have - len) / hop + 1);
        let n = len.next_power_of_two();
        if self.window.len() != len {
            self.window = (0..len)
                .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / len as f32).cos())
                .collect();
        }
        let fft = self.planner.plan_fft_forward(n);
        self.power.clear();
        self.power.resize(n, 0.0);
        for s in 0..segments {
            let end = have - s * hop;
            self.buf.clear();
            self.buf.extend(
                self.samples[end - len..end]
                    .iter()
                    .zip(&self.window)
                    .map(|(x, w)| x * w),
            );
            self.buf.resize(n, Iq::default());
            fft.process(&mut self.buf);
            for (p, b) in self.power.iter_mut().zip(&self.buf) {
                *p += b.norm_sqr();
            }
        }

        // On the waterfall's own scale (`Spectrum`: power over the transform
        // length), with the decimation put back: noise and modulated
        // carriers keep their colours whatever the resolution, and only a
        // steady tone stands higher the finer it is resolved.
        let scale = (self.ddc.decimation() as f64 / (len as f64 * segments as f64)) as f32;
        let step_hz = rate / n as f64;
        // The band covered is the middle of the transform.
        let half = ((self.cover_hz / 2.0 / step_hz) as isize).min(n as isize / 2);
        let lin: Vec<f32> = (-half..half)
            .map(|k| self.power[k.rem_euclid(n as isize) as usize] * scale)
            .collect();
        let db = lin.iter().map(|p| 10.0 * p.max(1e-20).log10()).collect();
        match &mut self.average {
            Some((size, average)) if *size == n => {
                for (a, p) in average.iter_mut().zip(&lin) {
                    *a += 0.15 * (p - *a);
                }
            }
            _ => self.average = Some((n, lin)),
        }
        Some(ZoomRow {
            lo_hz: self.centre_hz - half as f64 * step_hz,
            step_hz,
            db,
        })
    }

    /// The running average of the rows, for the spectrum trace.
    pub fn average(&self) -> Option<ZoomRow> {
        let (n, lin) = self.average.as_ref()?;
        let step_hz = self.ddc.out_rate() / *n as f64;
        Some(ZoomRow {
            lo_hz: self.centre_hz - (lin.len() / 2) as f64 * step_hz,
            step_hz,
            db: lin.iter().map(|p| 10.0 * p.max(1e-20).log10()).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spectrum::Spectrum;

    const RATE: f64 = 8e6;

    /// Noise of variance 2 (1 a part) and two tones 1600 Hz apart, 1.2 MHz
    /// under the centre.
    fn signal(seconds: f64) -> Vec<Iq> {
        let mut seed = 1u32;
        let mut noise = move || {
            // Twelve uniform draws make a fair bell curve.
            let mut sum = 0.0f32;
            for _ in 0..12 {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                sum += (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
            }
            sum
        };
        (0..(RATE * seconds) as usize)
            .map(|i| {
                let t = i as f64 / RATE;
                let (s1, c1) = (std::f64::consts::TAU * -1_200_800.0 * t).sin_cos();
                let (s2, c2) = (std::f64::consts::TAU * -1_199_200.0 * t).sin_cos();
                Iq::new(
                    (c1 + c2) as f32 * 0.05 + noise(),
                    (s1 + s2) as f32 * 0.05 + noise(),
                )
            })
            .collect()
    }

    #[test]
    fn resolves_finer_and_keeps_the_noise_level() {
        let input = signal(0.5);
        let fft = 4096;
        let base_s = fft as f64 / RATE;
        // A 40 kHz view 1600 pixels wide: the waterfall's bins are 1953 Hz.
        let (lo, hi) = (-1_220_000.0, -1_180_000.0);
        let seconds = stretch(base_s, lo, hi, 1600).unwrap();
        assert!(seconds > 30.0 * base_s && seconds < 0.03, "{seconds}");
        let mut zoom = Zoom::new(RATE, lo, hi).unwrap();
        assert!(zoom.fits(lo, hi));
        assert!(zoom.fits(lo - 10_000.0, hi - 10_000.0));
        assert!(!zoom.fits(lo, hi + 30_000.0) && !zoom.fits(-1_203_000.0, -1_197_000.0));
        let block = (RATE / 25.0) as usize;
        let mut row = None;
        for b in input.chunks(block) {
            zoom.feed(b);
            row = zoom.row(seconds, 0.04).or(row);
        }
        let row = row.unwrap();
        // A bin every two pixels (25 Hz each), or finer.
        assert!(row.step_hz < 51.0, "{}", row.step_hz);
        let (left, right) = row.edges();
        assert!(left <= lo && right >= hi, "{left} {right}");
        let at = |hz: f64| row.db[((hz - row.lo_hz) / row.step_hz).round() as usize];
        let (a, b, between) = (at(-1_200_800.0), at(-1_199_200.0), at(-1_200_000.0));
        assert!(
            a > between + 10.0 && b > between + 10.0,
            "{a} {between} {b}"
        );
        // The noise sits where the waterfall's own transform has it.
        let own = Spectrum::new(fft).compute(&input[..block]);
        let mean = |v: &mut dyn Iterator<Item = f32>| {
            let (sum, count) = v.fold((0.0, 0.0), |(s, c), p| (s + 10f32.powf(p / 10.0), c + 1.0));
            10.0 * (sum / count).log10()
        };
        let own_noise = mean(&mut own[fft / 2..].iter().copied());
        let far = |i: usize| (row.lo_hz + i as f64 * row.step_hz + 1_200_000.0).abs() > 3000.0;
        let noise = mean(&mut (0..row.db.len()).filter(|&i| far(i)).map(|i| row.db[i]));
        assert!((noise - own_noise).abs() < 0.5, "{noise} {own_noise}");
        // The same at the edge of the band covered: the filter is flat.
        let edge = mean(&mut row.db[..row.db.len() / 16].iter().copied());
        assert!((edge - own_noise).abs() < 1.0, "{edge} {own_noise}");
        let average = zoom.average().unwrap();
        assert_eq!((average.lo_hz, average.db.len()), (row.lo_hz, row.db.len()));
        // After a gap nothing is given until the stretch is gathered again.
        zoom.gap();
        assert!(zoom.row(seconds, 0.04).is_none());
    }

    #[test]
    fn no_zoom_on_a_wide_view() {
        assert!(Zoom::new(8e6, -1e6, 1e6).is_none());
        assert!(Zoom::new(8e6, -2e5, 2e5).is_some());
        // Nor while a pixel is wider than a bin.
        assert!(stretch(4096.0 / 8e6, -2e6, 2e6, 1600).is_none());
        assert!(stretch(4096.0 / 8e6, -2e5, 2e5, 1600).is_some());
    }
}
