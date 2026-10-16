// made by CasualArclamp
//! Return bursts demodulated live, by the hub's own plan.
//!
//! A Return pool VFO ([`super::pool`]) and an outbound VFO reading a
//! Newtec hub's burst plan (table D1h, [`decsat_ts::newtec`]) on the same
//! receiver: the outbound's FEC thread posts each plan frame to the
//! engine's [`PlanFeed`] as it is read, a third of a second before its
//! bursts arrive. Here, on the pool's thread:
//!
//! 1. every two seconds the plan is lined up with the pool — its zero in
//!    frequency ([`match_plan`], then [`refine_offset`] once timed) and the
//!    lead from a frame's arrival to its first slot ([`align_plan`], the
//!    last answer followed) — against each outbound that posts a plan, the
//!    best kept;
//! 2. each plan frame that comes is scheduled: every burst's slots on the
//!    receiver's source clock, its centre on the VFO, its symbol rate and
//!    waveform from its code ([`decsat_ts::newtec::code_rate`]). A code not
//!    in that table (a live hub uses more than a minute showed) gets the
//!    rate its learned width implies, which the demodulator searches round
//!    — dear: hundreds of rates for a long burst — so one burst of such a
//!    code is searched at a time, and the rate it finds is kept for the
//!    code's later bursts; a code that never demodulates (twenty tries) is
//!    let be;
//! 3. the pool's baseband is kept in a ring of [`RING_S`] (the lead, and
//!    the outbound's own delays, with room); once a burst's slots and a
//!    margin are in it, they are cut out and handed to [`BurstDemod`] on
//!    worker threads — a burst too old for the ring, or with every worker
//!    busy and the queue full, is counted and let go;
//! 4. per terminal: bursts planned, demodulated (the demodulator's `ok`:
//!    preamble and MER both good), failed; MER, carrier and timing against
//!    the plan; the latest data symbols for a constellation;
//! 5. the plan's zero and the bursts' start trimmed by the median carrier
//!    and timing offsets of the bursts lately demodulated (each terminal's
//!    own off-tune then shows against the rest): the pool's spectrum puts
//!    the zero to a couple of kHz, its envelope's rises the start to tens
//!    of µs; the bursts themselves to a few Hz and µs.
//!
//! On Record each terminal's bursts go to a file of their own,
//! `decsat-<VFO>-<frequency>-t<terminal>-<unix time>.rbursts`:
//!
//! ```text
//! "DSRET001"                          8-byte magic, once
//! per burst, little-endian:
//!   u32  plan frame counter
//!   u8   first slot, u8 slots
//!   u16  terminal, u16 code
//!   u8   waveform                     0 16APSK 8+8, 1 sync, 2 8PSK
//!   u8   bits a symbol (of the LLRs)
//!   f64  slot start, s                the receiver's source clock
//!   f32  MER, dB
//!   f32  carrier against the plan, Hz
//!   f32  first symbol against the slot start, µs
//!   f32  preamble fit, 0..1
//!   u32  n, then n × (f32 I, f32 Q)   data symbols, on the constellation
//!   u32  m, then m × f32              LLRs, max-log, positive for a 0
//! ```

use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use decsat_core::Iq;
use decsat_ts::newtec::{PlanClock, PlanFrame, PlanView, Waveform, code_rate, rate_from_width};

use super::burst_demod::{BurstDemod, BurstResult, ScheduledBurst};
use super::pool::{PlanTiming, PoolView, align_plan, match_plan, refine_offset};
use crate::plan_feed::{PlanFeed, PlanLink};
use crate::vfo::VfoId;

/// The pool's baseband kept, s: the plan's lead (0.3 s on 172B) and the
/// outbound's own delays (its FEC queue, the frames' journey here) with
/// room.
pub const RING_S: f64 = 1.5;

/// Signal kept before a burst's planned start and after its last slot, s:
/// the demodulator looks 0.6 ms before the start for its preamble.
const BEFORE_S: f64 = 0.7e-3;
const AFTER_S: f64 = 0.5e-3;

/// Signal before now whose bursts are cut when a plan is taken up, s.
const CATCH_UP_S: f64 = 0.4;

/// How often the plan is lined up with the pool again, s of signal.
const ALIGN_EVERY_S: f64 = 2.0;

/// Bursts queued for the workers before new ones are let go.
const QUEUE: usize = 256;

/// Data symbols kept per terminal for its constellation.
const KEEP_SYMBOLS: usize = 800;

/// Bursts of a code that never demodulates before it is let be.
const GIVE_UP: u32 = 20;

/// Good bursts the trims of the zero and the start wait for, and the most
/// either moves at a time (Hz, s).
const TRIM_BURSTS: usize = 30;
const TRIM_MAX_HZ: f64 = 5e3;
const TRIM_MAX_S: f64 = 0.3e-3;

/// How fast the per-terminal figures follow new bursts (a burst's weight).
const FOLLOW: f64 = 0.1;

/// A lead believed: it stands out from every other one.
fn sure(t: &PlanTiming) -> bool {
    t.corr >= 0.3 && t.corr >= t.next_corr + 0.08
}

/// The pool's baseband for the last [`RING_S`], with the source-clock time
/// of its first sample.
pub(super) struct Ring {
    rate: f64,
    pub(super) start_s: f64,
    buf: VecDeque<Iq>,
    cap: usize,
}

impl Ring {
    pub(super) fn new(rate: f64) -> Self {
        Ring {
            rate,
            start_s: 0.0,
            buf: VecDeque::new(),
            cap: (RING_S * rate) as usize,
        }
    }

    pub(super) fn end_s(&self) -> f64 {
        self.start_s + self.buf.len() as f64 / self.rate
    }

    /// Signal whose first sample came at `src_s`; a jump (blocks dropped,
    /// a file played round) starts the ring again.
    pub(super) fn push(&mut self, x: &[Iq], src_s: f64) {
        if self.buf.is_empty() || (src_s - self.end_s()).abs() > 2.0 / self.rate {
            self.buf.clear();
            self.start_s = src_s;
        }
        self.buf.extend(x);
        let over = self.buf.len().saturating_sub(self.cap);
        if over > 0 {
            self.buf.drain(..over);
            self.start_s += over as f64 / self.rate;
        }
    }

    /// The samples held, oldest first.
    pub(super) fn samples(&self) -> &VecDeque<Iq> {
        &self.buf
    }

    /// The samples from `t0` to `t1`, and the index of `at` among them;
    /// `None` unless the ring holds them all.
    pub(super) fn slice(&self, t0: f64, t1: f64, at: f64) -> Option<(Vec<Iq>, usize)> {
        let i0 = ((t0 - self.start_s) * self.rate).round();
        let i1 = ((t1 - self.start_s) * self.rate).round();
        if i0 < 0.0 || i1 > self.buf.len() as f64 || i1 <= i0 {
            return None;
        }
        let (i0, i1) = (i0 as usize, i1 as usize);
        let k = (((at - self.start_s) * self.rate).round() as usize).clamp(i0, i1) - i0;
        Some((self.buf.range(i0..i1).copied().collect(), k))
    }
}

/// A burst scheduled, waiting for its slots.
struct Planned {
    /// Its first slot's start and its last slot's end, source clock, s.
    start_s: f64,
    end_s: f64,
    counter: u32,
    slot: u8,
    burst: ScheduledBurst,
}

/// A burst cut out, for a worker.
struct Job {
    iq: Vec<Iq>,
    burst: ScheduledBurst,
    counter: u32,
    slot: u8,
    start_s: f64,
    /// The trims it was scheduled with (their generation).
    trims: u32,
}

/// One terminal's bursts as demodulated.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TerminalBursts {
    pub id: u16,
    /// Its bursts cut out and handed to the demodulator, those it took
    /// (preamble and MER good) and those it did not.
    pub cut: u64,
    pub demodulated: u64,
    pub failed: u64,
    /// Over its recent good bursts: MER, dB; the carrier against the plan
    /// (its frequency, the zero as found), Hz; the first symbol against the
    /// slot's start, µs; the preamble's fit.
    pub mer_db: f32,
    pub freq_hz: f64,
    pub timing_us: f64,
    pub fit: f32,
    /// Its latest good burst: code, waveform and symbol rate.
    pub code: u16,
    pub waveform: Option<Waveform>,
    pub symbol_rate: f64,
    /// Its latest data symbols, oldest first.
    pub symbols: Vec<Iq>,
}

/// When the followed plan's frames start on the receiver's source clock:
/// frame `counter` at `start_s`, the rest `frame_s` apart, their slots
/// `slot_s` apart — the start trimmed by the bursts themselves.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameClock {
    pub counter: u32,
    pub start_s: f64,
    pub frame_s: f64,
    pub slot_s: f64,
}

impl FrameClock {
    /// Frame `counter`'s first slot, s.
    pub fn start(&self, counter: u32) -> f64 {
        self.start_s + f64::from(counter.wrapping_sub(self.counter) as i32) * self.frame_s
    }
}

/// What the live burst demodulator is doing, for display.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BurstsView {
    /// The outbound VFO whose plan is followed, the plan's zero on this
    /// VFO (Hz from its centre) and its timing.
    pub from: Option<VfoId>,
    pub offset_hz: f64,
    pub timing: Option<PlanTiming>,
    /// What the bursts themselves moved the zero (Hz) and their start (s)
    /// by, from where the pool put them (included in `offset_hz`).
    pub zero_trim_hz: f64,
    pub start_trim_s: f64,
    /// Where the plan's frames fall, as the bursts are cut.
    pub frames: Option<FrameClock>,
    /// Bursts scheduled; cut and handed on; demodulated; failed; too old
    /// for the ring by the time their plan came; let go with the workers
    /// behind; of codes with neither a rate nor a width; not tried (a code
    /// whose rate is being searched, or one that never demodulates).
    pub planned: u64,
    pub cut: u64,
    pub demodulated: u64,
    pub failed: u64,
    pub late: u64,
    pub dropped: u64,
    pub unknown: u64,
    pub skipped: u64,
    /// Codes not in the table whose rate a search found, and codes let be.
    pub rates_found: usize,
    /// Times a plan was taken up, or its timing found moved.
    pub links: u32,
    /// The best timing the last look found, sure or not (how close a plan
    /// not yet taken up is).
    pub tried: Option<PlanTiming>,
    pub codes_given_up: usize,
    /// Bursts demodulated a second, lately; and the workers' time a second
    /// of signal (1.0 = one core flat out), and how many.
    pub per_s: f64,
    pub load: f64,
    pub workers: usize,
    /// By terminal.
    pub terminals: Vec<TerminalBursts>,
    /// Recording: the folder, and bursts written.
    pub recording: Option<(PathBuf, u64)>,
}

/// Each terminal's file while recording.
struct Recorder {
    dir: PathBuf,
    tag: String,
    stamp: u64,
    files: BTreeMap<u16, BufWriter<File>>,
    written: u64,
    failed: bool,
}

impl Recorder {
    fn write(&mut self, job: &Job, r: &BurstResult) {
        if self.failed {
            return;
        }
        let path = self.dir.join(format!(
            "decsat-{}-t{}-{}.rbursts",
            self.tag, r.terminal, self.stamp
        ));
        let f = match self.files.entry(r.terminal) {
            std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::btree_map::Entry::Vacant(e) => {
                let _ = std::fs::create_dir_all(&self.dir);
                match File::create(&path) {
                    Ok(f) => {
                        let mut w = BufWriter::new(f);
                        let _ = w.write_all(b"DSRET001");
                        e.insert(w)
                    }
                    Err(_) => {
                        self.failed = true;
                        return;
                    }
                }
            }
        };
        let wf = match r.waveform {
            Waveform::Apsk8p8 => 0u8,
            Waveform::Sync => 1,
            Waveform::Psk8 => 2,
            Waveform::Unknown => 255,
        };
        let mut rec = Vec::with_capacity(48 + 8 * r.symbols.len() + 4 * r.llrs.len());
        rec.extend_from_slice(&job.counter.to_le_bytes());
        rec.push(job.slot);
        rec.push(job.burst.slots);
        rec.extend_from_slice(&r.terminal.to_le_bytes());
        rec.extend_from_slice(&r.code.to_le_bytes());
        rec.push(wf);
        rec.push(r.bits_per_symbol);
        rec.extend_from_slice(&job.start_s.to_le_bytes());
        rec.extend_from_slice(&r.mer_db.to_le_bytes());
        rec.extend_from_slice(&(r.freq_offset_hz as f32).to_le_bytes());
        rec.extend_from_slice(&((r.timing_offset_s * 1e6) as f32).to_le_bytes());
        rec.extend_from_slice(&r.preamble_fit.to_le_bytes());
        rec.extend_from_slice(&(r.symbols.len() as u32).to_le_bytes());
        for s in &r.symbols {
            rec.extend_from_slice(&s.re.to_le_bytes());
            rec.extend_from_slice(&s.im.to_le_bytes());
        }
        rec.extend_from_slice(&(r.llrs.len() as u32).to_le_bytes());
        for l in &r.llrs {
            rec.extend_from_slice(&l.to_le_bytes());
        }
        if f.write_all(&rec).is_err() {
            self.failed = true;
        } else {
            self.written += 1;
        }
    }
}

/// What the workers share with the pool's thread.
#[derive(Default)]
struct Shared {
    terminals: BTreeMap<u16, TerminalBursts>,
    demodulated: u64,
    failed: u64,
    /// Worker time spent, s.
    busy_s: f64,
    /// Demodulated bursts' times (source clock), lately: the rate.
    recent: VecDeque<f64>,
    recorder: Option<Recorder>,
    /// The trims now in use, and the good bursts' carrier and timing
    /// offsets (Hz, s) scheduled with them.
    trims: u32,
    offsets: Vec<(f64, f64)>,
    /// What each code has done.
    codes: BTreeMap<u16, CodeState>,
}

/// A code's bursts so far.
#[derive(Default)]
struct CodeState {
    ok: u64,
    /// Failures since the last success.
    failing: u32,
    /// The rate a search found, for a code not in the table.
    rate: Option<f64>,
    /// A burst of it searching for its rate now.
    searching: bool,
}

impl Shared {
    fn take(&mut self, job: &Job, r: Option<BurstResult>, secs: f64) {
        self.busy_s += secs;
        let t = self
            .terminals
            .entry(job.burst.terminal)
            .or_insert_with(|| TerminalBursts {
                id: job.burst.terminal,
                ..Default::default()
            });
        t.cut += 1;
        let code = self.codes.entry(job.burst.code).or_default();
        if !job.burst.rate_exact {
            code.searching = false;
        }
        let Some(r) = r.filter(|r| r.ok) else {
            code.failing += 1;
            t.failed += 1;
            self.failed += 1;
            return;
        };
        code.ok += 1;
        code.failing = 0;
        if !job.burst.rate_exact {
            code.rate = Some(r.symbol_rate);
        }
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
        if job.trims == self.trims {
            self.offsets.push((r.freq_offset_hz, r.timing_offset_s));
        }
        t.timing_us = follow(t.timing_us, r.timing_offset_s * 1e6);
        t.fit = follow(f64::from(t.fit), f64::from(r.preamble_fit)) as f32;
        t.code = r.code;
        t.waveform = Some(r.waveform);
        t.symbol_rate = r.symbol_rate;
        t.symbols.extend_from_slice(&r.symbols);
        let over = t.symbols.len().saturating_sub(KEEP_SYMBOLS);
        t.symbols.drain(..over);
        self.recent.push_back(job.start_s);
        while self.recent.front().is_some_and(|&s| s < job.start_s - 4.0) {
            self.recent.pop_front();
        }
        if let Some(rec) = &mut self.recorder {
            rec.write(job, &r);
        }
    }
}

/// The plan followed: which outbound, its view, where its zero sits and
/// when its frames land.
struct Followed {
    from: VfoId,
    view: Arc<PlanView>,
    offset_hz: f64,
    timing: PlanTiming,
    /// The arrival line the timing was found with (the two go together).
    clock: PlanClock,
}

/// The median of `v` (reordered); 0 when empty.
pub(super) fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let m = v.len() / 2;
    *v.select_nth_unstable_by(m, f64::total_cmp).1
}

/// The live burst demodulator of a Return pool VFO.
pub struct PoolBursts {
    rate: f64,
    ring: Ring,
    cursor: BTreeMap<VfoId, u64>,
    /// Every outbound's latest plan view.
    views: BTreeMap<VfoId, Arc<PlanView>>,
    followed: Option<Followed>,
    /// The newest plan frame scheduled.
    last_counter: Option<u32>,
    since_align: f64,
    pending: VecDeque<Planned>,
    planned: u64,
    cut: u64,
    late: u64,
    dropped: u64,
    unknown: u64,
    skipped: u64,
    /// Times a plan was taken up (or its timing moved).
    links: u32,
    /// The newest frame scheduled when the trims last moved.
    trimmed_at: Option<u32>,
    /// The best-matching plan's frame and slots.
    hint: Option<(f64, usize)>,
    /// The best timing the last alignment found, sure or not.
    tried: Option<PlanTiming>,
    tx: Option<SyncSender<Job>>,
    workers: Vec<JoinHandle<()>>,
    shared: Arc<Mutex<Shared>>,
    /// Signal pushed, s (for the workers' load).
    signal_s: f64,
    /// Wait for the workers rather than let bursts go (a file played as
    /// fast as it can be).
    lossless: bool,
    /// The trims of the plan's zero (Hz) and of the bursts' start (s), and
    /// their generation.
    zero_trim_hz: f64,
    start_trim_s: f64,
    trims: u32,
}

impl PoolBursts {
    /// For a pool VFO's baseband at `rate`, with `workers` demodulator
    /// threads.
    pub fn new(rate: f64, workers: usize) -> Self {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (tx, rx) = mpsc::sync_channel::<Job>(QUEUE);
        // Rust note: a standard channel has one receiver; the workers
        // share it behind a mutex, each taking the next job in turn.
        let rx = Arc::new(Mutex::new(rx));
        let workers = (0..workers.max(1))
            .map(|i| {
                let (rx, shared) = (Arc::clone(&rx), Arc::clone(&shared));
                std::thread::Builder::new()
                    .name(format!("burst-demod-{i}"))
                    .spawn(move || work(rate, &rx, &shared))
                    .expect("spawning a burst demodulator")
            })
            .collect();
        PoolBursts {
            rate,
            ring: Ring::new(rate),
            cursor: BTreeMap::new(),
            views: BTreeMap::new(),
            followed: None,
            last_counter: None,
            since_align: 0.0,
            pending: VecDeque::new(),
            planned: 0,
            cut: 0,
            late: 0,
            dropped: 0,
            unknown: 0,
            skipped: 0,
            links: 0,
            trimmed_at: None,
            hint: None,
            tried: None,
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

    /// Wait for the workers rather than letting bursts go when they are
    /// behind (a file played as fast as it can be).
    pub fn set_lossless(&mut self, on: bool) {
        self.lossless = on;
    }

    /// Workers for a machine: half its threads, one to eight. A busy pool
    /// (172B: 400 bursts a second) takes about three cores' worth; the
    /// spare workers soak up the frames whose plans come together.
    pub fn default_workers() -> usize {
        std::thread::available_parallelism()
            .map_or(2, |n| n.get() / 2)
            .clamp(1, 8)
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
        self.ring.push(x, src_s);
        self.signal_s += x.len() as f64 / self.rate;
        if let Some(link) = link {
            self.read_feed(&link.feed, link.id);
        }
        self.since_align += x.len() as f64 / self.rate;
        if self.since_align >= ALIGN_EVERY_S {
            self.since_align = 0.0;
            self.align(pool);
            self.trim();
        }
        self.dispatch();
        self.follow_record(record);
    }

    /// New plan views and frames from the other VFOs.
    fn read_feed(&mut self, feed: &PlanFeed, me: VfoId) {
        for news in feed.read(me, &mut self.cursor) {
            if let Some(v) = news.view {
                self.views.insert(news.from, v);
            }
            if self.followed.as_ref().is_some_and(|f| f.from == news.from) {
                self.schedule(&news.frames);
            }
        }
        // An outbound gone: its plan with it.
        let cursor = &self.cursor;
        self.views.retain(|id, _| cursor.contains_key(id));
        if self
            .followed
            .as_ref()
            .is_some_and(|f| !self.views.contains_key(&f.from))
        {
            self.followed = None;
        }
    }

    /// Line every outbound's plan up with the pool; follow the best timed.
    fn align(&mut self, pool: &PoolView) {
        // The plan that lines up best in frequency tells the pool its
        // frame and slots, before (and whether or not) it is timed.
        self.hint = self
            .views
            .values()
            .filter_map(|v| Some((match_plan(pool, v)?.corr, v.period_s?, v.slots)))
            .filter(|h| h.0 >= 0.5 && h.2 > 0)
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .map(|h| (h.1, usize::from(h.2)));
        let mut best: Option<Followed> = None;
        self.tried = None;
        for (&from, view) in &self.views {
            let Some(clock) = view.clock else { continue };
            let Some(m) = match_plan(pool, view).filter(|m| m.corr >= 0.5) else {
                continue;
            };
            let prior = self
                .followed
                .as_ref()
                .filter(|f| f.from == from)
                .map(|f| f.timing);
            let Some(t) = align_plan(pool, view, m.offset_hz, prior.as_ref()) else {
                continue;
            };
            if self.tried.is_none_or(|b| t.corr > b.corr) {
                self.tried = Some(t);
            }
            if !sure(&t) {
                continue;
            }
            let offset_hz = refine_offset(pool, view, &t, m.offset_hz).map_or(m.offset_hz, |o| o.0);
            if best.as_ref().is_none_or(|b| t.corr > b.timing.corr) {
                best = Some(Followed {
                    from,
                    view: Arc::clone(view),
                    offset_hz,
                    timing: t,
                    clock,
                });
            }
        }
        let Some(new) = best else {
            // Nothing timed now: the last answer stands.
            return;
        };
        // The same plan where it was (its frames' starts within a quarter
        // slot, its zero within 10 kHz): keep the timing and zero followed,
        // which the trims have refined since — the pool's own look wanders
        // by tens of µs and hundreds of Hz from one to the next. Only the
        // plan's view and the fit's figures are new.
        if let Some(f) = &mut self.followed
            && f.from == new.from
        {
            // (Against where the bursts are cut: the start's trim in.)
            let c = new.clock.counter;
            let moved = (new.timing.frame_start(&new.clock, c)
                - f.timing.frame_start(&f.clock, c)
                - self.start_trim_s)
                .abs();
            if moved < f.timing.slot_s / 4.0 && (new.offset_hz - f.offset_hz).abs() < 10e3 {
                f.view = new.view;
                f.timing.corr = new.timing.corr;
                f.timing.next_corr = new.timing.next_corr;
                f.timing.median_corr = new.timing.median_corr;
                f.timing.frames = new.timing.frames;
                return;
            }
        }
        // A plan newly followed, or moved: the trims start again, and the
        // plan's recent frames whose bursts the ring still holds are
        // scheduled now.
        self.followed = Some(new);
        self.links += 1;
        self.trimmed_at = None;
        self.zero_trim_hz = 0.0;
        self.start_trim_s = 0.0;
        self.trims += 1;
        {
            let mut s = self.shared.lock().unwrap();
            s.trims = self.trims;
            s.offsets.clear();
        }
        self.last_counter = None;
        self.pending.clear();
        let Some(f) = &self.followed else { return };
        // (Only the last few frames': all the ring holds at once would
        // overflow the workers' queue.)
        let keep_from = (self.ring.start_s + BEFORE_S).max(self.ring.end_s() - CATCH_UP_S);
        let fresh: Vec<PlanFrame> = f
            .view
            .recent
            .iter()
            .filter(|fr| f.timing.frame_start(&f.clock, fr.counter) + f.timing.frame_s > keep_from)
            .cloned()
            .collect();
        self.schedule(&fresh);
    }

    /// The zero and the start moved by the median offsets of the good
    /// bursts scheduled with the trims now in use, once there are enough.
    ///
    /// The frame's length too: the pool measures it on the receiver's
    /// clock to only a few parts in 10⁶ (a HackRF is 6 ppm off the hub's),
    /// which over minutes walks the bursts' start by hundreds of µs. The
    /// start's drift since the last trim, over the frames since, says how
    /// far: half of it a frame goes into the frame's length (the line
    /// rebased on the newest frame first, so no start jumps) — with the
    /// start's own trim, a second-order loop.
    fn trim(&mut self) {
        let mut s = self.shared.lock().unwrap();
        if s.offsets.len() < TRIM_BURSTS {
            return;
        }
        let mut f: Vec<f64> = s.offsets.iter().map(|o| o.0).collect();
        let mut t: Vec<f64> = s.offsets.iter().map(|o| o.1).collect();
        let late = median(&mut t).clamp(-TRIM_MAX_S, TRIM_MAX_S);
        self.zero_trim_hz += median(&mut f).clamp(-TRIM_MAX_HZ, TRIM_MAX_HZ);
        self.start_trim_s += late;
        self.trims += 1;
        s.trims = self.trims;
        s.offsets.clear();
        drop(s);
        if let (Some(fl), Some(c)) = (&mut self.followed, self.last_counter) {
            if let Some(c0) = self.trimmed_at {
                let n = c.wrapping_sub(c0) as i32;
                if n >= 10 {
                    let start = fl.timing.frame_start(&fl.clock, c);
                    fl.clock.counter = c;
                    fl.clock.at_s = start - fl.timing.lead_s;
                    fl.timing.frame_s += 0.5 * late / f64::from(n);
                }
            }
            self.trimmed_at = Some(c);
        }
    }

    /// Each burst of `frames` (newer than any scheduled) waiting for its
    /// slots.
    fn schedule(&mut self, frames: &[PlanFrame]) {
        let Some(f) = &self.followed else { return };
        let (clock, t) = (f.clock, f.timing);
        for fr in frames {
            if self
                .last_counter
                .is_some_and(|c| fr.counter.wrapping_sub(c) as i32 <= 0)
            {
                continue;
            }
            self.last_counter = Some(fr.counter);
            let start = t.frame_start(&clock, fr.counter);
            for b in &fr.bursts {
                self.planned += 1;
                let slots = b.slots();
                let (rate, exact, waveform) = match code_rate(b.code) {
                    Some(c) if c.waveform != Waveform::Unknown => {
                        (c.symbol_rate_hz, true, c.waveform)
                    }
                    // The rate known, the waveform not: the commonest.
                    Some(c) => (c.symbol_rate_hz, true, Waveform::Apsk8p8),
                    _ => match f.view.width_of(b.code) {
                        // Not in the table: the rate its width implies,
                        // searched round, as the commonest waveform.
                        Some(w) => (rate_from_width(w), false, Waveform::Apsk8p8),
                        None => {
                            self.unknown += 1;
                            continue;
                        }
                    },
                };
                let s0 = start + f64::from(b.slot) * t.slot_s + self.start_trim_s;
                self.pending.push_back(Planned {
                    start_s: s0,
                    end_s: s0 + f64::from(slots) * t.slot_s,
                    counter: fr.counter,
                    slot: b.slot,
                    burst: ScheduledBurst {
                        terminal: b.terminal,
                        code: b.code,
                        slots,
                        centre_hz: b.freq_hz() + f.offset_hz + self.zero_trim_hz,
                        symbol_rate: rate,
                        rate_exact: exact,
                        waveform,
                        start: 0,
                        inroute: None,
                    },
                });
            }
        }
    }

    /// Bursts whose slots the ring now holds, cut out and handed on; those
    /// it no longer holds, let go.
    fn dispatch(&mut self) {
        let end = self.ring.end_s();
        let mut keep = VecDeque::with_capacity(self.pending.len());
        while let Some(p) = self.pending.pop_front() {
            if p.end_s + AFTER_S > end {
                keep.push_back(p);
                continue;
            }
            // A code that never demodulates is let be; one whose rate is
            // being searched waits for the search; a rate found is used.
            let mut burst = p.burst;
            {
                let mut s = self.shared.lock().unwrap();
                let c = s.codes.entry(burst.code).or_default();
                if c.ok == 0 && c.failing >= GIVE_UP {
                    self.skipped += 1;
                    continue;
                }
                if !burst.rate_exact {
                    if let Some(r) = c.rate {
                        burst.symbol_rate = r;
                        burst.rate_exact = true;
                    } else if c.searching {
                        self.skipped += 1;
                        continue;
                    } else {
                        c.searching = true;
                    }
                }
            }
            let Some((iq, start)) =
                self.ring
                    .slice(p.start_s - BEFORE_S, p.end_s + AFTER_S, p.start_s)
            else {
                self.late += 1;
                if !burst.rate_exact {
                    self.shared
                        .lock()
                        .unwrap()
                        .codes
                        .entry(burst.code)
                        .or_default()
                        .searching = false;
                }
                continue;
            };
            let job = Job {
                iq,
                burst: ScheduledBurst { start, ..burst },
                counter: p.counter,
                slot: p.slot,
                start_s: p.start_s,
                trims: self.trims,
            };
            let (burst_code, burst_exact) = (job.burst.code, job.burst.rate_exact);
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
                if !burst_exact {
                    self.shared
                        .lock()
                        .unwrap()
                        .codes
                        .entry(burst_code)
                        .or_default()
                        .searching = false;
                }
            }
        }
        self.pending = keep;
    }

    /// Start or stop the files.
    fn follow_record(&mut self, record: Option<(&Path, &str)>) {
        let mut s = self.shared.lock().unwrap();
        match (record, &s.recorder) {
            (Some((dir, tag)), None) => {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                s.recorder = Some(Recorder {
                    dir: dir.to_path_buf(),
                    tag: tag.to_string(),
                    stamp,
                    files: BTreeMap::new(),
                    written: 0,
                    failed: false,
                });
            }
            (None, Some(_)) => {
                if let Some(mut r) = s.recorder.take() {
                    for f in r.files.values_mut() {
                        let _ = f.flush();
                    }
                }
            }
            _ => {}
        }
    }

    /// The frame and its slots by the plan that best lines up with the
    /// pool in frequency (for the pool's own grid: see
    /// [`super::pool::PoolTracker::set_plan_hint`]).
    pub fn plan_hint(&self) -> Option<(f64, usize)> {
        self.hint
    }

    pub fn view(&self) -> BurstsView {
        let s = self.shared.lock().unwrap();
        let span = match (s.recent.front(), s.recent.back()) {
            (Some(a), Some(b)) if b > a => b - a,
            _ => 0.0,
        };
        BurstsView {
            from: self.followed.as_ref().map(|f| f.from),
            offset_hz: self
                .followed
                .as_ref()
                .map_or(0.0, |f| f.offset_hz + self.zero_trim_hz),
            timing: self.followed.as_ref().map(|f| f.timing),
            zero_trim_hz: self.zero_trim_hz,
            start_trim_s: self.start_trim_s,
            frames: self.followed.as_ref().map(|f| FrameClock {
                counter: f.clock.counter,
                start_s: f.timing.frame_start(&f.clock, f.clock.counter) + self.start_trim_s,
                frame_s: f.timing.frame_s,
                slot_s: f.timing.slot_s,
            }),
            planned: self.planned,
            cut: self.cut,
            demodulated: s.demodulated,
            failed: s.failed,
            late: self.late,
            dropped: self.dropped,
            unknown: self.unknown,
            skipped: self.skipped,
            links: self.links,
            tried: self.tried,
            rates_found: s.codes.values().filter(|c| c.rate.is_some()).count(),
            codes_given_up: s
                .codes
                .values()
                .filter(|c| c.ok == 0 && c.failing >= GIVE_UP)
                .count(),
            per_s: if span > 0.5 {
                s.recent.len() as f64 / span
            } else {
                0.0
            },
            load: s.busy_s / self.signal_s.max(1e-9),
            workers: self.workers.len(),
            terminals: s.terminals.values().cloned().collect(),
            recording: s.recorder.as_ref().map(|r| (r.dir.clone(), r.written)),
        }
    }
}

impl Drop for PoolBursts {
    fn drop(&mut self) {
        // The workers end when the queue closes.
        self.tx = None;
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
        if let Some(mut r) = self.shared.lock().unwrap().recorder.take() {
            for f in r.files.values_mut() {
                let _ = f.flush();
            }
        }
    }
}

/// A worker: bursts off the queue, through the demodulator, into the
/// tallies (and the files).
fn work(rate: f64, rx: &Mutex<Receiver<Job>>, shared: &Mutex<Shared>) {
    let mut demod = BurstDemod::new(rate);
    loop {
        // The lock is held only to take a job, not while demodulating.
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

    #[test]
    fn the_ring_cuts_what_it_holds() {
        let mut r = Ring::new(1000.0);
        let x: Vec<Iq> = (0..3000).map(|k| Iq::new(k as f32, 0.0)).collect();
        // 1.5 s kept of 3 s pushed from 10 s on.
        for (j, c) in x.chunks(100).enumerate() {
            r.push(c, 10.0 + j as f64 * 0.1);
        }
        assert!((r.start_s - 11.5).abs() < 1e-9);
        assert!((r.end_s() - 13.0).abs() < 1e-9);
        let (s, at) = r.slice(12.0, 12.01, 12.005).unwrap();
        assert_eq!(s.len(), 10);
        assert_eq!(s[0].re, 2000.0);
        assert_eq!(at, 5);
        assert!(r.slice(11.0, 11.1, 11.0).is_none());
        assert!(r.slice(12.9, 13.1, 13.0).is_none());
        // A jump starts it again.
        r.push(&x[..10], 20.0);
        assert!((r.start_s - 20.0).abs() < 1e-9 && r.buf.len() == 10);
    }
}
