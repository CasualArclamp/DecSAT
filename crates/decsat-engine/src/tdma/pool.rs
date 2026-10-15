//! A return pool: a block of spectrum a VSAT network's terminals share,
//! each on a carrier the hub sizes and places as its traffic needs —
//! continuous carriers that widen, narrow and move every few seconds
//! (Mx-DMA style) — or bursting into slots on a frame grid (MF-TDMA), or
//! both. Watched as a spectrogram and a burst envelope:
//!
//! - **the layout**: a 200 ms-averaged spectrum split at the occupied runs
//!   and, within them, at the guard dips between neighbours — the carriers
//!   there are now; a new layout is taken once it has held two columns, and
//!   the layouts are kept with their times (a map of the hub's choices).
//!   Carriers are not given identities: on a live pool a carrier's edge and
//!   its neighbour's guard move together, and a tracker split one carrier
//!   into several;
//! - **the grid**: the pool's short power dips (guard times), their
//!   strongest repeat found by autocorrelation and refined by folding — the
//!   slot — and the multiple of it the envelope's fold explains best — the
//!   frame.
//!
//! Seen live (Eutelsat 172B, a Newtec outbound's return): a 770 kHz pool of
//! roll-off-0.05 carriers resized every 1–3 s, the top of it bursting into
//! 5.025 ms slots of a 40.20 ms frame — the period of the outbound's private
//! D1h table and NCR, which ties the pool to that outbound.

use std::sync::Arc;

use decsat_core::Iq;
use rustfft::{Fft, FftPlanner};

/// Spectrogram bins about this wide, Hz.
const BIN_HZ: f64 = 2_000.0;
/// A column of the carriers' spectrogram: power averaged this long, s —
/// long enough that bursts in it average to a level (a 40 ms column caught
/// a bursting carrier half on, and its edges flickered column to column),
/// short against the seconds between a hub's changes.
const COLUMN_S: f64 = 0.2;
/// Occupied: this much over the noise floor, power ratio (4 dB).
const OCCUPIED: f32 = 2.5;
/// A guard dip between neighbours: this far under both sides' levels
/// (3 dB). Live, a guard between two carriers averaged over 200 ms is 4–9 dB
/// deep; 1.5 dB also took dips of 1 dB inside a bursting block that came and
/// went column to column, and no layout ever held.
const DIP: f32 = 0.5;
/// Narrowest carrier counted, Hz.
const MIN_WIDTH_HZ: f64 = 10_000.0;
/// A layout differs from the last when a carrier's edge moved this far
/// (bins) or the count changed; it is taken once it has held this many
/// columns.
const MOVE_BINS: usize = 3;
const HOLD_COLUMNS: u32 = 2;
/// Layouts kept for the map, s.
const HISTORY_S: f64 = 120.0;
/// Each bin's running minimum rises this much a column, so the floor
/// follows a slow drift (5 dB in 10 s) while a gap anywhere still sets it.
const MIN_RISE: f32 = 1.005;
/// The envelope's step, s: short enough for a burst's guard time (a few
/// tenths of a millisecond; an FFT's 0.5 ms smeared them out and only the
/// frame's empty slot showed).
const ENVELOPE_STEP_S: f64 = 50e-6;
/// Envelope kept for the grid, s; and how often the grid is looked for.
const ENVELOPE_S: f64 = 30.0;
const GRID_EVERY_S: f64 = 5.0;
/// Lags the grid is looked for in, s.
const GRID_MIN_S: f64 = 0.002;
const GRID_MAX_S: f64 = 0.25;
/// A repeat must stand this high in the normalised autocorrelation.
const GRID_FITS: f64 = 0.15;

/// A carrier in the pool now, for display: its edges, Hz from the VFO's
/// centre.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PoolCarrier {
    pub lo_hz: f64,
    pub hi_hz: f64,
}

impl PoolCarrier {
    pub fn width_hz(&self) -> f64 {
        self.hi_hz - self.lo_hz
    }

    pub fn centre_hz(&self) -> f64 {
        (self.lo_hz + self.hi_hz) / 2.0
    }
}

/// What the pool tracker sees, for display.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PoolView {
    /// The carriers now, lowest first.
    pub carriers: Vec<PoolCarrier>,
    /// The layouts of the last two minutes: when each began (VFO time, s)
    /// and its carriers.
    pub history: Vec<(f64, Vec<PoolCarrier>)>,
    /// The occupied span now, Hz from the VFO's centre.
    pub span: Option<(f64, f64)>,
    /// The VFO's passband looked at, Hz either side of its centre.
    pub half_band_hz: f64,
    /// Layout changes (a carrier resized, moved, come or gone) so far, and
    /// a minute.
    pub changes: u32,
    pub changes_per_min: f64,
    /// The grid: the shortest repeat (a slot), and the frame — the multiple
    /// of it the envelope's fold explains best — with the strength of the
    /// repeat (normalised autocorrelation).
    pub slot_s: Option<f64>,
    pub frame_s: Option<f64>,
    pub grid_strength: f64,
    /// Signal looked at, s.
    pub seconds: f64,
}

/// Follows a return pool's carriers and finds its frame grid.
pub struct PoolTracker {
    rate: f64,
    /// The VFO's passband in bins (beyond it, the down-converter's skirts).
    pass: std::ops::Range<usize>,
    /// Each bin's running minimum: the noise floor shows through in the
    /// gaps between carriers, wherever and whenever they open.
    minimum: Vec<f32>,
    n: usize,
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    pending: Vec<Iq>,
    work: Vec<Iq>,
    /// The column being summed, and how many FFTs are in it.
    column: Vec<f32>,
    in_column: usize,
    per_column: usize,
    /// The pool's power envelope, one value every `ENVELOPE_STEP_S`, and
    /// the block being summed into the next.
    envelope: std::collections::VecDeque<f32>,
    env_block: usize,
    env_sum: f32,
    env_n: usize,
    since_grid: f64,
    /// The layout taken, and one that differs from it with the columns it
    /// has held.
    layout: Vec<(usize, usize)>,
    candidate: Option<(Vec<(usize, usize)>, u32)>,
    t: f64,
    view: PoolView,
}

impl PoolTracker {
    /// For a VFO's baseband at `rate`, `bandwidth` wide.
    pub fn new(rate: f64, bandwidth: f64) -> Self {
        let n = ((rate / BIN_HZ).max(64.0) as usize).next_power_of_two();
        let half = ((bandwidth / 2.0 / (rate / n as f64)) as usize).clamp(4, n / 2);
        let fft = FftPlanner::new().plan_fft_forward(n);
        let window = (0..n)
            .map(|k| {
                let x = std::f32::consts::TAU * k as f32 / n as f32;
                0.5 - 0.5 * x.cos()
            })
            .collect();
        let fft_s = n as f64 / rate;
        PoolTracker {
            rate,
            pass: n / 2 - half..n / 2 + half,
            minimum: Vec::new(),
            n,
            fft,
            window,
            pending: Vec::new(),
            work: vec![Iq::new(0.0, 0.0); n],
            column: vec![0.0; n],
            in_column: 0,
            per_column: ((COLUMN_S / fft_s).round() as usize).max(1),
            envelope: Default::default(),
            env_block: ((ENVELOPE_STEP_S * rate).round() as usize).max(1),
            env_sum: 0.0,
            env_n: 0,
            since_grid: 0.0,
            layout: Vec::new(),
            candidate: None,
            t: 0.0,
            view: PoolView {
                half_band_hz: half as f64 * rate / n as f64,
                ..Default::default()
            },
        }
    }

    pub fn view(&self) -> &PoolView {
        &self.view
    }

    /// The FFT's time step, s.
    fn step_s(&self) -> f64 {
        self.n as f64 / self.rate
    }

    /// The envelope's time step, s.
    fn env_step_s(&self) -> f64 {
        self.env_block as f64 / self.rate
    }

    /// A bin's offset from the VFO's centre, Hz (bins FFT-shifted).
    fn bin_hz(&self, k: usize) -> f64 {
        (k as f64 - (self.n / 2) as f64) * self.rate / self.n as f64
    }

    pub fn push(&mut self, x: &[Iq]) {
        for s in x {
            self.env_sum += s.norm_sqr();
            self.env_n += 1;
            if self.env_n == self.env_block {
                self.envelope.push_back(self.env_sum);
                self.env_sum = 0.0;
                self.env_n = 0;
            }
        }
        self.pending.extend_from_slice(x);
        let n = self.n;
        let mut at = 0;
        while at + n <= self.pending.len() {
            for k in 0..n {
                self.work[k] = self.pending[at + k] * self.window[k];
            }
            at += n;
            self.fft.process(&mut self.work);
            for k in 0..n {
                // FFT-shifted: bin 0 is the lowest frequency.
                self.column[k] += self.work[(k + n / 2) % n].norm_sqr();
            }
            self.t += self.step_s();
            self.in_column += 1;
            if self.in_column == self.per_column {
                self.end_column();
            }
        }
        self.pending.drain(..at);
        let keep = (ENVELOPE_S / self.env_step_s()) as usize;
        while self.envelope.len() > keep {
            self.envelope.pop_front();
        }
        self.since_grid += x.len() as f64 / self.rate;
        if self.since_grid >= GRID_EVERY_S {
            self.since_grid = 0.0;
            self.find_grid();
        }
        self.view.seconds = self.t;
    }

    /// A column is in: split it into carriers and follow them.
    fn end_column(&mut self) {
        let mut col: Vec<f32> = self
            .column
            .iter()
            .map(|p| p / self.in_column as f32)
            .collect();
        self.column.fill(0.0);
        self.in_column = 0;
        // The floor: the quietest tenth of the passband's running minima (a
        // pool may fill nearly all of the VFO; its gaps still show the noise).
        if self.minimum.is_empty() {
            self.minimum = col.clone();
        }
        for (m, &c) in self.minimum.iter_mut().zip(&col) {
            *m = (*m * MIN_RISE).min(c);
        }
        let mut sorted = self.minimum[self.pass.clone()].to_vec();
        sorted.sort_by(f32::total_cmp);
        let floor = sorted[sorted.len() / 10].max(1e-30);
        // Only the passband: past it are the down-converter's skirts.
        for (k, c) in col.iter_mut().enumerate() {
            if !self.pass.contains(&k) {
                *c = 0.0;
            }
        }
        let segs = segments(&col, floor);
        let min_bins = (MIN_WIDTH_HZ * self.n as f64 / self.rate).ceil() as usize;
        let segs: Vec<(usize, usize)> = segs
            .into_iter()
            .filter(|&(a, b)| b - a >= min_bins)
            .collect();
        self.take_layout(segs);
        self.view.seconds = self.t;
    }

    /// A column's carriers: taken as the layout once they have differed
    /// from it for `HOLD_COLUMNS` in a row (a guard dip missed for a column
    /// is not a change).
    fn take_layout(&mut self, segs: Vec<(usize, usize)>) {
        let differs = |a: &[(usize, usize)], b: &[(usize, usize)]| {
            a.len() != b.len()
                || a.iter()
                    .zip(b)
                    .any(|(x, y)| x.0.abs_diff(y.0) >= MOVE_BINS || x.1.abs_diff(y.1) >= MOVE_BINS)
        };
        if !differs(&segs, &self.layout) {
            self.candidate = None;
            // Small wander: follow it.
            self.layout = segs;
        } else {
            let held = match &self.candidate {
                Some((c, n)) if !differs(&segs, c) => n + 1,
                _ => 1,
            };
            if held >= HOLD_COLUMNS {
                if !self.layout.is_empty() || !self.view.history.is_empty() {
                    self.view.changes += 1;
                }
                self.layout = segs;
                self.candidate = None;
                let carriers = self.carriers(&self.layout);
                let start = self.t - (HOLD_COLUMNS as f64) * COLUMN_S;
                self.view.history.push((start.max(0.0), carriers));
                let keep_from = self.t - HISTORY_S;
                self.view.history.retain(|(t, _)| *t >= keep_from);
            } else {
                self.candidate = Some((segs, held));
            }
        }
        self.view.carriers = self.carriers(&self.layout);
        self.view.span = match (self.layout.first(), self.layout.last()) {
            (Some(&(a, _)), Some(&(_, b))) => Some((self.bin_hz(a), self.bin_hz(b))),
            _ => None,
        };
        self.view.changes_per_min = if self.t > 10.0 {
            self.view.changes as f64 * 60.0 / self.t
        } else {
            0.0
        };
    }

    fn carriers(&self, segs: &[(usize, usize)]) -> Vec<PoolCarrier> {
        segs.iter()
            .map(|&(a, b)| PoolCarrier {
                lo_hz: self.bin_hz(a),
                hi_hz: self.bin_hz(b),
            })
            .collect()
    }

    /// The envelope's repeats. The slot: the first strong repeat of its
    /// short dips — the envelope's shortfall from its local mean over a
    /// millisecond either side, which keeps every burst's guard time and
    /// leaves out what changes slowly (an empty slot, a carrier resized) —
    /// refined by folding. The frame: the multiple of the slot whose fold of
    /// the envelope itself stands out most (empty slots and the slots'
    /// different occupants repeat frame by frame).
    fn find_grid(&mut self) {
        let dt = self.env_step_s();
        let e: Vec<f64> = self.envelope.iter().map(|&v| v as f64).collect();
        if e.len() as f64 * dt < 4.0 * GRID_MAX_S {
            return;
        }
        let dips = short_dips(&e, (0.001 / dt).round().max(2.0) as usize);
        let lo = (GRID_MIN_S / dt).ceil() as usize;
        let hi = ((GRID_MAX_S / dt) as usize).min(e.len() / 4);
        let Some(r) = autocorrelation(&dips, lo, hi) else {
            return;
        };
        // The first strong peak (a lag past which the curve falls), not the
        // highest — the slot's multiples peak as high as the slot.
        let top = r.iter().cloned().fold(f64::MIN, f64::max);
        if top < GRID_FITS {
            self.view.slot_s = None;
            self.view.frame_s = None;
            self.view.grid_strength = top.max(0.0);
            return;
        }
        let peak = (1..r.len() - 1)
            .find(|&i| r[i] >= 0.5 * top && r[i] >= r[i - 1] && r[i] >= r[i + 1])
            .unwrap_or(0);
        let slot = refine(&dips, dt, (lo + peak) as f64 * dt);
        // The frame: the multiple (up to the lags looked at) whose fold of
        // the envelope explains the most more than the slot's does.
        // Every multiple of the frame explains as much as the frame: the
        // shortest within a tenth of the best is it.
        let base = fold_explained(&e, dt, slot);
        let ks: Vec<(usize, f64)> = (2..=((GRID_MAX_S / slot) as usize).min(32))
            .map(|k| (k, fold_explained(&e, dt, slot * k as f64)))
            .collect();
        let best = ks.iter().map(|e| e.1).fold(f64::MIN, f64::max);
        let frame = (best > base * 1.2 + 0.002)
            .then(|| ks.iter().find(|e| e.1 >= 0.9 * best))
            .flatten()
            .map(|&(k, _)| slot * k as f64);
        self.view.slot_s = Some(slot);
        // The frame refined on the dips too (its own fold is flat across a
        // slot's width of error), within a sample step of k slots.
        self.view.frame_s = frame.map(|f| refine(&dips, dt, f));
        self.view.grid_strength = top;
    }
}

/// The shortfall of `e` from its mean over `half` samples either side, as
/// a share of that mean: the short dips, nothing else.
fn short_dips(e: &[f64], half: usize) -> Vec<f64> {
    let mut prefix = Vec::with_capacity(e.len() + 1);
    prefix.push(0.0);
    for v in e {
        prefix.push(prefix.last().unwrap() + v);
    }
    (0..e.len())
        .map(|i| {
            let (a, b) = (i.saturating_sub(half), (i + half + 1).min(e.len()));
            let m = (prefix[b] - prefix[a]) / (b - a) as f64;
            if m > 0.0 {
                ((m - e[i]) / m).max(0.0)
            } else {
                0.0
            }
        })
        .collect()
}

/// The normalised autocorrelation of `x` at lags `lo..=hi`, by FFT
/// (hundreds of thousands of samples by thousands of lags would be too
/// many products to take one by one); `None` for a flat `x`.
fn autocorrelation(x: &[f64], lo: usize, hi: usize) -> Option<Vec<f64>> {
    let n = x.len();
    let mean = x.iter().sum::<f64>() / n as f64;
    let var: f64 = x.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64;
    if var <= 0.0 || hi >= n {
        return None;
    }
    let size = (2 * n).next_power_of_two();
    let mut planner = FftPlanner::<f64>::new();
    let (fwd, inv) = (
        planner.plan_fft_forward(size),
        planner.plan_fft_inverse(size),
    );
    let mut buf: Vec<num_complex::Complex<f64>> = x
        .iter()
        .map(|v| num_complex::Complex::new(v - mean, 0.0))
        .chain(std::iter::repeat(num_complex::Complex::new(0.0, 0.0)))
        .take(size)
        .collect();
    fwd.process(&mut buf);
    for b in &mut buf {
        *b = num_complex::Complex::new(b.norm_sqr(), 0.0);
    }
    inv.process(&mut buf);
    Some(
        (lo..=hi)
            .map(|lag| buf[lag].re / size as f64 / ((n - lag) as f64 * var))
            .collect(),
    )
}

/// The period near `p` (within a sample step) whose fold of `e` stands out
/// most, in a hundred steps either side.
fn refine(e: &[f64], dt: f64, p: f64) -> f64 {
    let span = dt.min(p * 0.01);
    let mut best = (fold_contrast(e, dt, p), p);
    for k in -50..=50 {
        let q = p + span * k as f64 / 50.0;
        if q <= 0.0 {
            continue;
        }
        let c = fold_contrast(e, dt, q);
        if c > best.0 {
            best = (c, q);
        }
    }
    best.1
}

/// How far a fold of `e` (sampled every `dt`) over period `p` stands out:
/// the spread of the per-phase means over the spread a random fold leaves.
fn fold_contrast(e: &[f64], dt: f64, p: f64) -> f64 {
    let bins = ((p / dt).round() as usize).clamp(4, 2000);
    let mut sum = vec![0f64; bins];
    let mut cnt = vec![0usize; bins];
    for (i, &v) in e.iter().enumerate() {
        let ph = ((i as f64 * dt / p).fract() * bins as f64) as usize % bins;
        sum[ph] += v;
        cnt[ph] += 1;
    }
    let means: Vec<f64> = sum
        .iter()
        .zip(&cnt)
        .filter(|&(_, &c)| c > 0)
        .map(|(s, &c)| s / c as f64)
        .collect();
    let m = means.iter().sum::<f64>() / means.len() as f64;
    let spread = (means.iter().map(|v| (v - m).powi(2)).sum::<f64>() / means.len() as f64).sqrt();
    let all = e.iter().sum::<f64>() / e.len() as f64;
    let sd = (e.iter().map(|v| (v - all).powi(2)).sum::<f64>() / e.len() as f64).sqrt();
    let per = e.len() as f64 / bins as f64;
    spread / (sd / per.sqrt()).max(1e-30)
}

/// The share of `e`'s variance a fold over period `p` explains, less what
/// a fold of noise alone would (bins − 1 of N): fair between folds of
/// different lengths, which contrast is not (a longer fold has fewer
/// samples a phase).
fn fold_explained(e: &[f64], dt: f64, p: f64) -> f64 {
    let bins = ((p / dt).round() as usize).clamp(4, 8000);
    let mut sum = vec![0f64; bins];
    let mut cnt = vec![0usize; bins];
    for (i, &v) in e.iter().enumerate() {
        let ph = ((i as f64 * dt / p).fract() * bins as f64) as usize % bins;
        sum[ph] += v;
        cnt[ph] += 1;
    }
    let n = e.len() as f64;
    let all = e.iter().sum::<f64>() / n;
    let var = e.iter().map(|v| (v - all).powi(2)).sum::<f64>() / n;
    if var <= 0.0 {
        return 0.0;
    }
    let between: f64 = sum
        .iter()
        .zip(&cnt)
        .filter(|&(_, &c)| c > 0)
        .map(|(s, &c)| c as f64 * (s / c as f64 - all).powi(2))
        .sum::<f64>()
        / n;
    between / var - (bins - 1) as f64 / n
}

/// The carriers in a column: runs over the floor, split where a dip falls
/// under half of both sides' levels (the guard between two carriers).
fn segments(col: &[f32], floor: f32) -> Vec<(usize, usize)> {
    let on = floor * OCCUPIED;
    let mut out = Vec::new();
    let mut k = 0;
    while k < col.len() {
        if col[k] <= on {
            k += 1;
            continue;
        }
        let a = k;
        while k < col.len() && col[k] > on {
            k += 1;
        }
        split(col, a, k, &mut out);
    }
    out
}

/// Split the run `a..b` at its guard dips: a bin that is the lowest within
/// `DIP_HALF` bins either side and under `DIP` × the median levels a little
/// further out on both sides (live, a guard averaged over 200 ms is a dip
/// ~15 kHz wide: the levels are taken past it).
fn split(col: &[f32], a: usize, b: usize, out: &mut Vec<(usize, usize)>) {
    const DIP_HALF: usize = 6;
    const REACH: usize = 14;
    let level = |lo: usize, hi: usize| -> f32 {
        if hi <= lo + 2 {
            return 0.0;
        }
        let mut v: Vec<f32> = col[lo..hi].to_vec();
        v.sort_by(f32::total_cmp);
        v[v.len() / 2]
    };
    let mut start = a;
    let mut k = a + DIP_HALF;
    while k + DIP_HALF < b {
        let lowest = (k - DIP_HALF..=k + DIP_HALF).all(|j| col[j] >= col[k]);
        if lowest {
            let left = level(k.saturating_sub(DIP_HALF + REACH).max(start), k - DIP_HALF);
            let right = level(k + DIP_HALF + 1, (k + DIP_HALF + 1 + REACH).min(b));
            if left > 0.0 && right > 0.0 && col[k] < DIP * left.min(right) {
                if k > start {
                    out.push((start, k));
                }
                start = k + 1;
                k += DIP_HALF;
                continue;
            }
        }
        k += 1;
    }
    if b > start {
        out.push((start, b));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decsat_mod::Shaper;

    /// Random QPSK, shaped at `sps` with roll-off 0.05, mixed to `offset`
    /// (cycles a sample), `n` samples.
    fn carrier(n: usize, sps: usize, offset: f64, seed: u64) -> Vec<Iq> {
        let mut s = seed | 1;
        let syms: Vec<Iq> = (0..n / sps + 64)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                let a = std::f32::consts::FRAC_1_SQRT_2;
                Iq::new(
                    if s & 1 == 0 { a } else { -a },
                    if s & 2 == 0 { a } else { -a },
                )
            })
            .collect();
        let mut sh = Shaper::new(sps, 0.05, 24);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        x.truncate(n);
        x.iter()
            .enumerate()
            // (The phase wrapped in f64: as f32, millions of radians keep
            // no precision.)
            .map(|(k, &v)| {
                v * Iq::from_polar(
                    1.0,
                    (std::f64::consts::TAU * (offset * k as f64).fract()) as f32,
                )
            })
            .collect()
    }

    #[test]
    fn follows_a_pool_and_finds_its_grid() {
        // 1 MS/s: a 100 kS/s carrier that narrows to 50 kS/s half way, a
        // 125 kS/s one beside it, and a 200 kS/s one bursting in 5 ms slots
        // (a 0.4 ms guard each, every 8th slot empty: a 40 ms frame).
        let rate = 1e6;
        let n = 8_000_000;
        let a = carrier(n / 2, 10, -0.30, 1);
        let a2 = carrier(n / 2, 20, -0.325, 2);
        let b = carrier(n, 8, -0.12, 3);
        let c = carrier(n, 5, 0.20, 4);
        let mut s = 99u64;
        let mut noise = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            ((s >> 11) as f32 / (1u64 << 53) as f32 - 0.5) * 0.05
        };
        let x: Vec<Iq> = (0..n)
            .map(|k| {
                let t = k as f64 / rate;
                let slot = (t / 0.005) as u64;
                let in_slot = t - slot as f64 * 0.005;
                let bursting = in_slot > 0.0004 && slot % 8 != 7;
                let first = if k < n / 2 { a[k] } else { a2[k - n / 2] };
                first
                    + b[k]
                    + if bursting { c[k] } else { Iq::new(0.0, 0.0) }
                    + Iq::new(noise(), noise())
            })
            .collect();
        let mut p = PoolTracker::new(rate, 0.9e6);
        for chunk in x.chunks(100_000) {
            p.push(chunk);
        }
        let v = p.view();
        // Now: the narrowed carrier, its neighbour, the bursting one.
        assert_eq!(v.carriers.len(), 3, "{:#?}", v.carriers);
        let first = &v.carriers[0];
        assert!((first.width_hz() - 52e3).abs() < 12e3, "{first:?}");
        // The narrowing was a change, and the map shows the wide one before.
        assert!(v.changes >= 1, "{} changes", v.changes);
        assert!(
            v.history
                .iter()
                .any(|(t, cs)| *t < 4.0 && (cs[0].width_hz() - 105e3).abs() < 12e3),
            "{:#?}",
            v.history
        );
        let bursty = &v.carriers[2];
        assert!((bursty.centre_hz() - 200e3).abs() < 10e3, "{bursty:?}");
        let slot = v.slot_s.expect("slot");
        assert!((slot - 0.005).abs() < 2e-5, "slot {slot}");
        let frame = v.frame_s.expect("frame");
        assert!((frame - 0.040).abs() < 1e-4, "frame {frame}");
    }
}
