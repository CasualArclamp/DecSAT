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
//! Recording writes every burst's data symbols to a `.bursts` file (and a
//! line per burst to a `.csv`), for working out the coding offline:
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
pub mod demod;
pub mod layout;
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
use crate::identify::{cyclic_symbol_rate, fit_roll_off, identify_in};
use crate::spectrum::Spectrum;

use burst::{BurstDetector, Levels, RawBurst};
use demod::{BurstSymbols, demod_burst, nearest, points};
use layout::{Layout, commonest_length, find_layout, fit};
use slots::{BurstTime, SlotGrid, Terminal, Terminals, TerminalsById, find_grid};

/// Signal looked at before the carriers are looked for, seconds.
const FIRST_LOOK_S: f64 = 0.4;
/// Bursts that vote on the modulation before it is fixed.
const MODULATION_VOTES: u32 = 30;
/// Bursts gathered to read a layout from (and tried again with every
/// `LAYOUT_RETRY` more, up to `LAYOUT_KEEP`).
const LAYOUT_FIRST: usize = 120;
const LAYOUT_RETRY: usize = 100;
const LAYOUT_KEEP: usize = 400;
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
    /// Burst starts (channel samples) and lengths in the last second.
    recent: VecDeque<(u64, u64)>,
    mer_db: f32,
    snr_db: f32,
    idle_share: f32,
    code: CodeSearch,
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
            recent: VecDeque::new(),
            mer_db: 0.0,
            snr_db: 0.0,
            idle_share: 0.0,
            mixed: Vec::new(),
            filtered: Vec::new(),
            raw: Vec::new(),
            code: CodeSearch::default(),
        }
    }

    fn rate(&self) -> f64 {
        self.front.rate()
    }

    /// Take a block of the VFO's baseband; bursts finished in it to `done`.
    fn push(&mut self, index: usize, x: &[Iq], fixed: Option<Modulation>, done: &mut Vec<Done>) {
        self.mixed.clear();
        self.front.process(x, &mut self.mixed);
        self.filtered.clear();
        self.mf.process(&self.mixed, &mut self.filtered);
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
        )?;
        if modulation.is_none() {
            let k = match b.modulation {
                Modulation::Bpsk => 0,
                Modulation::Psk8 => 2,
                _ => 1,
            };
            self.votes[k] += 1;
            if self.votes.iter().sum::<u32>() >= MODULATION_VOTES {
                let best = (0..3).max_by_key(|&i| self.votes[i]).unwrap_or(1);
                self.modulation =
                    Some([Modulation::Bpsk, Modulation::Qpsk, Modulation::Psk8][best]);
            }
        }
        self.bursts += 1;
        let len = b.symbols.len();
        self.lens.push(len as u32);
        if self.lens.len() > 4000 {
            self.lens.drain(..2000);
        }
        self.recent
            .push_back((r.start + on.start as u64, on.len() as u64));
        let floor = r.floor.max(1e-20);
        let snr = 10.0 * ((10f32.powf(b.power_db / 10.0) - floor).max(1e-20) / floor).log10();
        self.snr_db += (snr - self.snr_db) * if self.bursts == 1 { 1.0 } else { 0.02 };

        // Gather for the layout until one is read: the newest
        // `LAYOUT_KEEP`, tried with the first `LAYOUT_FIRST` and again with
        // every `LAYOUT_RETRY` more.
        if self.layout.is_none() && self.modulation.is_some() {
            if self.gathered.len() >= LAYOUT_KEEP {
                self.gathered.remove(0);
            }
            self.gathered.push(b.symbols.clone());
            self.since_try += 1;
            if self.gathered.len() >= LAYOUT_FIRST
                && (!self.tried || self.since_try >= LAYOUT_RETRY)
            {
                self.tried = true;
                self.since_try = 0;
                self.read_layout();
            }
        }
        let modulation_now = self.modulation.unwrap_or(b.modulation);
        let mut field = Vec::new();
        let (t_sym, symbols, mer, idle, fitted) = match self.layout.as_ref().and_then(|l| {
            // Only bursts of the layout's length.
            ((len as f64 - l.len as f64).abs() <= l.len as f64 * 0.02 + 1.0)
                .then(|| fit(l, &b.symbols))
                .flatten()
        }) {
            Some(f) => {
                field = f.field;
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
        self.mer_db += (mer - self.mer_db) * if self.bursts == 1 { 1.0 } else { 0.02 };
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
            t_s: b.t0 / self.rate(),
            dur_s: on.len() as f64 / self.rate(),
            power_db: b.power_db,
            freq_hz,
            mer_db: mer,
            idle,
            fitted,
            modulation: modulation_now,
            symbols,
        })
    }

    fn read_layout(&mut self) {
        let lens: Vec<usize> = self.gathered.iter().map(|g| g.len()).collect();
        let Some((len, _)) = commonest_length(&lens) else {
            return;
        };
        let refs: Vec<&[Iq]> = self.gathered.iter().map(|g| g.as_slice()).collect();
        if let Some(l) = find_layout(&refs, len, self.modulation.unwrap_or(Modulation::Qpsk)) {
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

/// A TDMA VFO's decoder.
pub struct TdmaAnalyser {
    rate: f64,
    bandwidth: f64,
    first: Vec<Iq>,
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
            first: Vec::new(),
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
        if !self.started {
            self.first.extend_from_slice(x);
            if (self.first.len() as f64) < FIRST_LOOK_S * self.rate {
                return;
            }
            self.started = true;
            let first = std::mem::take(&mut self.first);
            self.channels = self.find_channels(&first);
            self.message = match self.channels.len() {
                0 => "no TDMA carrier found in this VFO".into(),
                n => format!(
                    "{n} carrier{} — finding bursts",
                    if n == 1 { "" } else { "s" }
                ),
            };
            self.feed(&first, modulation);
            return;
        }
        self.feed(x, modulation);
    }

    fn feed(&mut self, x: &[Iq], modulation: Option<Modulation>) {
        let mut done = std::mem::take(&mut self.done);
        done.clear();
        for (i, c) in self.channels.iter_mut().enumerate() {
            c.push(i, x, modulation, &mut done);
        }
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

    /// The carriers in the first look, each with its centre, symbol rate
    /// and roll-off: the centre from the carrier's own spectrum, the rate
    /// refined on its envelope's cyclic line from the spectrum's estimate,
    /// the roll-off fitted to the skirts — the parts of Identify that matter
    /// here (all of it took 0.6 s a carrier at 1.4 MS/s, on the VFO's
    /// thread). A VFO around one carrier, with no floor either side to see
    /// it by, is taken as that carrier.
    fn find_channels(&self, x: &[Iq]) -> Vec<Channel> {
        let mut spec = Spectrum::new(4096);
        let db = spec.compute(x);
        let bin = self.rate / db.len() as f64;
        let mut cands: Vec<(f64, f64, f64)> = detect_carriers(&db, self.rate, 4.0)
            .iter()
            .filter(|c| !c.narrow && c.bandwidth_hz >= 16.0 * bin)
            .filter(|c| c.center_hz.abs() <= self.bandwidth / 2.0)
            .map(|c| (c.center_hz, c.bandwidth_hz, c.symbol_rate_hz))
            .collect();
        if cands.is_empty() {
            cands.push((0.0, self.bandwidth, 0.0));
        }
        let mut out = Vec::new();
        for (centre, bw, enbw) in cands {
            let bw = (bw * 1.15).min(self.rate);
            let mut ddc = Ddc::new(self.rate, centre, bw);
            let mut y = Vec::new();
            ddc.process(x, &mut y);
            let r = ddc.out_rate();
            let mut cs = Spectrum::new(4096);
            let mut cdb = cs.compute(&y);
            // Outside the carrier's band, the floor: as Identify does, so
            // the neighbours' skirts do not pass for this carrier's.
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
            let rs = match cyclic_symbol_rate(&y, r, guess, 0.2) {
                Some((rs, _)) => rs,
                // No clean line (bursts too short or too weak to show one):
                // Identify, all of it.
                None => match identify_in(&y, r, Some(bw)).symbol_rate {
                    Some(rs) => rs,
                    None => continue,
                },
            };
            if r / rs < 2.0 {
                continue;
            }
            let shift = band.map_or(0.0, |b| b.center_hz);
            let alpha = fit_roll_off(&cdb, r, shift, rs).map_or(0.35, |ro| ro.as_f64());
            out.push(Channel::new(self.rate, centre + shift, rs, alpha));
        }
        out.sort_by(|a, b| a.center_hz.total_cmp(&b.center_hz));
        out
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
                        "channel,time_s,duration_s,power_db,freq_hz,mer_db,idle,fitted,symbols"
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
        let message = if !self.started {
            "listening…".into()
        } else if channels.is_empty() {
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
                (self.first.len() as f64 / (FIRST_LOOK_S * self.rate)) as f32
            },
            message,
        }
    }
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
        "{},{:.7},{:.7},{:.2},{:.1},{:.2},{},{},{}",
        d.channel,
        d.t_s,
        d.dur_s,
        d.power_db,
        d.freq_hz,
        d.mer_db,
        d.idle as u8,
        d.fitted as u8,
        d.symbols.len()
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
                    // Each terminal's own value in the preamble's field.
                    for (i, s) in b[13..18].iter_mut().enumerate() {
                        *s = p[(term * 5 + i * 3 + c) % 4];
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
        }
    }
}
