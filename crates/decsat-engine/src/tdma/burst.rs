// made by CasualArclamp
//! Finding bursts in one channel's matched-filtered samples, as they stream
//! in: the power envelope over a few symbols against two levels — the noise
//! floor and the bursts' own — kept as a slowly forgetting histogram, with
//! hysteresis between them.
//!
//! Where the noise fills most of the time (a slow TDMA carrier: live, a
//! 150 ms burst every 2 s), the noise level is the median of the noise's own
//! part of the histogram, and bursts are what stands clear of the noise's
//! spread — known, as the envelope averages a fixed number of symbols
//! ([`NOISE_FLOOR_DB`], [`NOISE_TAIL_DB`]): noise alone, however long, never
//! makes a burst. Where bursts fill most of it (a loaded carrier, on ~98 % of
//! the time, the noise only in the guard times between bursts: live, 14
//! symbols every 870), the floor is a deep percentile of the envelope, and the
//! bursts' level the median of what stands clear of it.

use decsat_core::Iq;

/// Symbols the envelope averages over: long enough to smooth a QPSK
/// envelope's swings, short enough to see a guard time of ~10 symbols.
const ENVELOPE_SYMBOLS: f64 = 6.0;
/// Shortest burst kept, symbols.
const MIN_BURST_SYMBOLS: f64 = 24.0;
/// A burst starts or ends only once the envelope has stayed past the
/// threshold this long, symbols (guard times are ~10–20).
const SETTLE_SYMBOLS: f64 = 3.0;
/// Margin kept either side of a burst, symbols.
const MARGIN_SYMBOLS: f64 = 4.0;
/// Longest burst before it is cut (a continuous carrier, or one never off).
const MAX_BURST_SYMBOLS: f64 = 100_000.0;
/// The floor's percentile.
const FLOOR_QUANTILE: f64 = 0.003;
/// The histogram forgets with this time constant, seconds: long enough to
/// remember a slow carrier's bursts between one and the next (live, every
/// 2 s; some every several).
const MEMORY_S: f64 = 10.0;
/// Bursts must stand this far above the floor to be looked for at all, dB.
const MIN_CONTRAST_DB: f32 = 3.0;
/// Noise alone, through the envelope's average of [`ENVELOPE_SYMBOLS`]
/// matched-filtered symbols: its [`FLOOR_QUANTILE`] point lies this far under
/// its median, and its 99.9 % point this far over, dB (simulated, the same
/// to ±0.15 dB for 3–8 samples a symbol and roll-offs 0.05–0.35); its mean is
/// 0.23 dB over the median.
const NOISE_FLOOR_DB: f32 = 5.5;
const NOISE_TAIL_DB: f32 = 4.5;
const NOISE_MEAN_DB: f32 = 0.23;
/// Bursts' share of the remembered envelope before they are looked for,
/// where the noise fills most of it (its own tail past [`NOISE_TAIL_DB`] is
/// 0.1 %).
const MIN_BURST_SHARE: f32 = 0.005;

/// The histogram's bins: dB from `HIST_LO`, a quarter dB each.
const HIST_LO: f32 = -160.0;
const HIST_STEP: f32 = 0.25;
const HIST_BINS: usize = 880;

/// A burst found: its samples with a margin either side.
#[derive(Debug, Clone)]
pub struct RawBurst {
    /// Channel-sample index (since the stream began) of `samples[0]`.
    pub start: u64,
    /// The burst within `samples`.
    pub on: std::ops::Range<usize>,
    pub samples: Vec<Iq>,
    /// The noise floor's power then (the envelope's units).
    pub floor: f32,
    /// Cut at the longest length rather than ended by a gap.
    pub cut: bool,
}

/// Envelope levels, as the histogram stands.
#[derive(Debug, Clone, Copy, Default)]
pub struct Levels {
    pub floor_db: f32,
    pub on_db: f32,
}

pub struct BurstDetector {
    sps: f64,
    /// Envelope: the last `avg` samples' powers and their running sum.
    avg: usize,
    ring: Vec<f32>,
    ring_pos: usize,
    sum: f64,
    hist: Vec<f32>,
    /// Add one envelope value in this many to the histogram.
    hist_every: usize,
    hist_count: usize,
    rate: f64,
    levels: Option<Levels>,
    on: bool,
    /// Sample index where the current burst began (envelope delay taken off).
    on_start: u64,
    /// Samples in a row past the threshold that would change the state,
    /// from `run_start`; the change needs `settle` of them.
    run: usize,
    run_start: u64,
    settle: usize,
    /// Samples kept: `buf[0]` is sample `buf_start`.
    buf: Vec<Iq>,
    buf_start: u64,
    /// The next sample's index.
    next: u64,
    /// Bursts ended but waiting for their trailing margin.
    pending: Vec<(u64, u64, bool)>,
    margin: usize,
    min_len: usize,
    max_len: usize,
}

impl BurstDetector {
    /// For a channel at `rate` samples a second, `sps` a symbol.
    pub fn new(rate: f64, sps: f64) -> Self {
        let avg = ((ENVELOPE_SYMBOLS * sps).round() as usize).max(2);
        BurstDetector {
            sps,
            avg,
            ring: vec![0.0; avg],
            ring_pos: 0,
            sum: 0.0,
            hist: vec![0.0; HIST_BINS],
            // One look a few symbols: the histogram is slow to forget
            // anyway, and a logarithm a sample was a third of the work.
            hist_every: ((4.0 * sps).floor() as usize).max(1),
            hist_count: 0,
            rate,
            levels: None,
            on: false,
            on_start: 0,
            run: 0,
            run_start: 0,
            settle: ((SETTLE_SYMBOLS * sps).round() as usize).max(1),
            buf: Vec::new(),
            buf_start: 0,
            next: 0,
            pending: Vec::new(),
            margin: (MARGIN_SYMBOLS * sps).ceil() as usize,
            min_len: (MIN_BURST_SYMBOLS * sps) as usize,
            max_len: (MAX_BURST_SYMBOLS * sps) as usize,
        }
    }

    pub fn levels(&self) -> Option<Levels> {
        self.levels
    }

    pub fn sps(&self) -> f64 {
        self.sps
    }

    /// Samples taken so far.
    pub fn position(&self) -> u64 {
        self.next
    }

    /// Fill the histogram from `x` without looking for bursts: the levels a
    /// stretch of signal already heard sets, before it is gone through.
    pub fn prime(&mut self, x: &[Iq]) {
        let mut ring = vec![0f32; self.avg];
        let mut sum = 0f64;
        for (i, s) in x.iter().enumerate() {
            let p = s.norm_sqr();
            sum += p as f64 - ring[i % self.avg] as f64;
            ring[i % self.avg] = p;
            if i >= self.avg && i % self.hist_every == 0 {
                let env = (sum / self.avg as f64).max(1e-16) as f32;
                let db = 10.0 * env.log10();
                let k = ((db - HIST_LO) / HIST_STEP).clamp(0.0, (HIST_BINS - 1) as f32) as usize;
                self.hist[k] += 1.0;
            }
        }
        self.update_levels();
    }

    /// The thresholds (linear power) from the histogram, or `None` while it
    /// is too thin or shows no contrast.
    fn update_levels(&mut self) {
        let total: f32 = self.hist.iter().sum();
        if total < 500.0 {
            self.levels = None;
            return;
        }
        let quantile = |hist: &[f32], from: usize, q: f64| -> Option<usize> {
            let t: f32 = hist[from..].iter().sum();
            if t <= 0.0 {
                return None;
            }
            let want = (q * t as f64) as f32;
            let mut acc = 0.0;
            for (k, &h) in hist.iter().enumerate().skip(from) {
                acc += h;
                if acc >= want {
                    return Some(k);
                }
            }
            Some(hist.len() - 1)
        };
        let Some(fk) = quantile(&self.hist, 0, FLOOR_QUANTILE) else {
            self.levels = None;
            return;
        };
        let floor_db = HIST_LO + (fk as f32 + 0.5) * HIST_STEP;
        // Mostly noise? Its part of the histogram: up to its tail, were the
        // floor its own deep percentile.
        let db_bin = |db: f32| (db / HIST_STEP).round() as usize;
        let tail = (fk + db_bin(NOISE_FLOOR_DB + NOISE_TAIL_DB)).min(HIST_BINS);
        let noise_mass: f32 = self.hist[..tail].iter().sum();
        if noise_mass >= 0.5 * total {
            // The noise's median, from its part alone; bursts are what
            // stands clear of its tail.
            let nk = quantile(&self.hist[..tail], 0, 0.5).unwrap_or(fk);
            let noise_db = HIST_LO + (nk as f32 + 0.5) * HIST_STEP;
            let clear = (nk + db_bin(NOISE_TAIL_DB)).min(HIST_BINS);
            let mass: f32 = self.hist[clear..].iter().sum();
            if mass < MIN_BURST_SHARE * total {
                self.levels = None;
                return;
            }
            let ok = quantile(&self.hist, clear, 0.5).unwrap_or(clear);
            let on_db = HIST_LO + (ok as f32 + 0.5) * HIST_STEP;
            self.levels = Some(Levels {
                floor_db: noise_db + NOISE_MEAN_DB,
                on_db,
            });
            return;
        }
        // The bursts' level: the median of everything clear of the floor.
        let clear = fk + (MIN_CONTRAST_DB / HIST_STEP) as usize;
        let mass: f32 = self.hist[clear.min(HIST_BINS)..].iter().sum();
        if clear >= HIST_BINS || mass < 0.01 * total {
            self.levels = None;
            return;
        }
        let ok = quantile(&self.hist, clear, 0.5).unwrap_or(clear);
        let on_db = HIST_LO + (ok as f32 + 0.5) * HIST_STEP;
        self.levels = (on_db - floor_db >= MIN_CONTRAST_DB).then_some(Levels { floor_db, on_db });
    }

    /// Take a block of samples; bursts complete in it (margins included)
    /// are appended to `out`.
    pub fn push(&mut self, x: &[Iq], out: &mut Vec<RawBurst>) {
        // Forget a little, then judge this block by the levels so far.
        let decay = (-(x.len() as f64) / self.rate / MEMORY_S).exp() as f32;
        for h in &mut self.hist {
            *h *= decay;
        }
        self.update_levels();
        // Detection: on 60 % of the way from floor to bursts in dB, off at
        // 40 % — low enough for a terminal some dB under the rest, high
        // enough over the floor that noise does not hold a burst on. The
        // edges this gives come early at high SNR (the envelope crosses a
        // threshold far below a strong burst's level); they are set again
        // per burst, at half its own level ([`refine_edges`]).
        let thresholds = self.levels.map(|l| {
            let lin = |db: f32| 10f32.powf(db / 10.0);
            let span = l.on_db - l.floor_db;
            (
                lin(l.floor_db + 0.6 * span),
                lin(l.floor_db + 0.4 * span),
                lin(l.floor_db),
            )
        });
        self.buf.extend_from_slice(x);
        // The envelope's running mean lags the signal by half its length.
        let delay = (self.avg / 2) as u64;
        for &s in x {
            let p = s.norm_sqr();
            self.sum += p as f64 - self.ring[self.ring_pos] as f64;
            self.ring[self.ring_pos] = p;
            self.ring_pos = (self.ring_pos + 1) % self.avg;
            let env = (self.sum / self.avg as f64).max(0.0) as f32;
            self.hist_count += 1;
            if self.hist_count >= self.hist_every {
                self.hist_count = 0;
                let db = 10.0 * env.max(1e-16).log10();
                let k = ((db - HIST_LO) / HIST_STEP).clamp(0.0, (HIST_BINS - 1) as f32) as usize;
                self.hist[k] += 1.0;
            }
            let n = self.next;
            self.next += 1;
            let Some((up, down, _)) = thresholds else {
                continue;
            };
            let at = n.saturating_sub(delay);
            // A change of state must hold for `settle` samples: at low SNR
            // a burst's envelope dips under the off threshold for a sample
            // or two, and noise pokes over the on one.
            if self.on {
                let long = at - self.on_start >= self.max_len as u64;
                if env < down {
                    if self.run == 0 {
                        self.run_start = at;
                    }
                    self.run += 1;
                } else {
                    self.run = 0;
                }
                if self.run >= self.settle || long {
                    let end = if long { at } else { self.run_start };
                    if end - self.on_start >= self.min_len as u64 {
                        self.pending.push((self.on_start, end, long));
                    }
                    self.on = long;
                    self.on_start = end;
                    self.run = 0;
                }
            } else {
                if env > up {
                    if self.run == 0 {
                        self.run_start = at;
                    }
                    self.run += 1;
                } else {
                    self.run = 0;
                }
                if self.run >= self.settle {
                    self.on = true;
                    self.on_start = self.run_start;
                    self.run = 0;
                }
            }
        }
        let floor = thresholds.map_or(0.0, |t| t.2);
        // Hand over the bursts whose trailing margin has arrived.
        let margin = self.margin as u64;
        let mut keep = Vec::new();
        for (s, e, cut) in std::mem::take(&mut self.pending) {
            if e + margin > self.next {
                keep.push((s, e, cut));
                continue;
            }
            let from = s.saturating_sub(margin).max(self.buf_start);
            let to = e + margin;
            let a = (from - self.buf_start) as usize;
            let b = (to - self.buf_start) as usize;
            let samples = self.buf[a..b].to_vec();
            let on = (s - from) as usize..(e - from) as usize;
            let on = if cut {
                on
            } else {
                refine_edges(&samples, on, self.avg, floor)
            };
            out.push(RawBurst {
                start: from,
                on,
                samples,
                floor,
                cut,
            });
        }
        self.pending = keep;
        // Keep what a burst under way, or one waiting, may still need.
        let need_from = self
            .pending
            .iter()
            .map(|p| p.0)
            .chain(self.on.then_some(self.on_start))
            .min()
            .unwrap_or(self.next)
            .saturating_sub(margin + self.avg as u64);
        if need_from > self.buf_start {
            let drop = ((need_from - self.buf_start) as usize).min(self.buf.len());
            self.buf.drain(..drop);
            self.buf_start += drop as u64;
        }
    }
}

/// A burst's edges set where its envelope crosses half the way (in power)
/// from the floor to the burst's own level, within `avg` samples of where
/// detection put them.
fn refine_edges(
    x: &[Iq],
    on: std::ops::Range<usize>,
    avg: usize,
    floor: f32,
) -> std::ops::Range<usize> {
    if on.len() < 4 * avg || x.len() < avg {
        return on;
    }
    // Centred running mean of the power.
    let half = avg / 2;
    let mut env = vec![0f32; x.len()];
    let mut sum = 0f64;
    for (i, s) in x.iter().enumerate() {
        sum += s.norm_sqr() as f64;
        if i >= avg {
            sum -= x[i - avg].norm_sqr() as f64;
        }
        if i + 1 >= avg {
            env[i + 1 - avg + half] = (sum / avg as f64) as f32;
        }
    }
    // The burst's level: the median over its middle.
    let mut mid: Vec<f32> = env[on.start + avg..on.end - avg].to_vec();
    if mid.is_empty() {
        return on;
    }
    let k = mid.len() / 2;
    let level = *mid.select_nth_unstable_by(k, |a, b| a.total_cmp(b)).1;
    let thr = floor + 0.5 * (level - floor);
    let lo = on.start.saturating_sub(avg);
    let start = (lo..on.start + avg)
        .find(|&i| env[i] >= thr)
        .unwrap_or(on.start);
    let hi = (on.end + avg).min(x.len() - half);
    let end = (on.end.saturating_sub(avg)..hi)
        .rev()
        .find(|&i| env[i] >= thr)
        .map_or(on.end, |i| i + 1);
    if end > start { start..end } else { on }
}

#[cfg(test)]
mod tests {
    use super::super::demod::tests::{Lcg, qpsk, shaped};
    use super::*;

    /// Noise at unit power, `n` samples.
    fn noise(n: usize, r: &mut Lcg) -> Vec<Iq> {
        let s = std::f32::consts::FRAC_1_SQRT_2;
        (0..n)
            .map(|_| Iq::new(r.gauss() * s, r.gauss() * s))
            .collect()
    }

    #[test]
    fn noise_alone_makes_no_bursts() {
        // Its own spread is no burst, however long it is listened to (it
        // was: a slow carrier's VFO counted 28 a second between bursts).
        let mut r = Lcg(31);
        let sps = 3.2;
        let mut d = BurstDetector::new(20_000.0, sps);
        let mut out = Vec::new();
        for _ in 0..40 {
            d.push(&noise(10_000, &mut r), &mut out);
        }
        assert!(out.is_empty(), "{} bursts in noise", out.len());
    }

    #[test]
    fn finds_a_slow_carriers_bursts_from_the_first() {
        // 1000-symbol bursts 8 dB over the noise, one every 2 s at 6.4
        // kS/s (3 % of the time): primed on what was heard, the detector
        // finds every one, the first too.
        let mut r = Lcg(37);
        let sps = 3.2;
        let rate = 6400.0 * sps;
        let gap = (2.0 * rate) as usize;
        let mut x = noise(20 * gap / 3, &mut r);
        let mut truth = Vec::new();
        for k in 0..6 {
            let t0 = gap / 4 + k * gap;
            let sym = qpsk(1000, &mut r);
            // `shaped` adds noise of its own; at 30 dB it is nothing.
            let s = shaped(&sym, sps, 0.001, 30.0, &mut r);
            let g = 10f32.powf(8.0 / 20.0);
            for (i, v) in s.iter().enumerate() {
                if t0 + i < x.len() {
                    x[t0 + i] += v * g;
                }
            }
            truth.push(t0 + (4.0 * sps) as usize);
        }
        truth.retain(|&t| t + (1000.0 * sps) as usize + 100 < x.len());
        let mut d = BurstDetector::new(rate, sps);
        d.prime(&x);
        let mut out = Vec::new();
        for c in x.chunks(4096) {
            d.push(c, &mut out);
        }
        let found: Vec<u64> = out.iter().map(|b| b.start + b.on.start as u64).collect();
        for t in &truth {
            assert!(
                found
                    .iter()
                    .any(|&f| (f as i64 - *t as i64).abs() < (4.0 * sps) as i64),
                "burst at {t} not found in {found:?}"
            );
        }
        assert_eq!(out.len(), truth.len(), "{found:?}");
        // The floor is the noise's mean, so the bursts read 8 dB over it.
        let l = d.levels().expect("levels");
        assert!((l.floor_db - 0.0).abs() < 0.5, "{l:?}");
    }

    #[test]
    fn finds_bursts_and_their_guard_times() {
        // Bursts of 300 symbols every 320 (a 20-symbol guard), at 4 sps,
        // with every fifth slot empty and the powers varying by terminal.
        let mut r = Lcg(11);
        let sps = 4.0;
        let mut x = Vec::new();
        let mut truth = Vec::new();
        for slot in 0..60 {
            let n0 = x.len();
            let mut seg = if slot % 5 == 4 {
                vec![Iq::new(0.0, 0.0); (320.0 * sps) as usize]
            } else {
                let sym = qpsk(300, &mut r);
                let g = [1.0f32, 0.7, 1.3][slot % 3];
                let mut s = shaped(&sym, sps, 0.001, 30.0, &mut r);
                for v in &mut s {
                    *v *= g;
                }
                s.truncate((320.0 * sps) as usize);
                truth.push(n0 + (4.0 * sps) as usize);
                s
            };
            seg.resize((320.0 * sps) as usize, Iq::new(0.0, 0.0));
            x.extend(seg);
        }
        for v in &mut x {
            *v += Iq::new(r.gauss() * 0.08, r.gauss() * 0.08);
        }
        let mut d = BurstDetector::new(1e6, sps);
        let mut out = Vec::new();
        for c in x.chunks(5000) {
            d.push(c, &mut out);
        }
        // The first few slots go by while the histogram fills (2000 looks
        // at one a symbol); every burst after that is found.
        truth.retain(|&t| t > 10_000);
        let found: Vec<u64> = out.iter().map(|b| b.start + b.on.start as u64).collect();
        let hits = truth
            .iter()
            .filter(|&&t| {
                found
                    .iter()
                    .any(|&f| (f as i64 - t as i64).abs() < (3.0 * sps) as i64)
            })
            .count();
        assert_eq!(hits, truth.len(), "{hits} of {} bursts", truth.len());
        for b in out.iter().filter(|b| b.start > 10_000) {
            let len = b.on.len() as f64 / sps;
            assert!((len - 300.0).abs() < 6.0, "length {len}");
        }
    }
}
