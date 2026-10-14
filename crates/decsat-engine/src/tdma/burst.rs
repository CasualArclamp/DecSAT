//! Finding bursts in one channel's matched-filtered samples, as they stream
//! in: the power envelope over a few symbols against two levels — the noise
//! floor and the bursts' own — kept as a slowly forgetting histogram, with
//! hysteresis between them.
//!
//! The floor is a deep percentile of the envelope, not the median: a loaded
//! TDMA carrier is on ~98 % of the time and the noise shows only in the
//! guard times between bursts (live: 14 symbols every 870). The bursts' level
//! is the median of what stands clear of the floor.

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
/// The histogram forgets with this time constant, seconds.
const MEMORY_S: f64 = 1.0;
/// Bursts must stand this far above the floor to be looked for at all, dB.
const MIN_CONTRAST_DB: f32 = 3.0;

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
