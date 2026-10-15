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
//! - **the slots**: once the grid is known, its phase (a guard time's
//!   centre) on the receiver's source clock, and each slot's spectrum — the
//!   FFTs inside it, its guard times left out — a row a slot for the last
//!   half minute ([`SpectrumRows`]), with the latest FFTs themselves for a
//!   waterfall fine enough to show the slots.
//!
//! Seen live (Eutelsat 172B, a Newtec outbound's return): a 770 kHz pool of
//! roll-off-0.05 carriers resized every 1–3 s, the top of it bursting into
//! 5.025 ms slots of a 40.20 ms frame — the period of the outbound's private
//! D1h table and NCR, which ties the pool to that outbound. That table is
//! the hub's burst plan; [`match_plan`] finds where its frequencies sit in
//! the VFO, [`align_plan`] when its frames land, and [`terminal_levels`]
//! how strong each terminal comes in.

use std::collections::VecDeque;
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
/// The mean spectrum's time constant, s: the pool's long-term shape, to
/// line up with an outbound's burst plan.
const MEAN_S: f64 = 30.0;
/// The slot rows' and fine rows' bins, about this wide, Hz: a burst of a
/// hundred kHz or more is still tens of bins.
const ROW_BIN_HZ: f64 = 4_000.0;
/// FFTs kept to build slots from and for the fine waterfall, s; and how
/// much of them the view holds.
const FINE_KEEP_S: f64 = 0.6;
const FINE_VIEW_S: f64 = 0.25;
/// The view's fine rows and slot rows are taken this often, s of signal
/// (the slot rows are megabytes: once a second is plenty).
const FINE_EVERY_S: f64 = 0.1;
const SLOTS_EVERY_S: f64 = 1.0;
/// Slot rows kept, s.
const SLOTS_KEEP_S: f64 = 30.0;
/// Each end of a slot left out of its row, share of a slot: the guard
/// times, and the bursts' ramps.
const SLOT_MARGIN: f64 = 0.04;
/// A bin occupied in a slot row: this far over the floor, dB.
const SLOT_OCCUPIED_DB: f32 = 4.0;
/// The source clock jumping by more than this between blocks is a gap
/// (blocks dropped, a file played round again), s.
const GAP_S: f64 = 5e-4;

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

/// Spectra over time on the receiver's source clock (the one all of its VFOs
/// share): a row each `dur_s` from `t_s[i]` (s), `bins` levels a row, dB
/// over the noise floor, the first bin centred `start_hz` from the VFO's
/// centre and each `bin_hz` wide.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpectrumRows {
    pub t_s: Vec<f64>,
    pub dur_s: f64,
    pub bins: usize,
    pub start_hz: f64,
    pub bin_hz: f64,
    /// Row after row.
    pub levels: Vec<f32>,
}

impl SpectrumRows {
    pub fn len(&self) -> usize {
        self.t_s.len()
    }

    pub fn is_empty(&self) -> bool {
        self.t_s.is_empty()
    }

    pub fn row(&self, i: usize) -> &[f32] {
        &self.levels[i * self.bins..(i + 1) * self.bins]
    }

    /// The row starting within a quarter row of `t`.
    pub fn row_at(&self, t: f64) -> Option<usize> {
        let tol = self.dur_s / 4.0;
        // Rust note: `partition_point` is a binary search for the first
        // element the test fails on — here the first row starting at or
        // after `t - tol`.
        let i = self.t_s.partition_point(|&x| x < t - tol);
        (i < self.t_s.len() && self.t_s[i] <= t + tol).then_some(i)
    }

    /// The bins whose centres lie in `lo_hz..hi_hz` (Hz from the VFO's
    /// centre), clipped to the row.
    pub fn bins_in(&self, lo_hz: f64, hi_hz: f64) -> std::ops::Range<usize> {
        if self.bin_hz <= 0.0 {
            return 0..0;
        }
        let at = |hz: f64| {
            ((hz - self.start_hz) / self.bin_hz)
                .ceil()
                .clamp(0.0, self.bins as f64) as usize
        };
        let (a, b) = (at(lo_hz), at(hi_hz));
        a..b.max(a)
    }

    /// Row `i`'s occupied bandwidth: its bins `SLOT_OCCUPIED_DB` or more
    /// over the floor, Hz.
    pub fn occupied_hz(&self, i: usize) -> f64 {
        self.row(i)
            .iter()
            .filter(|&&v| v >= SLOT_OCCUPIED_DB)
            .count() as f64
            * self.bin_hz
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
    /// The passband's mean spectrum over the last half minute or so, dB
    /// over the noise floor, a bin each `spectrum_bin_hz` from
    /// `spectrum_start_hz` (Hz from the VFO's centre).
    pub spectrum: Vec<f32>,
    pub spectrum_start_hz: f64,
    pub spectrum_bin_hz: f64,
    /// The grid's phase: a slot boundary (a guard time's centre) on the
    /// receiver's source clock, s, once the grid is found.
    pub boundary_s: Option<f64>,
    /// The source clock at the end of the signal looked at, s.
    pub now_s: f64,
    /// The pool slot by slot on that grid, the last half minute: each row
    /// a slot's spectrum, its guard times left out. Behind an `Arc` (a
    /// shared pointer): the view is cloned for the GUI every block.
    pub slot_rows: Arc<SpectrumRows>,
    /// The latest FFTs, about a millisecond each: the pool's own
    /// waterfall, fine enough to show the slots.
    pub fine: Arc<SpectrumRows>,
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
    /// The columns' running mean, and how many columns have gone in.
    mean: Vec<f32>,
    columns: u32,
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
    /// Samples pushed, and taken into FFTs.
    samples_in: u64,
    consumed: u64,
    /// The source clock less this tracker's own (samples pushed over the
    /// rate), s, as [`PoolTracker::push_at`] last gave it.
    src_offset: f64,
    src_known: bool,
    /// The envelope's first value's start on this tracker's own clock, s.
    env_t0: f64,
    /// The noise floor as last measured (a bin's power); 0 before the first
    /// column.
    floor: f32,
    /// FFT bins to a row bin, and a row's bins (the passband's).
    group: usize,
    row_bins: usize,
    /// The latest FFTs' power in row bins, each with its start on the source
    /// clock.
    fine: VecDeque<(f64, Vec<f32>)>,
    /// The slot rows: each start, and the levels (dB over the floor) row
    /// after row.
    slot_t: VecDeque<f64>,
    slot_levels: VecDeque<f32>,
    /// The grid on the source clock (a slot, a boundary), and where the
    /// next slot row starts.
    grid: Option<(f64, f64)>,
    next_slot: Option<f64>,
    /// When the view's fine and slot rows were last taken, source clock.
    fine_taken: f64,
    slots_taken: f64,
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
        let group = ((ROW_BIN_HZ / (rate / n as f64)).round() as usize).clamp(1, half);
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
            mean: vec![0.0; n],
            columns: 0,
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
            samples_in: 0,
            consumed: 0,
            src_offset: 0.0,
            src_known: false,
            env_t0: 0.0,
            floor: 0.0,
            group,
            row_bins: 2 * half / group,
            fine: VecDeque::new(),
            slot_t: VecDeque::new(),
            slot_levels: VecDeque::new(),
            grid: None,
            next_slot: None,
            fine_taken: f64::MIN,
            slots_taken: f64::MIN,
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

    /// Signal whose first sample came at `src_s` on the receiver's source
    /// clock (the one every VFO of the receiver shares): the slots are
    /// timed on it. A jump in it (blocks dropped, a file played round
    /// again) starts the envelope and the slot rows afresh; the grid, being
    /// on that clock, stays.
    pub fn push_at(&mut self, x: &[Iq], src_s: f64) {
        let offset = src_s - self.samples_in as f64 / self.rate;
        if self.src_known && (offset - self.src_offset).abs() > GAP_S {
            self.gap();
        }
        self.src_offset = offset;
        self.src_known = true;
        self.push(x);
    }

    /// The signal broke: what would span the break is dropped.
    fn gap(&mut self) {
        self.envelope.clear();
        self.env_sum = 0.0;
        self.env_n = 0;
        self.env_t0 = self.samples_in as f64 / self.rate;
        self.since_grid = 0.0;
        self.pending.clear();
        self.consumed = self.samples_in;
        self.fine.clear();
        self.next_slot = None;
    }

    /// Signal, timed on this tracker's own clock (see
    /// [`push_at`](Self::push_at)).
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
        self.samples_in += x.len() as u64;
        self.pending.extend_from_slice(x);
        let n = self.n;
        let mut at = 0;
        let mut row = vec![0f32; self.row_bins];
        while at + n <= self.pending.len() {
            for k in 0..n {
                self.work[k] = self.pending[at + k] * self.window[k];
            }
            at += n;
            self.fft.process(&mut self.work);
            row.fill(0.0);
            for k in 0..n {
                // FFT-shifted: bin 0 is the lowest frequency.
                let p = self.work[(k + n / 2) % n].norm_sqr();
                self.column[k] += p;
                if k >= self.pass.start {
                    let g = (k - self.pass.start) / self.group;
                    if g < self.row_bins {
                        row[g] += p;
                    }
                }
            }
            let start = self.consumed as f64 / self.rate + self.src_offset;
            self.consumed += n as u64;
            let per = self.group as f32;
            self.fine
                .push_back((start, row.iter().map(|p| p / per).collect()));
            while self.fine.front().is_some_and(|f| f.0 < start - FINE_KEEP_S) {
                self.fine.pop_front();
            }
            self.build_slots();
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
            self.env_t0 += self.env_step_s();
        }
        self.since_grid += x.len() as f64 / self.rate;
        if self.since_grid >= GRID_EVERY_S {
            self.since_grid = 0.0;
            self.find_grid();
        }
        self.view.seconds = self.t;
        self.view.now_s = self.samples_in as f64 / self.rate + self.src_offset;
        self.take_rows();
    }

    /// Slot rows for every slot the FFTs now reach past: the mean of the
    /// FFTs wholly inside it (its ends left out), in dB over the floor.
    fn build_slots(&mut self) {
        let (Some((slot, boundary)), true) = (self.grid, self.floor > 0.0) else {
            return;
        };
        let (Some(first), Some(last)) = (self.fine.front(), self.fine.back()) else {
            return;
        };
        let dur = self.step_s();
        let latest = last.0 + dur;
        let mut next = self
            .next_slot
            .unwrap_or_else(|| boundary + ((first.0 - boundary) / slot).ceil() * slot);
        let margin = SLOT_MARGIN * slot;
        let mut sum = vec![0f32; self.row_bins];
        while next + slot - margin <= latest {
            let (a, b) = (next + margin, next + slot - margin);
            sum.fill(0.0);
            let mut k = 0;
            for (t, r) in &self.fine {
                if *t >= a && *t + dur <= b {
                    sum.iter_mut().zip(r).for_each(|(s, v)| *s += v);
                    k += 1;
                }
            }
            if k == 0 {
                // FFTs too long for a slot's middle: those centred in it.
                for (t, r) in &self.fine {
                    let c = *t + dur / 2.0;
                    if c >= next && c < next + slot {
                        sum.iter_mut().zip(r).for_each(|(s, v)| *s += v);
                        k += 1;
                    }
                }
            }
            if k > 0 {
                let scale = 1.0 / (k as f32 * self.floor);
                self.slot_t.push_back(next);
                self.slot_levels
                    .extend(sum.iter().map(|p| 10.0 * (p * scale).max(1e-3).log10()));
            }
            next += slot;
        }
        self.next_slot = Some(next);
        while self
            .slot_t
            .front()
            .is_some_and(|&t| t < next - SLOTS_KEEP_S)
        {
            self.slot_t.pop_front();
            self.slot_levels.drain(..self.row_bins);
        }
    }

    /// The fine rows and slot rows into the view, as often as they are due.
    fn take_rows(&mut self) {
        let now = self.view.now_s;
        let fft_bin = self.rate / self.n as f64;
        let start_hz = self.bin_hz(self.pass.start) + (self.group - 1) as f64 / 2.0 * fft_bin;
        let bin_hz = self.group as f64 * fft_bin;
        if now - self.fine_taken >= FINE_EVERY_S && self.floor > 0.0 {
            self.fine_taken = now;
            let from = self.fine.partition_point(|f| f.0 < now - FINE_VIEW_S);
            let floor = self.floor;
            let rows = self.fine.range(from..);
            self.view.fine = Arc::new(SpectrumRows {
                t_s: rows.clone().map(|r| r.0).collect(),
                dur_s: self.step_s(),
                bins: self.row_bins,
                start_hz,
                bin_hz,
                // Rust note: `move` hands `floor` (a copy) to the inner
                // closure, which outlives this line inside the iterator.
                levels: rows
                    .flat_map(|r| {
                        r.1.iter()
                            .map(move |p| 10.0 * (p / floor).max(1e-3).log10())
                    })
                    .collect(),
            });
        }
        if now - self.slots_taken >= SLOTS_EVERY_S
            && let Some((slot, _)) = self.grid
        {
            self.slots_taken = now;
            self.view.slot_rows = Arc::new(SpectrumRows {
                t_s: self.slot_t.iter().copied().collect(),
                dur_s: slot,
                bins: self.row_bins,
                start_hz,
                bin_hz,
                levels: self.slot_levels.iter().copied().collect(),
            });
        }
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
        self.floor = floor;
        // The mean: a plain average at first, then exponential.
        self.columns += 1;
        let a = (1.0 / self.columns as f32).max((COLUMN_S / MEAN_S) as f32);
        for (m, &c) in self.mean.iter_mut().zip(&col) {
            *m += a * (c - *m);
        }
        self.view.spectrum = self.mean[self.pass.clone()]
            .iter()
            .map(|&m| 10.0 * (m / floor).max(1e-3).log10())
            .collect();
        self.view.spectrum_start_hz = self.bin_hz(self.pass.start);
        self.view.spectrum_bin_hz = self.rate / self.n as f64;
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
        // The slots' phase, on the frame's slot when there is one (k times
        // as precise): where the dips fold to their deepest.
        let slot = match self.view.frame_s {
            Some(f) => f / (f / slot).round(),
            None => slot,
        };
        let boundary = dip_phase(&dips, dt, self.env_t0, slot) + self.src_offset;
        self.view.boundary_s = Some(boundary);
        self.grid = Some((slot, boundary));
        // The next slot row onto the new grid: its nearest boundary, never
        // far back over a row already made.
        if let Some(t) = self.next_slot {
            let mut at = boundary + ((t - boundary) / slot).round() * slot;
            if at < t - slot / 4.0 {
                at += slot;
            }
            self.next_slot = Some(at);
        }
    }
}

/// Where in a slot the dips `d` (sampled every `dt`, the first at `t0` on
/// the tracker's clock) fold to their deepest — a guard time's centre — as
/// a time on that clock within `0..slot`: the fold smoothed over three
/// bins, its peak refined by a parabola.
fn dip_phase(d: &[f64], dt: f64, t0: f64, slot: f64) -> f64 {
    let bins = ((slot / dt).round() as usize).max(4);
    let mut fold = vec![0f64; bins];
    for (i, &v) in d.iter().enumerate() {
        let t = t0 + (i as f64 + 0.5) * dt;
        let ph = (t / slot).rem_euclid(1.0);
        fold[(ph * bins as f64) as usize % bins] += v;
    }
    let at = |k: usize| fold[k % bins];
    let smooth: Vec<f64> = (0..bins)
        .map(|k| at(k + bins - 1) + at(k) + at(k + 1))
        .collect();
    let m = (0..bins)
        .max_by(|&a, &b| smooth[a].total_cmp(&smooth[b]))
        .unwrap_or(0);
    let (l, c, r) = (
        smooth[(m + bins - 1) % bins],
        smooth[m],
        smooth[(m + 1) % bins],
    );
    let den = l - 2.0 * c + r;
    let d = if den < 0.0 { 0.5 * (l - r) / den } else { 0.0 };
    ((m as f64 + 0.5 + d) / bins as f64 * slot).rem_euclid(slot)
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

/// Where an outbound's burst plan sits in a pool VFO's band.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlanMatch {
    /// Add to a plan frequency for Hz from the VFO's centre.
    pub offset_hz: f64,
    /// How well the plan's occupancy follows the pool's mean spectrum
    /// there (correlation, -1..1).
    pub corr: f64,
}

/// Line a burst plan up with the pool: the plan's occupancy (how often
/// each frequency is in a burst) slid across the pool's mean spectrum (dB
/// over the floor), the offset where they correlate best — in 2 kHz steps,
/// then 250 Hz. The plan must lie mostly (70 %) inside the VFO's band.
/// Nothing in a Newtec hub's tables gives the plan's zero; this does, to a
/// few kHz.
pub fn match_plan(pool: &PoolView, plan: &decsat_ts::newtec::PlanView) -> Option<PlanMatch> {
    let (s, o) = (&pool.spectrum, &plan.occupancy);
    if s.len() < 8 || o.len() < 8 || pool.spectrum_bin_hz <= 0.0 {
        return None;
    }
    let bin = plan.occupancy_bin_hz;
    let g0 = plan.occupancy_start_hz + bin / 2.0;
    let level = |hz: f64| {
        let x = (hz - pool.spectrum_start_hz) / pool.spectrum_bin_hz;
        if x < 0.0 || x > (s.len() - 1) as f64 {
            return None;
        }
        let i = x as usize;
        let f = (x - i as f64) as f32;
        let v = s[i] * (1.0 - f) + s[(i + 1).min(s.len() - 1)] * f;
        Some(v.clamp(0.0, 30.0) as f64)
    };
    let score = |off: f64| {
        let (mut n, mut sx, mut sy, mut sxx, mut syy, mut sxy) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        for (j, &occ) in o.iter().enumerate() {
            if let Some(y) = level(g0 + j as f64 * bin + off) {
                let x = occ as f64;
                n += 1.0;
                sx += x;
                sy += y;
                sxx += x * x;
                syy += y * y;
                sxy += x * y;
            }
        }
        if n < 0.7 * o.len() as f64 {
            return None;
        }
        let cov = sxy - sx * sy / n;
        let var = (sxx - sx * sx / n) * (syy - sy * sy / n);
        (var > 0.0).then(|| cov / var.sqrt())
    };
    let lo = pool.spectrum_start_hz - g0 - o.len() as f64 * bin;
    let hi = pool.spectrum_start_hz + s.len() as f64 * pool.spectrum_bin_hz - g0;
    let mut best: Option<(f64, f64)> = None;
    let mut off = lo;
    while off <= hi {
        if let Some(c) = score(off)
            && best.is_none_or(|b| c > b.1)
        {
            best = Some((off, c));
        }
        off += 2_000.0;
    }
    let (coarse, _) = best?;
    for k in -8..=8 {
        let off = coarse + k as f64 * 250.0;
        if let Some(c) = score(off)
            && best.is_none_or(|b| c > b.1)
        {
            best = Some((off, c));
        }
    }
    best.map(|(offset_hz, corr)| PlanMatch { offset_hz, corr })
}

/// Leads looked at, frames: a plan sent up to two dozen frames ahead (the
/// 172B hub's is eight), and a little behind, which no hub does, to show
/// the search is fair.
const LEAD_MIN_FRAMES: f64 = -2.0;
const LEAD_MAX_FRAMES: f64 = 24.0;

/// When a burst plan's frames land in the pool.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlanTiming {
    /// From a frame's arrival (on the plan's arrival line, [`PlanClock`])
    /// to its first slot in the pool, s: how far ahead the hub sends it.
    ///
    /// [`PlanClock`]: decsat_ts::newtec::PlanClock
    pub lead_s: f64,
    /// A frame and a slot on the receiver's clock, s: the pool's grid,
    /// which measures them to a part in 10⁶ (the plan's arrivals, ±25 ms,
    /// could not).
    pub frame_s: f64,
    pub slot_s: f64,
    /// How well the pool follows the plan at that lead (the correlation of
    /// where the plan puts bursts with the pool's level, slot by slot and
    /// bin by bin); the best at any other lead; the median over the leads.
    pub corr: f64,
    pub next_corr: f64,
    pub median_corr: f64,
    /// Plan frames whose slots the pool has.
    pub frames: usize,
}

impl PlanTiming {
    /// The lead in frames.
    pub fn lead_frames(&self) -> f64 {
        self.lead_s / self.frame_s
    }

    /// Frame `counter`'s first slot in the pool, on the receiver's source
    /// clock, s.
    pub fn frame_start(&self, clock: &decsat_ts::newtec::PlanClock, counter: u32) -> f64 {
        arrival(clock, counter, self.frame_s) + self.lead_s
    }
}

/// Frame `counter`'s arrival on `clock`'s line, frames `frame_s` apart.
fn arrival(clock: &decsat_ts::newtec::PlanClock, counter: u32, frame_s: f64) -> f64 {
    clock.at_s + f64::from(counter.wrapping_sub(clock.counter) as i32) * frame_s
}

/// When a burst plan's frames land in the pool, its frequencies already
/// lined up (`offset_hz` from [`match_plan`]): each lead whose slots fall
/// on the pool's slot grid, from [`LEAD_MIN_FRAMES`] to
/// [`LEAD_MAX_FRAMES`], scored by the correlation over every slot and bin
/// of the plan's span between where it puts a burst (1, else 0) and the
/// pool's slot rows (dB over the floor, 0–30) — each frame placed by the
/// plan's arrival line, not its jittery arrival. The best lead wins, but a
/// `prior` one (the last answer) stays while it scores within 0.02 of it:
/// the lead is followed, not flipped by one noisy look.
///
/// Counts per slot alone (bursts against occupied bandwidth) correlate
/// 0.39 on 172B; slot by slot and bin by bin the right lead stands out far
/// more, as a burst's place in frequency must match too.
pub fn align_plan(
    pool: &PoolView,
    plan: &decsat_ts::newtec::PlanView,
    offset_hz: f64,
    prior: Option<&PlanTiming>,
) -> Option<PlanTiming> {
    let s = lead_scores(pool, plan, offset_hz)?;
    // Only leads with most of the plan's slots in the pool: one at the
    // edge of the rows, scored on a handful, could look good by chance.
    let most = s.tried.iter().map(|t| t.2).max()?;
    let fair: Vec<&(f64, f64, usize)> = s
        .tried
        .iter()
        .filter(|t| t.2 >= (most / 2).max(25 * s.slots))
        .collect();
    let best = **fair.iter().max_by(|a, b| a.1.total_cmp(&b.1))?;
    let same = |a: f64, b: f64| (a - b).abs() < s.slot_s / 2.0;
    let chosen = prior
        .and_then(|p| fair.iter().find(|t| same(t.0, p.lead_s)))
        .filter(|t| t.1 >= best.1 - 0.02)
        .map_or(best, |t| **t);
    let next_corr = fair
        .iter()
        .filter(|t| !same(t.0, chosen.0))
        .map(|t| t.1)
        .fold(-1.0, f64::max);
    let mut all: Vec<f64> = fair.iter().map(|t| t.1).collect();
    all.sort_by(f64::total_cmp);
    Some(PlanTiming {
        lead_s: chosen.0,
        frame_s: s.frame_s,
        slot_s: s.slot_s,
        corr: chosen.1,
        next_corr,
        median_corr: all[all.len() / 2],
        frames: chosen.2 / s.slots,
    })
}

/// Every lead [`align_plan`] tries.
#[derive(Debug, Clone, PartialEq)]
pub struct LeadScores {
    /// The plan's frame and slot on the receiver's clock, s, and its slots.
    pub frame_s: f64,
    pub slot_s: f64,
    pub slots: usize,
    /// Each lead (s), its correlation, and the plan's slots it found in the
    /// pool.
    pub tried: Vec<(f64, f64, usize)>,
}

/// The correlation at every lead [`align_plan`] looks at.
pub fn lead_scores(
    pool: &PoolView,
    plan: &decsat_ts::newtec::PlanView,
    offset_hz: f64,
) -> Option<LeadScores> {
    let rows = &*pool.slot_rows;
    let clock = plan.clock.as_ref()?;
    let slots = usize::from(plan.slots.max(1));
    if rows.len() < 4 * slots || plan.recent.len() < 25 {
        return None;
    }
    // The pool's grid must be the plan's; it is then the better measure.
    if (rows.dur_s * slots as f64 / clock.period_s - 1.0).abs() > 0.003 {
        return None;
    }
    let slot = rows.dur_s;
    let frame = slot * slots as f64;
    let (lo, hi) = plan.span?;
    let span = rows.bins_in(lo + offset_hz, hi + offset_hz);
    let w = span.len();
    if w < 4 {
        return None;
    }
    // Each row over the span: levels clamped (a peak or a deep null weighs
    // no more than on or off), their running sums (`prefix`, so a burst's
    // sum is two lookups), sum and sum of squares.
    let mut prefix = vec![0f32; rows.len() * (w + 1)];
    let mut sums = vec![(0f64, 0f64); rows.len()];
    for (j, s) in sums.iter_mut().enumerate() {
        let p = &mut prefix[j * (w + 1)..(j + 1) * (w + 1)];
        for (k, &v) in rows.row(j)[span.clone()].iter().enumerate() {
            let y = v.clamp(0.0, 30.0);
            p[k + 1] = p[k] + y;
            s.0 += f64::from(y);
            s.1 += f64::from(y * y);
        }
    }
    // Each plan frame: its arrival on the line, and its bursts as (first
    // slot, slots, first bin in the span, bin past the last).
    type Burst = (usize, usize, usize, usize);
    let frames: Vec<(f64, Vec<Burst>)> = plan
        .recent
        .iter()
        .map(|f| {
            let bursts = f
                .bursts
                .iter()
                .filter_map(|b| {
                    let (a, c) = plan.edges(b)?;
                    let r = rows.bins_in(a + offset_hz, c + offset_hz);
                    let (b0, b1) = (r.start.max(span.start), r.end.min(span.end));
                    (b1 > b0).then(|| {
                        (
                            usize::from(b.slot),
                            usize::from(b.slots()),
                            b0 - span.start,
                            b1 - span.start,
                        )
                    })
                })
                .collect();
            (arrival(clock, f.counter, frame), bursts)
        })
        .collect();
    let score = |lead: f64| -> (f64, usize) {
        let (mut n, mut sx, mut sy, mut syy, mut sxy) = (0.0, 0.0, 0.0, 0.0, 0.0);
        let mut matched = 0;
        let mut row_of = vec![None; slots];
        for (at, bursts) in &frames {
            for (k, r) in row_of.iter_mut().enumerate() {
                *r = rows.row_at(at + lead + k as f64 * slot);
                if let Some(j) = *r {
                    matched += 1;
                    n += w as f64;
                    sy += sums[j].0;
                    syy += sums[j].1;
                }
            }
            for &(k0, len, b0, b1) in bursts {
                for j in row_of[k0..(k0 + len).min(slots)].iter().flatten() {
                    let p = &prefix[j * (w + 1)..];
                    sx += (b1 - b0) as f64;
                    sxy += f64::from(p[b1] - p[b0]);
                }
            }
        }
        let cov = sxy - sx * sy / n.max(1.0);
        let var = (sx - sx * sx / n.max(1.0)) * (syy - sy * sy / n.max(1.0));
        (if var > 0.0 { cov / var.sqrt() } else { 0.0 }, matched)
    };
    // Leads whose slots start on the pool's: the newest frame's arrival
    // against the latest row, a slot at a time.
    let d0 = (rows.t_s[rows.len() - 1] - frames.last()?.0).rem_euclid(slot);
    let m_lo = ((LEAD_MIN_FRAMES * frame - d0) / slot).ceil() as i64;
    let m_hi = ((LEAD_MAX_FRAMES * frame - d0) / slot).floor() as i64;
    let tried = (m_lo..=m_hi)
        .map(|m| {
            let lead = d0 + m as f64 * slot;
            let (r, matched) = score(lead);
            (lead, r, matched)
        })
        .collect();
    Some(LeadScores {
        frame_s: frame,
        slot_s: slot,
        slots,
        tried,
    })
}

/// A burst's C/N as the pool received it, dB: the mean power over its
/// `len` slots from `start_s` and the middle of its band (a tenth of its
/// width in from each edge, a bin at least), as a ratio over the floor
/// less the noise. `None` where the pool has none of its slots.
pub fn burst_cn(
    rows: &SpectrumRows,
    start_s: f64,
    slot_s: f64,
    len: usize,
    lo_hz: f64,
    hi_hz: f64,
) -> Option<f32> {
    let edge = ((hi_hz - lo_hz) * 0.1).max(rows.bin_hz);
    let bins = rows.bins_in(lo_hz + edge, hi_hz - edge);
    if bins.is_empty() {
        return None;
    }
    let (mut sum, mut n) = (0f64, 0usize);
    for k in 0..len {
        if let Some(j) = rows.row_at(start_s + k as f64 * slot_s) {
            for &v in &rows.row(j)[bins.clone()] {
                sum += 10f64.powf(f64::from(v) / 10.0);
                n += 1;
            }
        }
    }
    (n > 0).then(|| (10.0 * (sum / n as f64 - 1.0).max(1e-3).log10()) as f32)
}

/// A terminal's bursts as the pool received them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TerminalLevel {
    pub id: u16,
    /// Its bursts in the plan whose slots the pool has, and those heard
    /// (C/N 0 dB or more: the band at least twice the floor).
    pub bursts: u32,
    pub heard: u32,
    /// The heard bursts' median C/N, dB, and the latest burst's.
    pub cn_db: Option<f32>,
    pub last_cn_db: Option<f32>,
}

/// Each terminal's level in the pool, from every plan frame the pool has
/// the slots of: the bursts placed by `timing`, their edges by the plan's
/// learned widths shifted by `offset_hz` (see [`burst_cn`]). By terminal.
pub fn terminal_levels(
    pool: &PoolView,
    plan: &decsat_ts::newtec::PlanView,
    timing: &PlanTiming,
    offset_hz: f64,
) -> Vec<TerminalLevel> {
    let rows = &*pool.slot_rows;
    let Some(clock) = &plan.clock else {
        return Vec::new();
    };
    let mut by: std::collections::BTreeMap<u16, (u32, Vec<f32>, Option<f32>)> = Default::default();
    for f in plan.recent.iter() {
        let start = timing.frame_start(clock, f.counter);
        for b in &f.bursts {
            let Some((a, c)) = plan.edges(b) else {
                continue;
            };
            let Some(cn) = burst_cn(
                rows,
                start + f64::from(b.slot) * timing.slot_s,
                timing.slot_s,
                usize::from(b.slots()),
                a + offset_hz,
                c + offset_hz,
            ) else {
                continue;
            };
            let e = by.entry(b.terminal).or_default();
            e.0 += 1;
            if cn >= 0.0 {
                e.1.push(cn);
            }
            e.2 = Some(cn);
        }
    }
    by.into_iter()
        .map(|(id, (bursts, mut heard, last))| {
            heard.sort_by(f32::total_cmp);
            TerminalLevel {
                id,
                bursts,
                heard: heard.len() as u32,
                cn_db: heard.get(heard.len() / 2).copied(),
                last_cn_db: last,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use decsat_mod::Shaper;

    #[test]
    fn a_plan_lines_up_with_its_pool() {
        // A plan whose bursts fill 200–900 kHz of its own frequencies, the
        // bottom 200 kHz always, the rest less often with a step; the pool
        // VFO sees the same shape 330 kHz lower, in dB, with noise.
        let mut plan = decsat_ts::newtec::PlanView {
            occupancy_start_hz: 200e3,
            occupancy_bin_hz: 2e3,
            ..Default::default()
        };
        plan.occupancy = (0..350)
            .map(|j| match j {
                0..100 => 1.0,
                100..220 => 0.6,
                _ => 0.25 + 0.1 * ((j as f32) * 0.3).sin(),
            })
            .collect();
        let mut pool = PoolView {
            spectrum_start_hz: -500e3,
            spectrum_bin_hz: 1953.125,
            ..Default::default()
        };
        let mut seed = 3u32;
        pool.spectrum = (0..512)
            .map(|k| {
                let hz = pool.spectrum_start_hz + k as f64 * pool.spectrum_bin_hz;
                let g = hz + 330e3; // the plan frequency here
                let j = ((g - 200e3) / 2e3).floor();
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (seed >> 9) as f32 / (1u32 << 23) as f32 - 0.5;
                let occ = if (0.0..350.0).contains(&j) {
                    plan.occupancy[j as usize]
                } else {
                    0.0
                };
                12.0 * occ + noise
            })
            .collect();
        let m = match_plan(&pool, &plan).expect("a match");
        assert!((m.offset_hz + 330e3).abs() < 1.5e3, "{m:?}");
        assert!(m.corr > 0.9, "{m:?}");
    }

    /// A terminal's C/N in the synthetic pools, dB.
    fn cn_of(terminal: u16) -> f32 {
        4.0 + f32::from(terminal % 10)
    }

    /// A hub's plan for 30 s of 40.2 ms frames: four 130.6 kHz one-slot
    /// bursts (code 0104h) packed upwards from 250 kHz in each of 8 slots,
    /// each place filled three times in four, by one of 12 terminals; each
    /// frame's section arriving `lead` before the frame starts in the pool
    /// (at `t0` and a frame after frame), give or take 20 ms. With the
    /// pool's slot rows for the same frames (4 kHz bins, ±400 kHz), the
    /// plan's frequencies 600 kHz down: every burst's band at its
    /// terminal's C/N for its slot, noise elsewhere.
    fn planned_pool(t0: f64, lead: f64) -> (decsat_ts::newtec::PlanView, PoolView) {
        use decsat_ts::newtec::{PlanBurst, PlanFrame, PlanRx};
        let (slot, frames, width) = (0.040_2 / 8.0, 750u32, 2612u32);
        let mut seed = 5u32;
        let mut rnd = move |n: u32| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) % n
        };
        let mut rx = PlanRx::new();
        let mut rows = SpectrumRows {
            dur_s: slot,
            bins: 200,
            start_hz: -398e3,
            bin_hz: 4e3,
            ..Default::default()
        };
        for c in 0..frames {
            let start = t0 + f64::from(c) * 0.040_2;
            let mut bursts = Vec::new();
            for k in 0..8u8 {
                for i in 0..4 {
                    if rnd(4) != 0 {
                        bursts.push(PlanBurst {
                            slot: k,
                            terminal: 1 + rnd(12) as u16,
                            code: 0x0104,
                            freq: 5000 + width / 2 + i * width,
                            extra: None,
                        });
                    }
                }
            }
            for k in 0..8u8 {
                rows.t_s.push(start + f64::from(k) * slot);
                let mut row = vec![0f32; rows.bins];
                for (j, v) in row.iter_mut().enumerate() {
                    *v = (rnd(1000) as f32 / 1000.0 - 0.5) * 0.6;
                    let hz = rows.start_hz + j as f64 * rows.bin_hz + 600e3;
                    if let Some(b) = bursts
                        .iter()
                        .find(|b| b.slot == k && (hz - b.freq_hz()).abs() < f64::from(width) * 25.0)
                    {
                        *v = 10.0 * (1.0 + 10f32.powf(cn_of(b.terminal) / 10.0)).log10();
                    }
                }
                rows.levels.extend(row);
            }
            let jitter = (f64::from(rnd(1000)) / 1000.0 - 0.5) * 0.04;
            rx.push(
                PlanFrame {
                    counter: 1000 + c,
                    time: u64::from(c) * 69_465_600,
                    bursts,
                    arrival_s: 0.0,
                },
                start - lead + jitter,
            );
        }
        let pool = PoolView {
            slot_rows: Arc::new(rows),
            ..Default::default()
        };
        (rx.view(), pool)
    }

    #[test]
    fn a_plan_is_timed_on_its_pool_and_its_terminals_measured() {
        let lead = 0.2946;
        let (plan, pool) = planned_pool(12.0013, lead);
        let clock = plan.clock.expect("an arrival line");
        assert!(clock.jitter_s > 0.005 && clock.jitter_s < 0.02, "{clock:?}");
        let t = align_plan(&pool, &plan, -600e3, None).expect("a timing");
        assert!((t.lead_s - lead).abs() < 1.5e-3, "{t:?}");
        assert!((t.lead_frames() - 7.33).abs() < 0.05, "{t:?}");
        // Other leads (a slot or a frame off) score well below.
        assert!(t.corr > 0.8 && t.corr > t.next_corr + 0.5, "{t:?}");
        assert!(t.frames > 700, "{t:?}");
        // A prior a frame off is not kept against a far better lead...
        let wrong = PlanTiming {
            lead_s: t.lead_s + t.frame_s,
            ..t
        };
        let again = align_plan(&pool, &plan, -600e3, Some(&wrong)).unwrap();
        assert!((again.lead_s - t.lead_s).abs() < 1e-9, "{again:?}");
        // ... and the right one is followed.
        let again = align_plan(&pool, &plan, -600e3, Some(&t)).unwrap();
        assert!((again.lead_s - t.lead_s).abs() < 1e-9, "{again:?}");
        // Each terminal at its own C/N.
        let levels = terminal_levels(&pool, &plan, &t, -600e3);
        assert_eq!(levels.len(), 12);
        for l in &levels {
            let cn = l.cn_db.expect("heard");
            assert!((cn - cn_of(l.id)).abs() < 0.5, "{l:?}");
            assert!(l.heard == l.bursts && l.bursts > 1000, "{l:?}");
        }
    }

    #[test]
    fn slot_rows_follow_the_pool_on_the_source_clock() {
        // 500 kS/s: a 125 kS/s carrier bursting in 5.025 ms slots, a
        // 0.4 ms guard about each boundary, the frame's last slot empty;
        // the signal's first sample at 7.5 s on the receiver's clock, its
        // slot boundaries at 3.1 ms past each multiple of the slot there.
        let rate = 500e3;
        let n = 3_500_000;
        let (slot, phase, t0) = (0.005_025, 0.003_1, 7.5);
        let c = carrier(n, 4, 0.1, 9);
        let mut s = 7u64;
        let mut noise = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            ((s >> 11) as f32 / (1u64 << 53) as f32 - 0.5) * 0.05
        };
        // Slot index and place in the slot of a time on the source clock.
        let at = |t: f64| {
            let u = (t - phase) / slot;
            (u.floor() as i64, (u - u.floor()) * slot)
        };
        let x: Vec<Iq> = (0..n)
            .map(|k| {
                let (i, r) = at(t0 + k as f64 / rate);
                let on = r > 0.0002 && r < slot - 0.0002 && i.rem_euclid(8) != 7;
                (if on { c[k] } else { Iq::new(0.0, 0.0) }) + Iq::new(noise(), noise())
            })
            .collect();
        let mut p = PoolTracker::new(rate, 400e3);
        for (j, chunk) in x.chunks(20_000).enumerate() {
            p.push_at(chunk, t0 + (j * 20_000) as f64 / rate);
        }
        let v = p.view();
        let b = v.boundary_s.expect("the grid's phase");
        let off = (b - phase) / slot;
        assert!(
            (off - off.round()).abs() * slot < 1e-4,
            "boundary {b} against {phase}"
        );
        assert!((v.now_s - (t0 + n as f64 / rate)).abs() < 1e-6);
        let rows = &v.slot_rows;
        assert!(rows.len() > 100, "{} rows", rows.len());
        assert!((rows.dur_s - slot).abs() < 2e-6, "{}", rows.dur_s);
        // Rows on the boundaries; the empty slot quiet, the others not,
        // across the carrier (25 kHz either side of +50 kHz).
        let band = rows.bins_in(25e3, 75e3);
        for i in 0..rows.len() {
            let (k, r) = at(rows.t_s[i] + slot / 2.0);
            assert!((r - slot / 2.0).abs() < 1e-4, "row {i} at {}", rows.t_s[i]);
            let level = rows.row(i)[band.clone()].iter().sum::<f32>() / band.len() as f32;
            if k.rem_euclid(8) == 7 {
                assert!(level < 1.5, "empty slot {k}: {level} dB");
            } else {
                assert!(level > 10.0, "slot {k}: {level} dB");
                assert!(rows.occupied_hz(i) > 100e3, "slot {k}");
            }
        }
        // The fine rows: the latest quarter second (as of the last tenth),
        // a millisecond an FFT.
        let fine = &v.fine;
        assert!(fine.len() > 150, "{}", fine.len());
        assert!(v.now_s - fine.t_s[0] <= 0.36, "{}", fine.t_s[0]);
    }

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
