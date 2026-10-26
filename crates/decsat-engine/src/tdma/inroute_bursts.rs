// made by CasualArclamp
//! Return bursts demodulated live, by the plans of a hub that signals in GSE.
//!
//! The hub ([`decsat_gse::inroute`]) sends each inroute's burst time plan
//! every 125 ms frame on its outbound; the outbound's VFO posts them to the
//! engine's [`PlanFeed`] ([`PlanFeed::read_hubs`]). Here, on a Return pool
//! VFO over the inroutes:
//!
//! 1. **the inroutes on the VFO**, every two seconds: the inroute table's
//!    uplink frequencies as a comb of carriers slid along the pool's mean
//!    spectrum, the best fit taken (the satellite's translation and the
//!    LNB's error are not known here);
//! 2. **the frames**: every inroute falls silent for ~3 ms at each frame's
//!    end (113 slots of 1.077 ms, or 129 of 0.945 ms, leave that much of
//!    125 ms), so the inroutes' own band power, in 20 µs blocks, shows each
//!    frame's start (the whole VFO's does not: a neighbour's skirt can fill
//!    the silences); a line through those starts gives the frame on the
//!    receiver's clock (its length to a few ppm: a HackRF is that far off);
//! 3. **the lead**: a plan applies some frames after it arrives (five on
//!    172B); the count is the one under which the slots' powers are best
//!    explained by whose slots they are — terminals differ in level by
//!    several dB, and a busy one holds many slots running (the fraction of
//!    the slot-power variance the owners explain: 0.9 at the right count,
//!    under 0.15 at any other, on a 90 s capture);
//! 4. **each plan as it comes**: every slot a burst, on the source clock
//!    (the slot's 27 MHz ticks scaled by the frames' measured length), cut
//!    out of the ring once it has come and handed to [`BurstDemod`] on
//!    worker threads; the bursts' median timing and carrier offsets trim
//!    the start and the zero;
//! 5. **per terminal**: its slots, bursts demodulated, MER, carrier and
//!    timing against the plan, its tag (three preamble symbols of its own)
//!    and its level over the noise, from the slot powers.
//!
//! On Record every good burst goes to one file,
//! `decsat-<VFO>-<frequency>-inroutes-<unix time>.ibursts`, for working
//! out the FEC offline:
//!
//! ```text
//! "DSINR001"                          8-byte magic, once
//! per burst, little-endian:
//!   u32  plan frame (24 bits)
//!   u16  inroute, u16 slot, u16 owner word (terminal << 4 | kind)
//!   f64  slot start, s                the receiver's source clock
//!   f32  MER, dB
//!   f32  carrier against the inroute's centre, Hz
//!   f32  first symbol against the slot's planned start, µs
//!   f32  preamble fit, 0..1
//!   3 × u8  the tag (preamble symbols 12–14, 8PSK phase indices)
//!   u32  n, then n × (f32 I, f32 Q)   data symbols (known ones left out)
//!   u32  m, then m × f32              LLRs, max-log, positive for a 0
//!                                     (DVB-S2 8PSK labels assumed)
//! ```

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use decsat_core::Iq;
use decsat_gse::inroute::{
    ArrivalLine, BurstConfig, FRAME_S, HubView, InroutePlan, Modulation, frame24_diff,
};
use rustfft::FftPlanner;

use super::burst_demod::{BurstDemod, BurstResult, InrouteShape, ScheduledBurst};
use super::pool::PoolView;
use super::pool_bursts::{Ring, median};
use crate::plan_feed::{PlanFeed, PlanLink};
use crate::vfo::VfoId;

/// The band power's block, s.
const BLOCK_S: f64 = 20e-6;
/// A frame's guard silence is at least this long, s.
const SILENCE_MIN_S: f64 = 1.5e-3;
/// Frame starts kept for the line (30 s).
const STARTS_KEEP: usize = 240;
/// How often the plans are lined up with the pool again, s of signal.
const ALIGN_EVERY_S: f64 = 2.0;
/// Leads tried, frames from the frame a plan arrives in.
const LEADS: std::ops::RangeInclusive<i32> = -2..=16;
/// Slots a lead must be judged on, and what makes it believed: the owners
/// explain this much of the slot powers' variance, this much more than
/// at any other lead.
const MIN_SLOTS: usize = 200;
const SURE_R2: f64 = 0.25;
const SURE_MARGIN: f64 = 0.08;
/// The comb fits the spectrum at least this well (correlation).
const MAP_MIN_CORR: f64 = 0.5;
/// Signal cut either side of a burst, symbols (the demodulator looks ±25
/// round the planned start).
const EDGE_SYMBOLS: f64 = 40.0;
/// Bursts queued for the workers before new ones are let go.
const QUEUE: usize = 1024;
/// Data symbols kept per terminal for its constellation.
const KEEP_SYMBOLS: usize = 800;
/// Good bursts the trims wait for, and the most either moves at a time.
const TRIM_BURSTS: usize = 50;
const TRIM_MAX_HZ: f64 = 3e3;
const TRIM_MAX_S: f64 = 30e-6;
/// How fast the per-terminal figures follow new bursts.
const FOLLOW: f64 = 0.1;
/// Plans kept per outbound for lining up (four seconds of six inroutes).
const PLANS_KEEP: usize = 256;

/// An inroute put on the VFO.
#[derive(Debug, Clone, PartialEq)]
pub struct Carrier {
    pub inroute: u16,
    /// Its centre, Hz from the VFO's.
    pub centre_hz: f64,
    pub config: BurstConfig,
}

impl Carrier {
    /// Its slot and its symbol rate on the receiver's clock, given the
    /// frame's measured length there.
    fn slot_s(&self, frame_s: f64) -> f64 {
        self.config.slot_s() * frame_s / FRAME_S
    }
    fn rate(&self, frame_s: f64) -> f64 {
        self.config.air_rate() * FRAME_S / frame_s
    }
}

/// How the plans line up with the pool's frames.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LeadFit {
    /// Frames from the one a plan arrives in to the one it applies to.
    pub lead: i32,
    /// From a plan's arrival (the arrival line) to its frame's start, s.
    pub lead_s: f64,
    /// The share of the slot powers' variance the owners explain, at this
    /// lead and at the next best, over this many slots.
    pub r2: f64,
    pub next_r2: f64,
    pub slots: usize,
}

/// An inroute as the pool sees it, for display.
#[derive(Debug, Clone, PartialEq)]
pub struct CarrierView {
    pub inroute: u16,
    pub centre_hz: f64,
    /// Symbols a second on the air, slot and burst symbols, slots a frame.
    pub symbol_rate: f64,
    pub slot_symbols: usize,
    pub burst_symbols: usize,
    pub slots: usize,
    pub modulation: Modulation,
    /// The slots' mean level over the guard silence's noise, dB.
    pub level_db: Option<f64>,
}

/// One terminal's bursts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InrouteTerminal {
    pub id: u16,
    /// Slots a frame in the recent plans, and the inroutes they were on.
    pub slots_per_frame: f64,
    pub inroutes: Vec<u16>,
    /// Bursts cut out and handed on, those the demodulator took, failed.
    pub cut: u64,
    pub demodulated: u64,
    pub failed: u64,
    /// Over its recent good bursts: MER, dB; carrier against the inroute's
    /// centre, Hz; first symbol against the slot's planned start, µs.
    pub mer_db: f32,
    pub freq_hz: f64,
    pub timing_us: f64,
    /// Its tag: the preamble's symbols 12–14 (8PSK phase indices), the
    /// commonest over its good bursts, and the share of them that had it
    /// (most terminals keep one; a few change it).
    pub tag: Option<[u8; 3]>,
    pub tag_share: f32,
    /// Its slots' level over the noise, dB.
    pub level_db: Option<f64>,
    /// Its latest data symbols, oldest first.
    pub symbols: Vec<Iq>,
    /// Its latest burst's waveform, for the GUI's plot. Empty until something
    /// fills it in.
    pub wave: BurstWave,
}

/// One burst's waveform, for display.
///
/// Placeholder: the GUI's plot reads this, and hides itself while `samples` is
/// empty. Nothing in the engine fills it in yet.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BurstWave {
    /// The burst's samples with its own carrier taken out.
    pub samples: Vec<Iq>,
    /// Time between samples, µs.
    pub step_us: f64,
    /// Indices into `samples` of the burst's first and last symbol.
    pub burst: (usize, usize),
    /// The inroute (carrier) and slot the burst was in.
    pub inroute: u16,
    pub slot: u16,
}

/// What the inroute demodulator is doing, for display.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InrouteBurstsView {
    /// The outbound VFO whose plans are followed.
    pub from: Option<VfoId>,
    pub carriers: Vec<CarrierView>,
    /// How well the inroute comb fits the pool's spectrum.
    pub map_corr: f64,
    pub fit: Option<LeadFit>,
    /// The best lead the last look found, believed or not.
    pub tried: Option<LeadFit>,
    /// The frame as measured on the receiver's clock, and the frame starts
    /// (guard silences' ends) the line holds.
    pub frame_s: Option<f64>,
    pub silences: usize,
    pub zero_trim_hz: f64,
    pub start_trim_s: f64,
    /// Bursts scheduled; cut and handed on; demodulated; failed; gone from
    /// the ring before their plan came; let go with the workers behind; on
    /// an inroute whose waveform is not read (its QPSK ones).
    pub planned: u64,
    pub cut: u64,
    pub demodulated: u64,
    pub failed: u64,
    pub late: u64,
    pub dropped: u64,
    pub unknown: u64,
    /// Times the plans were taken up, or found moved.
    pub links: u32,
    pub per_s: f64,
    pub load: f64,
    pub workers: usize,
    pub terminals: Vec<InrouteTerminal>,
    /// Recording: the file, and bursts written.
    pub recording: Option<(PathBuf, u64)>,
}

/// A burst scheduled, waiting for its slot to come: where in which plan.
struct Planned {
    start_s: f64,
    end_s: f64,
    inroute: u16,
    frame: u32,
    slot: u16,
    burst: ScheduledBurst,
}

/// A burst cut out, for a worker.
struct Job {
    iq: Vec<Iq>,
    burst: ScheduledBurst,
    inroute: u16,
    frame: u32,
    slot: u16,
    start_s: f64,
    trims: u32,
}

/// The bursts' file while recording.
struct Recorder {
    path: PathBuf,
    file: Option<BufWriter<File>>,
    written: u64,
    failed: bool,
}

impl Recorder {
    fn write(&mut self, job: &Job, r: &BurstResult) {
        if self.failed {
            return;
        }
        if self.file.is_none() {
            if let Some(d) = self.path.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            match File::create(&self.path) {
                Ok(f) => {
                    let mut w = BufWriter::new(f);
                    let _ = w.write_all(b"DSINR001");
                    self.file = Some(w);
                }
                Err(_) => {
                    self.failed = true;
                    return;
                }
            }
        }
        let mut rec = Vec::with_capacity(48 + 8 * r.symbols.len() + 4 * r.llrs.len());
        rec.extend_from_slice(&job.frame.to_le_bytes());
        rec.extend_from_slice(&job.inroute.to_le_bytes());
        rec.extend_from_slice(&job.slot.to_le_bytes());
        rec.extend_from_slice(&job.burst.code.to_le_bytes());
        rec.extend_from_slice(&job.start_s.to_le_bytes());
        rec.extend_from_slice(&r.mer_db.to_le_bytes());
        rec.extend_from_slice(&(r.freq_offset_hz as f32).to_le_bytes());
        rec.extend_from_slice(&((r.timing_offset_s * 1e6) as f32).to_le_bytes());
        rec.extend_from_slice(&r.preamble_fit.to_le_bytes());
        let mut tag = [0u8; 3];
        for (t, h) in tag.iter_mut().zip(&r.header) {
            *t = *h;
        }
        rec.extend_from_slice(&tag);
        rec.extend_from_slice(&(r.symbols.len() as u32).to_le_bytes());
        for v in &r.symbols {
            rec.extend_from_slice(&v.re.to_le_bytes());
            rec.extend_from_slice(&v.im.to_le_bytes());
        }
        rec.extend_from_slice(&(r.llrs.len() as u32).to_le_bytes());
        for l in &r.llrs {
            rec.extend_from_slice(&l.to_le_bytes());
        }
        let ok = self
            .file
            .as_mut()
            .is_some_and(|f| f.write_all(&rec).is_ok());
        if ok {
            self.written += 1;
        } else {
            self.failed = true;
        }
    }
}

#[derive(Default)]
struct Shared {
    recorder: Option<Recorder>,
    terminals: BTreeMap<u16, InrouteTerminal>,
    /// Each terminal's tags so far.
    tags: HashMap<u16, HashMap<[u8; 3], u32>>,
    demodulated: u64,
    failed: u64,
    busy_s: f64,
    recent: VecDeque<f64>,
    trims: u32,
    /// Good bursts' (carrier, timing) offsets under the trims in use.
    offsets: Vec<(f64, f64)>,
}

impl Shared {
    fn take(&mut self, job: &Job, r: Option<BurstResult>, secs: f64) {
        self.busy_s += secs;
        let t = self
            .terminals
            .entry(job.burst.terminal)
            .or_insert_with(|| InrouteTerminal {
                id: job.burst.terminal,
                ..Default::default()
            });
        t.cut += 1;
        let Some(r) = r.filter(|r| r.ok) else {
            t.failed += 1;
            self.failed += 1;
            return;
        };
        let first = t.demodulated == 0;
        t.demodulated += 1;
        self.demodulated += 1;
        let follow = |old: f64, new: f64| {
            if first {
                new
            } else {
                old + FOLLOW * (new - old)
            }
        };
        t.mer_db = follow(f64::from(t.mer_db), f64::from(r.mer_db)) as f32;
        t.freq_hz = follow(t.freq_hz, r.freq_offset_hz);
        t.timing_us = follow(t.timing_us, r.timing_offset_s * 1e6);
        if let [a, b, c] = r.header[..] {
            let h = self.tags.entry(job.burst.terminal).or_default();
            *h.entry([a, b, c]).or_default() += 1;
            let total: u32 = h.values().sum();
            if let Some((tag, n)) = h.iter().max_by_key(|(k, n)| (**n, std::cmp::Reverse(**k))) {
                t.tag = Some(*tag);
                t.tag_share = *n as f32 / total.max(1) as f32;
            }
        }
        t.symbols.extend_from_slice(&r.symbols);
        let over = t.symbols.len().saturating_sub(KEEP_SYMBOLS);
        t.symbols.drain(..over);
        if job.trims == self.trims {
            self.offsets.push((r.freq_offset_hz, r.timing_offset_s));
        }
        self.recent.push_back(job.start_s);
        while self.recent.front().is_some_and(|&s| s < job.start_s - 4.0) {
            self.recent.pop_front();
        }
        if let Some(rec) = &mut self.recorder {
            rec.write(job, &r);
        }
    }
}

/// The ends of the guard silences in a power envelope (block start, power):
/// runs of at least [`SILENCE_MIN_S`] under the level halfway (in dB)
/// between its floor and its carriers, each end the first block back over.
fn silence_ends(blocks: &[(f64, f32)], block_s: f64) -> Vec<f64> {
    if blocks.len() < 1000 {
        return Vec::new();
    }
    let mut sorted: Vec<f32> = blocks.iter().map(|b| b.1).collect();
    sorted.sort_by(f32::total_cmp);
    let (floor, on) = (sorted[sorted.len() / 100], sorted[sorted.len() * 6 / 10]);
    // Under 6 dB between them: no silences to speak of.
    if on <= 4.0 * floor || floor <= 0.0 {
        return Vec::new();
    }
    let thr = (floor * on).sqrt();
    let min_run = (SILENCE_MIN_S / block_s).round() as usize;
    let mut ends = Vec::new();
    let mut run = 0usize;
    for &(t, p) in blocks {
        if p < thr {
            run += 1;
        } else {
            if run >= min_run {
                ends.push(t);
            }
            run = 0;
        }
    }
    ends
}

/// A line through the frame starts: the newest start on it, and the
/// frame's length (both s, source clock); starts more than 100 µs off the
/// line are left out.
fn fit_frames(starts: &VecDeque<f64>) -> Option<(f64, f64)> {
    let &t_ref = starts.back()?;
    if starts.len() < 6 {
        return None;
    }
    let pts: Vec<(f64, f64)> = starts
        .iter()
        .map(|&t| (((t - t_ref) / FRAME_S).round(), t - t_ref))
        .collect();
    let mut keep: Vec<bool> = vec![true; pts.len()];
    let mut line = (0.0, FRAME_S);
    for _ in 0..3 {
        let (mut n, mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for ((k, y), _) in pts.iter().zip(&keep).filter(|(_, k)| **k) {
            n += 1.0;
            sx += k;
            sy += y;
            sxx += k * k;
            sxy += k * y;
        }
        let den = n * sxx - sx * sx;
        if n < 6.0 || den.abs() < 1e-9 {
            return None;
        }
        let b = (n * sxy - sx * sy) / den;
        let a = (sy - b * sx) / n;
        line = (a, b);
        for (k, (x, y)) in keep.iter_mut().zip(&pts) {
            *k = (y - (a + b * x)).abs() < 100e-6;
        }
    }
    let (a, b) = line;
    // A frame length far from 125 ms: not this hub's frames.
    ((b / FRAME_S - 1.0).abs() < 1e-3).then_some((t_ref + a, b))
}

/// A carrier's spectrum, 0..1, `f` Hz from its centre at `rs` symbols a
/// second: flat, then a raised-cosine skirt (roll-off 0.2) to nothing.
fn raised_cosine(f: f64, rs: f64) -> f64 {
    const RO: f64 = 0.2;
    let (lo, hi) = (rs * (1.0 - RO) / 2.0, rs * (1.0 + RO) / 2.0);
    let a = f.abs();
    if a <= lo {
        1.0
    } else if a >= hi {
        0.0
    } else {
        0.5 * (1.0 + (std::f64::consts::PI * (a - lo) / (hi - lo)).cos())
    }
}

/// The inroutes on the VFO: the table's uplink frequencies as a comb of
/// carrier shapes slid along the pool's mean spectrum; the best correlation
/// and the inroutes (mostly) inside the VFO's band there. (A shape with its
/// skirts, not a flat top: a flat top fits anywhere a plateau is wider.)
fn map_inroutes(pool: &PoolView, view: &HubView) -> Option<(f64, Vec<Carrier>)> {
    let rows: Vec<(u16, f64, BurstConfig)> = view
        .inroutes
        .iter()
        .filter_map(|i| Some((i.id, i.table?.uplink_hz, i.config?)))
        .filter(|r| r.2.symbol_rate > 0.0)
        .collect();
    if rows.is_empty() || pool.spectrum.len() < 16 || pool.spectrum_bin_hz <= 0.0 {
        return None;
    }
    let base = rows.iter().map(|r| r.1).fold(f64::INFINITY, f64::min);
    let span = rows.iter().map(|r| r.1).fold(0.0, f64::max) - base;
    let half = pool.half_band_hz;
    let bins: Vec<(f64, f64)> = pool
        .spectrum
        .iter()
        .enumerate()
        .map(|(k, &v)| {
            (
                pool.spectrum_start_hz + k as f64 * pool.spectrum_bin_hz,
                f64::from(v.max(0.0)),
            )
        })
        .filter(|(f, _)| f.abs() <= half)
        .collect();
    let score = |x: f64| -> Option<f64> {
        let centres: Vec<(f64, f64)> = rows
            .iter()
            .map(|r| (x + r.1 - base, r.2.air_rate()))
            .collect();
        if !centres.iter().any(|c| c.0.abs() < half) {
            return None;
        }
        let (mut n, mut sx, mut sy, mut sxx, mut syy, mut sxy) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        for &(f, v) in &bins {
            let t = centres
                .iter()
                .map(|c| raised_cosine(f - c.0, c.1))
                .fold(0.0, f64::max);
            n += 1.0;
            sx += t;
            sy += v;
            sxx += t * t;
            syy += v * v;
            sxy += t * v;
        }
        let d = ((n * sxx - sx * sx) * (n * syy - sy * sy)).sqrt();
        (n > 8.0 && d > 0.0).then(|| (n * sxy - sx * sy) / d)
    };
    let coarse = pool.spectrum_bin_hz.max(2e3);
    let mut best: Option<(f64, f64)> = None;
    let mut x = -half - span;
    while x <= half {
        if let Some(c) = score(x)
            && best.is_none_or(|b| c > b.0)
        {
            best = Some((c, x));
        }
        x += coarse;
    }
    let (mut corr, mut xb) = best?;
    let fine = coarse / 8.0;
    for k in -16..=16 {
        let x = xb + f64::from(k) * fine;
        if let Some(c) = score(x)
            && c > corr
        {
            (corr, xb) = (c, x);
        }
    }
    let carriers = rows
        .iter()
        .map(|r| Carrier {
            inroute: r.0,
            centre_hz: xb + r.1 - base,
            config: r.2,
        })
        .filter(|c| {
            c.centre_hz.abs() + 0.5 * c.config.air_rate() <= half + 0.1 * c.config.air_rate()
        })
        .collect::<Vec<_>>();
    (!carriers.is_empty()).then_some((corr, carriers))
}

/// Each carrier's band power, block by block, over the ring.
struct BandPower {
    t0: f64,
    block_s: f64,
    /// [carrier][block].
    p: Vec<Vec<f32>>,
}

impl BandPower {
    /// Every carrier's power together, block by block.
    fn envelope(&self) -> Vec<(f64, f32)> {
        let n = self.p.first().map_or(0, Vec::len);
        (0..n)
            .map(|b| {
                (
                    self.t0 + b as f64 * self.block_s,
                    self.p.iter().map(|c| c[b]).sum(),
                )
            })
            .collect()
    }

    fn mean(&self, c: usize, t0: f64, t1: f64) -> Option<f64> {
        let a = ((t0 - self.t0) / self.block_s).ceil();
        let b = ((t1 - self.t0) / self.block_s).floor();
        if a < 0.0 || b <= a {
            return None;
        }
        let (a, b) = (a as usize, b as usize);
        let v = self.p.get(c)?.get(a..b)?;
        Some(v.iter().map(|&x| f64::from(x)).sum::<f64>() / v.len() as f64)
    }
    fn end_s(&self) -> f64 {
        self.t0 + self.p.first().map_or(0, Vec::len) as f64 * self.block_s
    }
}

/// The frame plan `p` applies to at lead `lead`: its start, s.
fn frame_start(line: &ArrivalLine, frame: u32, s0: f64, period: f64, lead: i32) -> f64 {
    let j0 = ((line.at(frame) - s0) / period).floor();
    s0 + (j0 + f64::from(lead)) * period
}

/// The lead under which the plans' owners best explain the slots' powers
/// (in dB, the middle 70 % of each slot), with each terminal's level and
/// each carrier's, over the guard silence's noise.
#[allow(clippy::type_complexity)]
fn find_lead(
    bp: &BandPower,
    carriers: &[Carrier],
    plans: &VecDeque<InroutePlan>,
    line: &ArrivalLine,
    s0: f64,
    period: f64,
) -> Option<(LeadFit, HashMap<u16, f64>, Vec<Option<f64>>)> {
    let idx: HashMap<u16, usize> = carriers
        .iter()
        .enumerate()
        .map(|(k, c)| (c.inroute, k))
        .collect();
    let end = bp.end_s();
    let mut fits: Vec<(f64, i32, usize)> = Vec::new();
    for lead in LEADS {
        let mut groups: HashMap<u16, (f64, f64, f64)> = HashMap::new();
        for p in plans {
            let Some(&c) = idx.get(&p.inroute) else {
                continue;
            };
            let fs = frame_start(line, p.frame, s0, period, lead);
            if fs < bp.t0 || fs + period > end {
                continue;
            }
            let slot = carriers[c].slot_s(period);
            for (i, o) in p.owners.iter().enumerate() {
                let a = fs + i as f64 * slot;
                if let Some(v) = bp.mean(c, a + 0.15 * slot, a + 0.85 * slot) {
                    let db = 10.0 * v.max(1e-20).log10();
                    let g = groups.entry(o.terminal()).or_default();
                    g.0 += 1.0;
                    g.1 += db;
                    g.2 += db * db;
                }
            }
        }
        let n: f64 = groups.values().map(|g| g.0).sum();
        if (n as usize) < MIN_SLOTS {
            continue;
        }
        let (s1, s2): (f64, f64) = groups
            .values()
            .fold((0.0, 0.0), |a, g| (a.0 + g.1, a.1 + g.2));
        let total = s2 - s1 * s1 / n;
        let within: f64 = groups.values().map(|g| g.2 - g.1 * g.1 / g.0).sum();
        if total > 0.0 {
            fits.push((1.0 - within / total, lead, n as usize));
        }
    }
    fits.sort_by(|a, b| b.0.total_cmp(&a.0));
    let &(r2, lead, slots) = fits.first()?;
    let next_r2 = fits.get(1).map_or(0.0, |f| f.0);
    // Levels at that lead: each carrier's noise from its guard silence (the
    // last 2.5 ms before each frame), each terminal's slots over it.
    let mut floors: Vec<Vec<f64>> = vec![Vec::new(); carriers.len()];
    let mut powers: HashMap<u16, (f64, f64)> = HashMap::new();
    let mut carrier_p: Vec<Vec<f64>> = vec![Vec::new(); carriers.len()];
    let mut lead_s = Vec::new();
    for p in plans {
        let Some(&c) = idx.get(&p.inroute) else {
            continue;
        };
        let fs = frame_start(line, p.frame, s0, period, lead);
        if fs < bp.t0 + 3e-3 || fs + period > end {
            continue;
        }
        lead_s.push(fs - line.at(p.frame));
        if let Some(v) = bp.mean(c, fs - 2.8e-3, fs - 0.3e-3) {
            floors[c].push(v);
        }
        let slot = carriers[c].slot_s(period);
        for (i, o) in p.owners.iter().enumerate() {
            let a = fs + i as f64 * slot;
            if let Some(v) = bp.mean(c, a + 0.15 * slot, a + 0.85 * slot) {
                let e = powers.entry(o.terminal()).or_default();
                e.0 += v;
                e.1 += 1.0;
                carrier_p[c].push(v);
            }
        }
    }
    let floor: Vec<Option<f64>> = floors
        .iter_mut()
        .map(|f| (!f.is_empty()).then(|| median(f)))
        .collect();
    // A terminal's slots are on several carriers; the carriers' noise is
    // much the same: their median.
    let mut all: Vec<f64> = floor.iter().flatten().copied().collect();
    let noise = (!all.is_empty()).then(|| median(&mut all));
    let cn = |p: f64, n: f64| 10.0 * (p / n - 1.0).max(1e-3).log10();
    let levels = match noise {
        Some(n) if n > 0.0 => powers
            .iter()
            .map(|(&t, &(s, k))| (t, cn(s / k, n)))
            .collect(),
        _ => HashMap::new(),
    };
    let carrier_levels = carrier_p
        .iter()
        .zip(&floor)
        .map(|(p, f)| match f {
            Some(n) if *n > 0.0 && !p.is_empty() => {
                Some(cn(p.iter().sum::<f64>() / p.len() as f64, *n))
            }
            _ => None,
        })
        .collect();
    let lead_s = if lead_s.is_empty() {
        0.0
    } else {
        median(&mut lead_s)
    };
    Some((
        LeadFit {
            lead,
            lead_s,
            r2,
            next_r2,
            slots,
        },
        levels,
        carrier_levels,
    ))
}

/// The plans followed: from which outbound, the inroutes on the VFO, and
/// frame `frame_ref`'s start (the rest `frame_s` apart).
struct Followed {
    from: VfoId,
    carriers: Vec<Carrier>,
    map_corr: f64,
    fit: LeadFit,
    frame_ref: u32,
    start_ref: f64,
    frame_s: f64,
}

impl Followed {
    fn start(&self, frame: u32) -> f64 {
        self.start_ref + f64::from(frame24_diff(frame, self.frame_ref)) * self.frame_s
    }
}

/// The live inroute burst demodulator of a Return pool VFO.
pub struct InrouteBursts {
    rate: f64,
    ring: Ring,
    starts: VecDeque<f64>,
    line: Option<(f64, f64)>,
    cursor: BTreeMap<VfoId, u64>,
    views: BTreeMap<VfoId, Arc<HubView>>,
    plans: BTreeMap<VfoId, VecDeque<InroutePlan>>,
    followed: Option<Followed>,
    tried: Option<LeadFit>,
    carrier_levels: BTreeMap<u16, f64>,
    levels: HashMap<u16, f64>,
    since_align: f64,
    last_frame: BTreeMap<u16, u32>,
    pending: VecDeque<Planned>,
    planned: u64,
    cut: u64,
    late: u64,
    dropped: u64,
    unknown: u64,
    links: u32,
    planner: FftPlanner<f32>,
    tx: Option<SyncSender<Job>>,
    workers: Vec<JoinHandle<()>>,
    shared: Arc<Mutex<Shared>>,
    signal_s: f64,
    lossless: bool,
    zero_trim_hz: f64,
    start_trim_s: f64,
    trims: u32,
}

impl InrouteBursts {
    /// For a pool VFO's baseband at `rate`, with `workers` demodulator
    /// threads (started when a plan is first followed).
    pub fn new(rate: f64, workers: usize) -> Self {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (tx, rx) = mpsc::sync_channel::<Job>(QUEUE);
        let rx = Arc::new(Mutex::new(rx));
        let workers = (0..workers.max(1))
            .map(|i| {
                let (rx, shared) = (Arc::clone(&rx), Arc::clone(&shared));
                std::thread::Builder::new()
                    .name(format!("inroute-demod-{i}"))
                    .spawn(move || work(rate, &rx, &shared))
                    .expect("spawning an inroute burst demodulator")
            })
            .collect();
        InrouteBursts {
            rate,
            ring: Ring::new(rate),
            starts: VecDeque::new(),
            line: None,
            cursor: BTreeMap::new(),
            views: BTreeMap::new(),
            plans: BTreeMap::new(),
            followed: None,
            tried: None,
            carrier_levels: BTreeMap::new(),
            levels: HashMap::new(),
            since_align: 0.0,
            last_frame: BTreeMap::new(),
            pending: VecDeque::new(),
            planned: 0,
            cut: 0,
            late: 0,
            dropped: 0,
            unknown: 0,
            links: 0,
            planner: FftPlanner::new(),
            tx: Some(tx),
            workers,
            shared,
            signal_s: 0.0,
            lossless: false,
            zero_trim_hz: 0.0,
            start_trim_s: 0.0,
            trims: 0,
        }
    }

    /// Wait for the workers rather than letting bursts go (a file played
    /// as fast as it can be).
    pub fn set_lossless(&mut self, on: bool) {
        self.lossless = on;
    }

    /// Whether any outbound has posted a hub's plans.
    pub fn has_plans(&self) -> bool {
        !self.views.is_empty()
    }

    /// The pool's signal from `src_s` on the source clock, the tracker's
    /// view of it, this VFO's link to the plan feed, and where to record
    /// (folder, file tag) while recording.
    pub fn push(
        &mut self,
        x: &[Iq],
        src_s: f64,
        pool: &PoolView,
        link: Option<&PlanLink>,
        record: Option<(&Path, &str)>,
    ) {
        // A jump in the source clock (blocks dropped, a file played round):
        // the frames start again with the ring.
        if (src_s - self.ring.end_s()).abs() > 2.0 / self.rate && self.signal_s > 0.0 {
            self.starts.clear();
            self.line = None;
            self.pending.clear();
        }
        self.ring.push(x, src_s);
        self.signal_s += x.len() as f64 / self.rate;
        if let Some(link) = link {
            self.read_feed(&link.feed, link.id);
        }
        // Nothing to do until some outbound posts a hub's plans.
        if self.views.is_empty() {
            return;
        }
        self.follow_record(record);
        self.since_align += x.len() as f64 / self.rate;
        if self.since_align >= ALIGN_EVERY_S {
            self.since_align = 0.0;
            self.align(pool);
            self.trim();
        }
        self.dispatch();
    }

    fn read_feed(&mut self, feed: &PlanFeed, me: VfoId) {
        for news in feed.read_hubs(me, &mut self.cursor) {
            if let Some(v) = news.view {
                self.views.insert(news.from, v);
            }
            let q = self.plans.entry(news.from).or_default();
            q.extend(news.plans.iter().cloned());
            while q.len() > PLANS_KEEP {
                q.pop_front();
            }
            if self.followed.as_ref().is_some_and(|f| f.from == news.from) {
                self.schedule(&news.plans);
            }
        }
        let cursor = &self.cursor;
        self.views.retain(|id, _| cursor.contains_key(id));
        self.plans.retain(|id, _| cursor.contains_key(id));
        if self
            .followed
            .as_ref()
            .is_some_and(|f| !self.views.contains_key(&f.from))
        {
            self.followed = None;
        }
    }

    /// New frame starts from the inroutes' silences; the line through them.
    fn frames(&mut self, bp: &BandPower) {
        for t in silence_ends(&bp.envelope(), bp.block_s) {
            if self.starts.back().is_none_or(|&l| t > l + 0.06) {
                self.starts.push_back(t);
            }
        }
        while self.starts.len() > STARTS_KEEP {
            self.starts.pop_front();
        }
        self.line = fit_frames(&self.starts);
    }

    /// Each carrier's band power, block by block, over the ring.
    fn band_power(&mut self, carriers: &[Carrier]) -> Option<BandPower> {
        let block = ((BLOCK_S * self.rate).round() as usize).max(8);
        let n = block.next_power_of_two() * 2;
        let fft = self.planner.plan_fft_forward(n);
        let x = self.ring.samples();
        let nblk = x.len() / block;
        if nblk < 100 {
            return None;
        }
        let df = self.rate / n as f64;
        let bins: Vec<Vec<usize>> = carriers
            .iter()
            .map(|c| {
                let half = 0.4 * c.config.air_rate();
                (0..n)
                    .filter(|&k| {
                        let f = if k < n / 2 {
                            k as f64
                        } else {
                            k as f64 - n as f64
                        } * df;
                        (f - c.centre_hz).abs() <= half
                    })
                    .collect()
            })
            .collect();
        let win: Vec<f32> = (0..block)
            .map(|k| {
                let w = (std::f32::consts::PI * (k as f32 + 0.5) / block as f32).sin();
                w * w
            })
            .collect();
        let mut buf = vec![Iq::new(0.0, 0.0); n];
        let mut p = vec![Vec::with_capacity(nblk); carriers.len()];
        for b in 0..nblk {
            for (k, v) in buf.iter_mut().enumerate() {
                *v = if k < block {
                    x[b * block + k] * win[k]
                } else {
                    Iq::new(0.0, 0.0)
                };
            }
            fft.process(&mut buf);
            for (c, bs) in bins.iter().enumerate() {
                p[c].push(bs.iter().map(|&k| buf[k].norm_sqr()).sum());
            }
        }
        Some(BandPower {
            t0: self.ring.start_s,
            block_s: block as f64 / self.rate,
            p,
        })
    }

    /// Put every outbound's inroutes on the VFO and line its plans up with
    /// the frames; follow the best.
    fn align(&mut self, pool: &PoolView) {
        let mut best: Option<Followed> = None;
        let mut levels_best = None;
        self.tried = None;
        let views: Vec<(VfoId, Arc<HubView>)> = self
            .views
            .iter()
            .map(|(k, v)| (*k, Arc::clone(v)))
            .collect();
        for (from, view) in views {
            let Some(line) = view.line else { continue };
            let Some((corr, carriers)) = map_inroutes(pool, &view) else {
                continue;
            };
            if corr < MAP_MIN_CORR {
                continue;
            }
            let Some(bp) = self.band_power(&carriers) else {
                continue;
            };
            self.frames(&bp);
            let Some((s0, period)) = self.line else {
                continue;
            };
            let Some(plans) = self.plans.get(&from) else {
                continue;
            };
            let Some((fit, levels, carrier_levels)) =
                find_lead(&bp, &carriers, plans, &line, s0, period)
            else {
                continue;
            };
            if self.tried.is_none_or(|t| fit.r2 > t.r2) {
                self.tried = Some(fit);
            }
            if fit.r2 < SURE_R2 || fit.r2 < fit.next_r2 + SURE_MARGIN {
                continue;
            }
            if best.as_ref().is_some_and(|b| b.fit.r2 >= fit.r2) {
                continue;
            }
            let Some(last) = plans.back() else { continue };
            best = Some(Followed {
                from,
                frame_ref: last.frame,
                start_ref: frame_start(&line, last.frame, s0, period, fit.lead),
                frame_s: period,
                map_corr: corr,
                fit,
                carriers: carriers.clone(),
            });
            levels_best = Some((levels, carrier_levels, carriers));
        }
        let Some(new) = best else { return };
        if let Some((levels, carrier_levels, carriers)) = levels_best {
            for (t, l) in levels {
                let e = self.levels.entry(t).or_insert(l);
                *e += 0.3 * (l - *e);
            }
            for (c, l) in carriers.iter().zip(carrier_levels) {
                if let Some(l) = l {
                    self.carrier_levels.insert(c.inroute, l);
                }
            }
        }
        // The same plans where they were (a frame's start within a quarter
        // of the shortest slot, the inroutes within 20 kHz): the new line
        // taken, the trims kept.
        if let Some(f) = &mut self.followed
            && f.from == new.from
        {
            let shortest = new
                .carriers
                .iter()
                .map(|c| c.slot_s(new.frame_s))
                .fold(f64::INFINITY, f64::min);
            let moved = (new.start(new.frame_ref) - f.start(new.frame_ref)).abs();
            let shifted = new.carriers.len() == f.carriers.len()
                && new.carriers.iter().zip(&f.carriers).all(|(a, b)| {
                    a.inroute == b.inroute && (a.centre_hz - b.centre_hz).abs() < 20e3
                });
            if moved < shortest / 4.0 && shifted {
                f.fit = new.fit;
                f.map_corr = new.map_corr;
                f.frame_ref = new.frame_ref;
                f.start_ref = new.start_ref;
                f.frame_s = new.frame_s;
                return;
            }
        }
        // Newly followed, or moved: the trims start again, and the recent
        // plans whose bursts are still to come (or in the ring) scheduled.
        self.followed = Some(new);
        self.links += 1;
        self.zero_trim_hz = 0.0;
        self.start_trim_s = 0.0;
        self.trims += 1;
        {
            let mut s = self.shared.lock().unwrap();
            s.trims = self.trims;
            s.offsets.clear();
        }
        self.pending.clear();
        self.last_frame.clear();
        let from = self.followed.as_ref().map(|f| f.from);
        if let Some(q) = from.and_then(|f| self.plans.get(&f)) {
            let recent: Vec<InroutePlan> = q.iter().cloned().collect();
            self.schedule(&recent);
        }
    }

    /// Start or stop the file.
    fn follow_record(&mut self, record: Option<(&Path, &str)>) {
        let mut s = self.shared.lock().unwrap();
        match (record, s.recorder.is_some()) {
            (Some((dir, tag)), false) => {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                s.recorder = Some(Recorder {
                    path: dir.join(format!("decsat-{tag}-inroutes-{stamp}.ibursts")),
                    file: None,
                    written: 0,
                    failed: false,
                });
            }
            (None, true) => {
                if let Some(mut r) = s.recorder.take()
                    && let Some(f) = &mut r.file
                {
                    let _ = f.flush();
                }
            }
            _ => {}
        }
    }

    /// The start and the zero moved by the good bursts' median offsets.
    fn trim(&mut self) {
        let mut s = self.shared.lock().unwrap();
        if s.offsets.len() < TRIM_BURSTS {
            return;
        }
        let mut f: Vec<f64> = s.offsets.iter().map(|o| o.0).collect();
        let mut t: Vec<f64> = s.offsets.iter().map(|o| o.1).collect();
        self.zero_trim_hz += median(&mut f).clamp(-TRIM_MAX_HZ, TRIM_MAX_HZ);
        self.start_trim_s += median(&mut t).clamp(-TRIM_MAX_S, TRIM_MAX_S);
        self.trims += 1;
        s.trims = self.trims;
        s.offsets.clear();
    }

    /// Every slot of `plans` (each inroute's frames newer than any
    /// scheduled) a burst waiting for its signal.
    fn schedule(&mut self, plans: &[InroutePlan]) {
        let Some(f) = &self.followed else { return };
        let keep_from = self.ring.start_s;
        for p in plans {
            let Some(c) = f.carriers.iter().find(|c| c.inroute == p.inroute) else {
                continue;
            };
            if self
                .last_frame
                .get(&p.inroute)
                .is_some_and(|&l| frame24_diff(p.frame, l) <= 0)
            {
                continue;
            }
            self.last_frame.insert(p.inroute, p.frame);
            let shape = InrouteShape {
                symbols: c.config.burst_symbols(),
                modulation: c.config.modulation,
            };
            let slot = c.slot_s(f.frame_s);
            let rate = c.rate(f.frame_s);
            let burst_s = shape.symbols as f64 / rate;
            let fs = f.start(p.frame) + self.start_trim_s;
            for (i, o) in p.owners.iter().enumerate() {
                self.planned += 1;
                if shape.modulation != Modulation::Psk8 {
                    self.unknown += 1;
                    continue;
                }
                let s0 = fs + i as f64 * slot;
                if s0 < keep_from {
                    self.late += 1;
                    continue;
                }
                self.pending.push_back(Planned {
                    start_s: s0,
                    end_s: s0 + burst_s,
                    inroute: p.inroute,
                    frame: p.frame,
                    slot: i as u16,
                    burst: ScheduledBurst {
                        terminal: o.terminal(),
                        code: o.0,
                        slots: 1,
                        centre_hz: c.centre_hz + self.zero_trim_hz,
                        symbol_rate: rate,
                        rate_exact: true,
                        waveform: decsat_ts::newtec::Waveform::Unknown,
                        start: 0,
                        inroute: Some(shape),
                    },
                });
            }
        }
    }

    /// Bursts whose signal the ring now holds, cut out and handed on.
    fn dispatch(&mut self) {
        let end = self.ring.end_s();
        let mut keep = VecDeque::with_capacity(self.pending.len());
        while let Some(p) = self.pending.pop_front() {
            let edge = EDGE_SYMBOLS / p.burst.symbol_rate;
            if p.end_s + edge > end {
                keep.push_back(p);
                continue;
            }
            let Some((iq, start)) = self.ring.slice(p.start_s - edge, p.end_s + edge, p.start_s)
            else {
                self.late += 1;
                continue;
            };
            let job = Job {
                iq,
                burst: ScheduledBurst { start, ..p.burst },
                inroute: p.inroute,
                frame: p.frame,
                slot: p.slot,
                start_s: p.start_s,
                trims: self.trims,
            };
            let Some(tx) = &self.tx else {
                self.dropped += 1;
                continue;
            };
            let sent = if self.lossless {
                tx.send(job).is_ok()
            } else {
                match tx.try_send(job) {
                    Ok(()) => true,
                    Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
                }
            };
            if sent {
                self.cut += 1;
            } else {
                self.dropped += 1;
            }
        }
        self.pending = keep;
    }

    pub fn view(&self) -> InrouteBurstsView {
        let s = self.shared.lock().unwrap();
        let span = match (s.recent.front(), s.recent.back()) {
            (Some(a), Some(b)) if b > a => b - a,
            _ => 0.0,
        };
        let hub = self
            .followed
            .as_ref()
            .and_then(|f| self.views.get(&f.from))
            .or_else(|| self.views.values().next());
        let carriers = self.followed.as_ref().map_or(Vec::new(), |f| {
            f.carriers
                .iter()
                .map(|c| CarrierView {
                    inroute: c.inroute,
                    centre_hz: c.centre_hz + self.zero_trim_hz,
                    symbol_rate: c.config.air_rate(),
                    slot_symbols: c.config.slot_symbols(),
                    burst_symbols: c.config.burst_symbols(),
                    slots: hub
                        .and_then(|h| h.inroute(c.inroute))
                        .map_or(0, |i| i.slots),
                    modulation: c.config.modulation,
                    level_db: self.carrier_levels.get(&c.inroute).copied(),
                })
                .collect()
        });
        // Every terminal the plans name, with what its bursts gave.
        let mut terminals: Vec<InrouteTerminal> = hub.map_or(Vec::new(), |h| {
            h.terminals
                .iter()
                .map(|t| {
                    let mut v = s.terminals.get(&t.id).cloned().unwrap_or(InrouteTerminal {
                        id: t.id,
                        ..Default::default()
                    });
                    v.slots_per_frame = t.per_frame;
                    v.inroutes = t.inroutes.clone();
                    v
                })
                .collect()
        });
        for t in &mut terminals {
            t.level_db = self.levels.get(&t.id).copied();
        }
        InrouteBurstsView {
            from: self.followed.as_ref().map(|f| f.from),
            carriers,
            map_corr: self.followed.as_ref().map_or(0.0, |f| f.map_corr),
            fit: self.followed.as_ref().map(|f| f.fit),
            tried: self.tried,
            frame_s: self.line.map(|l| l.1),
            silences: self.starts.len(),
            zero_trim_hz: self.zero_trim_hz,
            start_trim_s: self.start_trim_s,
            planned: self.planned,
            cut: self.cut,
            demodulated: s.demodulated,
            failed: s.failed,
            late: self.late,
            dropped: self.dropped,
            unknown: self.unknown,
            links: self.links,
            per_s: if span > 0.5 {
                s.recent.len() as f64 / span
            } else {
                0.0
            },
            load: s.busy_s / self.signal_s.max(1e-9),
            workers: self.workers.len(),
            terminals,
            recording: s.recorder.as_ref().map(|r| (r.path.clone(), r.written)),
        }
    }
}

impl Drop for InrouteBursts {
    fn drop(&mut self) {
        self.tx = None;
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
        if let Some(mut r) = self.shared.lock().unwrap().recorder.take()
            && let Some(f) = &mut r.file
        {
            let _ = f.flush();
        }
    }
}

fn work(rate: f64, rx: &Mutex<Receiver<Job>>, shared: &Mutex<Shared>) {
    let mut demod = BurstDemod::new(rate);
    loop {
        let job = match rx.lock().unwrap().recv() {
            Ok(j) => j,
            Err(_) => return,
        };
        let t0 = Instant::now();
        let r = demod.demod(&job.iq, &job.burst);
        let secs = t0.elapsed().as_secs_f64();
        shared.lock().unwrap().take(&job, r, secs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decsat_gse::inroute::{Inroute, InrouteView, Owner};

    #[test]
    fn frames_from_the_guard_silences() {
        // 125 ms frames on a clock 6 ppm fast, each ending in 3.3 ms of
        // noise 10 dB under the carriers, starting 40 ms in.
        let block = BLOCK_S;
        let period = FRAME_S * (1.0 + 6e-6);
        let mut env = Vec::new();
        let mut t = 0.0f64;
        let mut r = crate::tdma::demod::tests::Lcg(4);
        while t < 2.9 {
            let ph = (t - 0.040).rem_euclid(period);
            let silent = ph > period - 3.3e-3;
            let p = if silent { 0.1 } else { 1.0 } * (1.0 + 0.2 * r.gauss());
            env.push((t, p.max(0.01)));
            t += block;
        }
        let ends = silence_ends(&env, block);
        assert!(ends.len() >= 21, "{}", ends.len());
        for &e in &ends {
            let ph = (e - 0.040).rem_euclid(period);
            assert!(ph < 2.0 * block || ph > period - block, "{e}: {ph}");
        }
        let starts: VecDeque<f64> = ends.into_iter().collect();
        let (s0, p) = fit_frames(&starts).unwrap();
        assert!((p - period).abs() < 2e-6, "{p}");
        assert!(
            ((s0 - 0.040).rem_euclid(period)).min(period - (s0 - 0.040).rem_euclid(period)) < 25e-6
        );
    }

    fn config(index: u8, ticks: u32) -> BurstConfig {
        BurstConfig {
            index,
            symbol_rate: 666_000.0,
            slot_ticks: ticks,
            modulation: Modulation::Psk8,
        }
    }

    fn hub(ids_up: &[(u16, f64, BurstConfig)]) -> HubView {
        HubView {
            inroutes: ids_up
                .iter()
                .map(|&(id, up, c)| InrouteView {
                    id,
                    table: Some(Inroute {
                        id,
                        uplink_hz: up,
                        slots: 114,
                        traffic_config: c.index,
                        other_config: c.index + 1,
                    }),
                    config: Some(c),
                    plans: 0,
                    last_frame: None,
                    missed: 0,
                    slots: 113,
                    terminals: 0,
                    kinds: [0; 16],
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn inroutes_put_on_the_spectrum() {
        // Four 800 kHz-spaced carriers across a 3.2 MHz VFO, two more
        // 20 MHz up (outside it): the comb lands on the four.
        let c = config(0, 29077);
        let v = hub(&[
            (0x2e, 14_427.1e6, c),
            (0x30, 14_427.9e6, c),
            (0x32, 14_428.7e6, c),
            (0x33, 14_429.5e6, c),
            (0x34, 14_446.8e6, c),
            (0x35, 14_447.4e6, c),
        ]);
        let centres = [-1_206_000.0, -406_000.0, 394_000.0, 1_194_000.0];
        let bin = 3_000.0;
        let start = -1_600_000.0;
        let spectrum: Vec<f32> = (0..1067)
            .map(|k| {
                let f = start + k as f64 * bin;
                if centres.iter().any(|c| (f - c).abs() < 0.45 * 665_780.0) {
                    10.0
                } else {
                    0.3
                }
            })
            .collect();
        let pool = PoolView {
            spectrum,
            spectrum_start_hz: start,
            spectrum_bin_hz: bin,
            half_band_hz: 1_600_000.0,
            ..Default::default()
        };
        let (corr, carriers) = map_inroutes(&pool, &v).unwrap();
        assert!(corr > 0.8, "{corr}");
        let ids: Vec<u16> = carriers.iter().map(|c| c.inroute).collect();
        assert_eq!(ids, [0x2e, 0x30, 0x32, 0x33]);
        assert!(
            (carriers[0].centre_hz - centres[0]).abs() < 2e3,
            "{}",
            carriers[0].centre_hz
        );
    }

    #[test]
    fn the_lead_is_where_the_owners_explain_the_powers() {
        // One carrier, 40 terminals at levels 0..20 dB; plans arrive 10 ms
        // into the frame they come in and apply five frames on.
        let c = Carrier {
            inroute: 0x2e,
            centre_hz: 0.0,
            config: config(0, 29077),
        };
        let period = FRAME_S;
        let s0 = 0.0;
        let slot = c.slot_s(period);
        let block = BLOCK_S;
        let frames = 14;
        let nblk = (frames as f64 * period / block) as usize;
        let mut p = vec![0.05f32; nblk];
        let mut r = crate::tdma::demod::tests::Lcg(8);
        let mut plans = VecDeque::new();
        let level = |t: u16| 10f32.powf(f32::from(t % 40) / 20.0);
        for f in 0..frames as u32 {
            let owners: Vec<Owner> = (0..113)
                .map(|_| Owner(((r.next() % 40) as u16) << 4 | 2))
                .collect();
            // Frame f's slots carry the owners of the plan that arrived in
            // frame f − 5.
            if f >= 5 {
                let prev: &InroutePlan = &plans[(f - 5) as usize];
                for (i, o) in prev.owners.iter().enumerate() {
                    let a = f as f64 * period + i as f64 * slot;
                    let (b0, b1) = ((a / block) as usize, ((a + slot) / block) as usize);
                    for v in p.iter_mut().take(b1.min(nblk)).skip(b0) {
                        *v = level(o.terminal()) * (1.0 + 0.1 * r.gauss());
                    }
                }
            }
            plans.push_back(InroutePlan {
                frame: 1000 + f,
                inroute: 0x2e,
                owners,
                extra: Vec::new(),
                arrival_s: Some(f as f64 * period + 0.010),
            });
        }
        let bp = BandPower {
            t0: 0.0,
            block_s: block,
            p: vec![p],
        };
        let line = ArrivalLine {
            frame: 1000,
            at_s: 0.010,
            jitter_s: 0.0,
            frames: frames as usize,
        };
        let (fit, levels, _) = find_lead(&bp, &[c], &plans, &line, s0, period).unwrap();
        assert_eq!(fit.lead, 5);
        assert!(fit.r2 > 0.8 && fit.next_r2 < 0.3, "{fit:?}");
        assert!((fit.lead_s - (5.0 * period - 0.010)).abs() < 1e-6);
        // Terminal 39 sits 19.5 dB over terminal 0.
        assert!(levels[&39] > levels[&0] + 15.0, "{levels:?}");
    }
}
