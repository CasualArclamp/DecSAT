//! TDMA: carriers that terminals take turns on, in bursts.
//!
//! A TDMA VFO may span one carrier or a whole group of them (an MF-TDMA
//! return, where terminals hop between carriers). Its first look finds the
//! carriers in it; each then gets its own down-converter and matched filter,
//! and from there:
//!
//! 1. [`burst`]: bursts found by their envelope;
//! 2. [`demod`]: each burst demodulated on its own (feedforward timing and
//!    carrier);
//! 3. [`layout`]: once enough bursts are in, what every burst has in common
//!    — preamble, pilots, fixed fields — read blind; each burst is then
//!    lined up on its known symbols and its data taken out;
//! 4. [`slots`]: the slot grid and frame the bursts keep, and the terminals
//!    sending them.
//!
//! [`pool`] is a decoder of its own (*Return pool*): the whole VFO as a
//! return pool, its layout over time and the frame grid of its bursts.
//! [`burst_demod`] demodulates a Newtec return burst the hub's plan has
//! scheduled (its terminal, code, frequency and slot known).
//!
//! Recording writes every burst's data symbols to a `.bursts` file (and a
//! line per burst to a `.csv`, with its header fields' values — the short
//! fields ahead of the payload, [`layout::Layout::header_runs`]), for
//! working out the coding offline:
//!
//! ```text
//! "DSTDMA01"                          8-byte magic, once
//! per burst, little-endian:
//!   u8   channel                      (as listed in the view)
//!   f64  time, s                      first symbol, since the VFO began
//!   f32  power, dB                    relative
//!   f32  carrier offset, Hz           from the VFO's centre
//!   f32  MER, dB
//!   u8   flags                        1 idle fill, 2 read through a layout
//!   u8   bits a symbol                1 BPSK, 2 QPSK, 3 8PSK
//!   u32  n
//!   n × u8                            symbols: the data symbols when read
//!                                     through a layout, else all; each the
//!                                     number of its constellation point
//! ```
//!
//! QPSK points are numbered counter-clockwise from the first quadrant
//! (0 = +1+j); orientation is the layout's (the known symbols fix it, but
//! not to the system's own numbering, which is still to be found).

pub mod burst;
pub mod burst_demod;
pub mod demod;
pub mod layout;
pub mod pool;
pub mod pool_bursts;
pub mod slots;

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use decsat_core::{Iq, Modulation};
use decsat_dsp::{Ddc, Fir, rrc_taps};
// (`Ddc` finds each carrier during the first look; channels then run their
// own lighter front.)

use crate::carriers::detect_carriers;
use crate::estimate::estimate_band;
use crate::identify::{cyclic_symbol_rate, fit_roll_off};
use crate::spectrum::Spectrum;

use burst::{BurstDetector, Levels, RawBurst};
use demod::{BurstSymbols, demod_burst, nearest, points, rotate};
use layout::{Layout, commonest_length, find_layout, fit};
use slots::{BurstTime, SlotGrid, Terminal, Terminals, TerminalsById, find_grid};

/// Signal looked at before the carriers are looked for, seconds; and how
/// long the look goes on, waiting for a burst, before it says none came. A
/// slow TDMA return bursts once every few seconds: the look is a peak hold,
/// and lasts until something shows.
const FIRST_LOOK_S: f64 = 0.4;
const LOOK_MAX_S: f64 = 20.0;
/// How often carriers not yet known are looked for again, s (over the peak
/// hold of the last two such stretches: a carrier bursting even every ~8 s
/// is caught).
const RELOOK_S: f64 = 5.0;
/// Baseband kept to work out a new carrier from its own bursts, s, and at
/// most this many samples (a wide VFO keeps less).
const HISTORY_S: f64 = 12.0;
const HISTORY_MAX: usize = 2_000_000;
/// The peak hold: frames of about this long, s (a slow carrier's bursts
/// last far longer: live, 150 ms), within these FFT sizes; each frame's
/// spectrum smoothed over this many bins, and this many half-overlapping
/// frames averaged before the hold — noise's own peaks then stand ~3 dB over
/// its mean where single frames' stood ~7 dB, as high as the live bursts.
const PEAK_FRAME_S: f64 = 0.01;
const PEAK_FFT_MIN: usize = 256;
const PEAK_FFT_MAX: usize = 4096;
const PEAK_SMOOTH_BINS: usize = 5;
const PEAK_FRAMES: usize = 4;
/// While waiting for the first burst, how often the peak hold is looked at,
/// s; and how long a carrier that could not be worked out is left before it
/// is tried again (more bursts by then).
const LOOK_EVERY_S: f64 = 0.5;
const RETRY_S: f64 = 1.5;
/// The longest wait before a carrier that keeps failing is tried again, s,
/// and how long one is remembered after it was last due.
const RETRY_MAX_S: f64 = 30.0;
/// Tries at `RETRY_S` before the waits double.
const RETRY_FLAT: u32 = 4;
const FAILED_KEPT_S: f64 = 120.0;
/// A carrier is worked out from this much of the latest signal kept, s
/// (a slow carrier bursts in it a couple of times), and carriers are worked
/// out for at most this long a block.
const LOOK_SPAN_S: f64 = 5.0;
const LOOK_BUDGET: std::time::Duration = std::time::Duration::from_millis(30);
/// How far the bursts' symbol-rate line must stand over its band's median,
/// dB ([`burst_rate`]).
const BURST_LINE_DB: f32 = 9.0;
/// The sharpest roll-off a channel's matched filter is given.
const MIN_ALPHA: f64 = 0.2;
/// How coherent a burst's constellation must be for its carrier's rate to
/// be taken ([`best_rate`]), at least (and four times what n symbols read
/// at random give).
const RATE_COHERENCE: f64 = 0.25;
/// How many times the median of the rates tried the best's coherence must
/// be, on most of the bursts tried.
const RATE_STANDS_OUT: f32 = 1.3;
/// The share of a burst's own best coherence the rate taken must reach on
/// it, and the coherence one burst alone must reach.
const RATE_NEAR_BEST: f32 = 0.9;
const RATE_COHERENCE_ALONE: f32 = 0.55;
/// The strongest lines tried as the symbol rate, by demodulating at each.
const RATE_TRIES: usize = 5;
/// The grid of rates tried besides, as a share (a burst's clock is measured
/// from within 1.5 %); and the symbols of a burst tried at each.
const RATE_GRID: f64 = 0.01;
const RATE_TRY_SYMBOLS: usize = 1500;
/// Bursts tried at each rate, at most.
const RATE_TRY_BURSTS: usize = 3;
/// The steps either side of the best rate tried, and their size (a share).
const RATE_FINE_STEPS: i32 = 6;
const RATE_FINE: f64 = 0.0005;
/// Gaps closed inside a burst, blocks of ~16 symbols ([`bursts_only`]).
const BURST_GAP_BLOCKS: usize = 4;
/// Fewest blocks in a row that make a burst.
const BURST_MIN_BLOCKS: usize = 3;
/// Bursts whose own clocks set the channel's symbol rate (their median),
/// and how far out it must be to be changed, as a share.
const CLOCK_VOTES: usize = 8;
const CLOCK_SETTLE: f64 = 0.0002;
/// Clocks kept for the median.
const CLOCK_KEPT: usize = 31;
/// Bursts' clocks scattering less than this (a share, as a standard
/// deviation) are one shared clock: a burst's own measurement scatters
/// ~0.1 % (live, 1000 symbols at 9 dB).
const CLOCK_SHARED: f64 = 0.002;
/// Bursts that vote on the modulation before it is fixed.
const MODULATION_VOTES: u32 = 30;
/// Votes that settle it early when four in five agree.
const MODULATION_EARLY: u32 = 8;
/// Bursts gathered to read a layout from (and tried again with every
/// `LAYOUT_RETRY` more, up to `LAYOUT_KEEP`).
const LAYOUT_FIRST: usize = 120;
const LAYOUT_RETRY: usize = 100;
const LAYOUT_KEEP: usize = 400;
/// Bursts a second under which a carrier is slow, and how many more bursts
/// before a slow carrier's layout is tried again.
const SLOW_PER_S: usize = 5;
const LAYOUT_SLOW_RETRY: usize = 10;
/// Bursts' times kept for the slot grid.
const TIMES_KEPT: usize = 20_000;
/// The slot grid is worked out again every this many bursts.
const GRID_EVERY: u64 = 1500;
/// Burst marks kept for the burst map, seconds.
const MARKS_S: f64 = 0.5;
/// Points kept for the constellation.
const SCATTER: usize = 3000;
/// Terminals listed: those seen in at least this many bursts.
const TERMINAL_MIN: u64 = 20;

/// One burst for the burst map.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BurstMark {
    /// Start, seconds since the VFO began.
    pub t_s: f64,
    pub dur_s: f64,
    pub channel: usize,
    pub power_db: f32,
    pub idle: bool,
}

/// What a TDMA VFO shows of one carrier.
#[derive(Debug, Clone, Default)]
pub struct ChannelView {
    /// Centre relative to the VFO's, Hz.
    pub center_hz: f64,
    pub symbol_rate: f64,
    pub alpha: f64,
    pub modulation: Option<Modulation>,
    pub bursts: u64,
    /// Bursts in the last second.
    pub per_s: f32,
    /// Share of the time a burst is on.
    pub duty: f32,
    /// The commonest burst length, symbols.
    pub len_symbols: Option<usize>,
    /// Smoothed MER, and the bursts' level over the noise floor, dB.
    pub mer_db: f32,
    pub snr_db: f32,
    pub levels: Option<Levels>,
    pub idle_share: f32,
    pub layout: Option<Layout>,
    pub grid: Option<SlotGrid>,
    pub terminals: Vec<Terminal>,
    /// The look for a convolutional code in the bursts' data.
    pub code: CodeView,
    /// The header fields, once the layout is read.
    pub header: Vec<HeaderFieldView>,
}

/// One of a channel's header fields ([`Layout::header_runs`]): what it
/// has read, burst by burst. Values are the symbols as numbered points
/// (QPSK 0–3 counter-clockwise from the first quadrant, in the layout's
/// orientation): what they mean, and how the bits map, is the system's own.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HeaderFieldView {
    /// The symbols, `from..=to` in the burst.
    pub from: usize,
    pub to: usize,
    /// The latest values, newest first.
    pub history: Vec<String>,
    /// Values seen (at most [`HEADER_VALUES_KEPT`] counted), and the share
    /// of bursts the commonest took.
    pub distinct: usize,
    pub commonest_share: f32,
    /// The share of bursts whose value differed from the one before.
    pub change_share: f32,
    pub bursts: u64,
}

/// Values of a header field counted, at most; and latest values shown.
pub const HEADER_VALUES_KEPT: usize = 512;
const HEADER_HISTORY: usize = 6;

/// A header field's values as they come.
struct FieldTrack {
    run: layout::Run,
    history: VecDeque<Vec<u8>>,
    counts: std::collections::HashMap<Vec<u8>, u64>,
    changes: u64,
    bursts: u64,
}

impl FieldTrack {
    fn new(run: layout::Run) -> Self {
        FieldTrack {
            run,
            history: VecDeque::new(),
            counts: std::collections::HashMap::new(),
            changes: 0,
            bursts: 0,
        }
    }

    /// A fitted burst's symbols ([`layout::Fitted::labels`]).
    fn push(&mut self, labels: &[u8]) {
        let Some(v) = labels.get(self.run.clone()) else {
            return;
        };
        if v.contains(&layout::NO_SYMBOL) {
            return;
        }
        if self
            .history
            .front()
            .is_some_and(|last| last.as_slice() != v)
        {
            self.changes += 1;
        }
        self.bursts += 1;
        if self.counts.len() < HEADER_VALUES_KEPT || self.counts.contains_key(v) {
            *self.counts.entry(v.to_vec()).or_insert(0) += 1;
        }
        self.history.push_front(v.to_vec());
        self.history.truncate(HEADER_HISTORY);
    }

    fn view(&self) -> HeaderFieldView {
        let text = |v: &Vec<u8>| v.iter().map(|&d| char::from(b'0' + d.min(9))).collect();
        HeaderFieldView {
            from: self.run.start,
            to: self.run.end - 1,
            history: self.history.iter().map(text).collect(),
            distinct: self.counts.len(),
            commonest_share: self.counts.values().copied().max().unwrap_or(0) as f32
                / self.bursts.max(1) as f32,
            change_share: self.changes as f32 / self.bursts.saturating_sub(1).max(1) as f32,
            bursts: self.bursts,
        }
    }
}

/// What the look for a convolutional code in a channel's bursts has found.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CodeView {
    /// The code, and the share of windows its parity check holds in.
    pub code: Option<decsat_modem::convblind::Code>,
    pub fit: f32,
    /// Looks made, coded bits gathered, and whether it has stopped looking.
    pub looks: u32,
    pub bits: usize,
    pub given_up: bool,
}

/// Coded bits gathered before each look, and looks before giving up.
const CODE_LOOK_BITS: usize = 12_000;
const CODE_LOOKS: u32 = 6;

/// The look for a convolutional code in a channel's bursts
/// ([`decsat_modem::convblind`]): the data of bursts fitted to the layout —
/// their phase fixed by its known symbols, so every burst reads the same way
/// up — one stretch a burst, as each is coded on its own.
#[derive(Default)]
struct CodeSearch {
    pieces: Vec<Vec<u8>>,
    view: CodeView,
}

impl CodeSearch {
    fn push(&mut self, data: &[Iq], modulation: Modulation) {
        if self.view.code.is_some() || self.view.given_up || data.is_empty() {
            return;
        }
        let bpsk = match modulation {
            Modulation::Bpsk | Modulation::Pi2Bpsk => true,
            Modulation::Qpsk => false,
            // (The code finder reads BPSK and QPSK.)
            _ => return,
        };
        self.pieces
            .push(decsat_modem::convblind::hard_bits(data, bpsk));
        self.view.bits = self.pieces.iter().map(|p| p.len()).sum();
        if self.view.bits < CODE_LOOK_BITS {
            return;
        }
        self.view.looks += 1;
        let refs: Vec<&[u8]> = self.pieces.iter().map(|p| p.as_slice()).collect();
        match decsat_modem::convblind::find_pieces(&refs) {
            Some(f) => {
                self.view.code = Some(f.code);
                self.view.fit = f.fit;
                self.pieces.clear();
            }
            None if self.view.looks >= CODE_LOOKS => {
                self.view.given_up = true;
                self.pieces.clear();
            }
            None => {
                // The newest half stays for the next look.
                let drop = self.pieces.len() / 2;
                self.pieces.drain(..drop);
            }
        }
        self.view.bits = self.pieces.iter().map(|p| p.len()).sum();
    }
}

/// What a TDMA VFO shows.
#[derive(Debug, Clone, Default)]
pub struct TdmaView {
    pub channels: Vec<ChannelView>,
    /// Bursts in the last half second, for the burst map.
    pub marks: Vec<BurstMark>,
    /// The time of the newest sample, seconds since the VFO began.
    pub now_s: f64,
    /// Gathering the first look (0..1), before the carriers are known.
    pub progress: f32,
    pub message: String,
}

/// A channel's front: a mixer, and a decimating FIR when the VFO's rate is
/// well over what the channel needs. The matched filter after it takes out
/// the neighbouring carriers, so the FIR need only keep aliases out of the
/// channel's own band — a gentle filter of ~30 taps where a VFO's
/// down-converter, built to stand alone, spent ~120 (live: three channels
/// in a 5 MHz VFO took 2.9 s of every 4 s of signal; now 0.3 s).
enum Front {
    Decim(Decimator),
    Mix(Mixer),
}

/// Mix down, low-pass, keep every `d`-th sample.
struct Decimator {
    mixer: Mixer,
    taps_rev: Vec<f32>,
    /// The last `taps − 1` inputs and the new block; the next output ends
    /// at `buf[next]`.
    buf: Vec<Iq>,
    next: usize,
    d: usize,
    out_rate: f64,
    mixed: Vec<Iq>,
}

impl Decimator {
    /// For a channel `pass` Hz either side of its centre, at `rate / d`.
    fn new(rate: f64, offset_hz: f64, pass: f64, d: usize) -> Self {
        let out_rate = rate / d as f64;
        let stop = out_rate - pass;
        // Blackman: transition ≈ 5.5 / N of the rate (see `decsat_dsp::ddc`).
        let n = ((5.5 * rate / (stop - pass).max(rate * 0.01)).ceil() as usize) | 1;
        let mut taps = decsat_dsp::lowpass((pass + stop) / 2.0 / rate, n.min(255));
        taps.reverse();
        let l = taps.len();
        Decimator {
            mixer: Mixer::new(rate, offset_hz),
            taps_rev: taps,
            buf: vec![Iq::new(0.0, 0.0); l - 1],
            next: l - 1,
            d,
            out_rate,
            mixed: Vec::new(),
        }
    }

    fn process(&mut self, x: &[Iq], out: &mut Vec<Iq>) {
        self.mixed.clear();
        self.mixer.process(x, &mut self.mixed);
        self.buf.extend_from_slice(&self.mixed);
        let l = self.taps_rev.len();
        while self.next < self.buf.len() {
            out.push(decsat_dsp::dot::dot_cr(
                &self.buf[self.next + 1 - l..=self.next],
                &self.taps_rev,
            ));
            self.next += self.d;
        }
        let drop = self.next + 1 - l;
        let drop = drop.min(self.buf.len());
        self.buf.drain(..drop);
        self.next -= drop;
    }
}

/// A numerically controlled oscillator mixing down by `freq` (cycles a
/// sample).
struct Mixer {
    rate: f64,
    phasor: Iq,
    step: Iq,
    n: u64,
    freq: f64,
}

impl Mixer {
    fn new(rate: f64, offset_hz: f64) -> Self {
        let freq = -offset_hz / rate;
        let w = std::f64::consts::TAU * freq;
        Mixer {
            rate,
            phasor: Iq::new(1.0, 0.0),
            step: Iq::new(w.cos() as f32, w.sin() as f32),
            n: 0,
            freq,
        }
    }

    fn process(&mut self, x: &[Iq], out: &mut Vec<Iq>) {
        out.reserve(x.len());
        for &s in x {
            out.push(s * self.phasor);
            self.phasor *= self.step;
            self.n += 1;
            if self.n.is_multiple_of(4096) {
                // Back on the unit circle, from the exact phase (the
                // fraction of a cycle, so it stays exact for days).
                let a = std::f64::consts::TAU * (self.freq * self.n as f64).fract();
                self.phasor = Iq::new(a.cos() as f32, a.sin() as f32);
            }
        }
    }
}

impl Front {
    fn rate(&self) -> f64 {
        match self {
            Front::Decim(d) => d.out_rate,
            Front::Mix(m) => m.rate,
        }
    }

    fn process(&mut self, x: &[Iq], out: &mut Vec<Iq>) {
        match self {
            Front::Decim(d) => d.process(x, out),
            Front::Mix(m) => m.process(x, out),
        }
    }
}

struct Channel {
    center_hz: f64,
    rs: f64,
    alpha: f64,
    front: Front,
    mf: Fir,
    sps: f64,
    det: BurstDetector,
    modulation: Option<Modulation>,
    votes: [u32; 3],
    gathered: Vec<Vec<Iq>>,
    tried: bool,
    since_try: usize,
    layout: Option<Layout>,
    grid: Option<SlotGrid>,
    times: VecDeque<BurstTime>,
    terminals: Terminals,
    /// Terminals by the field in the preamble, when the layout has one.
    by_id: TerminalsById,
    /// Carrier offsets within this of each other may be one terminal, Hz.
    freq_tol: f64,
    /// Where every burst's carrier is, cycles a symbol, when they agree.
    freq_prior: Option<f64>,
    bursts: u64,
    lens: Vec<u32>,
    /// The latest bursts' own clocks (samples a symbol), for the channel's
    /// symbol rate; and whether they agree as one shared clock would.
    clocks: VecDeque<f64>,
    clock_shared: bool,
    /// Burst starts (channel samples) and lengths in the last second.
    recent: VecDeque<(u64, u64)>,
    mer_db: f32,
    snr_db: f32,
    idle_share: f32,
    code: CodeSearch,
    header: Vec<FieldTrack>,
    /// The VFO's time at the channel's first sample, s (a carrier found
    /// later starts from the kept history).
    t0_s: f64,
    // Scratch.
    mixed: Vec<Iq>,
    filtered: Vec<Iq>,
    raw: Vec<RawBurst>,
}

/// A burst ready for the view, the map and the file.
struct Done {
    channel: usize,
    t_s: f64,
    dur_s: f64,
    power_db: f32,
    freq_hz: f64,
    mer_db: f32,
    idle: bool,
    fitted: bool,
    modulation: Modulation,
    symbols: Vec<Iq>,
    /// The header fields' values, '.' between ("" unless fitted).
    header: String,
}

impl Channel {
    fn new(vfo_rate: f64, center_hz: f64, rs: f64, alpha: f64) -> Self {
        // Down to at least 3.2 samples a symbol (feedforward timing wants
        // three or more).
        let d = ((vfo_rate / (3.2 * rs)).floor() as usize).max(1);
        let front = if d >= 2 {
            let pass = (1.0 + alpha) * rs / 2.0 * 1.05;
            Front::Decim(Decimator::new(vfo_rate, center_hz, pass, d))
        } else {
            Front::Mix(Mixer::new(vfo_rate, center_hz))
        };
        let rate = front.rate();
        let sps = rate / rs;
        Channel {
            center_hz,
            rs,
            alpha,
            // ±6 symbols of the root raised cosine: its truncation leaves
            // sidelobes ~30 dB down, under the noise of any burst seen.
            mf: Fir::new(rrc_taps(sps, alpha.clamp(0.05, 1.0), 6)),
            det: BurstDetector::new(rate, sps),
            front,
            sps,
            modulation: None,
            votes: [0; 3],
            gathered: Vec::new(),
            tried: false,
            since_try: 0,
            layout: None,
            grid: None,
            times: VecDeque::new(),
            terminals: Terminals::default(),
            by_id: TerminalsById::default(),
            freq_tol: 60.0,
            freq_prior: None,
            bursts: 0,
            lens: Vec::new(),
            clocks: VecDeque::new(),
            clock_shared: false,
            recent: VecDeque::new(),
            mer_db: 0.0,
            snr_db: 0.0,
            idle_share: 0.0,
            mixed: Vec::new(),
            filtered: Vec::new(),
            raw: Vec::new(),
            code: CodeSearch::default(),
            header: Vec::new(),
            t0_s: 0.0,
        }
    }

    fn rate(&self) -> f64 {
        self.front.rate()
    }

    /// How much of a new burst's MER or SNR goes into the smoothed one: the
    /// mean of all so far for the first fifty (a slow carrier's take
    /// minutes), then a fiftieth.
    fn smoothing(&self) -> f32 {
        1.0 / (self.bursts as f32).clamp(1.0, 50.0)
    }

    /// The band it takes, Hz either side of its centre.
    fn half_width(&self) -> f64 {
        self.rs * (1.0 + self.alpha) / 2.0
    }

    /// Take a block of the VFO's baseband; bursts finished in it to `done`.
    fn push(&mut self, index: usize, x: &[Iq], fixed: Option<Modulation>, done: &mut Vec<Done>) {
        self.filter(x);
        self.detect(index, fixed, done);
    }

    /// As [`Self::push`], for the signal kept from before the channel was
    /// found: the burst detector's levels set from all of it first (a slow
    /// carrier's first bursts are in it, and would go by before levels
    /// learnt from them could find them).
    fn push_heard(
        &mut self,
        index: usize,
        x: &[Iq],
        fixed: Option<Modulation>,
        done: &mut Vec<Done>,
    ) {
        self.filter(x);
        self.det.prime(&self.filtered);
        self.detect(index, fixed, done);
    }

    /// The channel's front and matched filter, into `filtered`.
    fn filter(&mut self, x: &[Iq]) {
        self.mixed.clear();
        self.front.process(x, &mut self.mixed);
        self.filtered.clear();
        self.mf.process(&self.mixed, &mut self.filtered);
    }

    /// Bursts in `filtered`, demodulated, to `done`.
    fn detect(&mut self, index: usize, fixed: Option<Modulation>, done: &mut Vec<Done>) {
        self.raw.clear();
        self.det.push(&self.filtered, &mut self.raw);
        let raw = std::mem::take(&mut self.raw);
        for r in &raw {
            for on in self.pieces(r) {
                if let Some(d) = self.burst(index, r, on, fixed) {
                    done.push(d);
                }
            }
        }
        self.raw = raw;
        // Bursts in the last second.
        let now = self.det.position();
        let second = self.rate() as u64;
        while self.recent.front().is_some_and(|r| r.0 + second < now) {
            self.recent.pop_front();
        }
    }

    /// The bursts in a detected one: itself, or — once the layout and the
    /// slots are known — bursts that ran together over a short guard time
    /// split again a slot apart.
    fn pieces(&self, r: &RawBurst) -> Vec<std::ops::Range<usize>> {
        let (Some(l), Some(g)) = (&self.layout, &self.grid) else {
            return vec![r.on.clone()];
        };
        let len = r.on.len() as f64 / self.sps;
        if len < 1.6 * l.len as f64 || r.cut {
            return vec![r.on.clone()];
        }
        let slot = g.period * self.sps;
        let one = l.len as f64 * self.sps;
        let n = ((len + g.guard) / g.period).round().max(1.0) as usize;
        (0..n)
            .map(|k| {
                let a = r.on.start + (k as f64 * slot) as usize;
                a..(a + one as usize).min(r.on.end)
            })
            .filter(|p| p.len() as f64 > 0.5 * one)
            .collect()
    }

    fn burst(
        &mut self,
        index: usize,
        r: &RawBurst,
        on: std::ops::Range<usize>,
        fixed: Option<Modulation>,
    ) -> Option<Done> {
        let modulation = fixed.or(self.modulation);
        let b: BurstSymbols = demod_burst(
            &r.samples,
            on.clone(),
            self.sps,
            r.start,
            modulation,
            self.freq_prior,
            !self.clock_shared,
        )?;
        if modulation.is_none() {
            let k = match b.modulation {
                Modulation::Bpsk => 0,
                Modulation::Psk8 => 2,
                _ => 1,
            };
            self.votes[k] += 1;
            // Settled by enough votes, or early by a clear majority (a slow
            // carrier's thirty bursts take a minute).
            let total = self.votes.iter().sum::<u32>();
            let top = self.votes.iter().copied().max().unwrap_or(0);
            if total >= MODULATION_VOTES || (total >= MODULATION_EARLY && 5 * top >= 4 * total) {
                let best = (0..3).max_by_key(|&i| self.votes[i]).unwrap_or(1);
                self.modulation =
                    Some([Modulation::Bpsk, Modulation::Qpsk, Modulation::Psk8][best]);
            }
        }
        self.bursts += 1;
        // The symbol rate from the bursts' own clocks — a carrier found from
        // one short burst has it only roughly (live, 0.9 % out) — their
        // median over the latest. Where they scatter no more than measuring
        // one burst's clock does, the terminals keep one clock (the hub's),
        // and every burst is read at the median: more exact than its own
        // (live, ±5 Hz in 6400 a burst, which cost a 1000-symbol burst up to
        // 4 dB of MER).
        self.clocks.push_back(b.sps);
        if self.clocks.len() > CLOCK_KEPT {
            self.clocks.pop_front();
        }
        if self.clocks.len() >= CLOCK_VOTES {
            let mut c: Vec<f64> = self.clocks.iter().copied().collect();
            c.sort_by(f64::total_cmp);
            let med = c[c.len() / 2];
            let mut dev: Vec<f64> = c.iter().map(|x| (x - med).abs()).collect();
            dev.sort_by(f64::total_cmp);
            let spread = 1.4826 * dev[dev.len() / 2] / med;
            if (med / self.sps - 1.0).abs() > CLOCK_SETTLE {
                self.sps = med;
                self.rs = self.rate() / self.sps;
            }
            self.clock_shared = spread < CLOCK_SHARED;
        }
        let len = b.symbols.len();
        self.lens.push(len as u32);
        if self.lens.len() > 4000 {
            self.lens.drain(..2000);
        }
        self.recent
            .push_back((r.start + on.start as u64, on.len() as u64));
        let floor = r.floor.max(1e-20);
        let snr = 10.0 * ((10f32.powf(b.power_db / 10.0) - floor).max(1e-20) / floor).log10();
        self.snr_db += (snr - self.snr_db) * self.smoothing();

        // Gather for the layout until one is read: the newest
        // `LAYOUT_KEEP`, tried with the first `LAYOUT_FIRST` and again with
        // every `LAYOUT_RETRY` more.
        if self.layout.is_none() && self.modulation.is_some() {
            if self.gathered.len() >= LAYOUT_KEEP {
                self.gathered.remove(0);
            }
            self.gathered.push(b.symbols.clone());
            self.since_try += 1;
            // A slow carrier (a few bursts a second at most, over the time
            // it has been heard) is tried once there are as many bursts of
            // its commonest length as a layout is read from (live, three in
            // four; the rest split or cut short), and every few more: its
            // 120 take minutes.
            let heard_s = self.det.position() as f64 / self.rate();
            let slow = (self.bursts as f64) < SLOW_PER_S as f64 * heard_s;
            let ready = if slow && self.gathered.len() <= LAYOUT_FIRST {
                let lens: Vec<usize> = self.gathered.iter().map(|g| g.len()).collect();
                let same = commonest_length(&lens).map_or(0, |c| c.1);
                same >= layout::MIN_BURSTS && (!self.tried || self.since_try >= LAYOUT_SLOW_RETRY)
            } else {
                self.gathered.len() >= LAYOUT_FIRST
                    && (!self.tried || self.since_try >= LAYOUT_RETRY)
            };
            if ready {
                self.tried = true;
                self.since_try = 0;
                self.read_layout();
            }
        }
        let modulation_now = self.modulation.unwrap_or(b.modulation);
        let mut field = Vec::new();
        let mut header = String::new();
        let (t_sym, symbols, mer, idle, fitted) = match self.layout.as_ref().and_then(|l| {
            // Only bursts of the layout's length.
            ((len as f64 - l.len as f64).abs() <= l.len as f64 * 0.02 + 1.0)
                .then(|| fit(l, &b.symbols))
                .flatten()
        }) {
            Some(f) => {
                field = f.field;
                for h in &mut self.header {
                    h.push(&f.labels);
                }
                // For the file: each field's symbols, '.' between fields.
                header = self
                    .header
                    .iter()
                    .map(|h| {
                        f.labels[h.run.clone()]
                            .iter()
                            .map(|&d| char::from(b'0' + d.min(9)))
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join(".");
                (
                    b.t0 / self.sps + f.lag as f64,
                    f.data,
                    f.mer_db,
                    f.idle,
                    true,
                )
            }
            None => (b.t0 / self.sps, b.symbols, b.mer_db, false, false),
        };
        if fitted && !idle {
            self.code.push(&symbols, modulation_now);
        }
        self.mer_db += (mer - self.mer_db) * self.smoothing();
        self.idle_share += ((idle as u8 as f32) - self.idle_share) * 0.01;
        let freq_hz = self.center_hz + b.freq * self.rs;
        let time = BurstTime {
            t: t_sym,
            len: len as f64,
            power_db: b.power_db,
            freq_hz,
            idle,
        };
        self.times.push_back(time);
        if self.times.len() > TIMES_KEPT {
            self.times.pop_front();
        }
        // Often until the grid and its frame are found, then now and then.
        let settled = self.grid.is_some_and(|g| g.frame.is_some());
        if self.bursts.is_multiple_of(GRID_EVERY) || (!settled && self.bursts.is_multiple_of(200)) {
            let v: Vec<BurstTime> = self.times.iter().copied().collect();
            let g = find_grid(&v);
            // How far apart two bursts' carriers may be and still be one
            // terminal: the measurement's own scatter where every terminal
            // is locked to the hub (live, ±40 Hz at 10 dB, ±300 at 6 dB),
            // robustly (median absolute deviation, as a standard deviation).
            let mut f: Vec<f64> = v.iter().map(|b| b.freq_hz).collect();
            f.sort_by(f64::total_cmp);
            let med = f[f.len() / 2];
            let mut dev: Vec<f64> = f.iter().map(|x| (x - med).abs()).collect();
            dev.sort_by(f64::total_cmp);
            let sigma = 1.4826 * dev[dev.len() / 2];
            self.freq_tol = (2.5 * sigma).max(60.0);
            // Every burst's carrier close to the same (terminals locked to
            // the hub): demodulate from there, without each burst's own
            // frequency search — well inside the block phases' reach.
            self.freq_prior =
                (3.0 * sigma / self.rs < 0.0008).then(|| (med - self.center_hz) / self.rs);
            if g.is_some_and(|g| {
                self.grid
                    .is_none_or(|old| (old.period - g.period).abs() > 0.5)
            }) {
                self.terminals.clear();
            }
            self.grid = g;
        }
        // A short field in the preamble names the terminal (live: ~17
        // values, each at its own power); else they are told apart by
        // power, carrier and slot offset.
        if (1..=12).contains(&field.len()) {
            self.by_id.add(&field, &time, self.grid.as_ref());
        } else if let Some(g) = &self.grid {
            self.terminals.add(&time, g, self.freq_tol);
        }
        Some(Done {
            channel: index,
            t_s: self.t0_s + b.t0 / self.rate(),
            dur_s: on.len() as f64 / self.rate(),
            power_db: b.power_db,
            freq_hz,
            mer_db: mer,
            idle,
            fitted,
            modulation: modulation_now,
            symbols,
            header,
        })
    }

    fn read_layout(&mut self) {
        let lens: Vec<usize> = self.gathered.iter().map(|g| g.len()).collect();
        let Some((len, _)) = commonest_length(&lens) else {
            return;
        };
        let refs: Vec<&[Iq]> = self.gathered.iter().map(|g| g.as_slice()).collect();
        if let Some(l) = find_layout(&refs, len, self.modulation.unwrap_or(Modulation::Qpsk)) {
            self.header = l.header_runs().into_iter().map(FieldTrack::new).collect();
            self.layout = Some(l);
            self.gathered = Vec::new();
        }
    }

    fn view(&self) -> ChannelView {
        let duty = {
            let on: u64 = self.recent.iter().map(|r| r.1).sum();
            (on as f64 / self.rate()).min(1.0) as f32
        };
        let len_symbols = {
            let l: Vec<usize> = self.lens.iter().map(|&x| x as usize).collect();
            commonest_length(&l).map(|x| x.0)
        };
        ChannelView {
            center_hz: self.center_hz,
            symbol_rate: self.rs,
            alpha: self.alpha,
            modulation: self.modulation,
            bursts: self.bursts,
            per_s: self.recent.len() as f32,
            duty,
            len_symbols,
            mer_db: self.mer_db,
            snr_db: self.snr_db,
            levels: self.det.levels(),
            idle_share: self.idle_share,
            layout: self.layout.clone(),
            grid: self.grid,
            terminals: if self.by_id.is_empty() {
                self.terminals.list(TERMINAL_MIN)
            } else {
                self.by_id.list(TERMINAL_MIN)
            },
            code: self.code.view,
            header: self.header.iter().map(|h| h.view()).collect(),
        }
    }
}

/// The open recording.
struct Writer {
    bin: BufWriter<File>,
    csv: BufWriter<File>,
    path: PathBuf,
    bursts: u64,
}

/// The spectrum's peak hold over short frames: a burst shows at its own
/// level however rarely it comes, where an average spreads it over the
/// silence between. Two stretches, the newest and the one before.
struct PeakHold {
    n: usize,
    spec: Spectrum,
    carry: Vec<Iq>,
    /// The last [`PEAK_FRAMES`] frames, smoothed across bins.
    frames: VecDeque<Vec<f32>>,
    cur: Vec<f32>,
    prev: Vec<f32>,
}

impl PeakHold {
    /// For a baseband at `rate`.
    fn new(rate: f64) -> Self {
        let want = (rate * PEAK_FRAME_S).max(1.0) as usize;
        // The largest power of two not over `want`, within the limits.
        let n = (1usize << want.ilog2()).clamp(PEAK_FFT_MIN, PEAK_FFT_MAX);
        PeakHold {
            n,
            spec: Spectrum::new(n),
            carry: Vec::new(),
            frames: VecDeque::new(),
            cur: vec![0.0; n],
            prev: vec![0.0; n],
        }
    }

    fn push(&mut self, x: &[Iq]) {
        self.carry.extend_from_slice(x);
        if self.carry.len() < 4 * self.n {
            return;
        }
        let n = self.n;
        // Rust note: the fields the callback changes are borrowed one by one
        // (a closure borrowing `self` whole would clash with `self.spec`).
        let (frames, cur) = (&mut self.frames, &mut self.cur);
        let (_, hop) = self.spec.compute_max_with(&self.carry, usize::MAX, |p| {
            // Across bins: a running sum over `PEAK_SMOOTH_BINS` (fewer at
            // the ends).
            let h = PEAK_SMOOTH_BINS / 2;
            let mut f = if frames.len() >= PEAK_FRAMES {
                frames.pop_front().unwrap_or_default()
            } else {
                vec![0.0; n]
            };
            let mut acc: f32 = p[..h].iter().sum();
            for (k, v) in f.iter_mut().enumerate() {
                if k + h < n {
                    acc += p[k + h];
                }
                if k > h {
                    acc -= p[k - h - 1];
                }
                let width = (k + h).min(n - 1) + 1 - k.saturating_sub(h);
                *v = acc / width as f32;
            }
            frames.push_back(f);
            // Over time: the mean of the last frames, held at its highest.
            if frames.len() >= PEAK_FRAMES {
                for (k, c) in cur.iter_mut().enumerate() {
                    let m = frames.iter().map(|f| f[k]).sum::<f32>() / PEAK_FRAMES as f32;
                    *c = c.max(m);
                }
            }
        });
        // Keep the tail the segments did not reach.
        let used = (self.carry.len() - n) / hop * hop + hop;
        self.carry.drain(..used.min(self.carry.len()));
    }

    /// The peak hold over both stretches, dB.
    fn db(&self) -> Vec<f32> {
        self.cur
            .iter()
            .zip(&self.prev)
            .map(|(&a, &b)| 10.0 * a.max(b).max(1e-30).log10())
            .collect()
    }

    /// A new stretch begins.
    fn turn(&mut self) {
        std::mem::swap(&mut self.cur, &mut self.prev);
        self.cur.iter_mut().for_each(|v| *v = 0.0);
    }
}

/// A carrier that could not be worked out.
struct Failed {
    centre: f64,
    half_width: f64,
    /// When it may be tried again, s; and how many times it has failed.
    next: f64,
    tries: u32,
}

/// A TDMA VFO's decoder.
pub struct TdmaAnalyser {
    rate: f64,
    bandwidth: f64,
    /// The look's peak hold, the newest baseband (for a carrier found
    /// late, worked out from its own bursts) and the VFO time of its first
    /// sample, and the time since the last look.
    peak: PeakHold,
    history: VecDeque<Iq>,
    history_t0: f64,
    since_look: f64,
    /// While waiting: when the peak hold was last looked at, s.
    last_look: f64,
    /// Carriers that could not be worked out, left a while before they are
    /// tried again — longer each time.
    failed: Vec<Failed>,
    /// Carriers seen in the peak hold, waiting to be worked out (centre,
    /// width, the spectrum's rate estimate), a few a block: a whole MF-TDMA
    /// group at once held the VFO's thread up for seconds.
    pending: VecDeque<(f64, f64, f64)>,
    started: bool,
    channels: Vec<Channel>,
    samples: u64,
    marks: VecDeque<BurstMark>,
    scatter: VecDeque<Iq>,
    writer: Option<Writer>,
    last_file: Option<(PathBuf, u64)>,
    open_failed: bool,
    message: String,
    done: Vec<Done>,
}

impl TdmaAnalyser {
    /// For a VFO's baseband at `rate`, `bandwidth` wide.
    pub fn new(rate: f64, bandwidth: f64) -> Self {
        TdmaAnalyser {
            rate,
            bandwidth,
            peak: PeakHold::new(rate),
            history: VecDeque::new(),
            history_t0: 0.0,
            since_look: 0.0,
            last_look: f64::NEG_INFINITY,
            failed: Vec::new(),
            pending: VecDeque::new(),
            started: false,
            channels: Vec::new(),
            samples: 0,
            marks: VecDeque::new(),
            scatter: VecDeque::new(),
            writer: None,
            last_file: None,
            open_failed: false,
            message: "listening…".into(),
            done: Vec::new(),
        }
    }

    /// Take a block of the VFO's baseband. `modulation`: the constellation
    /// to demodulate as, `None` to find it.
    pub fn push(&mut self, x: &[Iq], modulation: Option<Modulation>) {
        self.samples += x.len() as u64;
        self.peak.push(x);
        self.history.extend(x.iter().copied());
        let keep = ((HISTORY_S * self.rate) as usize).min(HISTORY_MAX);
        if self.history.len() > keep {
            let over = self.history.len() - keep;
            self.history.drain(..over);
            self.history_t0 += over as f64 / self.rate;
        }
        let listened = self.samples as f64 / self.rate;
        // The carriers known already take the block.
        let mut done = std::mem::take(&mut self.done);
        done.clear();
        for (i, c) in self.channels.iter_mut().enumerate() {
            c.push(i, x, modulation, &mut done);
        }
        // Look at the peak hold: while waiting for the first burst now and
        // then, after it every `RELOOK_S` for carriers that had not burst
        // yet; and work out a few of those seen.
        self.since_look += x.len() as f64 / self.rate;
        let due = if self.started {
            self.since_look >= RELOOK_S
        } else {
            listened >= FIRST_LOOK_S && listened - self.last_look >= LOOK_EVERY_S
        };
        if due {
            self.last_look = listened;
            self.look();
            if self.started {
                self.since_look = 0.0;
                self.peak.turn();
            }
        }
        self.work(modulation, &mut done);
        self.deliver(done);
        if !self.started {
            let waited = listened >= LOOK_MAX_S;
            if self.channels.is_empty() && !waited {
                self.message = format!(
                    "listening for bursts… {listened:.0} s (a slow TDMA carrier bursts every \
                     few seconds)"
                );
                return;
            }
            self.started = true;
            self.since_look = 0.0;
            self.peak.turn();
            self.message = format!(
                "no burst in {LOOK_MAX_S:.0} s — still listening for carriers that start later"
            );
        }
    }

    /// Carriers in the peak hold that no channel covers yet, queued to be
    /// worked out.
    fn look(&mut self) {
        let mut db = self.peak.db();
        let n = db.len();
        let bin = self.rate / n as f64;
        // Only the VFO's band: past its edges the VFO's own filter, whose
        // stopband would pass for the noise floor.
        let inside =
            |k: usize| ((k as f64 / n as f64) - 0.5).abs() * self.rate <= self.bandwidth / 2.0;
        let mut within: Vec<f32> = (0..n).filter(|&k| inside(k)).map(|k| db[k]).collect();
        if within.len() >= 16 {
            within.sort_unstable_by(f32::total_cmp);
            let floor = within[within.len() * 3 / 100];
            for (k, d) in db.iter_mut().enumerate() {
                if !inside(k) {
                    *d = floor;
                }
            }
        }
        let now = self.samples as f64 / self.rate;
        self.failed.retain(|f| now < f.next + FAILED_KEPT_S);
        let known = |c: f64| {
            self.channels
                .iter()
                .any(|k| (k.center_hz - c).abs() < k.half_width())
                || self
                    .failed
                    .iter()
                    .any(|f| now < f.next && (f.centre - c).abs() < f.half_width)
                || self.pending.iter().any(|p| (p.0 - c).abs() < p.1 / 2.0)
        };
        let seen: Vec<_> = detect_carriers(&db, self.rate, 4.0)
            .into_iter()
            .filter(|c| !c.narrow && c.bandwidth_hz >= 16.0 * bin)
            .filter(|c| c.center_hz.abs() <= self.bandwidth / 2.0)
            .collect();
        // What the VFO's edge cuts is a neighbour (live, 2 kHz of the next
        // 6.4 kS/s carrier either side; a continuous carrier's skirt): not
        // one to work out — unless nothing lies wholly inside (a VFO drawn
        // tight round one carrier cuts that carrier's skirts too).
        let touches = |c: &crate::carriers::Carrier| {
            c.center_hz.abs() + c.bandwidth_hz / 2.0 >= self.bandwidth / 2.0 - 2.0 * bin
        };
        let inner = seen.iter().any(|c| !touches(c));
        let cut = |c: &crate::carriers::Carrier| inner && touches(c);
        let mut cands: Vec<(f64, f64, f64)> = seen
            .iter()
            .filter(|c| !cut(c))
            .filter(|c| !known(c.center_hz))
            .map(|c| (c.center_hz, c.bandwidth_hz, c.symbol_rate_hz))
            .collect();
        // A VFO around one carrier, with no floor either side to see it by,
        // is taken as that carrier — once the wait for bursts is over.
        if cands.is_empty()
            && self.channels.is_empty()
            && self.pending.is_empty()
            && now >= LOOK_MAX_S
            && !known(0.0)
        {
            cands.push((0.0, self.bandwidth, 0.0));
        }
        // In frequency order, so a group's carriers are numbered across it.
        cands.sort_by(|a, b| a.0.total_cmp(&b.0));
        self.pending.extend(cands);
    }

    /// Work out carriers waiting in the queue, for at most `LOOK_BUDGET`:
    /// each from its bursts in the latest `LOOK_SPAN_S` of what was kept,
    /// then fed all of it (its bursts so far, on the VFO's clock).
    fn work(&mut self, modulation: Option<Modulation>, done: &mut Vec<Done>) {
        if self.pending.is_empty() {
            return;
        }
        let t0 = std::time::Instant::now();
        let now = self.samples as f64 / self.rate;
        // Rust note: `make_contiguous` puts the ring's two halves in one
        // slice in place, so it can be borrowed whole without a copy.
        self.history.make_contiguous();
        let mut found = Vec::new();
        {
            let heard = self.history.as_slices().0;
            let span = heard
                .len()
                .saturating_sub((LOOK_SPAN_S * self.rate) as usize);
            while t0.elapsed() < LOOK_BUDGET {
                let Some((centre, bw, enbw)) = self.pending.pop_front() else {
                    break;
                };
                match self.channel_at(&heard[span..], centre, bw, enbw) {
                    Some(mut c) => {
                        c.t0_s = self.history_t0;
                        c.push_heard(self.channels.len() + found.len(), heard, modulation, done);
                        found.push(c);
                    }
                    None => {
                        let near = self
                            .failed
                            .iter_mut()
                            .find(|f| (f.centre - centre).abs() < f.half_width);
                        match near {
                            Some(f) => {
                                f.tries += 1;
                                // The first few as often as bursts may
                                // come, then less and less.
                                let back = f.tries.saturating_sub(RETRY_FLAT) as i32;
                                f.next = now + (RETRY_S * 2f64.powi(back)).min(RETRY_MAX_S);
                            }
                            None => self.failed.push(Failed {
                                centre,
                                half_width: bw / 2.0,
                                next: now + RETRY_S,
                                tries: 0,
                            }),
                        }
                    }
                }
            }
        }
        if !found.is_empty() {
            // The others keep their numbers (the file and the map use them).
            self.channels.append(&mut found);
            let n = self.channels.len();
            if !self.started {
                self.message = format!(
                    "{n} carrier{} — finding bursts",
                    if n == 1 { "" } else { "s" }
                );
            }
        }
    }

    /// Bursts finished: to the burst map, the constellation and the file.
    fn deliver(&mut self, mut done: Vec<Done>) {
        done.sort_by(|a, b| a.t_s.total_cmp(&b.t_s));
        for d in &done {
            self.marks.push_back(BurstMark {
                t_s: d.t_s,
                dur_s: d.dur_s,
                channel: d.channel,
                power_db: d.power_db,
                idle: d.idle,
            });
            if !d.idle {
                let step = (d.symbols.len() / 64).max(1);
                self.scatter.extend(d.symbols.iter().step_by(step).copied());
            }
            if let Some(w) = &mut self.writer
                && write_burst(w, d).is_err()
            {
                self.open_failed = true;
                self.writer = None;
            }
        }
        while self.scatter.len() > SCATTER {
            self.scatter.pop_front();
        }
        let now = self.samples as f64 / self.rate;
        while self.marks.front().is_some_and(|m| m.t_s + MARKS_S < now) {
            self.marks.pop_front();
        }
        self.done = done;
    }

    /// A carrier at `centre`, `bw` wide (`enbw`: the spectrum's rate
    /// estimate, 0 if none), worked out from `x`: its centre from its own
    /// spectrum, the rate refined on its envelope's cyclic line from the
    /// spectrum's estimate, the roll-off fitted to the skirts — the parts of
    /// Identify that matter here (all of it took 0.6 s a carrier at 1.4
    /// MS/s, on the VFO's thread). Only its bursts are looked at: a slow
    /// TDMA carrier is silent most of the time, and the silence would bury
    /// its spectrum and its cyclic line.
    fn channel_at(&self, x: &[Iq], centre: f64, bw: f64, enbw: f64) -> Option<Channel> {
        let bw = (bw * 1.15).min(self.rate);
        let mut ddc = Ddc::new(self.rate, centre, bw);
        let mut all = Vec::new();
        ddc.process(x, &mut all);
        let r = ddc.out_rate();
        let (mut y, runs) = bursts_only(&all, r, bw);
        // One burst is enough (live, a slow carrier's: 1000 symbols, 3200
        // samples here).
        if y.len() < 1024 {
            return None;
        }
        let size = (1usize << (y.len() / 2).ilog2()).clamp(256, 4096);
        let mut cs = Spectrum::new(size);
        let mut cdb = cs.compute(&y);
        // Outside the carrier's band, the floor: as Identify does, so the
        // neighbours' skirts do not pass for this carrier's.
        let n = cdb.len();
        let inside = |k: usize| ((k as f64 / n as f64) - 0.5).abs() * r <= bw / 2.0 / 1.15;
        let mut within: Vec<f32> = (0..n).filter(|&k| inside(k)).map(|k| cdb[k]).collect();
        if within.len() >= 16 {
            within.sort_unstable_by(f32::total_cmp);
            let floor = within[within.len() * 3 / 100];
            for (k, d) in cdb.iter_mut().enumerate() {
                if !inside(k) {
                    *d = floor;
                }
            }
        }
        let band = estimate_band(&cdb, r, 0.99);
        let guess = band
            .map(|b| b.symbol_rate_hz)
            .filter(|&g| g > 0.0)
            .unwrap_or(if enbw > 0.0 { enbw } else { bw / 1.25 });
        // The bursts' own line, between the rates the band's width allows
        // (its estimate reads the occupied width: live, 7.9 kHz for a
        // 6.4 kS/s carrier of roll-off 0.2); else as one stretch; else it
        // waits for more bursts.
        let mut rates: Vec<f64> = burst_lines(&y, &runs, r, 0.7 * guess, 1.15 * guess)
            .into_iter()
            .filter(|l| l.1 >= BURST_LINE_DB)
            .take(RATE_TRIES)
            .map(|l| l.0)
            .collect();
        if let Some((rs, _)) = cyclic_symbol_rate(&y, r, guess, 0.2) {
            rates.push(rs);
        }
        // And a grid across the range, each point near enough for a burst's
        // own clock to be measured from it (one burst's line may be lost in
        // the noise: synthetic, 9 dB, under five noise peaks) — which also
        // gives the wrong rates the right one must stand clear of.
        let mut g = 0.7 * guess;
        while g <= 1.15 * guess {
            rates.push(g);
            g *= 1.0 + RATE_GRID;
        }
        // A rate is taken when the bursts demodulate at it (live, the first
        // look at one partial burst once read 5026 S/s for 6400; at 4400,
        // 5000, 5200, 5600 — simple fractions of it — bursts of idle fill
        // keep some coherence): else the carrier waits for more bursts.
        let longest = runs.iter().map(|r| r.len()).max().unwrap_or(0) as f64;
        let bar = RATE_COHERENCE.max(4.0 / (0.5 * longest * guess / r).max(1.0).sqrt()) as f32;
        // The carrier's centre from its bursts' own power line, and the
        // bursts brought there before their rate is chosen: the band's
        // centre is pulled by neighbours bursting at the same moment (live,
        // ~1 kHz on a 6.4 kS/s carrier — past the ±800 Hz a QPSK burst's
        // own carrier search reaches, so its rate never demodulated).
        let line = burst_carrier(&y, &runs, r);
        if let Some(f) = line {
            rotate(&mut y, f / r, 0.0);
        }
        let rs = best_rate(&y, &runs, r, &rates, bar)?;
        if r / rs < 2.0 {
            return None;
        }
        let shift = line.unwrap_or(band.map_or(0.0, |b| b.center_hz));
        // The roll-off fitted, but no sharper than `MIN_ALPHA`: a burst's
        // clock and timing come from the excess band the matched filter
        // lets through, and a fit to a few bursts with neighbours either
        // side reads low (live, 0.05–0.1 for a carrier of 0.2, whose
        // bursts' clocks then scattered ±30 Hz in 6400, ±3 at 0.2).
        let alpha = fit_roll_off(&cdb, r, shift, rs)
            .map_or(0.35, |ro| ro.as_f64())
            .max(MIN_ALPHA);
        Some(Channel::new(self.rate, centre + shift, rs, alpha))
    }

    /// Start or stop writing bursts (`dir`, files named with `tag`).
    pub fn set_recording(&mut self, on: bool, dir: &std::path::Path, tag: &str) {
        match (on, self.writer.is_some()) {
            (true, false) if !self.open_failed => {
                let stem = format!("decsat-{tag}-tdma-{}", crate::vfo::unix_stamp());
                let path = dir.join(format!("{stem}.bursts"));
                let _ = std::fs::create_dir_all(dir);
                let open = || -> std::io::Result<Writer> {
                    let mut bin = BufWriter::new(File::create(&path)?);
                    bin.write_all(b"DSTDMA01")?;
                    let mut csv = BufWriter::new(File::create(dir.join(format!("{stem}.csv")))?);
                    writeln!(
                        csv,
                        "channel,time_s,duration_s,power_db,freq_hz,mer_db,idle,fitted,symbols,header"
                    )?;
                    Ok(Writer {
                        bin,
                        csv,
                        path: path.clone(),
                        bursts: 0,
                    })
                };
                match open() {
                    Ok(w) => self.writer = Some(w),
                    Err(_) => {
                        self.open_failed = true;
                        self.last_file = Some((path, 0));
                    }
                }
            }
            (false, true) => {
                if let Some(mut w) = self.writer.take() {
                    let _ = w.bin.flush();
                    let _ = w.csv.flush();
                    self.last_file = Some((w.path, w.bursts));
                }
            }
            (false, false) => self.open_failed = false,
            _ => {}
        }
    }

    /// The file being written (or the last), its bursts, and whether it is
    /// open now.
    pub fn recording(&self) -> Option<(PathBuf, u64, bool)> {
        match &self.writer {
            Some(w) => Some((w.path.clone(), w.bursts, true)),
            None => self.last_file.clone().map(|(p, n)| (p, n, false)),
        }
    }

    pub fn open_failed(&self) -> bool {
        self.open_failed
    }

    /// Recent symbols for the constellation (busy bursts' data).
    pub fn scatter(&self) -> Vec<Iq> {
        self.scatter.iter().copied().collect()
    }

    pub fn view(&self) -> TdmaView {
        let channels: Vec<ChannelView> = self.channels.iter().map(|c| c.view()).collect();
        let per_s: f32 = channels.iter().map(|c| c.per_s).sum();
        let message = if !self.started || channels.is_empty() {
            self.message.clone()
        } else {
            let layouts = channels.iter().filter(|c| c.layout.is_some()).count();
            format!(
                "{} carrier{} · {:.0} bursts/s · {} of {} layouts read",
                channels.len(),
                if channels.len() == 1 { "" } else { "s" },
                per_s,
                layouts,
                channels.len()
            )
        };
        TdmaView {
            channels,
            marks: self.marks.iter().copied().collect(),
            now_s: self.samples as f64 / self.rate,
            progress: if self.started {
                1.0
            } else {
                // Up to the first look; then the wait for a burst.
                let s = self.samples as f64 / self.rate;
                (s / LOOK_MAX_S).max((s / FIRST_LOOK_S).min(1.0) * 0.1) as f32
            },
            message,
        }
    }
}

/// A carrier's bursts alone, end to end, from its baseband `y` at `rate`
/// (`bw` wide): blocks of ~16 symbols whose power stands 3 dB over the
/// quietest tenth, with a block either side for the ramps; and where each
/// run of them lies in what is returned. A continuous carrier keeps
/// everything.
fn bursts_only(y: &[Iq], rate: f64, bw: f64) -> (Vec<Iq>, Vec<std::ops::Range<usize>>) {
    let blk = ((rate / bw * 16.0) as usize).max(16);
    let n = y.len() / blk;
    if n < 8 {
        // Rust note: `std::iter::once(…).collect()` rather than `vec![0..n]`,
        // which reads like a vector of the numbers 0 to n.
        return (y.to_vec(), std::iter::once(0..y.len()).collect());
    }
    let p: Vec<f32> = y
        .chunks_exact(blk)
        .map(|c| c.iter().map(|s| s.norm_sqr()).sum::<f32>() / blk as f32)
        .collect();
    let mut sorted = p.clone();
    sorted.sort_by(f32::total_cmp);
    let floor = sorted[n / 10].max(1e-30);
    let mut on: Vec<bool> = p.iter().map(|&v| v > 2.0 * floor).collect();
    // A burst's envelope dips under the bar now and then (live, a slow
    // carrier's 150 ms bursts came out in a dozen pieces): gaps of a few
    // blocks are closed, so each burst stays one run.
    let mut last_on: Option<usize> = None;
    for i in 0..n {
        if on[i] {
            if let Some(l) = last_on
                && i - l <= BURST_GAP_BLOCKS
            {
                on[l..i].iter_mut().for_each(|v| *v = true);
            }
            last_on = Some(i);
        }
    }
    // And a lone block or two over the bar is noise (live, one in fifty
    // between a slow carrier's bursts).
    let mut i = 0;
    while i < n {
        let j = (i..n).find(|&j| !on[j]).unwrap_or(n);
        if j - i < BURST_MIN_BLOCKS {
            on[i..j].iter_mut().for_each(|v| *v = false);
        }
        i = j + 1;
    }
    let mut out = Vec::new();
    let mut runs: Vec<std::ops::Range<usize>> = Vec::new();
    let mut was = false;
    for i in 0..n {
        let near = on[i] || (i > 0 && on[i - 1]) || (i + 1 < n && on[i + 1]);
        if near {
            if !was {
                runs.push(out.len()..out.len());
            }
            out.extend_from_slice(&y[i * blk..(i + 1) * blk]);
            if let Some(r) = runs.last_mut() {
                r.end = out.len();
            }
        }
        was = near;
    }
    (out, runs)
}

/// Where a carrier is, Hz from the centre of `y` (its baseband at `rate`):
/// the line the bursts' samples leave raised to the power that strips the
/// modulation — the square for BPSK, the fourth power for QPSK (and for a
/// BPSK preamble) — the stronger, over the longest few bursts; `None` when
/// neither stands out (8PSK, or too little signal). At the sample level its
/// reach is a quarter of the sample rate either side for QPSK.
fn burst_carrier(y: &[Iq], runs: &[std::ops::Range<usize>], rate: f64) -> Option<f64> {
    let mut longest: Vec<&std::ops::Range<usize>> = runs.iter().collect();
    longest.sort_by_key(|r| std::cmp::Reverse(r.len()));
    longest.truncate(RATE_TRY_BURSTS);
    let mut best: Option<(f64, f32)> = None;
    for m in [2u32, 4] {
        // The bursts end to end: each line's phase differs, but a burst
        // holds it for thousands of samples.
        let seg: Vec<Iq> = longest
            .iter()
            .flat_map(|r| y[(*r).clone()].iter().copied())
            .collect();
        if seg.len() < 256 {
            return None;
        }
        let (f, strength) = demod::power_line(&seg, m);
        if best.is_none_or(|b| strength > b.1) {
            best = Some((f * rate, strength));
        }
    }
    let n = longest.iter().map(|r| r.len()).sum::<usize>().max(1) as f32;
    best.filter(|b| b.1 > CARRIER_LINE.max(4.0 / n.sqrt()))
        .map(|b| b.0)
}

/// How strong a burst's power line must be to place its carrier.
const CARRIER_LINE: f32 = 0.05;

/// Lines in a carrier's bursts' squared envelope that may be its symbol
/// rate: the spectrum taken over each burst on its own and summed — the
/// symbol-rate line's phase differs from one burst to the next, so the
/// bursts add in power, where run end to end they partly cancel — and its
/// strongest peaks between `lo` and `hi` Hz, each with how far it stands
/// over the median there, dB, strongest first. (Live, the symbol-rate line
/// of a slow carrier's bursts stood 20 dB clear, but its data repeated and
/// put another line nearly as strong 12 % lower: which is which is for the
/// demodulator to say.)
fn burst_lines(
    y: &[Iq],
    runs: &[std::ops::Range<usize>],
    rate: f64,
    lo: f64,
    hi: f64,
) -> Vec<(f64, f32)> {
    let Some(longest) = runs.iter().map(|r| r.len()).max() else {
        return Vec::new();
    };
    if longest < 256 {
        return Vec::new();
    }
    // Zero-padded four times over, for the peak's place between bins.
    let n = (4 * longest).next_power_of_two().min(1 << 18);
    let fft = rustfft::FftPlanner::new().plan_fft_forward(n);
    let mut acc = vec![0f32; n / 2];
    let mut buf = vec![Iq::new(0.0, 0.0); n];
    for r in runs.iter().filter(|r| 4 * r.len() >= longest) {
        let seg = &y[r.start..r.start + r.len().min(n)];
        let m = seg.len();
        let mean = seg.iter().map(|s| s.norm_sqr()).sum::<f32>() / m as f32;
        buf.iter_mut().for_each(|b| *b = Iq::new(0.0, 0.0));
        for (k, s) in seg.iter().enumerate() {
            // Hann over the burst, so its ends do not spread the line.
            let w = 0.5 - 0.5 * (std::f32::consts::TAU * k as f32 / m as f32).cos();
            buf[k] = Iq::new((s.norm_sqr() - mean) * w, 0.0);
        }
        fft.process(&mut buf);
        for (a, b) in acc.iter_mut().zip(&buf) {
            *a += b.norm_sqr();
        }
    }
    let kl = (lo / rate * n as f64).ceil() as usize;
    let kh = ((hi / rate * n as f64).floor() as usize).min(n / 2 - 2);
    if kh < kl + 8 || kl < 2 {
        return Vec::new();
    }
    let mut band: Vec<f32> = acc[kl..=kh].to_vec();
    let mid = band.len() / 2;
    let median = band
        .select_nth_unstable_by(mid, f32::total_cmp)
        .1
        .max(1e-30);
    // Peaks: the highest bin within a main lobe's width (a burst's Hann
    // window, zero-padded four times: ±8 bins) either side.
    let lobe = 8;
    let mut peaks: Vec<(f64, f32)> = (kl..=kh)
        .filter(|&k| {
            let (a, b) = (k.saturating_sub(lobe).max(kl), (k + lobe).min(kh));
            (a..=b).all(|j| acc[j] <= acc[k])
        })
        .map(|k| {
            let (l, peak, r) = (acc[k - 1], acc[k], acc[k + 1]);
            let den = l - 2.0 * peak + r;
            let frac = if den.abs() > 1e-30 {
                (0.5 * (l - r) / den).clamp(-0.5, 0.5)
            } else {
                0.0
            };
            (
                (k as f64 + frac as f64) * rate / n as f64,
                10.0 * (peak / median).log10(),
            )
        })
        .collect();
    peaks.sort_by(|a, b| b.1.total_cmp(&a.1));
    peaks
}

/// Of the symbol `rates` offered, the one the longest few of `runs`
/// demodulate best at — through a matched filter at that rate, each burst's
/// own clock measured from there (within 1.5 %), then the rates just either
/// side read as they stand — as measured, if it stands clear of the rest on
/// most of those bursts. "Best" is the coherence of the constellation of a
/// burst's second half: the strength of the line its M-th power leaves (1 a
/// clean constellation; live QPSK at 9 dB 0.6–0.77, the rates tried around
/// it a median of 0.36–0.46). The first half may be a preamble on one line
/// through the origin (live, 180 BPSK symbols), which any rate leaves on
/// that line; MER does not tell rates apart (at a wrong one a live burst
/// still read 5–6 dB, against 6–9); and with dozens of rates tried, one
/// wrong rate may stand clear on one burst by chance, but not on most.
fn best_rate(
    y: &[Iq],
    runs: &[std::ops::Range<usize>],
    rate: f64,
    rates: &[f64],
    bar: f32,
) -> Option<f64> {
    let mut longest: Vec<&std::ops::Range<usize>> = runs.iter().collect();
    longest.sort_by_key(|r| std::cmp::Reverse(r.len()));
    // The few longest, and none much shorter than the longest (a scrap of
    // a burst says little).
    let top = longest.first()?.len();
    longest.retain(|r| 2 * r.len() >= top);
    longest.truncate(RATE_TRY_BURSTS);
    let n = longest.len();
    // The bursts read at `rs`, from there at each one's own clock or not:
    // the rate they ran at (their mean), and each one's coherence (0 for
    // one that would not demodulate).
    let score = |rs: f64, own_clock: bool| -> Option<(f64, Vec<f32>)> {
        let sps = rate / rs;
        let (mut clock, mut read) = (0.0, 0);
        let mut coherence = Vec::with_capacity(n);
        for run in &longest {
            // At most a few thousand symbols of it.
            let end = run
                .end
                .min(run.start + (RATE_TRY_SYMBOLS as f64 * sps) as usize);
            let mut mf = Fir::new(rrc_taps(sps, 0.2, 6));
            let mut x = Vec::new();
            mf.process(&y[run.start..end], &mut x);
            let Some(b) = demod_burst(&x, 0..x.len(), sps, 0, None, None, own_clock) else {
                coherence.push(0.0);
                continue;
            };
            // The fourth power for all but 8PSK (BPSK's is as clean as its
            // square): one measure, so rates read as different
            // constellations compare (a wrong rate read as BPSK beat the
            // right one read as QPSK on its square).
            let m = if b.modulation == Modulation::Psk8 {
                8
            } else {
                4
            };
            let half = &b.symbols[b.symbols.len() / 2..];
            clock += if own_clock { rate / b.sps } else { rs };
            coherence.push(demod::power_line(half, m).1);
            read += 1;
        }
        (read > 0).then(|| (clock / read as f64, coherence))
    };
    let mean = |c: &[f32]| c.iter().sum::<f32>() / c.len().max(1) as f32;
    let tried: Vec<(f64, Vec<f32>)> = rates
        .iter()
        .filter(|&&rs| rate / rs >= 2.0)
        .filter_map(|&rs| score(rs, true))
        .collect();
    // Each burst's median over the rates tried.
    let medians: Vec<f32> = (0..n)
        .map(|j| {
            let mut c: Vec<f32> = tried.iter().map(|t| t.1[j]).collect();
            c.sort_by(f32::total_cmp);
            c.get(c.len() / 2).copied().unwrap_or(0.0)
        })
        .collect();
    let mut best = tried
        .iter()
        .max_by(|a, b| mean(&a.1).total_cmp(&mean(&b.1)))?
        .clone();
    // Finer than a burst's own clock can be measured at low SNR (live and
    // synthetic, 0.1 % on 1000 symbols at 9 dB, which a burst feels): the
    // rates either side, each read as it stands, the most coherent kept.
    let around = best.0;
    for k in -RATE_FINE_STEPS..=RATE_FINE_STEPS {
        let rs = around * (1.0 + k as f64 * RATE_FINE);
        if let Some(t) = score(rs, false)
            && mean(&t.1) > mean(&best.1)
        {
            best = t;
        }
    }
    // On most bursts it must be (near enough) each one's own best, and
    // stand clear of the median: a wrong rate wins on one burst's chance
    // (live, bursts that would not demodulate at their carrier's rate —
    // terminals colliding — gave wrong winners up to 0.49 on one burst and
    // 0.2 on the next). One burst alone must be clean beyond doubt (the
    // right rate on one live burst read 0.40–0.72; a wrong one up to 0.49).
    let each_best: Vec<f32> = (0..n)
        .map(|j| tried.iter().map(|t| t.1[j]).fold(0.0, f32::max))
        .collect();
    let bar = if n == 1 {
        bar.max(RATE_COHERENCE_ALONE)
    } else {
        bar
    };
    let clear = (0..n)
        .filter(|&j| {
            best.1[j] >= bar
                && best.1[j] >= RATE_STANDS_OUT * medians[j]
                && best.1[j] >= RATE_NEAR_BEST * each_best[j]
        })
        .count();
    (3 * clear >= 2 * n).then_some(best.0)
}

fn write_burst(w: &mut Writer, d: &Done) -> std::io::Result<()> {
    let pts = points(d.modulation);
    let bits: u8 = match d.modulation {
        Modulation::Bpsk => 1,
        Modulation::Psk8 => 3,
        _ => 2,
    };
    let b = &mut w.bin;
    b.write_all(&[d.channel as u8])?;
    b.write_all(&d.t_s.to_le_bytes())?;
    b.write_all(&d.power_db.to_le_bytes())?;
    b.write_all(&(d.freq_hz as f32).to_le_bytes())?;
    b.write_all(&d.mer_db.to_le_bytes())?;
    b.write_all(&[(d.idle as u8) | ((d.fitted as u8) << 1), bits])?;
    b.write_all(&(d.symbols.len() as u32).to_le_bytes())?;
    let labels: Vec<u8> = d.symbols.iter().map(|&s| nearest(s, pts) as u8).collect();
    b.write_all(&labels)?;
    writeln!(
        w.csv,
        "{},{:.7},{:.7},{:.2},{:.1},{:.2},{},{},{},{}",
        d.channel,
        d.t_s,
        d.dur_s,
        d.power_db,
        d.freq_hz,
        d.mer_db,
        d.idle as u8,
        d.fitted as u8,
        d.symbols.len(),
        d.header
    )?;
    w.bursts += 1;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::demod::tests::{Lcg, shaped};
    use super::layout::tests::live_like;
    use super::*;

    /// Two TDMA carriers side by side at a VFO's rate: 856-symbol bursts of
    /// the live layout in 870-symbol slots, with empty slots and terminals
    /// of different power and offset.
    fn group(seconds: f64, rate: f64, rs: f64, r: &mut Lcg) -> Vec<Iq> {
        let p = points(Modulation::Qpsk);
        let fixed: Vec<Iq> = (0..856).map(|_| p[(r.next() % 4) as usize]).collect();
        let fill: Vec<Iq> = (0..900).map(|_| p[(r.next() % 4) as usize]).collect();
        let n = (seconds * rate) as usize;
        let mut out = vec![Iq::new(0.0, 0.0); n];
        let sps = rate / rs;
        let slot = 870.0 * sps;
        for (c, centre) in [-0.35 * rate, 0.25 * rate].into_iter().enumerate() {
            let mut t = 1000.0 + c as f64 * 333.0;
            let mut k = 0usize;
            while t + slot < n as f64 {
                k += 1;
                if k % 12 != 5 {
                    let term = (k * 7 + c) % 3;
                    let mut b = live_like(r, k.is_multiple_of(3), &fill, &fixed);
                    // Each terminal's own value in the preamble's field —
                    // values no turn or one-symbol shift of another makes
                    // (a field stepping a quarter turn a symbol was read
                    // the same for every terminal off by a symbol).
                    const IDS: [[usize; 5]; 3] =
                        [[0, 1, 1, 3, 2], [2, 0, 3, 3, 1], [1, 3, 0, 2, 2]];
                    for (i, s) in b[13..18].iter_mut().enumerate() {
                        *s = p[(IDS[term][i] + c) % 4];
                    }
                    let f = [0.0005, -0.0008, 0.0012][term];
                    let g = [1.0f32, 0.6, 1.4][term];
                    let x = shaped(&b, sps, f, 60.0, r);
                    let t0 = t as usize;
                    for (i, v) in x.iter().enumerate() {
                        if t0 + i < n {
                            let a = std::f64::consts::TAU * centre / rate * (t0 + i) as f64;
                            out[t0 + i] += v * g * Iq::new(a.cos() as f32, a.sin() as f32);
                        }
                    }
                }
                t += slot;
            }
        }
        for v in &mut out {
            *v += Iq::new(r.gauss() * 0.12, r.gauss() * 0.12);
        }
        out
    }

    #[test]
    fn waits_for_a_slow_carrier_and_finds_one_that_starts_later() {
        // 50 kBd QPSK bursts of 2000 symbols (40 ms), one every 2 s at
        // −80 kHz from 0.9 s; from 7 s another such carrier at +90 kHz;
        // in a 400 kS/s VFO, Es/N0 ~15 dB.
        let mut r = Lcg(5);
        let (rate, rs) = (400e3, 50e3);
        let sps = rate / rs;
        let p = points(Modulation::Qpsk);
        let n = (16.0 * rate) as usize;
        let mut x = vec![Iq::new(0.0, 0.0); n];
        for (centre, from) in [(-80e3, 0.9), (90e3, 7.0)] {
            let mut t = from;
            while t + 0.1 < 16.0 {
                let sym: Vec<Iq> = (0..2000).map(|_| p[(r.next() % 4) as usize]).collect();
                let b = shaped(&sym, sps, 0.0, 60.0, &mut r);
                let t0 = (t * rate) as usize;
                for (i, v) in b.iter().enumerate() {
                    let a = std::f64::consts::TAU * centre / rate * (t0 + i) as f64;
                    x[t0 + i] += v * Iq::new(a.cos() as f32, a.sin() as f32);
                }
                t += 2.0;
            }
        }
        for v in &mut x {
            *v += Iq::new(r.gauss() * 0.13, r.gauss() * 0.13);
        }
        let mut a = TdmaAnalyser::new(rate, rate * 0.9);
        let mut seen_at = None;
        for (k, c) in x.chunks(8_000).enumerate() {
            a.push(c, None);
            if seen_at.is_none() && !a.view().channels.is_empty() {
                seen_at = Some((k + 1) as f64 * 8_000.0 / rate);
            }
        }
        // The first burst comes at 0.9 s: waited for, not missed at 0.4 s.
        let seen = seen_at.expect("the slow carrier found");
        assert!((0.9..2.0).contains(&seen), "found at {seen} s");
        let v = a.view();
        assert_eq!(v.channels.len(), 2, "{}", v.message);
        let late = &v.channels[1];
        assert!((late.center_hz - 90e3).abs() < 3e3, "{}", late.center_hz);
        assert!(
            (late.symbol_rate - rs).abs() < rs * 0.02,
            "{}",
            late.symbol_rate
        );
        // Its bursts from before it was found count too, and every channel's
        // bursts are timed on the VFO's clock.
        assert!(late.bursts >= 4, "{} bursts", late.bursts);
        assert!(v.channels[0].bursts >= 7, "{} bursts", v.channels[0].bursts);
    }

    #[test]
    fn reads_a_slow_group_like_the_live_one() {
        // As live (Intelsat 19): 6.4 kS/s QPSK carriers 7.7 kHz apart, all
        // bursting together once every 2 s — 1000 symbols, 180 of them a
        // BPSK preamble — at Es/N0 ~9 dB, in a 48 kS/s VFO.
        let mut r = Lcg(41);
        let (rate, rs) = (48_000.0, 6400.0);
        let sps = rate / rs;
        let p = points(Modulation::Qpsk);
        let pre: Vec<Iq> = (0..180).map(|_| p[2 * (r.next() % 2) as usize]).collect();
        let n = (24.0 * rate) as usize;
        let sigma = (0.5f64 * (1.0 / 10f64.powf(0.9)) * rate / rs).sqrt() as f32;
        let mut x: Vec<Iq> = (0..n)
            .map(|_| Iq::new(r.gauss() * sigma, r.gauss() * sigma))
            .collect();
        let centres = [-7700.0, 0.0, 7700.0];
        let mut t = 0.6;
        while t + 0.2 < 24.0 {
            for &centre in &centres {
                let mut sym = pre.clone();
                sym.extend((0..820).map(|_| p[(r.next() % 4) as usize]));
                let b = shaped(&sym, sps, 0.0, 60.0, &mut r);
                let t0 = (t * rate) as usize;
                for (i, v) in b.iter().enumerate() {
                    let a = std::f64::consts::TAU * centre / rate * (t0 + i) as f64;
                    x[t0 + i] += v * Iq::new(a.cos() as f32, a.sin() as f32);
                }
            }
            t += 2.0;
        }
        let mut a = TdmaAnalyser::new(rate, 30_000.0);
        let mut seen_at = None;
        for (k, c) in x.chunks(4_800).enumerate() {
            a.push(c, None);
            if seen_at.is_none() && a.view().channels.len() == 3 {
                seen_at = Some((k + 1) as f64 * 0.1);
            }
        }
        // All three at the first burst.
        let seen = seen_at.expect("three carriers");
        assert!(seen < 1.5, "found at {seen} s");
        let v = a.view();
        assert_eq!(v.channels.len(), 3, "{}", v.message);
        for (c, want) in v.channels.iter().zip(centres) {
            assert!((c.center_hz - want).abs() < 300.0, "centre {}", c.center_hz);
            assert!(
                (c.symbol_rate - rs).abs() < rs * 1e-3,
                "rate {}",
                c.symbol_rate
            );
            assert_eq!(c.modulation, Some(Modulation::Qpsk));
            assert!(c.mer_db > 7.0, "MER {}", c.mer_db);
            // Every burst, and nothing between them.
            assert!((11..=12).contains(&c.bursts), "{} bursts", c.bursts);
            assert_eq!(c.len_symbols.map(|l| l.abs_diff(1000) <= 6), Some(true));
        }
    }

    #[test]
    fn finds_a_group_its_layouts_and_slots() {
        let mut r = Lcg(21);
        let rate = 2.0e6;
        let rs = 400e3;
        let x = group(1.6, rate, rs, &mut r);
        let mut a = TdmaAnalyser::new(rate, rate * 0.95);
        for c in x.chunks(20_000) {
            a.push(c, None);
        }
        let v = a.view();
        assert_eq!(v.channels.len(), 2, "{}", v.message);
        for c in &v.channels {
            assert!(
                (c.symbol_rate - rs).abs() < rs * 0.01,
                "rate {}",
                c.symbol_rate
            );
            assert_eq!(c.modulation, Some(Modulation::Qpsk));
            let l = c.layout.as_ref().expect("a layout");
            assert_eq!(
                l.pilots.map(|p| (p.period, p.block)),
                Some((30, 2)),
                "{}",
                l.describe()
            );
            let g = c.grid.expect("a slot grid");
            assert!((g.period - 870.0).abs() < 0.2, "{g:?}");
            // Every twelfth slot empty: a frame of twelve.
            let f = g.frame.expect("a frame");
            assert!((f.period - 12.0 * 870.0).abs() < 3.0, "{f:?}");
            assert!(c.terminals.len() >= 3, "{:?}", c.terminals);
            assert!(c.mer_db > 10.0, "MER {}", c.mer_db);
            // The header: the terminals' field in the preamble, changing
            // from slot to slot as the terminals take turns (three values,
            // and now and then a symbol misread).
            assert_eq!(c.header.len(), 1, "{:?}", c.header);
            let h = &c.header[0];
            assert!((4..=5).contains(&(h.to - h.from + 1)), "{h:?}");
            assert!(h.distinct >= 3 && h.commonest_share < 0.6, "{h:?}");
            assert!(h.change_share > 0.4 && h.bursts > 100, "{h:?}");
        }
    }
}
