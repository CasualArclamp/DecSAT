// made by CasualArclamp
//! The modulator as a source of samples: a PLFRAME at a time, scaled to
//! the level asked for, as 8-bit IQ for the HackRF or as floats for a file;
//! with what the window shows about it (a spectrum of what goes out, the
//! peak, clipping, how busy the modulator is).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use decsat_core::Iq;
use decsat_mod::datv::{DatvConfig, DatvModulator, FeedCounters, LiveMode, TsInput};
use rustfft::{Fft, FftPlanner};

/// Spectrum bins (one FFT of this many samples).
pub const SPECTRUM_BINS: usize = 1024;
/// Spectra a second.
const SPECTRA_PER_SEC: f64 = 20.0;

/// What the pump has done, shared with the window. Relaxed atomics.
#[derive(Debug, Default)]
pub struct PumpStats {
    pub frames: AtomicU64,
    pub samples: AtomicU64,
    /// Nanoseconds spent making samples: over the wall time, the
    /// modulator's share of a core.
    pub busy_ns: AtomicU64,
    /// 8-bit samples (I or Q) clipped at ±127.
    pub clipped: AtomicU64,
    /// The largest |I| or |Q| in the last spectrum's span, of 127.
    pub peak: AtomicU64,
}

/// The modulator and its level.
pub struct Pump {
    m: DatvModulator,
    gain: f32,
    buf: Vec<Iq>,
    pub stats: Arc<PumpStats>,
    pub counters: Arc<FeedCounters>,
    /// The frame format, changeable while it runs.
    pub live: Arc<LiveMode>,
    /// The latest spectrum, dB (power, full scale 0 dB), centre in the
    /// middle.
    pub spectrum: Arc<Mutex<Vec<f32>>>,
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    fft_buf: Vec<rustfft::num_complex::Complex32>,
    every: u64,
    next_at: u64,
    peak: u8,
}

impl Pump {
    /// `level`: RMS of full scale (0.25 leaves room for every
    /// constellation's peaks).
    pub fn new(config: DatvConfig, input: Box<dyn TsInput>, level: f32) -> Self {
        let m = DatvModulator::new(config, input);
        let fft = FftPlanner::new().plan_fft_forward(SPECTRUM_BINS);
        let n = SPECTRUM_BINS as f32;
        let window = (0..SPECTRUM_BINS)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n).cos())
            .collect();
        Pump {
            counters: m.counters.clone(),
            live: m.live.clone(),
            m,
            gain: level.clamp(0.01, 0.7),
            buf: Vec::new(),
            stats: Arc::new(PumpStats::default()),
            spectrum: Arc::new(Mutex::new(vec![-120.0; SPECTRUM_BINS])),
            fft,
            window,
            fft_buf: Vec::with_capacity(SPECTRUM_BINS),
            every: (config.sample_rate() / SPECTRA_PER_SEC) as u64,
            next_at: 0,
            peak: 0,
        }
    }

    pub fn config(&self) -> DatvConfig {
        self.m.config
    }

    pub fn input_described(&self) -> String {
        self.m
            .input_described
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    /// One PLFRAME as cs8, appended to `out`: what the HackRF's thread pulls.
    pub fn fill_cs8(&mut self, out: &mut Vec<u8>) {
        let t = Instant::now();
        self.buf.clear();
        self.m.next_frame(&mut self.buf);
        let g = 127.0 * self.gain;
        out.reserve(self.buf.len() * 2);
        let mut clipped = 0u64;
        let mut peak = self.peak;
        for s in &self.buf {
            let (i, q) = (s.re * g, s.im * g);
            clipped += u64::from(i.abs() > 127.0) + u64::from(q.abs() > 127.0);
            let (i, q) = (
                i.round().clamp(-127.0, 127.0),
                q.round().clamp(-127.0, 127.0),
            );
            peak = peak.max(i.abs() as u8).max(q.abs() as u8);
            out.push(i as i8 as u8);
            out.push(q as i8 as u8);
        }
        self.peak = peak;
        if clipped > 0 {
            self.stats.clipped.fetch_add(clipped, Ordering::Relaxed);
        }
        self.after(t);
    }

    /// One PLFRAME as floats (full scale 1), appended to `out`.
    pub fn fill_iq(&mut self, out: &mut Vec<Iq>) {
        let t = Instant::now();
        self.buf.clear();
        self.m.next_frame(&mut self.buf);
        let g = self.gain;
        let mut peak = 0f32;
        out.extend(self.buf.iter().map(|s| {
            peak = peak.max(s.re.abs()).max(s.im.abs());
            *s * g
        }));
        // As the HackRF would see it: 8-bit full scale is 127.
        let p8 = (peak * g * 127.0).min(127.0) as u8;
        self.peak = self.peak.max(p8);
        if peak * g > 1.0 {
            self.stats.clipped.fetch_add(1, Ordering::Relaxed);
        }
        self.after(t);
    }

    fn after(&mut self, t: Instant) {
        let n = self.buf.len() as u64;
        let done = self.stats.samples.fetch_add(n, Ordering::Relaxed) + n;
        self.stats.frames.fetch_add(1, Ordering::Relaxed);
        if done >= self.next_at && self.buf.len() >= SPECTRUM_BINS {
            self.next_at = done + self.every;
            self.spectrum_of_frame();
            self.stats
                .peak
                .store(u64::from(self.peak), Ordering::Relaxed);
            self.peak = 0;
        }
        self.stats
            .busy_ns
            .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    /// A spectrum of this frame's first samples (as sent: scaled), averaged
    /// into the shown one.
    fn spectrum_of_frame(&mut self) {
        self.fft_buf.clear();
        for (s, w) in self.buf.iter().zip(&self.window) {
            self.fft_buf.push(*s * (self.gain * w));
        }
        self.fft.process(&mut self.fft_buf);
        // Full scale: a full-scale tone through the Hann window peaks at
        // (N/2)².
        let norm = (SPECTRUM_BINS as f32 / 2.0).powi(2);
        let mut sp = self.spectrum.lock().unwrap();
        let half = SPECTRUM_BINS / 2;
        for (k, c) in self.fft_buf.iter().enumerate() {
            let db = 10.0 * (c.norm_sqr() / norm).max(1e-14).log10();
            // FFT order to centred.
            let at = (k + half) % SPECTRUM_BINS;
            sp[at] = 0.85 * sp[at] + 0.15 * db;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decsat_mod::datv::TsTest;

    #[test]
    fn the_pump_scales_counts_and_draws_a_spectrum() {
        let cfg = DatvConfig::default();
        let mut p = Pump::new(cfg, Box::new(TsTest::new()), 0.25);
        let mut out = Vec::new();
        for _ in 0..30 {
            p.fill_cs8(&mut out);
        }
        let rms =
            (out.iter().map(|&b| (b as i8 as f64).powi(2)).sum::<f64>() / out.len() as f64).sqrt();
        assert!((rms / 127.0 - 0.25 / 2f64.sqrt()).abs() < 0.03, "rms {rms}");
        assert_eq!(p.stats.frames.load(Ordering::Relaxed), 30);
        assert_eq!(p.stats.clipped.load(Ordering::Relaxed), 0);
        let sp = p.spectrum.lock().unwrap().clone();
        // The carrier (1 MS/s at 4 samples a symbol: the middle half) well
        // above the band's edges.
        let mid = sp[SPECTRUM_BINS / 2 - 50..SPECTRUM_BINS / 2 + 50]
            .iter()
            .sum::<f32>()
            / 100.0;
        let edge = sp[..40].iter().sum::<f32>() / 40.0;
        assert!(mid - edge > 30.0, "mid {mid} edge {edge}");
    }
}
