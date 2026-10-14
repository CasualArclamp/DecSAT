//! Bursty bands across the wideband span: TDMA channels, where terminals
//! take turns and the spectrum comes and goes, found apart from the
//! continuous (SCPC) carriers the averaged spectrum shows.
//!
//! An average hides bursts: a TDMA return a few dB over the noise and on
//! part of the time barely lifts it, and a detector working on it misses
//! the channel or merges it into a neighbour. What gives bursts away is how
//! each bin's power varies from one short frame to the next. Averaged over
//! a few bins and a few milliseconds (shorter than any burst), a stationary
//! bin — noise, a continuous carrier's top, its skirts — varies by a fixed
//! share of its mean, set by the averaging alone (its coefficient of
//! variation, ~0.3 here). A bin that is on for a burst and off between
//! varies far more: 0.7–1.9 for a live Intelsat 19 MF-TDMA group whose
//! bursts stood only ~5 dB over the noise, while every continuous carrier
//! beside it, 1 to 17 dB up, stayed at the noise's 0.2–0.3. So: the noise's
//! variation measured on the noise itself, and bins varying 1.6× as much
//! and standing over the floor are bursty; neighbouring ones make a band.
//!
//! Not every burst is TDMA. A HackRF tuned below 2.17 GHz mixes up to an IF
//! of 2650 − f/7 MHz (its firmware, `tuning.c`), inside the 2.4 GHz band,
//! and nearby Bluetooth leaks in: [`bluetooth_offsets`] says where, so such
//! a band can be told for what it is.

/// A band whose power comes and goes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BurstBand {
    /// Its edges relative to the wideband centre, Hz.
    pub lo_hz: f64,
    pub hi_hz: f64,
    /// Its mean power over the noise floor, dB.
    pub level_db: f32,
    /// How much more its power varies than the noise's (the coefficient of
    /// variation's ratio): 1 for anything steady.
    pub variation: f32,
}

impl BurstBand {
    pub fn center_hz(&self) -> f64 {
        (self.lo_hz + self.hi_hz) / 2.0
    }

    pub fn width_hz(&self) -> f64 {
        self.hi_hz - self.lo_hz
    }
}

/// Each averaged frame lasts about this long, s: shorter than any TDMA
/// burst, long enough to steady the noise.
const FRAME_SECS: f64 = 2e-3;
/// Bins are smoothed over about this width, Hz (narrower than a TDMA
/// channel; single-bin noise fades).
const SMOOTH_HZ: f64 = 4e3;
/// Bursty: varying this many times as much as the noise, and standing this
/// far over the floor (power ratio).
const VARIATION_MIN: f32 = 1.6;
const LEVEL_MIN: f32 = 1.15;
/// Gaps bridged inside a band, and its least width, Hz: an MF-TDMA
/// group's carriers sit a few kHz apart and make one band (one VFO takes
/// the group; its analyser finds the carriers in it).
const GAP_HZ: f64 = 20e3;
const MIN_WIDTH_HZ: f64 = 3e3;
/// Averaged frames wanted before judging.
const MIN_FRAMES: usize = 40;

/// Gathers short-frame spectra and finds the bursty bands in them.
#[derive(Default)]
pub struct BurstMeter {
    rate: f64,
    /// Bins smoothed over, FFT frames per averaged frame.
    smooth: usize,
    per: usize,
    /// The averaged frame being built, and FFT frames in it.
    acc: Vec<f32>,
    in_acc: usize,
    /// Per bin: sum and sum of squares of the averaged frames' powers.
    sum: Vec<f64>,
    sq: Vec<f64>,
    frames: usize,
}

impl BurstMeter {
    /// One FFT frame's power per bin (linear, FFT-shifted: bin 0 is
    /// −rate/2) at sample rate `rate`, frames coming every `spacing` s.
    pub fn push(&mut self, power: &[f32], rate: f64, spacing: f64) {
        let n = power.len();
        if n < 16 || spacing <= 0.0 {
            return;
        }
        let per = ((FRAME_SECS / spacing).round() as usize).max(1);
        if self.acc.len() != n || self.rate != rate || self.per != per {
            // New FFT size or rate: start over.
            let bin_hz = rate / n as f64;
            *self = BurstMeter {
                rate,
                smooth: ((SMOOTH_HZ / bin_hz).round() as usize).clamp(1, 9),
                per,
                acc: vec![0.0; n],
                in_acc: 0,
                sum: vec![0.0; n],
                sq: vec![0.0; n],
                frames: 0,
            };
        }
        for (a, &p) in self.acc.iter_mut().zip(power) {
            *a += p;
        }
        self.in_acc += 1;
        if self.in_acc < self.per {
            return;
        }
        // The averaged frame, smoothed over neighbouring bins (a running
        // sum: the same for every bin's window).
        let half = self.smooth / 2;
        let mut run = 0f64;
        let mut smoothed = vec![0f64; n];
        for i in 0..n + half {
            if i < n {
                run += self.acc[i] as f64;
            }
            if i >= self.smooth && i - self.smooth < n {
                run -= self.acc[i - self.smooth] as f64;
            }
            if i >= half && i - half < n {
                smoothed[i - half] = run / self.smooth as f64 / self.in_acc as f64;
            }
        }
        for ((s, q), &v) in self.sum.iter_mut().zip(&mut self.sq).zip(&smoothed) {
            *s += v;
            *q += v * v;
        }
        self.frames += 1;
        self.acc.iter_mut().for_each(|a| *a = 0.0);
        self.in_acc = 0;
    }

    /// Averaged frames gathered since the last [`BurstMeter::take`].
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// The bursty bands in what was gathered (none until enough was), and
    /// a fresh start.
    pub fn take(&mut self) -> Vec<BurstBand> {
        let bands = self.bands();
        self.sum.iter_mut().for_each(|s| *s = 0.0);
        self.sq.iter_mut().for_each(|s| *s = 0.0);
        self.frames = 0;
        bands
    }

    fn bands(&self) -> Vec<BurstBand> {
        let n = self.sum.len();
        if self.frames < MIN_FRAMES || n < 16 {
            return Vec::new();
        }
        let k = self.frames as f64;
        let mean: Vec<f32> = self.sum.iter().map(|&s| (s / k) as f32).collect();
        let cv: Vec<f32> = self
            .sum
            .iter()
            .zip(&self.sq)
            .map(|(&s, &q)| {
                let m = s / k;
                ((q / k - m * m).max(0.0).sqrt() / m.max(1e-30)) as f32
            })
            .collect();
        // The passband (no filter stopbands at the edges), its noise floor
        // (the 20th percentile: a busy span is mostly carriers), and the
        // noise's own variation, measured on the bins at the floor.
        let db: Vec<f32> = mean.iter().map(|&m| 10.0 * m.max(1e-30).log10()).collect();
        let (lo, hi) = crate::carriers::passband(&db);
        let mut sorted: Vec<f32> = mean[lo..=hi].to_vec();
        sorted.sort_by(f32::total_cmp);
        let floor = sorted[sorted.len() / 5].max(1e-30);
        let mut quiet: Vec<f32> = (lo..=hi)
            .filter(|&i| mean[i] < 1.3 * floor)
            .map(|i| cv[i])
            .collect();
        if quiet.len() < 16 {
            return Vec::new();
        }
        quiet.sort_by(f32::total_cmp);
        let base = quiet[quiet.len() / 2].max(1e-6);
        let bin_hz = self.rate / n as f64;
        let bursty = |i: usize| cv[i] > VARIATION_MIN * base && mean[i] > LEVEL_MIN * floor;
        let gap = ((GAP_HZ / bin_hz).ceil() as usize).max(1);
        let mut out = Vec::new();
        let mut i = lo;
        while i <= hi {
            if !bursty(i) {
                i += 1;
                continue;
            }
            // A band: bursty bins with gaps of at most `gap` between.
            let start = i;
            let mut end = i;
            let mut j = i + 1;
            while j <= hi && j - end <= gap {
                if bursty(j) {
                    end = j;
                }
                j += 1;
            }
            let width = (end - start + 1) as f64 * bin_hz;
            if width >= MIN_WIDTH_HZ {
                let m = mean[start..=end].iter().sum::<f32>() / (end - start + 1) as f32;
                let mut v: Vec<f32> = (start..=end)
                    .filter(|&b| bursty(b))
                    .map(|b| cv[b])
                    .collect();
                v.sort_by(f32::total_cmp);
                out.push(BurstBand {
                    lo_hz: (start as f64 - n as f64 / 2.0) * bin_hz,
                    hi_hz: (end as f64 + 1.0 - n as f64 / 2.0) * bin_hz,
                    level_db: 10.0 * (m / floor).log10(),
                    variation: v[v.len() / 2] / base,
                });
            }
            i = end + 1;
        }
        out
    }
}

/// The bands of several takes as one list: overlapping (or touching)
/// bands merged, keeping the widest extent, the highest level and
/// variation. A slow TDMA carrier bursting every few seconds shows in some
/// takes and not others; the last few together keep it on the waterfall.
pub fn union(takes: &[Vec<BurstBand>]) -> Vec<BurstBand> {
    let mut all: Vec<BurstBand> = takes.iter().flatten().copied().collect();
    all.sort_by(|a, b| a.lo_hz.total_cmp(&b.lo_hz));
    let mut out: Vec<BurstBand> = Vec::new();
    for b in all {
        match out.last_mut() {
            Some(last) if b.lo_hz <= last.hi_hz => {
                last.hi_hz = last.hi_hz.max(b.hi_hz);
                last.level_db = last.level_db.max(b.level_db);
                last.variation = last.variation.max(b.variation);
            }
            _ => out.push(b),
        }
    }
    out
}

/// Where Bluetooth LE's advertising (2402, 2426 and 2480 MHz, the channels
/// every device calls on) lands in a HackRF tuned to `tuned_hz`, relative
/// to the centre, Hz — spectrally inverted, at IF − channel. None above
/// 2.17 GHz (no IF stage there) and none outside ±`half_span_hz` (plus a
/// packet's width).
pub fn bluetooth_offsets(tuned_hz: f64, half_span_hz: f64) -> Vec<f64> {
    let mhz = tuned_hz / 1e6;
    if !(0.0..2170.0).contains(&mhz) {
        return Vec::new();
    }
    let if_mhz = 2650.0 - mhz / 7.0;
    [2402.0, 2426.0, 2480.0]
        .into_iter()
        .map(|ch| (if_mhz - ch) * 1e6)
        .filter(|o| o.abs() <= half_span_hz + 1e6)
        .collect()
}

/// Whether a band sits where Bluetooth would leak in (within a packet's
/// width of one of `offsets`).
pub fn is_bluetooth(band: &BurstBand, offsets: &[f64]) -> bool {
    offsets
        .iter()
        .any(|&o| band.center_hz() > o - 0.8e6 && band.center_hz() < o + 0.8e6)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xorshift64* and Box–Muller: complex Gaussian power samples.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn uniform(&mut self) -> f64 {
            ((self.next() >> 11) as f64 + 1.0) / (1u64 << 53) as f64
        }
        /// |z|² of a unit-power complex Gaussian: exponential.
        fn power(&mut self) -> f32 {
            (-self.uniform().ln()) as f32
        }
    }

    /// A 1 MS/s span, 1024 bins (~1 kHz): noise, a continuous carrier
    /// 12 dB up over bins 200–299, and a TDMA channel 5 dB up over bins
    /// 600–639 that is on in 40 % of 20 ms stretches.
    fn span(meter: &mut BurstMeter, secs: f64) {
        let (rate, n) = (1e6, 1024usize);
        let frames = (secs * rate / n as f64) as usize;
        let mut r = Rng(0x5eed);
        let mut on = false;
        for f in 0..frames {
            // A new burst decision every ~20 ms (19 frames).
            if f % 19 == 0 {
                on = r.uniform() < 0.4;
            }
            let p: Vec<f32> = (0..n)
                .map(|b| {
                    let level = match b {
                        200..=299 => 1.0 + 15.8,
                        600..=639 if on => 1.0 + 3.16,
                        _ => 1.0,
                    };
                    level * r.power()
                })
                .collect();
            meter.push(&p, rate, n as f64 / rate);
        }
    }

    #[test]
    fn finds_the_bursty_channel_not_the_steady_carrier() {
        let mut m = BurstMeter::default();
        span(&mut m, 2.0);
        assert!(m.frames() >= MIN_FRAMES);
        let bands = m.take();
        assert_eq!(bands.len(), 1, "{bands:?}");
        let b = bands[0];
        // Bins 600–639 of 1024 at ~977 Hz: +85.9…+125 kHz from the centre
        // (the smoothing may widen it by a couple of bins either side).
        assert!(
            (b.lo_hz - 85.9e3).abs() < 4e3 && (b.hi_hz - 125e3).abs() < 4e3,
            "{b:?}"
        );
        assert!(b.variation > 2.0, "{b:?}");
        assert_eq!(m.frames(), 0, "taking starts afresh");
    }

    #[test]
    fn takes_merge_into_one_list() {
        let b = |lo: f64, hi: f64, v: f32| BurstBand {
            lo_hz: lo,
            hi_hz: hi,
            level_db: 2.0,
            variation: v,
        };
        let u = union(&[
            vec![b(0.0, 10e3, 2.0), b(50e3, 60e3, 3.0)],
            vec![b(5e3, 20e3, 4.0)],
            vec![],
        ]);
        assert_eq!(u, vec![b(0.0, 20e3, 4.0), b(50e3, 60e3, 3.0)]);
    }

    #[test]
    fn bluetooth_lands_where_it_was_seen() {
        // Seen live: centred +0.48 MHz at 1732.9 MHz, near 0 at 1735.5 MHz.
        let o = bluetooth_offsets(1732.9e6, 2e6);
        assert_eq!(o.len(), 1);
        assert!((o[0] - 0.443e6).abs() < 1e3, "{o:?}");
        let o = bluetooth_offsets(1735.5e6, 4e6);
        assert!((o[0] - 0.071e6).abs() < 1e3, "{o:?}");
        // The other advertising channels at their own tunings; none above
        // 2.17 GHz.
        assert!(
            bluetooth_offsets(1568.0e6, 1e6)
                .iter()
                .any(|o| o.abs() < 1e3)
        );
        assert!(
            bluetooth_offsets(1190.0e6, 1e6)
                .iter()
                .any(|o| o.abs() < 1e3)
        );
        assert!(bluetooth_offsets(2400.0e6, 10e6).is_empty());
        let band = BurstBand {
            lo_hz: 0.0,
            hi_hz: 0.9e6,
            level_db: 5.0,
            variation: 4.0,
        };
        assert!(is_bluetooth(&band, &bluetooth_offsets(1732.9e6, 2e6)));
        assert!(!is_bluetooth(&band, &bluetooth_offsets(1720.0e6, 2e6)));
    }
}
