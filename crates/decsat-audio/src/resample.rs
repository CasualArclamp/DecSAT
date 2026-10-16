// made by CasualArclamp
//! Stereo sample-rate conversion: whatever the stream is (48, 44.1, 32 kHz,
//! an HE-AAC core at 24 kHz, G.711 at 8 kHz) to the sound card's rate.
//!
//! Band-limited interpolation: a Blackman-windowed sinc, 32 taps, tabulated
//! at 256 phases and linearly interpolated between them, with its cutoff at
//! the lower of the two Nyquist rates. The step can be nudged while running
//! ([`Resampler::set_ratio`]), which is how the player holds its buffer level
//! against the drift between the broadcaster's clock and the sound card's.

use std::f64::consts::PI;

const TAPS: usize = 32;
const HALF: usize = TAPS / 2;
const PHASES: usize = 256;

pub struct Resampler {
    in_rate: u32,
    out_rate: u32,
    /// Input frames per output frame.
    step: f64,
    /// Nominal `step`, before any trim.
    nominal: f64,
    /// Position of the next output frame within `hist`, in input frames.
    pos: f64,
    hist: Vec<[f32; 2]>,
    /// `(PHASES + 1) × TAPS` kernel values.
    table: Vec<f32>,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        let nominal = in_rate as f64 / out_rate as f64;
        // Cutoff as a fraction of the input rate, a little inside Nyquist.
        let fc = 0.5 * (out_rate as f64 / in_rate as f64).min(1.0) * 0.94;
        let mut table = vec![0f32; (PHASES + 1) * TAPS];
        for p in 0..=PHASES {
            let frac = p as f64 / PHASES as f64;
            let mut sum = 0.0;
            let row = &mut table[p * TAPS..(p + 1) * TAPS];
            for (k, v) in row.iter_mut().enumerate() {
                // Tap k sits at input offset k − (HALF − 1) from the frame
                // before the output instant; t is the distance from it.
                let t = k as f64 - (HALF as f64 - 1.0) - frac;
                let x = 2.0 * fc * t;
                let sinc = if x.abs() < 1e-12 {
                    1.0
                } else {
                    (PI * x).sin() / (PI * x)
                };
                let n = (t + HALF as f64) / TAPS as f64; // 0..1 over the span
                let w = 0.42 - 0.5 * (2.0 * PI * n).cos() + 0.08 * (4.0 * PI * n).cos();
                let h = 2.0 * fc * sinc * w.max(0.0);
                *v = h as f32;
                sum += h;
            }
            // Unity gain at DC for every phase.
            for v in row.iter_mut() {
                *v /= sum as f32;
            }
        }
        Resampler {
            in_rate,
            out_rate,
            step: nominal,
            nominal,
            pos: (HALF - 1) as f64,
            hist: vec![[0.0; 2]; HALF - 1],
            table,
        }
    }

    pub fn rates(&self) -> (u32, u32) {
        (self.in_rate, self.out_rate)
    }

    /// Trim the conversion ratio: > 1 consumes input faster (fewer output
    /// frames), < 1 slower. Kept within ±1 %.
    pub fn set_ratio(&mut self, trim: f64) {
        self.step = self.nominal * trim.clamp(0.99, 1.01);
    }

    /// Convert interleaved stereo `input`, appending interleaved stereo to
    /// `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.hist.extend(input.as_chunks::<2>().0.iter().copied());
        // An output frame at `pos` needs frames up to floor(pos) + HALF.
        while (self.pos as usize) + HALF < self.hist.len() {
            let i = self.pos as usize;
            let frac = self.pos - i as f64;
            let ph = frac * PHASES as f64;
            let p0 = ph as usize;
            let a = (ph - p0 as f64) as f32;
            let k0 = &self.table[p0 * TAPS..(p0 + 1) * TAPS];
            let k1 = &self.table[(p0 + 1) * TAPS..(p0 + 2) * TAPS];
            let base = i + 1 - HALF;
            let (mut l, mut r) = (0f32, 0f32);
            for k in 0..TAPS {
                let h = k0[k] + a * (k1[k] - k0[k]);
                let x = self.hist[base + k];
                l += h * x[0];
                r += h * x[1];
            }
            out.push(l);
            out.push(r);
            self.pos += self.step;
        }
        // Drop what no output frame will reach back to.
        let keep_from = (self.pos as usize + 1).saturating_sub(HALF);
        if keep_from > 4096 {
            self.hist.drain(..keep_from);
            self.pos -= keep_from as f64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Amplitude and frequency of a stereo tone, by correlating with
    /// quadrature references at the expected frequency.
    fn tone_level(x: &[f32], f: f64, rate: f64) -> f64 {
        let n = x.len() / 2;
        let (mut c, mut s) = (0.0, 0.0);
        for i in 0..n {
            let ph = 2.0 * PI * f * i as f64 / rate;
            c += x[2 * i] as f64 * ph.cos();
            s += x[2 * i] as f64 * ph.sin();
        }
        2.0 * (c * c + s * s).sqrt() / n as f64
    }

    fn tone(f: f64, rate: f64, n: usize) -> Vec<f32> {
        (0..n)
            .flat_map(|i| {
                let v = (0.5 * (2.0 * PI * f * i as f64 / rate).sin()) as f32;
                [v, v]
            })
            .collect()
    }

    #[test]
    fn tones_keep_their_pitch_and_level() {
        for (fin, fout, f) in [
            (44_100, 48_000, 1000.0),
            (24_000, 48_000, 5000.0),
            (48_000, 44_100, 3000.0),
            (8_000, 48_000, 440.0),
        ] {
            let x = tone(f, fin as f64, fin as usize);
            let mut r = Resampler::new(fin, fout);
            let mut y = Vec::new();
            // In awkward pieces, as packets arrive.
            for piece in x.chunks(2 * 333) {
                r.process(piece, &mut y);
            }
            // One second in, one second out, less the half filter length
            // still waiting for input after it.
            let frames = y.len() / 2;
            let expect = fout as usize;
            let held_back = (HALF as f64 * fout as f64 / fin as f64) as usize + 2;
            assert!(
                frames <= expect + 1 && expect.saturating_sub(frames) <= held_back,
                "{fin}->{fout}: {frames} frames"
            );
            // Skip the filter's start-up, then measure at the output rate.
            let level = tone_level(&y[2000..], f, fout as f64);
            assert!((level - 0.5).abs() < 0.01, "{fin}->{fout}: level {level}");
        }
    }

    #[test]
    fn content_above_the_output_nyquist_is_removed() {
        // 20 kHz at 48 kHz in, 32 kHz out: above the new 16 kHz Nyquist.
        let x = tone(20_000.0, 48_000.0, 48_000);
        let mut r = Resampler::new(48_000, 32_000);
        let mut y = Vec::new();
        r.process(&x, &mut y);
        let rms = (y[2000..].iter().map(|v| v * v).sum::<f32>() / (y.len() - 2000) as f32).sqrt();
        assert!(rms < 0.01, "rms {rms}");
    }

    #[test]
    fn trimming_the_ratio_changes_the_output_count() {
        let x = tone(1000.0, 48_000.0, 48_000);
        let mut a = Resampler::new(48_000, 48_000);
        let mut b = Resampler::new(48_000, 48_000);
        b.set_ratio(1.005);
        let (mut ya, mut yb) = (Vec::new(), Vec::new());
        a.process(&x, &mut ya);
        b.process(&x, &mut yb);
        let (na, nb) = (ya.len() / 2, yb.len() / 2);
        assert!(na > nb + 200, "{na} vs {nb}");
    }
}
